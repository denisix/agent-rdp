//! Request types for CLI to daemon communication.

use crate::automation::AutomateRequest;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A request from the CLI to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Connect to an RDP server.
    Connect(ConnectRequest),

    /// Disconnect from the RDP server.
    Disconnect,

    /// Take a screenshot.
    Screenshot(ScreenshotRequest),

    /// Mouse operation.
    Mouse(MouseRequest),

    /// Keyboard operation.
    Keyboard(KeyboardRequest),

    /// Scroll operation.
    Scroll(ScrollRequest),

    /// Clipboard operation.
    Clipboard(ClipboardRequest),

    /// Drive mapping operation.
    Drive(DriveRequest),

    /// UI Automation operation.
    Automate(AutomateRequest),

    /// OCR-based text location.
    Locate(LocateRequest),

    /// Click a caller-supplied point, refusing if it is ambiguously close to
    /// more than one detected text region.
    ClickAt(ClickAtRequest),

    /// Copy a local file to the remote machine.
    FilePush(FilePushRequest),

    /// Copy a file from the remote machine to the local machine.
    FilePull(FilePullRequest),

    /// Existence, size, hash and modification time of a remote file, without
    /// transferring it.
    FileStat(FileStatRequest),

    /// Relaunch the UI Automation agent without a full RDP reconnect.
    ///
    /// Requires automation to have been initialized at `connect` (i.e.
    /// `--enable-win-automation` was passed, even if the agent later died or
    /// never came up) - the drive it relies on is only mapped at connect
    /// time and cannot be added afterward.
    AutomationRestart,

    /// Get session info.
    SessionInfo,

    /// Ping the daemon (for health checks).
    Ping,

    /// Shutdown the daemon gracefully.
    Shutdown,
}

impl Request {
    /// Whether the request can be re-sent without changing anything on the
    /// remote side. The CLI retries such a request once when the daemon
    /// closes the connection before answering; a mutating request is never
    /// retried automatically, because "did it apply?" is exactly the
    /// question a dropped connection leaves open.
    pub fn is_read_only(&self) -> bool {
        match self {
            Request::Ping
            | Request::SessionInfo
            | Request::Screenshot(_)
            | Request::Locate(_)
            | Request::FileStat(_) => true,
            Request::Clipboard(ClipboardRequest::Get) => true,
            Request::Drive(DriveRequest::List) => true,
            Request::Automate(automate) => automate.is_read_only(),
            _ => false,
        }
    }
}

/// A drive to map at connect time.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct DriveMapping {
    /// Local path to map.
    pub path: String,
    /// Name for the mapped drive (shown in Windows).
    pub name: String,
}

/// RDP connection parameters.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ConnectRequest {
    /// Server hostname or IP address.
    pub host: String,

    /// Server port (default: 3389).
    pub port: u16,

    /// Username for authentication.
    pub username: String,

    /// Password for authentication.
    pub password: String,

    /// Optional domain.
    #[serde(default)]
    #[ts(optional)]
    pub domain: Option<String>,

    /// Desktop width in pixels.
    pub width: u16,

    /// Desktop height in pixels.
    pub height: u16,

    /// Drives to map at connect time.
    #[serde(default)]
    pub drives: Vec<DriveMapping>,

    /// Enable Windows UI Automation.
    #[serde(default)]
    pub enable_win_automation: bool,

    /// WebSocket streaming port (0 = disabled).
    #[serde(default)]
    pub stream_port: u16,

    /// Address the streaming server binds to (default: 127.0.0.1).
    ///
    /// The stream grants full mouse/keyboard/clipboard control of the session
    /// and has no authentication, so it stays on loopback unless explicitly
    /// widened (e.g. to 0.0.0.0 inside a trusted network).
    #[serde(default = "default_stream_bind")]
    pub stream_bind: String,

    /// Streaming frame rate (default: 10).
    #[serde(default = "default_stream_fps")]
    pub stream_fps: u32,

    /// Streaming JPEG quality 0-100 (default: 80).
    #[serde(default = "default_stream_quality")]
    pub stream_quality: u8,

    /// Serve the embedded HTML viewer on the streaming port (default: false).
    /// When false, only WebSocket connections are accepted.
    #[serde(default)]
    pub serve_viewer: bool,
}

fn default_stream_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_stream_fps() -> u32 {
    10
}

fn default_stream_quality() -> u8 {
    80
}

impl Default for ConnectRequest {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 3389,
            username: String::new(),
            password: String::new(),
            domain: None,
            width: 1280,
            height: 800,
            drives: Vec::new(),
            enable_win_automation: false,
            stream_port: 0,
            stream_bind: default_stream_bind(),
            stream_fps: default_stream_fps(),
            stream_quality: default_stream_quality(),
            serve_viewer: false,
        }
    }
}

/// A rectangular sub-area of the desktop, in framebuffer pixels.
///
/// Used to restrict a screenshot or an OCR pass to part of the screen. The
/// origin is always the top-left of the full desktop, and any coordinate the
/// daemon reports back for a region request is translated back into that same
/// full-desktop space - callers never have to add the offset themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// Intersect the region with a `width` x `height` framebuffer.
    ///
    /// Returns `None` when the region lies entirely outside the framebuffer,
    /// which callers should treat as an error rather than silently widening to
    /// the full screen.
    pub fn clamp_to(&self, width: u32, height: u32) -> Option<Region> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let right = self.x.saturating_add(self.width).min(width);
        let bottom = self.y.saturating_add(self.height).min(height);

        if right <= x || bottom <= y {
            return None;
        }

        Some(Region {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }
}

/// Screenshot request parameters.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ScreenshotRequest {
    /// Image format.
    #[serde(default)]
    pub format: ImageFormat,

    /// Capture only this part of the desktop (default: the whole desktop).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub region: Option<Region>,
}

/// Supported image formats.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
}

/// Mouse operation request.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MouseRequest {
    /// Move the mouse cursor.
    Move { x: u16, y: u16 },

    /// Left click.
    Click { x: u16, y: u16 },

    /// Right click.
    RightClick { x: u16, y: u16 },

    /// Double click.
    DoubleClick { x: u16, y: u16 },

    /// Middle click.
    MiddleClick { x: u16, y: u16 },

    /// Drag from one position to another.
    Drag {
        from_x: u16,
        from_y: u16,
        to_x: u16,
        to_y: u16,
    },

    /// Press and hold a mouse button.
    ButtonDown { button: MouseButton },

    /// Release a mouse button.
    ButtonUp { button: MouseButton },
}

/// Mouse button identifiers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Keyboard operation request.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum KeyboardRequest {
    /// Type a text string (Unicode).
    Type {
        text: String,
        /// Pause in milliseconds between batches of characters. Only needed for
        /// remote applications that drop input arriving too quickly; omitted
        /// means send as fast as the connection allows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional, type = "number")]
        delay_ms: Option<u64>,
    },

    /// Press a key combination (e.g., "ctrl+c", "alt+tab", or single key like "enter").
    Press { keys: String },

    /// Press and hold a key.
    KeyDown { key: String },

    /// Release a held key.
    KeyUp { key: String },

    /// Set the remote clipboard to `text`, then send Ctrl+V.
    ///
    /// One atomic command rather than `clipboard set` + `keyboard press`, so
    /// focus cannot move between the two. This is the reliable path for long
    /// or non-Latin text: it cannot lose individual keystrokes and is immune
    /// to autocomplete popups eating input mid-string.
    Paste { text: String },
}

/// Scroll operation request.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ScrollRequest {
    /// Scroll direction.
    pub direction: ScrollDirection,

    /// Amount to scroll (in notches, default: 3).
    #[serde(default = "default_scroll_amount")]
    pub amount: u32,

    /// Optional position to scroll at.
    #[serde(default)]
    #[ts(optional)]
    pub x: Option<u16>,

    #[serde(default)]
    #[ts(optional)]
    pub y: Option<u16>,
}

fn default_scroll_amount() -> u32 {
    3
}

/// Scroll direction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Clipboard operation request.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ClipboardRequest {
    /// Get clipboard text content.
    Get,

    /// Set clipboard text content.
    Set { text: String },
}

/// Drive mapping operation request.
/// Note: Drives are configured at connect time with --drive flag.
/// Dynamic mapping/unmapping is not supported by the RDP protocol.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DriveRequest {
    /// List mapped drives.
    List,
}

/// OCR-based text location request.
/// Uses screenshot + OCR to find text on screen and return coordinates.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct LocateRequest {
    /// Text to search for (ignored if `all` is true).
    #[serde(default)]
    pub text: String,

    /// Use pattern matching (glob-style: * and ?).
    #[serde(default)]
    pub pattern: bool,

    /// Require the whole OCR line to equal `text` (respecting
    /// `ignore_case`), instead of substring containment. Takes precedence
    /// over `pattern` when both are set.
    ///
    /// Default substring mode matches "Провести" against a line reading
    /// "Провести и закрыть" - usually harmless (`--click` already refuses to
    /// guess when a query is ambiguous), but `exact` gives a named way to
    /// avoid the ambiguity in the first place rather than relying on
    /// `--pattern` with no wildcards, which happens to require a full match
    /// as an undocumented side effect of glob anchoring.
    #[serde(default)]
    pub exact: bool,

    /// Case-insensitive matching (default: true).
    #[serde(default = "default_true")]
    pub ignore_case: bool,

    /// Return all text on screen (ignores text/pattern/ignore_case).
    #[serde(default)]
    pub all: bool,

    /// Restrict OCR to this part of the desktop (default: the whole desktop).
    ///
    /// Match coordinates are still reported in full-desktop space.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub region: Option<Region>,

    /// Keep re-running OCR until the text appears or this many milliseconds
    /// pass (default: single pass).
    ///
    /// Lets a caller block server-side on "the dialog is now visible" instead
    /// of polling screenshot/locate in a loop from the outside. On timeout the
    /// response is a `timeout` error rather than an empty success, so a
    /// wait-then-click sequence cannot fall through to clicking nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub wait_ms: Option<u64>,

    /// Constrain matches to those within `near_distance` px of a line
    /// containing this anchor text (substring match).
    ///
    /// The same text often appears in more than one place on screen (a
    /// column header repeated in several rows, a label that also appears in
    /// a tooltip); anchoring to a nearby, more distinctive label disambiguates
    /// without needing `--exact` to already know the one true full string.
    /// The anchor itself is matched by substring, same as the default `text`
    /// mode - if it isn't found at all, the result is zero matches (not an
    /// error), since there is nothing to anchor to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub near: Option<String>,

    /// Maximum distance in pixels from the anchor's bounding box, used only
    /// when `near` is set.
    #[serde(default = "default_near_distance")]
    pub near_distance: u32,
}

fn default_true() -> bool {
    true
}

fn default_near_distance() -> u32 {
    150
}

/// Click a caller-supplied point, refusing if it's ambiguously close to more
/// than one OCR-detected text region.
///
/// For callers that already know where to click (e.g. a screenshot read by a
/// vision model) and want the same "don't guess when it's ambiguous" safety
/// `locate --click` gives text search - without needing OCR to correctly
/// *read* the label, only to detect that one exists nearby. Detection and
/// recognition are separate OCR stages, so this works even for text OCR
/// recognition can't read (e.g. some Cyrillic renderers).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ClickAtRequest {
    pub x: u16,
    pub y: u16,

    /// Width of the OCR detection window centered on the point (default: 400).
    #[serde(default = "default_click_at_window_w")]
    pub window_width: u32,

    /// Height of the OCR detection window centered on the point (default: 160).
    #[serde(default = "default_click_at_window_h")]
    pub window_height: u32,

    /// Minimum pixel gap between the point's containing region and any other
    /// detected region's boundary, below which the click is refused as
    /// ambiguous (default: 10).
    #[serde(default = "default_click_at_min_gap")]
    pub min_gap: u32,

    #[serde(default)]
    pub double_click: bool,

    #[serde(default)]
    pub right_click: bool,

    /// A second, independently measured point for the same target - e.g. a
    /// vision model queried twice, or two different vision calls. Formalizes
    /// the "two independent measurements, click the intersection" workflow:
    /// when both points roughly agree, their midpoint is used as the click
    /// target instead of the first point alone, which cancels out
    /// measurement noise from either single call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub confirm_x: Option<u16>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub confirm_y: Option<u16>,

    /// Maximum allowed distance in pixels between `(x, y)` and
    /// `(confirm_x, confirm_y)` before the click is refused as diverging
    /// measurements rather than noise (default: 40). Ignored unless both
    /// confirm coordinates are set.
    #[serde(default = "default_click_at_max_divergence")]
    pub max_divergence: u32,
}

fn default_click_at_window_w() -> u32 {
    400
}

fn default_click_at_window_h() -> u32 {
    160
}

fn default_click_at_min_gap() -> u32 {
    10
}

fn default_click_at_max_divergence() -> u32 {
    40
}

/// Copy a local file to the remote machine, in chunks over the automation
/// channel.
///
/// The drive-redirection share (`\\TSCLIENT\...`) is not a usable substitute:
/// reaching it from inside the automation agent blocks indefinitely, taking
/// the session's frame processor with it. Chunked transfer goes over the same
/// DVC channel every other automate command uses, and is verified end to end.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct FilePushRequest {
    /// Path of the file to read on this machine.
    pub local_path: String,
    /// Destination path on the remote machine.
    pub remote_path: String,
}

/// Copy a file from the remote machine to this one.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct FileStatRequest {
    /// Path to inspect on the remote machine.
    pub remote_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct FilePullRequest {
    /// Path of the file to read on the remote machine.
    pub remote_path: String,
    /// Destination path on this machine.
    pub local_path: String,
    /// Refuse (with `stale_file`) if the remote file was last written more
    /// than this many seconds ago, by the remote machine's clock. Nothing is
    /// transferred in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub max_age_secs: Option<u64>,
}

#[cfg(test)]
mod read_only_tests {
    use super::*;

    #[test]
    fn reads_are_retryable_and_writes_are_not() {
        assert!(Request::Ping.is_read_only());
        assert!(Request::SessionInfo.is_read_only());
        assert!(Request::Clipboard(ClipboardRequest::Get).is_read_only());
        assert!(Request::Drive(DriveRequest::List).is_read_only());
        assert!(Request::FileStat(FileStatRequest { remote_path: "C:\\x".into() }).is_read_only());
        assert!(Request::Automate(AutomateRequest::Status).is_read_only());

        assert!(!Request::Disconnect.is_read_only());
        assert!(!Request::Shutdown.is_read_only());
        assert!(!Request::Clipboard(ClipboardRequest::Set { text: "x".into() }).is_read_only());
        assert!(!Request::AutomationRestart.is_read_only());
        assert!(!Request::Automate(AutomateRequest::Click { selector: "e1".into(), double_click: false })
            .is_read_only());
        assert!(!Request::FilePull(FilePullRequest {
            remote_path: "a".into(),
            local_path: "b".into(),
            max_age_secs: None
        })
        .is_read_only());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_region_clamp_inside_framebuffer() {
        let r = Region { x: 100, y: 380, width: 400, height: 30 };
        assert_eq!(r.clamp_to(1280, 800), Some(r));
    }

    #[test]
    fn test_region_clamp_trims_overhang() {
        let r = Region { x: 1200, y: 780, width: 400, height: 300 };
        assert_eq!(
            r.clamp_to(1280, 800),
            Some(Region { x: 1200, y: 780, width: 80, height: 20 })
        );
    }

    #[test]
    fn test_region_clamp_rejects_offscreen() {
        // Fully past the right edge, and a zero-width region: both are errors
        // rather than a silent fall back to the full desktop.
        assert_eq!(Region { x: 1280, y: 0, width: 100, height: 100 }.clamp_to(1280, 800), None);
        assert_eq!(Region { x: 10, y: 10, width: 0, height: 50 }.clamp_to(1280, 800), None);
    }

    #[test]
    fn test_region_clamp_does_not_overflow() {
        let r = Region { x: 10, y: 10, width: u32::MAX, height: u32::MAX };
        assert_eq!(
            r.clamp_to(1280, 800),
            Some(Region { x: 10, y: 10, width: 1270, height: 790 })
        );

        // Origin at the maximum too: saturating_add must not wrap round to a
        // small value and produce a bogus in-bounds region.
        let r = Region { x: u32::MAX, y: u32::MAX, width: u32::MAX, height: u32::MAX };
        assert_eq!(r.clamp_to(1280, 800), None);
    }

    #[test]
    fn test_region_clamp_exactly_the_framebuffer() {
        let r = Region { x: 0, y: 0, width: 1280, height: 800 };
        assert_eq!(r.clamp_to(1280, 800), Some(r));
    }

    #[test]
    fn test_region_clamp_last_pixel_is_in_bounds() {
        // The bottom-right pixel is a classic off-by-one; it must survive.
        let r = Region { x: 1279, y: 799, width: 1, height: 1 };
        assert_eq!(r.clamp_to(1280, 800), Some(r));
    }

    #[test]
    fn test_region_clamp_trims_one_axis_at_a_time() {
        // Overhanging only horizontally leaves the vertical extent untouched,
        // and vice versa - a clamp that trimmed both would go unnoticed if
        // only the symmetric case were tested.
        assert_eq!(
            Region { x: 1000, y: 100, width: 500, height: 50 }.clamp_to(1280, 800),
            Some(Region { x: 1000, y: 100, width: 280, height: 50 })
        );
        assert_eq!(
            Region { x: 100, y: 700, width: 50, height: 500 }.clamp_to(1280, 800),
            Some(Region { x: 100, y: 700, width: 50, height: 100 })
        );
    }

    #[test]
    fn test_region_clamp_rejects_below_bottom_edge() {
        // The x axis is fully in bounds here, so only a y-axis check can
        // reject it.
        assert_eq!(Region { x: 0, y: 800, width: 100, height: 100 }.clamp_to(1280, 800), None);
        assert_eq!(Region { x: 0, y: 5000, width: 100, height: 100 }.clamp_to(1280, 800), None);
    }

    #[test]
    fn test_region_clamp_against_empty_framebuffer() {
        // Before the first frame arrives the framebuffer can be 0x0; every
        // region is then out of bounds rather than a panic or an empty crop.
        assert_eq!(Region { x: 0, y: 0, width: 10, height: 10 }.clamp_to(0, 0), None);
    }

    #[test]
    fn test_region_clamp_is_idempotent() {
        // Clamping an already-clamped region must not shrink it further.
        let once = Region { x: 1200, y: 780, width: 400, height: 300 }
            .clamp_to(1280, 800)
            .unwrap();
        assert_eq!(once.clamp_to(1280, 800), Some(once));
    }

    #[test]
    fn test_screenshot_region_is_optional() {
        // Old clients omit `region` entirely; it must still deserialize.
        let req: ScreenshotRequest = serde_json::from_str(r#"{"format":"png"}"#).unwrap();
        assert!(req.region.is_none());

        let req: LocateRequest = serde_json::from_str(r#"{"text":"OK"}"#).unwrap();
        assert!(req.region.is_none());
    }

    #[test]
    fn test_request_serialization() {
        let req = Request::Connect(ConnectRequest {
            host: "192.168.1.100".to_string(),
            port: 3389,
            username: "admin".to_string(),
            password: "secret".to_string(),
            domain: Some("WORKGROUP".to_string()),
            width: 1920,
            height: 1080,
            drives: vec![],
            enable_win_automation: false,
            ..Default::default()
        });

        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();

        match parsed {
            Request::Connect(c) => {
                assert_eq!(c.host, "192.168.1.100");
                assert_eq!(c.port, 3389);
            }
            _ => panic!("unexpected request type"),
        }
    }

    #[test]
    fn test_connect_with_drives() {
        let req = Request::Connect(ConnectRequest {
            host: "192.168.1.100".to_string(),
            port: 3389,
            username: "admin".to_string(),
            password: "secret".to_string(),
            domain: None,
            width: 1920,
            height: 1080,
            drives: vec![
                DriveMapping {
                    path: "/home/user/docs".to_string(),
                    name: "Documents".to_string(),
                },
                DriveMapping {
                    path: "/tmp/shared".to_string(),
                    name: "Shared".to_string(),
                },
            ],
            enable_win_automation: false,
            ..Default::default()
        });

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"drives\""));
        assert!(json.contains("Documents"));
        assert!(json.contains("Shared"));

        let parsed: Request = serde_json::from_str(&json).unwrap();
        match parsed {
            Request::Connect(c) => {
                assert_eq!(c.drives.len(), 2);
                assert_eq!(c.drives[0].name, "Documents");
                assert_eq!(c.drives[1].path, "/tmp/shared");
            }
            _ => panic!("unexpected request type"),
        }
    }

    #[test]
    fn test_mouse_request_serialization() {
        let req = Request::Mouse(MouseRequest::Click { x: 100, y: 200 });
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"action\":\"click\""));
        assert!(json.contains("\"x\":100"));
    }

    #[test]
    fn test_keyboard_request_serialization() {
        let req = Request::Keyboard(KeyboardRequest::Press {
            keys: "ctrl+c".to_string(),
        });
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"action\":\"press\""));
        assert!(json.contains("ctrl+c"));
    }
}
