//! agent-rdp: CLI tool for AI agents to control Windows Remote Desktop sessions.

mod cli;
mod ipc_client;
mod output;
mod session_manager;

use clap::Parser;
use tracing::error;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    // Initialize logging.
    //
    // EnvFilter defaults to ERROR-only when RUST_LOG is unset, which is right
    // for the CLI (INFO would pollute command output) but useless for the
    // daemon: it runs detached with its output going to <session>/daemon.log,
    // and that log is the only record of why a session ended. Default the
    // daemon to info so an unexpected teardown is explained; RUST_LOG still
    // overrides either way.
    let is_daemon = {
        let args: Vec<String> = std::env::args().collect();
        args.windows(2)
            .any(|w| w[0] == "session" && w[1] == "daemon")
    };

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

    let cli = Cli::parse();
    let json = cli.json;
    let watchdog_ms = watchdog_budget_ms(&cli);

    let run_result = match tokio::time::timeout(
        std::time::Duration::from_millis(watchdog_ms),
        run(cli),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            // A defense-in-depth backstop, not the primary fix: every daemon-
            // side wait that legitimately blocks (locate --wait, automate
            // run --process-timeout, connect) already extends its own IPC
            // timeout past the CLI default - this only fires if something
            // hangs *past* that extended budget, e.g. a daemon handler stuck
            // on a dead socket read with no timeout of its own. A CLI command
            // must never hang indefinitely regardless of which internal path
            // misbehaves.
            let message = format!(
                "Command exceeded its {}s watchdog budget and was aborted. This does not mean \
                 the daemon crashed - check `session info` and `<session>/daemon.log`.",
                watchdog_ms / 1000
            );
            if json {
                output::Output::new(true).print_error("watchdog_timeout", &message);
            } else {
                eprintln!("Error [watchdog_timeout]: {}", message);
            }
            std::process::exit(1);
        }
    };

    if let Err(e) = run_result {
        // Errors that reach here are the ones no command formatted itself -
        // a timed-out request, a dropped socket, "daemon failed to start".
        // Printing them via `error!` put a non-JSON tracing line on stderr
        // even under `--json`, so a JSON-consuming caller got unparseable
        // output on exactly the paths most likely to need a *specific*
        // failure reason (a caller retrying on `timeout` vs. giving up on
        // `connection_failed`, for instance).
        if json {
            output::Output::new(true).print_error("cli_error", &e.to_string());
        } else {
            error!("{}", e);
        }
        std::process::exit(1);
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
/// aborting it. Mirrors the same per-command extensions the IPC timeout
/// itself applies (see `cli/commands/locate.rs`, `cli/commands/automate.rs`,
/// `cli/commands/wait.rs`), plus `WATCHDOG_GRACE_MS` slack, so a command that
/// is legitimately still waiting on its own documented timeout is never
/// mistaken for a hang.
fn watchdog_budget_ms(cli: &Cli) -> u64 {
    let base = cli.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    let extension = match &cli.command {
        Commands::Connect(_) => cli.timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        Commands::Locate(args) => args.wait.unwrap_or(0),
        Commands::Automate(args) => match &args.action {
            cli::AutomateAction::Run { wait: true, process_timeout, .. } => {
                process_timeout.unwrap_or(10_000)
            }
            cli::AutomateAction::WaitFor { timeout, .. } => timeout.unwrap_or(0),
            _ => 0,
        },
        Commands::Wait { ms } => *ms,
        _ => 0,
    };
    base + extension + WATCHDOG_GRACE_MS
}

/// Default timeout for `connect`. Connecting is not one round-trip: it covers
/// the TCP/TLS/CredSSP handshake, RDP capability exchange and - with
/// --enable-win-automation - bootstrapping the agent on the remote desktop,
/// which alone spends several seconds in fixed waits plus a backing-off retry
/// loop. 30s is not enough on a real host.
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 90_000;

async fn run(cli: Cli) -> anyhow::Result<()> {
    use output::Output;

    let output = Output::new(cli.json);
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
    }
}
