//! agent-rdp: CLI tool for AI agents to control Windows Remote Desktop sessions.

mod cli;
mod ipc_client;
mod output;
mod session_manager;

use clap::Parser;
use tracing::error;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands};

/// Fewest tokio worker threads the daemon runs with.
///
/// The frame processor holds a synchronous `parking_lot` write lock across
/// RDP frame processing (including synchronous RDPDR disk I/O), and handlers
/// block a worker on the matching read lock. With `worker_threads` defaulting
/// to the CPU count, a 2-vCPU host had both workers parked on that lock at
/// once and nothing left to answer the CLI's health-check `Ping` - which the
/// CLI then reported as the daemon having died.
const DAEMON_MIN_WORKER_THREADS: usize = 4;

fn main() {
    let is_daemon = {
        let args: Vec<String> = std::env::args().collect();
        args.windows(2)
            .any(|w| w[0] == "session" && w[1] == "daemon")
    };

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if is_daemon {
        let available = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        builder.worker_threads(available.max(DAEMON_MIN_WORKER_THREADS));
    }
    let runtime = builder.build().expect("failed to build tokio runtime");
    runtime.block_on(async_main(is_daemon));
}

async fn async_main(is_daemon: bool) {
    // Initialize logging.
    //
    // EnvFilter defaults to ERROR-only when RUST_LOG is unset, which is right
    // for the CLI (INFO would pollute command output) but useless for the
    // daemon: it runs detached with its output going to <session>/daemon.log,
    // and that log is the only record of why a session ended. Default the
    // daemon to info so an unexpected teardown is explained; RUST_LOG still
    // overrides either way.
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else if is_daemon {
        EnvFilter::new("info")
    } else {
        EnvFilter::from_default_env()
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // A panic in a spawned task does not end the daemon, but it does end
    // whatever that task was doing - a client connection, a capture, a
    // supervisor. Log it in daemon.log's own format (timestamped, marked) in
    // addition to the default hook, so a report that says "the command just
    // returned EOF" can be matched to the moment something went wrong.
    if is_daemon {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown location".to_string());
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            error!("DAEMON PANIC at {}: {}", location, message);
            default_hook(info);
        }));
    }

    let cli = Cli::parse();
    let json = cli.json;
    let session = cli.session.clone();
    let command = command_label(&cli);

    // The daemon is the one invocation that must run forever. It used to be
    // wrapped like everything else, so every daemon exited 90s after it was
    // spawned - the "daemon dies between commands" and "EOF while parsing a
    // value" reports of two releases. The decision is made from the parsed
    // command, not the argv heuristic used for runtime sizing above, which
    // `automate run session daemon` would also match.
    let run_result = match watchdog_budget_ms(&cli) {
        None => run(cli).await,
        Some(watchdog_ms) => match tokio::time::timeout(
            std::time::Duration::from_millis(watchdog_ms),
            run(cli),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                // A defense-in-depth backstop, not the primary fix: every
                // daemon-side wait that legitimately blocks (locate --wait,
                // automate run --process-timeout, connect) already extends
                // its own IPC timeout past the CLI default - this only fires
                // if something hangs *past* that extended budget, e.g. a
                // daemon handler stuck on a dead socket read with no timeout
                // of its own. A CLI command must never hang indefinitely
                // regardless of which internal path misbehaves.
                let message = format!(
                    "`{}` exceeded its {}s watchdog budget and was aborted (base {}s + the \
                     command's own budget + {}s grace; raise it with --timeout or the \
                     command's --process-timeout/--timeout). This does not mean the daemon \
                     crashed - check `session info` and `<session>/daemon.log`.",
                    command,
                    watchdog_ms / 1000,
                    DEFAULT_TIMEOUT_MS / 1000,
                    WATCHDOG_GRACE_MS / 1000
                );
                let output = output::Output::with_context(json, &session, &command);
                output.print_error("watchdog_timeout", &message);
                std::process::exit(1);
            }
        },
    };

    if let Err(e) = run_result {
        // Errors that reach here are the ones no command formatted itself -
        // a timed-out request, a dropped socket, "daemon failed to start".
        // Printing them via `error!` put a non-JSON tracing line on stderr
        // even under `--json`, so a JSON-consuming caller got unparseable
        // output on exactly the paths most likely to need a *specific*
        // failure reason (a caller retrying on `timeout` vs. giving up on
        // `connection_failed`, for instance).
        let output = output::Output::with_context(json, &session, &command);
        if json {
            output.print_error("cli_error", &e.to_string());
        } else {
            output.record_error("cli_error", &e.to_string());
            error!("{}", e);
        }
        std::process::exit(1);
    }
}

/// Short name of the invoked command for messages and the transcript:
/// `automate run`, `file pull`, `session daemon`.
fn command_label(cli: &Cli) -> String {
    match &cli.command {
        Commands::Connect(_) => "connect".into(),
        Commands::Disconnect => "disconnect".into(),
        Commands::Screenshot(_) => "screenshot".into(),
        Commands::Mouse(_) => "mouse".into(),
        Commands::Keyboard(_) => "keyboard".into(),
        Commands::Scroll(_) => "scroll".into(),
        Commands::Clipboard(_) => "clipboard".into(),
        Commands::Drive(_) => "drive".into(),
        Commands::File(args) => match args.action {
            cli::FileAction::Push { .. } => "file push".into(),
            cli::FileAction::Pull { .. } => "file pull".into(),
            cli::FileAction::Stat { .. } => "file stat".into(),
        },
        Commands::Automate(args) => {
            let action = match &args.action {
                cli::AutomateAction::Run { .. } => "run",
                cli::AutomateAction::RunPoll { .. } => "run-poll",
                cli::AutomateAction::WaitFor { .. } => "wait-for",
                cli::AutomateAction::Restart => "restart",
                cli::AutomateAction::Status => "status",
                cli::AutomateAction::Snapshot { .. } => "snapshot",
                _ => "action",
            };
            format!("automate {}", action)
        }
        Commands::Locate(_) => "locate".into(),
        Commands::ClickAt(_) => "click-at".into(),
        Commands::Session(args) => match args.action {
            cli::SessionAction::List => "session list".into(),
            cli::SessionAction::Info => "session info".into(),
            cli::SessionAction::Daemon => "session daemon".into(),
        },
        Commands::Wait { .. } => "wait".into(),
        Commands::View(_) => "view".into(),
        Commands::Diagnose(_) => "diagnose".into(),
    }
}

/// Default timeout for ordinary commands, which are a single round-trip.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Grace period added on top of a command's own (possibly extended) IPC
/// timeout before the watchdog gives up. Generous on purpose - this is a
/// backstop against a daemon-side hang with no timeout of its own, not the
/// primary timeout mechanism, so it should essentially never fire on a
/// healthy daemon.
const WATCHDOG_GRACE_MS: u64 = 60_000;

/// Total time the watchdog allows a single CLI invocation to run before
/// aborting it, or `None` for the one invocation that must never be
/// bounded: the daemon itself (`session daemon`). Mirrors the same
/// per-command extensions the IPC timeout itself applies (see
/// `cli/commands/locate.rs`, `cli/commands/automate.rs`,
/// `cli/commands/wait.rs`), plus `WATCHDOG_GRACE_MS` slack, so a command that
/// is legitimately still waiting on its own documented timeout is never
/// mistaken for a hang.
fn watchdog_budget_ms(cli: &Cli) -> Option<u64> {
    if matches!(
        cli.command,
        Commands::Session(cli::SessionArgs { action: cli::SessionAction::Daemon })
    ) {
        return None;
    }

    let base = cli.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    let extension = match &cli.command {
        Commands::Connect(_) => cli.timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        Commands::Locate(args) => args.wait.unwrap_or(0),
        Commands::Automate(args) => match &args.action {
            cli::AutomateAction::Run { wait: true, process_timeout, .. } => {
                process_timeout.unwrap_or(10_000)
            }
            // A detached launch's IPC timeout is raised by the daemon's spawn
            // deadline (a PowerShell start under load); the watchdog has to
            // clear that too.
            cli::AutomateAction::Run { wait: false, .. } => {
                agent_rdp_daemon::handlers::automate::SPAWN_TIMEOUT.as_millis() as u64
            }
            // `--follow` is a CLI-side loop of ordinary polls; only the
            // wall-clock budget grows, each poll keeps its own IPC timeout.
            // `--follow-timeout` alone implies `--follow` (the command runs
            // the loop either way), so it must budget the same - matching
            // `follow: true` only left the watchdog killing a loop it had
            // never been told about.
            cli::AutomateAction::RunPoll { follow, follow_timeout, .. }
                if *follow || follow_timeout.is_some() =>
            {
                follow_timeout.unwrap_or(cli::commands::automate::DEFAULT_FOLLOW_TIMEOUT_MS)
            }
            // The request carries the same default when the flag is absent;
            // budgeting 0 here only worked because the grace period covered
            // the default.
            cli::AutomateAction::WaitFor { timeout, .. } => {
                timeout.unwrap_or(cli::commands::automate::DEFAULT_WAIT_FOR_TIMEOUT_MS)
            }
            // Relaunching the agent retries the Win+R/handshake sequence up
            // to three times; the command's own IPC timeout is raised to
            // match, so the watchdog has to clear that too.
            cli::AutomateAction::Restart => cli::commands::automate::RESTART_MIN_TIMEOUT_MS,
            _ => 0,
        },
        // A transfer is many chunked round trips plus a hash of the whole
        // file at both ends; the command's own IPC timeout is raised to
        // match, so the watchdog has to clear that too.
        Commands::File(_) => cli::commands::file::TRANSFER_TIMEOUT_MS,
        Commands::Wait { ms } => *ms,
        // Several best-effort daemon round trips (ping, info, status,
        // screenshot, remote log pull), each with its own budget.
        Commands::Diagnose(_) => cli::commands::diagnose::TOTAL_DAEMON_BUDGET_MS,
        _ => 0,
    };
    Some(base + extension + WATCHDOG_GRACE_MS)
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("agent-rdp").chain(args.iter().copied()))
    }

    /// The daemon ran under the watchdog for two releases and every daemon
    /// died 90s after spawn. This is the regression test for that.
    #[test]
    fn daemon_is_never_bounded() {
        assert_eq!(watchdog_budget_ms(&parse(&["session", "daemon"])), None);
        assert_eq!(
            watchdog_budget_ms(&parse(&["--session", "work", "session", "daemon"])),
            None
        );
    }

    #[test]
    fn ordinary_commands_get_base_plus_grace() {
        assert_eq!(
            watchdog_budget_ms(&parse(&["session", "info"])),
            Some(DEFAULT_TIMEOUT_MS + WATCHDOG_GRACE_MS)
        );
        // A command whose args merely mention "session daemon" is still bounded.
        assert!(watchdog_budget_ms(&parse(&["automate", "run", "session", "daemon"])).is_some());
    }

    /// The bootstrap's worst case (three launches with growing, once-
    /// extendable handshake windows) must fit inside both budgets that wait
    /// on it, or the shortest one silently decides the real limit.
    #[test]
    fn connect_and_restart_budgets_cover_the_bootstrap_worst_case() {
        // Connect pays for the survivor wait as well; restart instead pays
        // for evicting the agent it is replacing and probing the new one.
        let connect_worst_ms =
            agent_rdp_daemon::automation::connect_bootstrap_worst_case().as_millis() as u64;
        let restart_worst_ms =
            agent_rdp_daemon::automation::restart_worst_case().as_millis() as u64;
        assert!(
            DEFAULT_CONNECT_TIMEOUT_MS > connect_worst_ms + 10_000,
            "connect budget {}ms does not clear the {}ms bootstrap worst case",
            DEFAULT_CONNECT_TIMEOUT_MS,
            connect_worst_ms
        );
        assert!(
            cli::commands::automate::RESTART_MIN_TIMEOUT_MS > restart_worst_ms + 10_000,
            "restart budget does not clear the {}ms bootstrap worst case",
            restart_worst_ms
        );
    }

    #[test]
    fn follow_extends_the_watchdog_only() {
        let budget = watchdog_budget_ms(&parse(&[
            "automate", "run-poll", "42", "--follow", "--follow-timeout", "5000",
        ]))
        .unwrap();
        assert_eq!(budget, DEFAULT_TIMEOUT_MS + 5000 + WATCHDOG_GRACE_MS);
        // Without --follow, a poll is an ordinary single round trip.
        assert_eq!(
            watchdog_budget_ms(&parse(&["automate", "run-poll", "42"])),
            Some(DEFAULT_TIMEOUT_MS + WATCHDOG_GRACE_MS)
        );
    }

    /// `--follow-timeout` implies `--follow`, and the watchdog has to know:
    /// budgeting it as a single poll killed a five-minute follow at 90s.
    #[test]
    fn follow_timeout_alone_budgets_the_follow_loop() {
        let budget = watchdog_budget_ms(&parse(&[
            "automate", "run-poll", "42", "--follow-timeout", "300000",
        ]))
        .unwrap();
        assert_eq!(budget, DEFAULT_TIMEOUT_MS + 300_000 + WATCHDOG_GRACE_MS);
        // And a bare --follow uses the loop's default budget.
        assert_eq!(
            watchdog_budget_ms(&parse(&["automate", "run-poll", "42", "--follow"])),
            Some(
                DEFAULT_TIMEOUT_MS
                    + cli::commands::automate::DEFAULT_FOLLOW_TIMEOUT_MS
                    + WATCHDOG_GRACE_MS
            )
        );
    }

    /// `wait-for` without --timeout still waits its default; the watchdog
    /// must budget that default, not zero.
    #[test]
    fn wait_for_default_is_budgeted() {
        assert_eq!(
            watchdog_budget_ms(&parse(&["automate", "wait-for", "name=OK"])),
            Some(
                DEFAULT_TIMEOUT_MS
                    + cli::commands::automate::DEFAULT_WAIT_FOR_TIMEOUT_MS
                    + WATCHDOG_GRACE_MS
            )
        );
    }

    /// The three transfer layers: daemon budget < CLI IPC timeout < watchdog.
    #[test]
    fn transfer_layers_are_ordered() {
        let daemon_ms =
            agent_rdp_daemon::handlers::file_transfer::TRANSFER_BUDGET.as_millis() as u64;
        let ipc_ms = DEFAULT_TIMEOUT_MS + cli::commands::file::TRANSFER_TIMEOUT_MS;
        let watchdog_ms =
            watchdog_budget_ms(&parse(&["file", "push", "/tmp/a", "C:\\a"])).unwrap();
        assert!(daemon_ms < ipc_ms, "daemon {daemon_ms} must give up before the CLI {ipc_ms}");
        assert!(ipc_ms < watchdog_ms, "CLI {ipc_ms} must give up before the watchdog {watchdog_ms}");
    }

    /// The `--timeout` help text quotes the connect default; keep it honest.
    #[test]
    fn timeout_help_quotes_the_real_connect_default() {
        use clap::CommandFactory;
        let mut help = Vec::new();
        Cli::command().write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(
            help.contains(&DEFAULT_CONNECT_TIMEOUT_MS.to_string()),
            "help does not mention the connect default {}",
            DEFAULT_CONNECT_TIMEOUT_MS
        );
    }

    /// Refused at parse time so no daemon is spawned for a request the
    /// daemon would refuse anyway.
    #[test]
    fn keep_alive_below_the_floor_is_a_usage_error() {
        let base = ["connect", "--host", "h", "--username", "u", "--password", "p"];
        let with = |extra: &[&str]| {
            let args: Vec<&str> = base.iter().chain(extra.iter()).copied().collect();
            Cli::try_parse_from(std::iter::once("agent-rdp").chain(args))
        };
        assert!(with(&["--keep-alive-secs", "5"]).is_err());
        assert!(with(&["--keep-alive-secs", "0"]).is_ok());
        assert!(with(&["--keep-alive-secs", "10"]).is_ok());
        // --defer-agent without automation is refused too.
        assert!(with(&["--defer-agent"]).is_err());
        assert!(with(&["--defer-agent", "--enable-win-automation"]).is_ok());
    }
}

/// Default timeout for `connect`. Connecting is not one round-trip: it covers
/// the TCP/TLS/CredSSP handshake, RDP capability exchange and - with
/// --enable-win-automation - bootstrapping the agent on the remote desktop.
/// That bootstrap retries the launch up to three times, each ~5s of fixed
/// waits plus a backing-off handshake wait of up to ~25s, so its worst case
/// is ~91s before any network latency. The previous 90s default sat exactly
/// on that line and cold starts timed out with the daemon still legitimately
/// working. Since then the handshake windows grew (25/45/75s, each extendable
/// once on a host that is visibly still starting PowerShell), and connect
/// additionally waits briefly for an agent that survived the last drop and
/// may have to evict a stale one, so its worst case is
/// `connect_bootstrap_worst_case()` ≈ 336s; this clears it with room for the
/// RDP handshake itself. A unit test keeps the two in step.
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 360_000;

async fn run(cli: Cli) -> anyhow::Result<()> {
    use output::Output;

    let output = Output::with_context(cli.json, &cli.session, &command_label(&cli));
    let timeout = cli.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);

    match cli.command {
        Commands::Connect(args) => {
            cli::commands::connect::run(
                &cli.session,
                args,
                &output,
                cli.timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
                cli.stream_port,
                cli.stream_bind.clone(),
            )
            .await
        }
        Commands::Disconnect => {
            cli::commands::disconnect::run(&cli.session, &output, timeout).await
        }
        Commands::Screenshot(args) => {
            cli::commands::screenshot::run(&cli.session, args, &output, timeout).await
        }
        Commands::Mouse(args) => {
            cli::commands::mouse::run(&cli.session, args, &output, timeout).await
        }
        Commands::Keyboard(args) => {
            cli::commands::keyboard::run(&cli.session, args, &output, timeout).await
        }
        Commands::Scroll(args) => {
            cli::commands::scroll::run(&cli.session, args, &output, timeout).await
        }
        Commands::Clipboard(args) => {
            cli::commands::clipboard::run(&cli.session, args, &output, timeout).await
        }
        Commands::Drive(args) => {
            cli::commands::drive::run(&cli.session, args, &output, timeout).await
        }
        Commands::File(args) => {
            cli::commands::file::run(&cli.session, args, &output, timeout).await
        }
        Commands::Automate(args) => {
            cli::commands::automate::run(&cli.session, args, &output, timeout).await
        }
        Commands::Locate(args) => {
            cli::commands::locate::run(&cli.session, args, &output, timeout).await
        }
        Commands::ClickAt(args) => {
            cli::commands::locate::run_click_at(&cli.session, args, &output, timeout).await
        }
        Commands::Session(args) => {
            cli::commands::session::run(&cli.session, args, &output, timeout).await
        }
        Commands::Wait { ms } => {
            cli::commands::wait::run(ms, &output).await
        }
        Commands::View(args) => {
            cli::commands::view::run(args, &output).await
        }
        Commands::Diagnose(args) => {
            cli::commands::diagnose::run(&cli.session, args, &output).await
        }
    }
}
