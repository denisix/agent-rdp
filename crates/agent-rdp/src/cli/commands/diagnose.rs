//! `agent-rdp diagnose`: bundle everything a bug report needs into one zip.
//!
//! Everything on disk is collected first and the daemon is asked only
//! afterwards, each round trip best-effort with its own budget, so the
//! command still produces a useful bundle when the daemon is the thing that
//! is broken - which is exactly when a bug report is being written.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agent_rdp_daemon::timefmt;
use agent_rdp_daemon::{get_session_dir, DIAGNOSTICS_DIR, TRANSCRIPT_FILE, TRANSCRIPT_PREV_FILE};
use agent_rdp_protocol::{
    AutomateRequest, FilePullRequest, ImageFormat, Request, ResponseData, ScreenshotRequest,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::cli::DiagnoseArgs;
use crate::ipc_client::IpcClient;
use crate::output::Output;
use crate::session_manager::{daemon_log_path, SessionManager, CLI_VERSION};

const PING_TIMEOUT_MS: u64 = 3_000;
const INFO_TIMEOUT_MS: u64 = 5_000;
const STATUS_TIMEOUT_MS: u64 = 5_000;
const SCREENSHOT_TIMEOUT_MS: u64 = 10_000;
const REMOTE_LOG_TIMEOUT_MS: u64 = 60_000;

/// Sum of the daemon round-trip budgets above, for the CLI watchdog.
pub const TOTAL_DAEMON_BUDGET_MS: u64 = PING_TIMEOUT_MS
    + INFO_TIMEOUT_MS
    + STATUS_TIMEOUT_MS
    + SCREENSHOT_TIMEOUT_MS
    + REMOTE_LOG_TIMEOUT_MS;

/// Text files are included from the end, capped at this many bytes.
const MAX_TEXT_BYTES: usize = 5 * 1024 * 1024;

/// One file in the bundle.
struct Entry {
    name: String,
    bytes: Vec<u8>,
}

/// The bundle under construction.
#[derive(Default)]
struct Bundle {
    entries: Vec<Entry>,
    /// Things that were attempted and not included, with the reason.
    skipped: Vec<(String, String)>,
}

impl Bundle {
    fn add(&mut self, name: impl Into<String>, bytes: Vec<u8>) {
        self.entries.push(Entry { name: name.into(), bytes });
    }

    fn skip(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        self.skipped.push((name.into(), reason.into()));
    }

    /// Add a text file from disk, keeping only its tail if it is large.
    fn add_text_file(&mut self, name: &str, path: &Path) {
        match std::fs::read(path) {
            Ok(bytes) => self.add(name, tail_capped(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.skip(name, "not present");
            }
            Err(e) => self.skip(name, format!("unreadable: {}", e)),
        }
    }
}

/// Keep the last `MAX_TEXT_BYTES` of a text file, cut at a line boundary,
/// with a note about what was dropped.
fn tail_capped(bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() <= MAX_TEXT_BYTES {
        return bytes;
    }
    let mut start = bytes.len() - MAX_TEXT_BYTES;
    if let Some(nl) = bytes[start..].iter().position(|&b| b == b'\n') {
        start += nl + 1;
    }
    let mut out = format!("[agent-rdp diagnose: first {} bytes omitted]\n", start).into_bytes();
    out.extend_from_slice(&bytes[start..]);
    out
}

/// Blank out the RDP password wherever it appears verbatim in text.
///
/// Defence in depth: nothing is supposed to log it, but the bundle is meant
/// to be handed to other people, so a stray occurrence must not travel.
fn redact_password(bytes: Vec<u8>) -> Vec<u8> {
    let Ok(password) = std::env::var("AGENT_RDP_PASSWORD") else {
        return bytes;
    };
    if password.len() < 4 {
        return bytes;
    }
    let text = String::from_utf8_lossy(&bytes);
    if !text.contains(&password) {
        return bytes;
    }
    text.replace(&password, "***").into_bytes()
}

fn is_text_entry(name: &str) -> bool {
    name.ends_with(".log")
        || name.ends_with(".prev")
        || name.ends_with(".json")
        || name.ends_with(".jsonl")
        || name.ends_with(".txt")
}

pub async fn run(session: &str, args: DiagnoseArgs, output: &Output) -> anyhow::Result<()> {
    let now = SystemTime::now();
    let session_dir = get_session_dir(session);
    let manager = SessionManager::new(session.to_string());
    let mut bundle = Bundle::default();

    // ---- Local evidence first: never depends on the daemon. ----
    bundle.add_text_file("daemon.log", &daemon_log_path(session));
    bundle.add_text_file("daemon.log.prev", &daemon_log_path(session).with_extension("log.prev"));
    bundle.add_text_file(TRANSCRIPT_FILE, &session_dir.join(TRANSCRIPT_FILE));
    bundle.add_text_file(TRANSCRIPT_PREV_FILE, &session_dir.join(TRANSCRIPT_PREV_FILE));
    add_diagnostics_dir(&mut bundle, &session_dir.join(DIAGNOSTICS_DIR));

    // ---- Daemon, best-effort. ----
    let daemon_status = manager.daemon_status();
    let mut daemon_info = serde_json::json!({
        "pid": daemon_status.map(|(pid, _)| pid),
        "process_alive": daemon_status.map(|(_, alive)| alive),
        "reachable": false,
        "version": serde_json::Value::Null,
    });
    let mut session_info = serde_json::Value::Null;
    let mut automation_status = serde_json::Value::Null;

    let client = match daemon_status {
        Some((_, true)) => match crate::ipc_client::try_connect(&manager.socket_path(), 1, 100).await {
            Ok(client) => Some(client),
            Err(e) => {
                bundle.skip("daemon", format!("socket connect failed: {}", e));
                None
            }
        },
        Some((_, false)) => {
            bundle.skip("daemon", "pid file present but the process is gone");
            None
        }
        None => {
            bundle.skip("daemon", "not running (no pid file)");
            None
        }
    };

    if let Some(mut client) = client {
        match client.send(&Request::Ping, PING_TIMEOUT_MS).await {
            Ok(resp) if resp.success => {
                daemon_info["reachable"] = serde_json::Value::Bool(true);
                if let Some(ResponseData::Pong { version }) = resp.data {
                    daemon_info["version"] = serde_json::Value::String(version);
                }
            }
            Ok(resp) => bundle.skip("daemon", format!("ping refused: {:?}", resp.error)),
            Err(e) => bundle.skip("daemon", format!("ping timed out ({}s): {}", PING_TIMEOUT_MS / 1000, e)),
        }

        if daemon_info["reachable"].as_bool() == Some(true) {
            match client.send(&Request::SessionInfo, INFO_TIMEOUT_MS).await {
                Ok(resp) => session_info = serde_json::to_value(&resp).unwrap_or_default(),
                Err(e) => bundle.skip("session_info", e.to_string()),
            }

            let status_req = Request::Automate(AutomateRequest::Status);
            let mut remote_log_path = None;
            match client.send(&status_req, STATUS_TIMEOUT_MS).await {
                Ok(resp) => {
                    if let Some(ResponseData::AutomationStatus(ref status)) = resp.data {
                        if status.agent_running {
                            remote_log_path = status.log_path.clone();
                        }
                    }
                    automation_status = serde_json::to_value(&resp).unwrap_or_default();
                }
                Err(e) => bundle.skip("automation_status", e.to_string()),
            }

            let shot = Request::Screenshot(ScreenshotRequest {
                format: ImageFormat::Png,
                region: None,
            });
            match client.send(&shot, SCREENSHOT_TIMEOUT_MS).await {
                Ok(resp) => match resp.data {
                    Some(ResponseData::Screenshot { base64, .. }) => {
                        use base64::Engine;
                        match base64::engine::general_purpose::STANDARD.decode(&base64) {
                            Ok(png) => bundle.add("screenshot-now.png", png),
                            Err(e) => bundle.skip("screenshot-now.png", format!("bad base64: {}", e)),
                        }
                    }
                    _ => bundle.skip(
                        "screenshot-now.png",
                        resp.error
                            .map(|e| format!("{}: {}", agent_rdp_daemon::transcript::code_name(&e.code), e.message))
                            .unwrap_or_else(|| "no screenshot data".into()),
                    ),
                },
                Err(e) => bundle.skip("screenshot-now.png", e.to_string()),
            }

            match remote_log_path {
                Some(remote_path) => pull_remote_log(&mut client, &mut bundle, &remote_path).await,
                None => bundle.skip("remote-agent.log", "automation agent not running"),
            }
        }
    }

    // ---- info.json ----
    let mut env_names: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("AGENT_RDP_"))
        .collect();
    env_names.sort();
    let info = serde_json::json!({
        "generated_at": timefmt::utc_rfc3339(now),
        "cli_version": CLI_VERSION,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "session": session,
        "session_dir": session_dir.display().to_string(),
        "daemon": daemon_info,
        "session_info": session_info,
        "automation_status": automation_status,
        "env": {
            "set": env_names,
            "host": std::env::var("AGENT_RDP_HOST").ok(),
            "port": std::env::var("AGENT_RDP_PORT").ok(),
        },
        "skipped": bundle.skipped.iter().map(|(n, r)| serde_json::json!({"name": n, "reason": r})).collect::<Vec<_>>(),
    });
    bundle.add("info.json", serde_json::to_vec_pretty(&info)?);

    // ---- Write the zip. ----
    let out_path = match args.output {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(format!(
            "agent-rdp-diagnostics-{}-{}.zip",
            session,
            timefmt::utc_compact(now)
        )),
    };
    let bytes = write_zip(&out_path, &bundle)?;

    if output.is_json() {
        let report = serde_json::json!({
            "success": true,
            "data": {
                "type": "diagnostics",
                "path": out_path.display().to_string(),
                "bytes": bytes,
                "files": bundle.entries.iter().map(|e| serde_json::json!({"name": e.name, "bytes": e.bytes.len()})).collect::<Vec<_>>(),
                "skipped": bundle.skipped.iter().map(|(n, r)| serde_json::json!({"name": n, "reason": r})).collect::<Vec<_>>(),
            }
        });
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("Wrote {} ({} bytes)", out_path.display(), bytes);
        for entry in &bundle.entries {
            println!("  + {} ({} bytes)", entry.name, entry.bytes.len());
        }
        for (name, reason) in &bundle.skipped {
            println!("  - {}: {}", name, reason);
        }
        println!("Attach this file to the bug report. It contains no credentials: the RDP password is never logged and is blanked if it appears anyway.");
    }

    Ok(())
}

/// Add every file under `<session_dir>/diagnostics/`.
fn add_diagnostics_dir(bundle: &mut Bundle, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        bundle.skip(format!("{}/", DIAGNOSTICS_DIR), "no failure captures");
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    paths.sort();
    if paths.is_empty() {
        bundle.skip(format!("{}/", DIAGNOSTICS_DIR), "no failure captures");
    }
    for path in paths {
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let name = format!("{}/{}", DIAGNOSTICS_DIR, file_name);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let bytes = if is_text_entry(&name) { tail_capped(bytes) } else { bytes };
                bundle.add(name, bytes);
            }
            Err(e) => bundle.skip(name, format!("unreadable: {}", e)),
        }
    }
}

/// Pull the remote automation agent's log through the daemon's file-pull
/// path into a temp file, then into the bundle.
async fn pull_remote_log(client: &mut IpcClient, bundle: &mut Bundle, remote_path: &str) {
    let local = std::env::temp_dir().join(format!(
        "agent-rdp-diagnose-{}-{}.log",
        std::process::id(),
        timefmt::utc_compact(SystemTime::now())
    ));
    let request = Request::FilePull(FilePullRequest {
        remote_path: remote_path.to_string(),
        local_path: local.display().to_string(),
        max_age_secs: None,
    });
    match client.send(&request, REMOTE_LOG_TIMEOUT_MS).await {
        Ok(resp) if resp.success => match std::fs::read(&local) {
            Ok(bytes) => bundle.add("remote-agent.log", tail_capped(bytes)),
            Err(e) => bundle.skip("remote-agent.log", format!("pulled but unreadable: {}", e)),
        },
        Ok(resp) => bundle.skip(
            "remote-agent.log",
            resp.error
                .map(|e| format!("{}: {}", agent_rdp_daemon::transcript::code_name(&e.code), e.message))
                .unwrap_or_else(|| "pull failed".into()),
        ),
        Err(e) => bundle.skip("remote-agent.log", e.to_string()),
    }
    let _ = std::fs::remove_file(&local);
}

/// Write the bundle; returns the zip's size in bytes.
fn write_zip(path: &Path, bundle: &Bundle) -> anyhow::Result<u64> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = std::fs::File::create(path)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {}", path.display(), e))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for entry in &bundle.entries {
        zip.start_file(&entry.name, options)?;
        if is_text_entry(&entry.name) {
            zip.write_all(&redact_password(entry.bytes.clone()))?;
        } else {
            zip.write_all(&entry.bytes)?;
        }
    }
    let file = zip.finish()?;
    Ok(file.metadata().map(|m| m.len()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_cap_keeps_the_end_at_a_line_boundary() {
        let mut big = Vec::new();
        for i in 0..600_000 {
            big.extend_from_slice(format!("line {}\n", i).as_bytes());
        }
        assert!(big.len() > MAX_TEXT_BYTES);
        let capped = tail_capped(big.clone());
        assert!(capped.len() <= MAX_TEXT_BYTES + 64);
        let text = String::from_utf8(capped).unwrap();
        assert!(text.starts_with("[agent-rdp diagnose: first "));
        // The first kept line is complete, and the last line is the original last line.
        let mut lines = text.lines();
        lines.next();
        assert!(lines.next().unwrap().starts_with("line "));
        assert!(text.ends_with("line 599999\n"));
    }

    #[test]
    fn tail_cap_is_identity_for_small_input() {
        let small = b"hello\nworld\n".to_vec();
        assert_eq!(tail_capped(small.clone()), small);
    }

    #[test]
    fn text_entries_are_recognised() {
        assert!(is_text_entry("daemon.log"));
        assert!(is_text_entry("daemon.log.prev"));
        assert!(is_text_entry("transcript.jsonl"));
        assert!(is_text_entry("diagnostics/20260902-144501-locate-no_match.json"));
        assert!(!is_text_entry("diagnostics/20260902-144501-locate-no_match.png"));
    }

    #[test]
    fn zip_round_trip() {
        let dir = std::env::temp_dir().join(format!("agent-rdp-diagnose-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bundle.zip");

        let mut bundle = Bundle::default();
        bundle.add("info.json", b"{\"ok\":true}".to_vec());
        bundle.add("screenshot-now.png", vec![0x89, b'P', b'N', b'G']);
        let size = write_zip(&path, &bundle).unwrap();
        assert!(size > 0);

        let file = std::fs::File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, vec!["info.json", "screenshot-now.png"]);

        let mut info = String::new();
        std::io::Read::read_to_string(&mut archive.by_name("info.json").unwrap(), &mut info).unwrap();
        assert_eq!(info, "{\"ok\":true}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
