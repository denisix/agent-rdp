//! Screenshot command implementation.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use agent_rdp_protocol::{ImageFormat, Request, ResponseData, ScreenshotRequest};
use base64::Engine;

use crate::cli::ScreenshotArgs;
use crate::output::Output;
use crate::session_manager::SessionManager;

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
    let response = client.send(&request, timeout_ms).await?;

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
        if frame_age_ms > 5000 {
            eprintln!(
                "Warning: this frame is {}s old. The desktop may simply be idle (RDP servers \
                 send nothing when nothing changes), but if the connection has actually died \
                 this is a stale frame - check `session info` for connection health.",
                frame_age_ms / 1000
            );
        }
    }

    Ok(())
}
