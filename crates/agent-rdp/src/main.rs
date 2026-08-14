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
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        error!("{}", e);
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
            cli::commands::wait::run(ms).await
        }
        Commands::View(args) => {
            cli::commands::view::run(args, &output).await
        }
    }
}
