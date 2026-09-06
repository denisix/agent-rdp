//! Screenshot command implementation.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use agent_rdp_protocol::{ImageFormat, Request, ResponseData, ScreenshotRequest};
use base64::Engine;

use crate::cli::ScreenshotArgs;
use crate::output::Output;
use crate::session_manager::SessionManager;

/// Frame age past which a screenshot is warned about. Comfortably above
/// the default 45s keep-alive interval plus the ~30s the OS takes to
/// declare a black-holed transport dead.
const STALE_FRAME_WARN_MS: u64 = 120_000;

pub async fn run(
    session: &str,
    args: ScreenshotArgs,
    output: &Output,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    let manager = SessionManager::new(session.to_string());

    let mut client = match manager.connect_existing().await {
        Ok(client) => client,
        Err(unavailable) => {
            output.print_error(unavailable.code(), unavailable.message());
            std::process::exit(1);
        }
    };

    let format = match args.format.to_lowercase().as_str() {
        "png" => ImageFormat::Png,
        "jpeg" | "jpg" => ImageFormat::Jpeg,
        _ => {
            output.print_error("invalid_format", "Format must be 'png' or 'jpeg'");
            std::process::exit(1);
        }
    };

    let request = Request::Screenshot(ScreenshotRequest {
        format,
        region: args.region,
    });
    let response = manager.send_with_retry(&mut client, &request, timeout_ms).await?;

    if !response.success {
        output.print_response(&response);
        std::process::exit(1);
    }

    // Handle the screenshot data - save to file
    if let Some(ResponseData::Screenshot {
        width,
        height,
        base64,
        offset_x,
        offset_y,
        frame_age_ms,
        frame_seq,
        frame_hash,
        ..
    }) = response.data
    {
        let image_data = base64::engine::general_purpose::STANDARD.decode(&base64)?;

        let path = Path::new(&args.output);
        let mut file = File::create(path)?;
        file.write_all(&image_data)?;

        let (offset_x, offset_y) = (offset_x.unwrap_or(0), offset_y.unwrap_or(0));

        if output.is_json() {
            // `frame_seq`/`frame_hash` let a caller prove two screenshots are
            // (or aren't) pixel-identical without hashing the file itself -
            // previously the only way to detect a stale/stuck frame was an
            // external md5 of the saved file plus reading an on-screen clock.
            println!(
                "{}",
                serde_json::json!({
                    "success": true,
                    "data": {
                        "type": "screenshot",
                        "path": path,
                        "width": width,
                        "height": height,
                        "offset_x": offset_x,
                        "offset_y": offset_y,
                        "frame_age_ms": frame_age_ms,
                        "frame_seq": frame_seq,
                        "frame_hash": frame_hash,
                    }
                })
            );
        } else if args.region.is_some() {
            // Spell out the offset: a coordinate read off this crop is only
            // clickable once the offset is added back.
            println!(
                "Screenshot saved to {} ({}x{} region at offset ({}, {}))",
                path.display(),
                width,
                height,
                offset_x,
                offset_y
            );
        } else {
            println!("Screenshot saved to {} ({}x{})", path.display(), width, height);
        }

        // The frame itself is always the last one the background decoder
        // painted, even if the transport died since - a large, stale age is
        // the only signal that this frame might not reflect current state.
        // Threshold well past the keep-alive interval: a live server answers
        // each keep-alive refresh, so an idle desktop no longer produces a
        // frame age in the tens of seconds. At 5s this warned on every
        // screenshot of a static desktop and taught callers to ignore it.
        if frame_age_ms > STALE_FRAME_WARN_MS {
            eprintln!(
                "Warning: no data from the server for {}s. With keep-alive on, a live server \
                 answers each refresh, so this usually means the transport is dead rather than \
                 the desktop idle - `session info` shows the drop once the OS reports it.",
                frame_age_ms / 1000
            );
        }
    }

    Ok(())
}
