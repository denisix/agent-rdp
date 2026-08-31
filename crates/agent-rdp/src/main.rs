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

    if let Err(e) = run(cli).await {
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
