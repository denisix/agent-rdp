//! Request/response transcript: one redacted JSON line per IPC request.
//!
//! `daemon.log` says what the daemon was doing; this says what it was asked
//! to do, in order, with timings and outcomes - the "what sequence of
//! commands led here" half of a bug report. Covers the CLI and the TypeScript
//! SDK alike, since both go through the same IPC path.
//!
//! Written from the per-connection task, never from the daemon's main
//! `select!` loop, and always via `spawn_blocking`, so a slow disk can't
//! stall request handling.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use agent_rdp_protocol::{Request, Response, ResponseData};
use serde_json::{json, Value};
use tracing::debug;

use crate::{get_session_dir, timefmt, TRANSCRIPT_FILE, TRANSCRIPT_PREV_FILE};

/// Rotate to `.prev` once the transcript passes this size.
const MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Strings longer than this are truncated in summaries.
const MAX_STRING_CHARS: usize = 256;

/// A request or response summary larger than this collapses to its type.
const MAX_SUMMARY_BYTES: usize = 8 * 1024;

/// Whether the transcript and failure captures are enabled
/// (`AGENT_RDP_DIAGNOSTICS=0` turns both off). Read once.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("AGENT_RDP_DIAGNOSTICS").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// The wire name of an error code (`not_connected`), not its Display text
/// (`not connected`) - the same string the CLI's `--json` output carries.
pub fn code_name(code: &agent_rdp_protocol::ErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| code.to_string())
}

/// Record one request/response pair. Cheap on the caller: the summary is
/// built inline (it is small by construction), the write is offloaded.
pub fn record(session: &str, request: &Request, response: &Response, elapsed: Duration) {
    if !enabled() || matches!(request, Request::Ping) {
        return;
    }

    let line = json!({
        "ts": timefmt::utc_rfc3339(SystemTime::now()),
        "request": summarize_request(request),
        "success": response.success,
        "error_code": response.error.as_ref().map(|e| code_name(&e.code)),
        "error_message": response.error.as_ref().map(|e| e.message.clone()),
        "duration_ms": elapsed.as_millis() as u64,
        "response": summarize_response(response),
    });
    let dir = get_session_dir(session);
    tokio::task::spawn_blocking(move || {
        if let Err(e) = append_line(&dir, &line) {
            debug!("transcript write failed: {}", e);
        }
    });
}

/// Record a CLI-side event - a watchdog abort, a daemon-unavailable verdict,
/// an argument error - so the transcript explains gaps the daemon never
/// saw. Synchronous: the CLI is usually about to exit.
pub fn append_event(session: &str, event: Value) {
    if !enabled() {
        return;
    }
    let line = json!({
        "ts": timefmt::utc_rfc3339(SystemTime::now()),
        "event": event,
    });
    if let Err(e) = append_line(&get_session_dir(session), &line) {
        debug!("transcript event write failed: {}", e);
    }
}

/// The request as JSON, with secrets and bulk removed.
pub fn summarize_request(request: &Request) -> Value {
    let mut value = serde_json::to_value(request).unwrap_or(Value::Null);
    redact(&mut value);
    cap(value)
}

/// The response payload as JSON, with bulk removed. The screenshot payload
/// (multi-MB base64) is never serialized at all - it is summarized by hand.
pub fn summarize_response(response: &Response) -> Value {
    let Some(data) = response.data.as_ref() else {
        return Value::Null;
    };
    let mut value = match data {
        ResponseData::Screenshot {
            width,
            height,
            format,
            base64,
            frame_age_ms,
            frame_seq,
            frame_hash,
            ..
        } => json!({
            "type": "screenshot",
            "width": width,
            "height": height,
            "format": format,
            "base64_len": base64.len(),
            "frame_age_ms": frame_age_ms,
            "frame_seq": frame_seq,
            "frame_hash": frame_hash,
        }),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    };
    redact(&mut value);
    cap(value)
}

/// In place: blank passwords, replace base64 payloads with their size, and
/// truncate long strings. Recursive over objects and arrays.
pub fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if key == "password" {
                    *v = Value::String("***".into());
                } else if key == "data_b64" || key == "base64" {
                    if let Value::String(s) = v {
                        *v = json!({ "bytes": s.len() });
                    }
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        Value::String(s) => {
            if s.chars().count() > MAX_STRING_CHARS {
                let total = s.chars().count();
                let head: String = s.chars().take(MAX_STRING_CHARS).collect();
                *s = format!("{}…({} chars)", head, total);
            }
        }
        _ => {}
    }
}

/// Collapse a summary that is still too big after redaction to its type.
fn cap(value: Value) -> Value {
    let size = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
    if size <= MAX_SUMMARY_BYTES {
        return value;
    }
    let kind = value
        .get("type")
        .or_else(|| value.get("op"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({ "type": kind, "truncated_bytes": size })
}

/// Append one line, rotating to `.prev` when the file is large.
fn append_line(dir: &Path, line: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(TRANSCRIPT_FILE);
    if std::fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join(TRANSCRIPT_PREV_FILE));
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    let mut bytes = serde_json::to_vec(line)?;
    bytes.push(b'\n');
    file.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_rdp_protocol::{ConnectRequest, ErrorCode};

    #[test]
    fn password_is_blanked_and_host_kept() {
        let request = Request::Connect(ConnectRequest {
            host: "10.0.0.1".into(),
            port: 3389,
            username: "qa".into(),
            password: "hunter2".into(),
            domain: None,
            width: 1280,
            height: 720,
            drives: Vec::new(),
            enable_win_automation: false,
            stream_port: 0,
            stream_bind: "127.0.0.1".into(),
            stream_fps: 10,
            stream_quality: 60,
            serve_viewer: false,
        });
        let summary = summarize_request(&request);
        let text = summary.to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert_eq!(summary["password"], "***");
        assert_eq!(summary["host"], "10.0.0.1");
    }

    #[test]
    fn base64_payloads_become_sizes_and_long_strings_truncate() {
        let mut v = json!({
            "data_b64": "A".repeat(10_000),
            "nested": { "text": "x".repeat(1000), "base64": "QUJD" },
            "list": ["short", "y".repeat(300)],
        });
        redact(&mut v);
        assert_eq!(v["data_b64"]["bytes"], 10_000);
        assert_eq!(v["nested"]["base64"]["bytes"], 4);
        let text = v["nested"]["text"].as_str().unwrap();
        assert!(text.ends_with("…(1000 chars)"));
        assert!(text.len() < 300);
        assert_eq!(v["list"][0], "short");
        assert!(v["list"][1].as_str().unwrap().ends_with("…(300 chars)"));
    }

    #[test]
    fn screenshot_response_is_summarized_without_the_payload() {
        let response = Response::success(ResponseData::Screenshot {
            width: 1280,
            height: 720,
            format: "png".into(),
            base64: "Z".repeat(3_000_000),
            offset_x: None,
            offset_y: None,
            frame_age_ms: 12,
            frame_seq: 7,
            frame_hash: "deadbeef".into(),
        });
        let summary = summarize_response(&response);
        assert_eq!(summary["type"], "screenshot");
        assert_eq!(summary["base64_len"], 3_000_000);
        assert_eq!(summary["frame_seq"], 7);
        assert!(summary.to_string().len() < 512);
    }

    #[test]
    fn oversized_summaries_collapse_to_their_type() {
        let mut items = Vec::new();
        for i in 0..2000 {
            items.push(json!({ "text": format!("line {}", i), "x": i }));
        }
        let capped = cap(json!({ "type": "locate_result", "matches": items }));
        assert_eq!(capped["type"], "locate_result");
        assert!(capped["truncated_bytes"].as_u64().unwrap() > MAX_SUMMARY_BYTES as u64);
    }

    #[test]
    fn error_fields_are_recorded() {
        let response = Response::error(ErrorCode::NotConnected, "Not connected to an RDP server");
        assert_eq!(
            response.error.as_ref().map(|e| code_name(&e.code)).as_deref(),
            Some("not_connected")
        );
    }

    #[test]
    fn append_rotates_at_the_cap() {
        let dir = std::env::temp_dir().join(format!("agent-rdp-transcript-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Fake a file already past the cap, then append once.
        let path = dir.join(TRANSCRIPT_FILE);
        std::fs::write(&path, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        append_line(&dir, &json!({ "ts": "now" })).unwrap();

        assert!(dir.join(TRANSCRIPT_PREV_FILE).exists());
        let fresh = std::fs::read_to_string(&path).unwrap();
        assert_eq!(fresh, "{\"ts\":\"now\"}\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
