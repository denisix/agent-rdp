//! Failure captures: when a request fails in a way a screenshot would
//! explain, save the current frame and the request context next to it.
//!
//! `<session_dir>/diagnostics/<utc-ts>-<kind>-<code>.png` + `.json`. For an
//! OCR miss the `.json` also carries every line OCR *did* read, which is the
//! artefact needed to answer "why couldn't it find X". Best-effort, rate
//! limited, bounded to the newest `MAX_CAPTURES` pairs, and never triggered
//! by errors that mean the frame processor itself is wedged - reading the
//! framebuffer would then only add another blocked thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_rdp_protocol::{AutomateRequest, ErrorCode, ImageFormat, Request, Response, ResponseData};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::handlers::imaging::{encode_image, hash_pixels};
use crate::rdp_session::RdpSession;
use crate::{get_session_dir, timefmt, transcript, DIAGNOSTICS_DIR};

/// Newest capture pairs kept per session.
pub const MAX_CAPTURES: usize = 20;

/// Minimum spacing between captures: a `locate` polling loop must not turn
/// into a screenshot-per-iteration.
const MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Millis-since-epoch of the last capture (0 = never).
static LAST_CAPTURE_MS: AtomicU64 = AtomicU64::new(0);

/// What a failed request turned out to be, for the file name and the
/// decision whether an OCR dump is worth the CPU.
#[derive(Debug, PartialEq, Eq)]
pub struct Trigger {
    pub kind: &'static str,
    pub code: String,
    pub with_ocr: bool,
}

/// Errors that describe the daemon's own state rather than the screen: a
/// capture would show nothing useful and - for the unresponsive family -
/// would block on the same lock that is already stuck.
fn is_infrastructure_error(code: &ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::NotConnected
            | ErrorCode::InvalidRequest
            | ErrorCode::Timeout
            | ErrorCode::AutomationIndeterminate
            | ErrorCode::AutomationNotEnabled
            | ErrorCode::DaemonNotRunning
            | ErrorCode::DaemonUnresponsive
            | ErrorCode::DaemonVersionMismatch
            | ErrorCode::IpcError
            // Facts about the file, not the screen.
            | ErrorCode::StaleFile
            | ErrorCode::FileChangedDuringTransfer
    )
}

/// Decide whether this request/response pair deserves a capture.
pub fn trigger_for(request: &Request, response: &Response) -> Option<Trigger> {
    let error_code = response.error.as_ref().map(|e| &e.code);
    if let Some(code) = error_code {
        if is_infrastructure_error(code) {
            return None;
        }
    }
    let code_text = || error_code.map(transcript::code_name).unwrap_or_else(|| "error".into());

    match request {
        Request::Locate(params) => {
            if !response.success {
                return Some(Trigger { kind: "locate", code: code_text(), with_ocr: true });
            }
            if params.all {
                return None;
            }
            match response.data.as_ref() {
                Some(ResponseData::LocateResult(result)) if result.matches.is_empty() => {
                    Some(Trigger { kind: "locate", code: "no_match".into(), with_ocr: true })
                }
                _ => None,
            }
        }
        Request::ClickAt(_) if !response.success => {
            Some(Trigger { kind: "click_at", code: code_text(), with_ocr: true })
        }
        Request::Automate(action) => {
            if matches!(action, AutomateRequest::Status | AutomateRequest::RunPoll { .. }) {
                return None;
            }
            if !response.success {
                return Some(Trigger { kind: "automate", code: code_text(), with_ocr: false });
            }
            // A waited `run` that exited non-zero is the "it said ok but
            // nothing happened" case made visible.
            match response.data.as_ref() {
                Some(ResponseData::RunResult(result)) => match result.exit_code {
                    Some(code) if code != 0 => Some(Trigger {
                        kind: "run",
                        code: format!("exit_{}", code),
                        with_ocr: false,
                    }),
                    _ => None,
                },
                _ => None,
            }
        }
        Request::Mouse(_) | Request::Keyboard(_) | Request::Scroll(_) | Request::Clipboard(_)
            if !response.success =>
        {
            let kind = match request {
                Request::Mouse(_) => "mouse",
                Request::Keyboard(_) => "keyboard",
                Request::Scroll(_) => "scroll",
                _ => "clipboard",
            };
            Some(Trigger { kind, code: code_text(), with_ocr: false })
        }
        _ => None,
    }
}

/// Capture if warranted. Returns immediately; the work runs in a task.
pub fn maybe_capture(
    session: &str,
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    request: &Request,
    response: &Response,
) {
    if !transcript::enabled() {
        return;
    }
    let Some(trigger) = trigger_for(request, response) else {
        return;
    };
    if !rate_limit_allows() {
        debug!("diagnostics: skipping {}/{} capture (rate limited)", trigger.kind, trigger.code);
        return;
    }

    let context = json!({
        "ts": timefmt::utc_rfc3339(SystemTime::now()),
        "kind": trigger.kind,
        "code": trigger.code,
        "request": transcript::summarize_request(request),
        "success": response.success,
        "error": response.error.as_ref().map(|e| json!({ "code": transcript::code_name(&e.code), "message": e.message })),
        "response": transcript::summarize_response(response),
    });
    let dir = get_session_dir(session).join(DIAGNOSTICS_DIR);
    let stem = format!(
        "{}-{}-{}",
        timefmt::utc_compact(SystemTime::now()),
        trigger.kind,
        sanitize(&trigger.code)
    );
    let rdp_session = Arc::clone(rdp_session);
    let with_ocr = trigger.with_ocr;

    tokio::spawn(async move {
        if let Err(e) = capture(rdp_session, with_ocr, dir, stem, context).await {
            warn!("diagnostics capture failed: {}", e);
        }
    });
}

fn rate_limit_allows() -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_CAPTURE_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < MIN_INTERVAL.as_millis() as u64 {
        return false;
    }
    LAST_CAPTURE_MS
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Keep file names boring: the code is an error code or `exit_N`, but it
/// still passes through here in case a future one carries punctuation.
fn sanitize(code: &str) -> String {
    code.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

async fn capture(
    rdp_session: Arc<Mutex<Option<RdpSession>>>,
    with_ocr: bool,
    dir: std::path::PathBuf,
    stem: String,
    mut context: Value,
) -> anyhow::Result<()> {
    // One frame copy, reused for the PNG and the OCR pass. `get_image_data`
    // takes the frame processor's sync lock; `block_in_place` keeps that
    // wait off the async worker.
    let (width, height, data, frame_seq) = {
        let session = rdp_session.lock().await;
        let Some(rdp) = session.as_ref() else {
            anyhow::bail!("session gone before capture");
        };
        let frame_seq = rdp.frame_generation();
        let (w, h, data) = tokio::task::block_in_place(|| rdp.get_image_data());
        (w as u32, h as u32, data, frame_seq)
    };

    let ocr = if with_ocr {
        crate::handlers::locate::get_or_init_ocr_service().await
    } else {
        None
    };

    let (png, frame_hash, ocr_lines) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let rgba = image::RgbaImage::from_raw(width, height, data)
            .ok_or_else(|| anyhow::anyhow!("framebuffer size mismatch"))?;
        let frame_hash = hash_pixels(rgba.as_raw());
        let png = encode_image(&rgba, ImageFormat::Png).map_err(|e| anyhow::anyhow!(e))?;
        let ocr_lines = match ocr {
            Some(ocr) => {
                let rgb = image::DynamicImage::ImageRgba8(rgba).into_rgb8();
                match ocr.get_all_lines_rgb(&rgb) {
                    Ok((lines, _)) => Some(serde_json::to_value(lines).unwrap_or(Value::Null)),
                    Err(e) => Some(json!({ "error": e.to_string() })),
                }
            }
            None => None,
        };
        Ok((png, frame_hash, ocr_lines))
    })
    .await??;

    context["frame_seq"] = json!(frame_seq);
    context["frame_hash"] = json!(frame_hash);
    context["width"] = json!(width);
    context["height"] = json!(height);
    context["screenshot"] = json!(format!("{}.png", stem));
    if let Some(lines) = ocr_lines {
        context["ocr_lines"] = lines;
    }

    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{}.png", stem)), &png)?;
    std::fs::write(dir.join(format!("{}.json", stem)), serde_json::to_vec_pretty(&context)?)?;
    info!("diagnostics: saved {}/{}.{{png,json}}", dir.display(), stem);

    prune(&dir, MAX_CAPTURES);
    Ok(())
}

/// Delete the oldest capture pairs beyond `keep`. Names start with a UTC
/// timestamp, so lexical order is chronological.
pub fn prune(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut stems: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(str::to_string)
        })
        .collect();
    stems.sort();
    if stems.len() <= keep {
        return;
    }
    for stem in &stems[..stems.len() - keep] {
        for ext in ["png", "json"] {
            let _ = std::fs::remove_file(dir.join(format!("{}.{}", stem, ext)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_rdp_protocol::{LocateRequest, LocateResult, RunResult};

    fn locate(all: bool) -> Request {
        let mut req: LocateRequest = serde_json::from_str(r#"{"text":"OK"}"#).unwrap();
        req.all = all;
        Request::Locate(req)
    }

    fn locate_result(n: usize) -> Response {
        let matches = (0..n)
            .map(|i| serde_json::from_value(json!({
                "text": format!("m{i}"), "x": 0, "y": 0, "width": 1, "height": 1, "center_x": 0, "center_y": 0
            })).unwrap())
            .collect();
        Response::success(ResponseData::LocateResult(LocateResult { matches, total_words: 10 }))
    }

    fn run(exit_code: Option<i32>) -> (Request, Response) {
        let request: AutomateRequest = serde_json::from_str(r#"{"op":"run","command":"x","wait":true}"#).unwrap();
        let response = Response::success(ResponseData::RunResult(RunResult {
            exit_code,
            stdout: None,
            stderr: None,
            pid: None,
            replayed: false,
            early_exit: false,
            started_unix: None,
            finished_unix: None,
            command_line: None,
            replayed_at_unix: None,
            streamed: false,
        }));
        (Request::Automate(request), response)
    }

    #[test]
    fn locate_no_match_triggers_with_ocr() {
        let t = trigger_for(&locate(false), &locate_result(0)).unwrap();
        assert_eq!(t, Trigger { kind: "locate", code: "no_match".into(), with_ocr: true });
        assert!(trigger_for(&locate(false), &locate_result(2)).is_none());
        // `--all` returns whatever is on screen; empty is not a failure.
        assert!(trigger_for(&locate(true), &locate_result(0)).is_none());
    }

    #[test]
    fn nonzero_exit_triggers_and_zero_does_not() {
        let (req, resp) = run(Some(1));
        assert_eq!(trigger_for(&req, &resp).unwrap().code, "exit_1");
        let (req, resp) = run(Some(0));
        assert!(trigger_for(&req, &resp).is_none());
        let (req, resp) = run(None);
        assert!(trigger_for(&req, &resp).is_none());
    }

    #[test]
    fn infrastructure_errors_never_trigger() {
        for code in [
            ErrorCode::NotConnected,
            ErrorCode::Timeout,
            ErrorCode::AutomationIndeterminate,
            ErrorCode::InvalidRequest,
        ] {
            let resp = Response::error(code, "x");
            assert!(trigger_for(&locate(false), &resp).is_none(), "{:?}", resp.error);
        }
        let resp = Response::error(ErrorCode::InternalError, "OCR failed");
        assert_eq!(trigger_for(&locate(false), &resp).unwrap().code, "internal_error");
    }

    #[test]
    fn status_and_poll_are_ignored() {
        let resp = Response::error(ErrorCode::AutomationError, "x");
        assert!(trigger_for(&Request::Automate(AutomateRequest::Status), &resp).is_none());
        assert!(trigger_for(&Request::Automate(AutomateRequest::RunPoll { pid: 1 }), &resp).is_none());
        assert!(trigger_for(&Request::Ping, &resp).is_none());
    }

    #[test]
    fn prune_keeps_the_newest_pairs() {
        let dir = std::env::temp_dir().join(format!("agent-rdp-diag-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..25 {
            let stem = format!("20260902-1445{:02}-locate-no_match", i);
            std::fs::write(dir.join(format!("{stem}.png")), b"png").unwrap();
            std::fs::write(dir.join(format!("{stem}.json")), b"{}").unwrap();
        }
        prune(&dir, 20);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), 40);
        assert!(left[0].starts_with("20260902-144505-"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_is_file_name_safe() {
        assert_eq!(sanitize("exit_-1"), "exit_-1");
        assert_eq!(sanitize("a/b:c d"), "a_b_c_d");
    }
}
