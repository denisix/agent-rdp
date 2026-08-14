//! View command implementation - opens the web viewer served by the daemon.

use crate::cli::ViewArgs;
use crate::output::Output;

pub async fn run(args: ViewArgs, output: &Output) -> anyhow::Result<()> {
    // The daemon serves the viewer HTML on the same port as the WebSocket server
    let url = format!("http://localhost:{}", args.port);

    if output.is_json() {
        println!(r#"{{"url":"{}"}}"#, url);
    } else {
        println!("Opening viewer at: {}", url);
    }

    // Open browser
    if let Err(e) = open_url(&url) {
        output.print_error("open_failed", &format!("Failed to open browser: {}", e));
        std::process::exit(1);
    }

    Ok(())
}

/// Open a URL in the default browser, without depending on the `open` crate.
fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).status()?;
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()?;
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Under WSL, `xdg-open` isn't available/won't reach the host browser.
        // Detect WSL and shell out to the Windows host instead.
        let is_wsl = std::fs::read_to_string("/proc/version")
            .map(|v| v.to_lowercase().contains("microsoft"))
            .unwrap_or(false);

        if is_wsl {
            std::process::Command::new("cmd.exe")
                .args(["/C", "start", "", url])
                .status()?;
        } else {
            std::process::Command::new("xdg-open").arg(url).status()?;
        }
    }

    Ok(())
}
