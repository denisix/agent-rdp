//! OCR-based text location handler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_rdp_protocol::{
    ErrorCode, LocateRequest, LocateResult, OcrMatch, Region, Response, ResponseData,
};
use tokio::sync::Mutex;
use tracing::info;

use crate::ocr::{find_models_dir, OcrService};
use crate::rdp_session::RdpSession;

/// How long to wait between OCR passes while `--wait` is polling.
///
/// Text on a redrawing screen (a dialog animating in, a grid re-laying out)
/// typically settles within one or two passes; 500ms keeps the daemon
/// responsive to other commands between polls without busy-looping OCR.
const WAIT_POLL_INTERVAL_MS: u64 = 500;

/// Lazily initialized, retry-able OCR service.
///
/// A plain `OnceLock<Option<OcrService>>` (the previous design) caches a
/// failed init forever - a daemon started before the OCR models were
/// installed would report "OCR service not available" for its whole life even
/// after the models appeared. Retrying on every `None` fixes that, and the
/// `Arc` lets `connect` pre-warm this off the request path without holding
/// the lock for the life of the daemon.
static OCR_SERVICE: Mutex<Option<Arc<OcrService>>> = Mutex::const_new(None);

/// Get the OCR service, initializing it if necessary.
///
/// Model loading is blocking disk I/O plus building the `rten` graphs, so it
/// runs on a blocking-pool thread rather than the async worker handling this
/// request.
pub async fn get_or_init_ocr_service() -> Option<Arc<OcrService>> {
    let mut slot = OCR_SERVICE.lock().await;
    if let Some(service) = slot.as_ref() {
        return Some(Arc::clone(service));
    }

    let service = tokio::task::spawn_blocking(|| {
        let models_dir = find_models_dir().map_err(|e| {
            tracing::error!("Failed to find OCR models: {}", e);
        })?;
        OcrService::new(&models_dir).map_err(|e| {
            tracing::error!("Failed to initialize OCR service: {}", e);
        })
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .map(Arc::new);

    *slot = service.clone();
    service
}

/// Translate matches found in a cropped image back into full-desktop
/// coordinates.
///
/// Without this a region search would return coordinates relative to the crop,
/// and feeding them to `mouse click` would land somewhere near the top-left of
/// the screen. Keeping it a free function makes it testable without an RDP
/// session.
fn offset_matches(matches: &mut [OcrMatch], region: Region) {
    let (dx, dy) = (region.x as i32, region.y as i32);
    for m in matches {
        m.x += dx;
        m.y += dy;
        m.center_x += dx;
        m.center_y += dy;
    }
}

/// Whether a `--wait` poll loop should try again or give up, given how long
/// it has already waited.
///
/// A pure function so the timing decision is testable without any actual
/// sleeping, OCR, or RDP session.
#[derive(Debug, PartialEq, Eq)]
enum WaitDecision {
    Retry,
    GiveUp,
}

fn decide_wait(elapsed: Duration, wait_ms: u64) -> WaitDecision {
    if elapsed.as_millis() as u64 >= wait_ms {
        WaitDecision::GiveUp
    } else {
        WaitDecision::Retry
    }
}

/// One screenshot-and-OCR pass, cropped to `region` when given.
///
/// Returns matches already translated into full-desktop coordinates.
async fn run_one_pass(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    ocr: &Arc<OcrService>,
    params: &LocateRequest,
) -> Result<(Vec<OcrMatch>, u32), Response> {
    let rgb_image = {
        let session = rdp_session.lock().await;
        let rdp = match session.as_ref() {
            Some(rdp) => rdp,
            None => {
                return Err(Response::error(ErrorCode::NotConnected, "Not connected to an RDP server"));
            }
        };

        match params.region {
            None => {
                let (width, height, data) = rdp.get_image_data();
                let rgba = image::RgbaImage::from_raw(width as u32, height as u32, data).ok_or_else(|| {
                    Response::error(ErrorCode::InternalError, "Failed to create image from desktop data")
                })?;
                (image::DynamicImage::ImageRgba8(rgba).into_rgb8(), None)
            }
            Some(region) => {
                let (full_width, full_height) = (rdp.width() as u32, rdp.height() as u32);
                let clamped = region.clamp_to(full_width, full_height).ok_or_else(|| {
                    Response::error(
                        ErrorCode::InvalidRequest,
                        format!(
                            "Region {}x{} at ({}, {}) lies outside the {}x{} desktop",
                            region.width, region.height, region.x, region.y, full_width, full_height
                        ),
                    )
                })?;

                // Copies only the region's rows instead of the whole
                // framebuffer, which also makes a region OCR pass cheaper
                // than the full-screen one it replaces.
                let data = rdp.get_region_data(clamped).ok_or_else(|| {
                    Response::error(
                        ErrorCode::InternalError,
                        "Failed to read the requested region from the framebuffer",
                    )
                })?;
                let rgba = image::RgbaImage::from_raw(clamped.width, clamped.height, data).ok_or_else(|| {
                    Response::error(ErrorCode::InternalError, "Failed to create image from desktop data")
                })?;
                (image::DynamicImage::ImageRgba8(rgba).into_rgb8(), Some(clamped))
            }
        }
    }; // session lock dropped here

    let (rgb_image, applied_region) = rgb_image;
    let ocr = Arc::clone(ocr);
    let all = params.all;
    let text = params.text.clone();
    let pattern = params.pattern;
    let ignore_case = params.ignore_case;

    // OCR is CPU-bound and can run to multiple seconds on a full desktop;
    // keep it off the async worker so other requests keep being served.
    let result = tokio::task::spawn_blocking(move || {
        if all {
            ocr.get_all_lines_rgb(&rgb_image)
        } else {
            ocr.find_text_rgb(&rgb_image, &text, pattern, ignore_case)
        }
    })
    .await
    .map_err(|e| Response::error(ErrorCode::InternalError, format!("OCR task failed: {}", e)))?;

    match result {
        Ok((mut matches, total_lines)) => {
            if let Some(region) = applied_region {
                offset_matches(&mut matches, region);
            }
            Ok((matches, total_lines))
        }
        Err(e) => Err(Response::error(ErrorCode::InternalError, format!("OCR failed: {}", e))),
    }
}

/// Handle a locate request.
pub async fn handle(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    params: LocateRequest,
) -> Response {
    info!(
        "Locate request: text='{}', pattern={}, ignore_case={}, all={}, region={:?}, wait_ms={:?}",
        params.text, params.pattern, params.ignore_case, params.all, params.region, params.wait_ms
    );

    let ocr = match get_or_init_ocr_service().await {
        Some(ocr) => ocr,
        None => {
            return Response::error(
                ErrorCode::InternalError,
                "OCR service not available. Make sure OCR models are installed.",
            );
        }
    };

    // `all` has no target text to wait for, so a wait is ignored rather than
    // rejected outright - the CLI already refuses to combine `--all` with
    // `--wait`, this is defense in depth for other IPC callers.
    let wait_ms = params.wait_ms.filter(|_| !params.all);
    let start = Instant::now();

    loop {
        let (matches, total_lines) = match run_one_pass(rdp_session, &ocr, &params).await {
            Ok(pass) => pass,
            Err(response) => return response,
        };

        let found = params.all || !matches.is_empty();

        match (found, wait_ms) {
            (true, _) | (false, None) => {
                info!("Found {} lines out of {} total", matches.len(), total_lines);
                return Response::success(ResponseData::LocateResult(LocateResult {
                    matches,
                    total_words: total_lines, // Now represents total lines
                }));
            }
            (false, Some(wait_ms)) => match decide_wait(start.elapsed(), wait_ms) {
                WaitDecision::Retry => {
                    tokio::time::sleep(Duration::from_millis(WAIT_POLL_INTERVAL_MS)).await;
                }
                WaitDecision::GiveUp => {
                    return Response::error(
                        ErrorCode::Timeout,
                        format!("Text '{}' did not appear within {}ms", params.text, wait_ms),
                    );
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(x: i32, y: i32, width: i32, height: i32) -> OcrMatch {
        OcrMatch {
            text: "123".to_string(),
            x,
            y,
            width,
            height,
            center_x: x + width / 2,
            center_y: y + height / 2,
        }
    }

    #[test]
    fn test_offset_matches_maps_crop_to_full_screen() {
        // A row cropped at (100, 380): text 10px into the crop is really at 390.
        let mut matches = vec![m(20, 10, 40, 12)];
        offset_matches(&mut matches, Region { x: 100, y: 380, width: 400, height: 30 });

        let got = &matches[0];
        assert_eq!((got.x, got.y), (120, 390));
        assert_eq!((got.center_x, got.center_y), (140, 396));
        // Size is unaffected by the translation.
        assert_eq!((got.width, got.height), (40, 12));
    }

    #[test]
    fn test_offset_matches_at_origin_is_identity() {
        let mut matches = vec![m(20, 10, 40, 12)];
        let before = matches.clone();
        offset_matches(&mut matches, Region { x: 0, y: 0, width: 1280, height: 800 });
        assert_eq!(matches[0].x, before[0].x);
        assert_eq!(matches[0].center_y, before[0].center_y);
    }

    #[test]
    fn test_offset_matches_shifts_every_match() {
        // Every match must move, not just the first - a loop that returned
        // early would still pass a single-element test.
        let mut matches = vec![m(0, 0, 10, 10), m(50, 20, 10, 10), m(300, 200, 10, 10)];
        offset_matches(&mut matches, Region { x: 7, y: 13, width: 400, height: 300 });

        assert_eq!((matches[0].x, matches[0].y), (7, 13));
        assert_eq!((matches[1].x, matches[1].y), (57, 33));
        assert_eq!((matches[2].x, matches[2].y), (307, 213));
    }

    #[test]
    fn test_offset_matches_on_empty_slice_is_a_no_op() {
        // A region search that found nothing still goes through the offset
        // step; it must not panic.
        let mut matches: Vec<OcrMatch> = Vec::new();
        offset_matches(&mut matches, Region { x: 100, y: 380, width: 400, height: 30 });
        assert!(matches.is_empty());
    }

    #[test]
    fn test_offset_matches_only_translates_never_rescales() {
        // The distance between two matches is a property of the screen, not of
        // the crop, so it must survive the translation unchanged.
        let mut matches = vec![m(10, 10, 20, 20), m(110, 60, 20, 20)];
        offset_matches(&mut matches, Region { x: 640, y: 400, width: 400, height: 300 });

        assert_eq!(matches[1].x - matches[0].x, 100);
        assert_eq!(matches[1].y - matches[0].y, 50);
        assert_eq!((matches[0].width, matches[0].height), (20, 20));
    }

    #[test]
    fn test_offset_matches_handles_a_far_corner_region() {
        // Regions are clamped to the framebuffer before they reach this
        // function, so the largest realistic offset is still far below the
        // point where the i32 cast could misbehave.
        let mut matches = vec![m(5, 5, 10, 10)];
        offset_matches(&mut matches, Region { x: 7679, y: 4319, width: 1, height: 1 });

        assert_eq!((matches[0].x, matches[0].y), (7684, 4324));
        assert!(matches[0].center_x > 0 && matches[0].center_y > 0);
    }

    #[test]
    fn test_decide_wait_retries_before_the_deadline() {
        assert_eq!(decide_wait(Duration::from_millis(0), 5000), WaitDecision::Retry);
        assert_eq!(decide_wait(Duration::from_millis(4999), 5000), WaitDecision::Retry);
    }

    #[test]
    fn test_decide_wait_gives_up_at_and_past_the_deadline() {
        // At exactly the deadline: no more retries, so the last poll's result
        // (or lack of one) is final rather than kicking off one more wait.
        assert_eq!(decide_wait(Duration::from_millis(5000), 5000), WaitDecision::GiveUp);
        assert_eq!(decide_wait(Duration::from_millis(5001), 5000), WaitDecision::GiveUp);
        assert_eq!(decide_wait(Duration::from_secs(60), 5000), WaitDecision::GiveUp);
    }

    #[test]
    fn test_decide_wait_zero_budget_gives_up_immediately() {
        assert_eq!(decide_wait(Duration::from_millis(0), 0), WaitDecision::GiveUp);
    }
}
