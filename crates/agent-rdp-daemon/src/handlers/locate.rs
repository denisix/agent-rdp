//! OCR-based text location handler.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_rdp_protocol::{
    ClickAtRequest, ClickAtResult, ErrorCode, LocateRequest, LocateResult, OcrMatch, Region,
    Response, ResponseData,
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
    let exact = params.exact;

    // OCR is CPU-bound and can run to multiple seconds on a full desktop;
    // keep it off the async worker so other requests keep being served.
    let result = tokio::task::spawn_blocking(move || {
        if all {
            ocr.get_all_lines_rgb(&rgb_image)
        } else {
            ocr.find_text_rgb(&rgb_image, &text, pattern, ignore_case, exact)
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
        "Locate request: text='{}', pattern={}, exact={}, ignore_case={}, all={}, region={:?}, wait_ms={:?}",
        params.text, params.pattern, params.exact, params.ignore_case, params.all, params.region, params.wait_ms
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

/// Chebyshev (axis-aligned) distance from a point to an `OcrMatch` box: 0
/// when the point is inside the box, otherwise the larger of the
/// horizontal/vertical gaps to its nearest edge.
///
/// Chosen over Euclidean distance for simplicity and because it matches how
/// "still in the same visual cluster" reads for roughly text-height-scaled UI
/// labels - two labels stacked closely either horizontally or vertically are
/// exactly the ambiguous case, and Chebyshev distance catches both without
/// needing floating point.
fn rect_distance(m: &OcrMatch, x: i32, y: i32) -> i32 {
    let dx = (m.x - x).max(0).max(x - (m.x + m.width));
    let dy = (m.y - y).max(0).max(y - (m.y + m.height));
    dx.max(dy)
}

/// Result of checking whether `(x, y)` is safe to click.
struct ClickAtCheck {
    /// Recognized text of the sole nearby region, when there was exactly one.
    matched_text: Option<String>,
    /// The nearby regions, populated (2 or more) only when the click should
    /// be refused as ambiguous.
    nearby: Vec<OcrMatch>,
}

/// Decide whether clicking `(x, y)` is safe, given the OCR-detected regions
/// in the surrounding window.
///
/// Pure geometry over already-detected boxes, so it is testable with
/// synthetic boxes and does not depend on OCR *recognition* being correct -
/// only on *detection* having found that something is there, a separate,
/// script-agnostic stage. Zero or one region within `min_gap` of the point is
/// safe (nothing nearby, or exactly the target); two or more is the ambiguous
/// case this command exists to catch.
fn check_click_safety(boxes: &[OcrMatch], x: i32, y: i32, min_gap: i32) -> ClickAtCheck {
    let candidates: Vec<OcrMatch> = boxes
        .iter()
        .filter(|m| rect_distance(m, x, y) <= min_gap)
        .cloned()
        .collect();

    match candidates.len() {
        0 => ClickAtCheck { matched_text: None, nearby: Vec::new() },
        1 => ClickAtCheck {
            matched_text: Some(candidates[0].text.clone()),
            nearby: Vec::new(),
        },
        _ => ClickAtCheck { matched_text: None, nearby: candidates },
    }
}

/// Handle a `ClickAt` request.
pub async fn handle_click_at(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    params: ClickAtRequest,
) -> Response {
    info!(
        "ClickAt request: ({}, {}), window={}x{}, min_gap={}",
        params.x, params.y, params.window_width, params.window_height, params.min_gap
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

    let (px, py) = (params.x as i32, params.y as i32);

    let rgb_image = {
        let session = rdp_session.lock().await;
        let rdp = match session.as_ref() {
            Some(rdp) => rdp,
            None => {
                return Response::error(ErrorCode::NotConnected, "Not connected to an RDP server");
            }
        };

        let (full_width, full_height) = (rdp.width() as u32, rdp.height() as u32);

        // The point itself must be on the desktop - the detection window can
        // still overlap the desktop even when the point isn't, so the
        // clamp_to check below would not catch this on its own.
        if params.x as u32 >= full_width || params.y as u32 >= full_height {
            return Response::error(
                ErrorCode::InvalidRequest,
                format!(
                    "Point ({}, {}) lies outside the {}x{} desktop",
                    params.x, params.y, full_width, full_height
                ),
            );
        }

        // Window centered on the point, clamped to the desktop. Restricting
        // detection to a local window (rather than the whole screen) is not
        // just an optimization - "ambiguously close to a neighboring label"
        // is inherently about labels near this point, not ones elsewhere on
        // screen.
        let region = Region {
            x: (params.x as u32).saturating_sub(params.window_width / 2),
            y: (params.y as u32).saturating_sub(params.window_height / 2),
            width: params.window_width,
            height: params.window_height,
        };
        let Some(clamped) = region.clamp_to(full_width, full_height) else {
            return Response::error(
                ErrorCode::InvalidRequest,
                format!(
                    "Point ({}, {}) lies outside the {}x{} desktop",
                    params.x, params.y, full_width, full_height
                ),
            );
        };

        let data = match rdp.get_region_data(clamped) {
            Some(data) => data,
            None => {
                return Response::error(
                    ErrorCode::InternalError,
                    "Failed to read the requested region from the framebuffer",
                );
            }
        };
        let rgba = match image::RgbaImage::from_raw(clamped.width, clamped.height, data) {
            Some(img) => img,
            None => {
                return Response::error(ErrorCode::InternalError, "Failed to create image from desktop data");
            }
        };
        (image::DynamicImage::ImageRgba8(rgba).into_rgb8(), clamped)
    }; // session lock dropped here

    let (rgb_image, region) = rgb_image;
    let ocr = Arc::clone(&ocr);

    // OCR detection can take real time even over a small window; keep it off
    // the async worker.
    let result = tokio::task::spawn_blocking(move || ocr.get_all_lines_rgb(&rgb_image))
        .await
        .map_err(|e| format!("OCR task failed: {}", e))
        .and_then(|r| r.map_err(|e| format!("OCR failed: {}", e)));

    let mut boxes = match result {
        Ok((boxes, _total)) => boxes,
        Err(message) => return Response::error(ErrorCode::InternalError, message),
    };
    offset_matches(&mut boxes, region);

    let min_gap = params.min_gap as i32;
    let check = check_click_safety(&boxes, px, py, min_gap);

    if !check.nearby.is_empty() {
        info!(
            "ClickAt ({}, {}) refused: {} regions within {}px",
            params.x, params.y, check.nearby.len(), params.min_gap
        );
        return Response::success(ResponseData::ClickAtResult(ClickAtResult {
            clicked: false,
            x: params.x,
            y: params.y,
            matched_text: None,
            nearby: check.nearby,
        }));
    }

    let mouse_action = if params.double_click {
        agent_rdp_protocol::MouseRequest::DoubleClick { x: params.x, y: params.y }
    } else if params.right_click {
        agent_rdp_protocol::MouseRequest::RightClick { x: params.x, y: params.y }
    } else {
        agent_rdp_protocol::MouseRequest::Click { x: params.x, y: params.y }
    };

    let click_response = crate::handlers::mouse::handle(rdp_session, mouse_action).await;
    if !click_response.success {
        return click_response;
    }

    info!("ClickAt clicked ({}, {})", params.x, params.y);
    Response::success(ResponseData::ClickAtResult(ClickAtResult {
        clicked: true,
        x: params.x,
        y: params.y,
        matched_text: check.matched_text,
        nearby: Vec::new(),
    }))
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

    fn boxed(text: &str, x: i32, y: i32, width: i32, height: i32) -> OcrMatch {
        OcrMatch {
            text: text.to_string(),
            x,
            y,
            width,
            height,
            center_x: x + width / 2,
            center_y: y + height / 2,
        }
    }

    #[test]
    fn test_rect_distance_inside_is_zero() {
        let m = boxed("Провести", 600, 200, 100, 20);
        assert_eq!(rect_distance(&m, 650, 210), 0);
        // Exactly on the edges counts as inside/touching, not a gap.
        assert_eq!(rect_distance(&m, 600, 200), 0);
        assert_eq!(rect_distance(&m, 700, 220), 0);
    }

    #[test]
    fn test_rect_distance_outside_measures_the_gap() {
        let m = boxed("Провести", 600, 200, 100, 20);
        assert_eq!(rect_distance(&m, 710, 210), 10); // 10px right of the box
        assert_eq!(rect_distance(&m, 650, 190), 10); // 10px above
        assert_eq!(rect_distance(&m, 590, 190), 10); // diagonal: max of the axis gaps
        assert_eq!(rect_distance(&m, 560, 170), 40);
    }

    #[test]
    fn test_check_click_safety_single_containing_box_is_safe() {
        // The reported scenario: clicking the center of «Провести» with
        // «Провести и закрыть» far enough away (the smoke test had a 65px
        // margin) must proceed - one region within the gap, the target.
        let boxes = vec![
            boxed("Провести", 620, 200, 90, 18),
            boxed("Провести и закрыть", 730, 200, 170, 18),
        ];
        let check = check_click_safety(&boxes, 665, 209, 10);
        assert!(check.nearby.is_empty(), "click must be allowed");
        assert_eq!(check.matched_text.as_deref(), Some("Провести"));
    }

    #[test]
    fn test_check_click_safety_two_close_boxes_refuse() {
        // Point on the boundary between two labels only min_gap apart on each
        // side: exactly the "which button did I mean" ambiguity.
        let boxes = vec![
            boxed("Провести", 600, 200, 100, 18),
            boxed("Провести и закрыть", 710, 200, 170, 18),
        ];
        let check = check_click_safety(&boxes, 705, 209, 10);
        assert_eq!(check.nearby.len(), 2, "both neighbors must be reported");
        assert!(check.matched_text.is_none());
    }

    #[test]
    fn test_check_click_safety_no_boxes_at_all_is_safe() {
        // An icon-only button with no detectable text nearby is legitimate -
        // refusing would make the command useless for exactly the custom-
        // rendered UIs it exists for.
        let check = check_click_safety(&[], 665, 209, 10);
        assert!(check.nearby.is_empty());
        assert!(check.matched_text.is_none());

        // Same when text exists on screen but nowhere near the point.
        let boxes = vec![boxed("Далеко", 10, 10, 60, 14)];
        let check = check_click_safety(&boxes, 665, 209, 10);
        assert!(check.nearby.is_empty());
        assert!(check.matched_text.is_none());
    }

    #[test]
    fn test_check_click_safety_gap_boundary() {
        let target = boxed("Провести", 600, 200, 100, 18);
        // Neighbor exactly min_gap past the point: still counted (<=), so
        // refused - the boundary errs toward safety.
        let at_gap = boxed("Провести и закрыть", 715, 200, 170, 18);
        let check = check_click_safety(&[target.clone(), at_gap], 705, 209, 10);
        assert_eq!(check.nearby.len(), 2);

        // One pixel past the gap: allowed.
        let past_gap = boxed("Провести и закрыть", 716, 200, 170, 18);
        let check = check_click_safety(&[target, past_gap], 705, 209, 10);
        assert!(check.nearby.is_empty());
        assert_eq!(check.matched_text.as_deref(), Some("Провести"));
    }

    #[test]
    fn test_check_click_safety_larger_gap_is_stricter() {
        let boxes = vec![
            boxed("Провести", 600, 200, 100, 18),
            boxed("Провести и закрыть", 760, 200, 170, 18),
        ];
        // 50px gap between point (705) and the neighbor at 760.
        assert!(check_click_safety(&boxes, 705, 209, 10).nearby.is_empty());
        assert_eq!(check_click_safety(&boxes, 705, 209, 60).nearby.len(), 2);
    }
}
