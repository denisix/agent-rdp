//! Screenshot handler.

use std::sync::Arc;

use agent_rdp_protocol::{ErrorCode, ImageFormat, Response, ResponseData, ScreenshotRequest};
use base64::Engine;
use tokio::sync::Mutex;
use tracing::info;

use crate::handlers::imaging::{encode_image, hash_pixels};
use crate::rdp_session::RdpSession;

/// Handle a screenshot request.
pub async fn handle(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    params: ScreenshotRequest,
) -> Response {
    // Copy the pixels out and release the session lock immediately after -
    // everything past this point (encode, base64) used to run with the lock
    // still held, blocking every mouse/keyboard/frame-decode operation on the
    // daemon for the length of a PNG deflate.
    let (img_width, img_height, data, offset_x, offset_y, frame_age_ms, frame_seq) = {
        let session = rdp_session.lock().await;
        let rdp = match session.as_ref() {
            Some(rdp) => rdp,
            None => {
                return Response::error(ErrorCode::NotConnected, "Not connected to an RDP server");
            }
        };

        let frame_age_ms = rdp.last_frame_age().as_millis() as u64;
        // Read together with the pixels below, under the same lock, so the
        // seq reported can never race ahead of or behind the actual frame
        // returned - a seq/pixel mismatch would defeat the whole point of
        // reporting it.
        let frame_seq = rdp.frame_generation();

        let (w, h, data, off_x, off_y) = match params.region {
            None => {
                // The background frame processor keeps this up-to-date.
                let (w, h, data) = rdp.get_image_data();
                (w as u32, h as u32, data, None, None)
            }
            Some(region) => {
                let (full_width, full_height) = (rdp.width() as u32, rdp.height() as u32);
                let Some(clamped) = region.clamp_to(full_width, full_height) else {
                    return Response::error(
                        ErrorCode::InvalidRequest,
                        format!(
                            "Region {}x{} at ({}, {}) lies outside the {}x{} desktop",
                            region.width, region.height, region.x, region.y, full_width, full_height
                        ),
                    );
                };

                // Copies only the region's rows instead of the whole
                // framebuffer - a one-row `--region` request no longer pays
                // for a full-desktop copy it immediately throws most of away.
                let Some(data) = rdp.get_region_data(clamped) else {
                    return Response::error(
                        ErrorCode::InternalError,
                        "Failed to read the requested region from the framebuffer",
                    );
                };
                (clamped.width, clamped.height, data, Some(clamped.x), Some(clamped.y))
            }
        };
        (w, h, data, off_x, off_y, frame_age_ms, frame_seq)
    };

    let rgba_image = match image::RgbaImage::from_raw(img_width, img_height, data) {
        Some(img) => img,
        None => {
            return Response::error(
                ErrorCode::InternalError,
                "Failed to create image from decoded data",
            );
        }
    };

    let format = params.format;
    let format_str = match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpeg",
    };

    // PNG deflate/JPEG encode (and the pixel hash below) can run to hundreds
    // of milliseconds on a full desktop; run both off the async runtime so
    // neither stalls other requests being polled on the same worker thread.
    let (encoded, frame_hash) = match tokio::task::spawn_blocking(move || {
        let hash = hash_pixels(rgba_image.as_raw());
        encode_image(&rgba_image, format).map(|bytes| (bytes, hash))
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(message)) => return Response::error(ErrorCode::InternalError, message),
        Err(e) => {
            return Response::error(ErrorCode::InternalError, format!("Encoding task failed: {}", e))
        }
    };

    let base64_data = base64::engine::general_purpose::STANDARD.encode(encoded);

    info!(
        "Screenshot {}x{} ({}) at offset ({}, {})",
        img_width,
        img_height,
        format_str,
        offset_x.unwrap_or(0),
        offset_y.unwrap_or(0),
    );

    Response::success(ResponseData::Screenshot {
        width: img_width,
        height: img_height,
        format: format_str.to_string(),
        base64: base64_data,
        offset_x,
        offset_y,
        frame_age_ms,
        frame_seq,
        frame_hash,
    })
}
