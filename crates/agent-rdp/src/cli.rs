//! CLI command definitions using clap.

use clap::{ArgGroup, Parser, Subcommand};

pub mod commands;

/// CLI tool for AI agents to control Windows Remote Desktop sessions.
#[derive(Parser)]
#[command(name = "agent-rdp", bin_name = "agent-rdp")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Session name
    #[arg(long, default_value = "default", env = "AGENT_RDP_SESSION")]
    pub session: String,

    /// Output in JSON format for AI consumption
    #[arg(long, global = true)]
    pub json: bool,

    /// Command timeout in milliseconds. Defaults to 30000 for ordinary
    /// commands and 90000 for `connect`, which additionally has to cover the
    /// TLS/CredSSP handshake and the automation agent bootstrap
    #[arg(long, global = true)]
    pub timeout: Option<u64>,

    /// WebSocket streaming port (0 = disabled, enables browser viewer for debugging)
    #[arg(long, default_value = "0", env = "AGENT_RDP_STREAM_PORT", global = true)]
    pub stream_port: u16,

    /// Address the streaming server binds to. The stream is unauthenticated and
    /// grants full control of the session, so it stays on loopback unless you
    /// explicitly widen it (e.g. 0.0.0.0 on a trusted network)
    #[arg(
        long,
        default_value = "127.0.0.1",
        env = "AGENT_RDP_STREAM_BIND",
        global = true
    )]
    pub stream_bind: String,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Connect to an RDP server
    Connect(ConnectArgs),

    /// Disconnect from RDP and close the session
    Disconnect,

    /// Take a screenshot
    Screenshot(ScreenshotArgs),

    /// Mouse operations
    Mouse(MouseArgs),

    /// Keyboard operations
    Keyboard(KeyboardArgs),

    /// Scroll operations
    Scroll(ScrollArgs),

    /// Clipboard operations
    Clipboard(ClipboardArgs),

    /// Drive mapping operations
    Drive(DriveArgs),

    /// Copy files to and from the remote machine
    File(FileArgs),

    /// Windows UI Automation operations
    Automate(AutomateArgs),

    /// OCR-based text location (find text on screen)
    Locate(LocateArgs),

    /// Click a known point, refusing if it's ambiguously close to more than
    /// one detected text region
    ///
    /// The safety net for coordinates computed outside agent-rdp (a vision
    /// model reading a screenshot, a manual crop) where OCR text search
    /// can't be used - detection-only, so it works even for text OCR can't
    /// read.
    #[command(name = "click-at")]
    ClickAt(ClickAtArgs),

    /// Session management
    Session(SessionArgs),

    /// Wait for specified milliseconds
    Wait {
        /// Milliseconds to wait
        ms: u64,
    },

    /// Open the web viewer in a browser
    View(ViewArgs),
}

/// View command arguments.
#[derive(Parser)]
pub struct ViewArgs {
    /// WebSocket streaming port to connect to
    #[arg(long, default_value = "9224")]
    pub port: u16,
}

/// Connect command arguments.
#[derive(Parser)]
pub struct ConnectArgs {
    /// Server hostname or IP (or set AGENT_RDP_HOST)
    #[arg(long, env = "AGENT_RDP_HOST", required = true)]
    pub host: String,

    /// Server port (or set AGENT_RDP_PORT)
    #[arg(long, default_value = "3389", env = "AGENT_RDP_PORT")]
    pub port: u16,

    /// Username (or set AGENT_RDP_USERNAME)
    #[arg(long, short = 'u', env = "AGENT_RDP_USERNAME", required = true)]
    pub username: String,

    /// Password (or set AGENT_RDP_PASSWORD, or use --password-stdin)
    #[arg(long, short = 'p', env = "AGENT_RDP_PASSWORD")]
    pub password: Option<String>,

    /// Read password from stdin (more secure than command line)
    #[arg(long)]
    pub password_stdin: bool,

    /// Domain
    #[arg(long, short = 'd')]
    pub domain: Option<String>,

    /// Desktop width
    #[arg(long, default_value = "1280")]
    pub width: u16,

    /// Desktop height
    #[arg(long, default_value = "800")]
    pub height: u16,

    /// Map local directories as drives (format: /path:DriveName, can be specified multiple times)
    #[arg(long = "drive", value_name = "PATH:NAME")]
    pub drives: Vec<String>,

    /// Enable Windows UI Automation (requires automation agent on remote host)
    #[arg(long)]
    pub enable_win_automation: bool,
}

/// Screenshot command arguments.
#[derive(Parser)]
pub struct ScreenshotArgs {
    /// Save to file path
    #[arg(long, short = 'o', default_value = "./screenshot.png")]
    pub output: String,

    /// Image format
    #[arg(long, default_value = "png")]
    pub format: String,

    /// Capture only part of the screen (X,Y,WIDTH,HEIGHT in screen pixels)
    #[arg(long, value_name = "X,Y,W,H", value_parser = crate::cli::commands::parse_region)]
    pub region: Option<agent_rdp_protocol::Region>,
}

/// Mouse command arguments.
#[derive(Parser)]
pub struct MouseArgs {
    #[command(subcommand)]
    pub action: MouseAction,
}

#[derive(Subcommand)]
pub enum MouseAction {
    /// Left click at position
    Click {
        /// X coordinate
        x: u16,
        /// Y coordinate
        y: u16,
    },

    /// Right click at position
    RightClick {
        /// X coordinate
        x: u16,
        /// Y coordinate
        y: u16,
    },

    /// Double click at position
    DoubleClick {
        /// X coordinate
        x: u16,
        /// Y coordinate
        y: u16,
    },

    /// Move cursor to position
    Move {
        /// X coordinate
        x: u16,
        /// Y coordinate
        y: u16,
    },

    /// Drag from one position to another
    Drag {
        /// Start X coordinate
        x1: u16,
        /// Start Y coordinate
        y1: u16,
        /// End X coordinate
        x2: u16,
        /// End Y coordinate
        y2: u16,
    },
}

/// Keyboard command arguments.
#[derive(Parser)]
pub struct KeyboardArgs {
    #[command(subcommand)]
    pub action: KeyboardAction,
}

#[derive(Subcommand)]
pub enum KeyboardAction {
    /// Type a text string
    Type {
        /// Text to type
        text: String,

        /// Pause in milliseconds between batches of characters. Only needed for
        /// remote apps that drop input arriving too fast
        #[arg(long)]
        delay: Option<u64>,
    },

    /// Press a key combination (e.g., "ctrl+c", "alt+tab") or single key (e.g., "enter")
    Press {
        /// Key combination or single key
        keys: String,
    },

    /// Press and hold a key without releasing it (for shift-click, hold-and-drag, ...)
    Down {
        /// Key name
        key: String,
    },

    /// Release a key previously held with `down`
    Up {
        /// Key name
        key: String,
    },

    /// Set the clipboard to `text` and paste it with Ctrl+V, as one command
    ///
    /// More reliable than `type` for long or non-Latin text: it cannot lose
    /// individual keystrokes, and setting the clipboard then pasting in one
    /// daemon-side command means focus cannot move in between.
    Paste {
        /// Text to paste
        text: String,
    },
}

/// Scroll command arguments.
#[derive(Parser)]
pub struct ScrollArgs {
    #[command(subcommand)]
    pub direction: ScrollDirection,
}

#[derive(Subcommand)]
pub enum ScrollDirection {
    /// Scroll up
    Up {
        /// Amount to scroll (clamped to 1-100 notches)
        #[arg(default_value = "3", value_parser = clap::value_parser!(u32).range(1..=100))]
        amount: u32,
        /// Position to scroll at (x y)
        #[arg(long = "at", num_args = 2, value_names = ["X", "Y"])]
        at: Option<Vec<u16>>,
    },

    /// Scroll down
    Down {
        /// Amount to scroll (clamped to 1-100 notches)
        #[arg(default_value = "3", value_parser = clap::value_parser!(u32).range(1..=100))]
        amount: u32,
        /// Position to scroll at (x y)
        #[arg(long = "at", num_args = 2, value_names = ["X", "Y"])]
        at: Option<Vec<u16>>,
    },

    /// Scroll left
    Left {
        /// Amount to scroll (clamped to 1-100 notches)
        #[arg(default_value = "3", value_parser = clap::value_parser!(u32).range(1..=100))]
        amount: u32,
        /// Position to scroll at (x y)
        #[arg(long = "at", num_args = 2, value_names = ["X", "Y"])]
        at: Option<Vec<u16>>,
    },

    /// Scroll right
    Right {
        /// Amount to scroll (clamped to 1-100 notches)
        #[arg(default_value = "3", value_parser = clap::value_parser!(u32).range(1..=100))]
        amount: u32,
        /// Position to scroll at (x y)
        #[arg(long = "at", num_args = 2, value_names = ["X", "Y"])]
        at: Option<Vec<u16>>,
    },
}

/// Clipboard command arguments.
#[derive(Parser)]
pub struct ClipboardArgs {
    #[command(subcommand)]
    pub action: ClipboardAction,
}

#[derive(Subcommand)]
pub enum ClipboardAction {
    /// Get clipboard text
    Get,

    /// Set clipboard text
    Set {
        /// Text to set
        text: String,
    },
}

/// Drive command arguments.
#[derive(Parser)]
pub struct DriveArgs {
    #[command(subcommand)]
    pub action: DriveAction,
}

#[derive(Subcommand)]
pub enum DriveAction {
    /// List mapped drives (drives are configured at connect time with --drive)
    List,
}

/// Session command arguments.
#[derive(Parser)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub action: SessionAction,
}

#[derive(Subcommand)]
pub enum SessionAction {
    /// List active sessions
    List,

    /// Get current session info
    Info,

    /// Run as background daemon for this session (starts automatically on connect)
    Daemon,
}

/// Automate command arguments.
#[derive(Parser)]
pub struct AutomateArgs {
    #[command(subcommand)]
    pub action: AutomateAction,
}

#[derive(Subcommand)]
pub enum AutomateAction {
    /// Take a snapshot of the accessibility tree
    Snapshot {
        /// Filter to interactive elements only (buttons, inputs, focusable)
        #[arg(short = 'i', long)]
        interactive: bool,

        /// Compact mode - remove empty structural elements
        #[arg(short = 'c', long)]
        compact: bool,

        /// Maximum tree depth (default: 10)
        #[arg(short = 'd', long, default_value = "10")]
        depth: u32,

        /// Scope to a specific element (window, panel, etc.) via selector
        #[arg(short = 's', long)]
        selector: Option<String>,

        /// Start from the currently focused element
        #[arg(short = 'f', long)]
        focused: bool,
    },

    /// Show the element that currently has keyboard focus
    ///
    /// Shorthand for `snapshot --focused --compact --depth 1`. Use it to confirm
    /// which field a Tab or Enter actually landed in before typing into it.
    Focused,

    /// Get element properties
    Get {
        /// Element selector
        selector: String,

        /// Property to retrieve (name, value, states, bounds, or all)
        #[arg(long)]
        property: Option<String>,
    },

    /// Set focus to an element
    Focus {
        /// Element selector
        selector: String,
    },

    /// Click an element - for buttons, links, menu items
    Click {
        /// Element selector
        selector: String,

        /// Use double-click instead of single click
        #[arg(long, short = 'd')]
        double_click: bool,
    },

    /// Select an element or item (SelectionItemPattern) - for list items, radio buttons
    Select {
        /// Element selector (item directly, or container if --item is specified)
        selector: String,

        /// Item name to select within container
        #[arg(long)]
        item: Option<String>,
    },

    /// Toggle an element (TogglePattern) - for checkboxes
    Toggle {
        /// Element selector
        selector: String,

        /// Target state: on or off (omit to just toggle)
        #[arg(long)]
        state: Option<String>,
    },

    /// Expand an element (ExpandCollapsePattern) - for menus, tree items, combo boxes
    Expand {
        /// Element selector
        selector: String,
    },

    /// Collapse an element (ExpandCollapsePattern)
    Collapse {
        /// Element selector
        selector: String,
    },

    /// Open context menu for an element (Focus + Shift+F10)
    ContextMenu {
        /// Element selector
        selector: String,
    },

    /// Clear and fill text in an element
    Fill {
        /// Element selector
        selector: String,

        /// Text to fill
        text: String,
    },

    /// Clear text from an element
    Clear {
        /// Element selector
        selector: String,
    },

    /// Scroll an element
    Scroll {
        /// Element selector
        selector: String,

        /// Scroll direction (up, down, left, right)
        #[arg(long)]
        direction: Option<String>,

        /// Scroll amount
        #[arg(long)]
        amount: Option<i32>,

        /// Child selector to scroll into view
        #[arg(long)]
        to_child: Option<String>,
    },

    /// Window operations
    Window {
        /// Action: list, focus, maximize, minimize, restore, close
        action: String,

        /// Window selector (optional)
        selector: Option<String>,
    },

    /// Run a PowerShell command
    Run {
        /// Command to run
        command: String,

        /// Command arguments
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,

        /// Wait for command to complete
        #[arg(long)]
        wait: bool,

        /// Run with hidden window
        #[arg(long)]
        hidden: bool,

        /// Process timeout in milliseconds when waiting (default: 10000)
        #[arg(long = "process-timeout")]
        process_timeout: Option<u64>,

        /// Shell executable to run the command through (default: powershell.exe)
        #[arg(long)]
        shell: Option<String>,

        /// Redirect output and keep the process alive for incremental
        /// retrieval via `automate run-poll <pid>`, instead of waiting for
        /// exit or discarding output. Ignored if --wait is also set.
        #[arg(long)]
        stream: bool,
    },

    /// Poll a process started with `run --stream` for output produced since the last poll
    RunPoll {
        /// Process ID returned by the initial `run --stream` call
        pid: u32,
    },

    /// Wait for an element to reach a state
    WaitFor {
        /// Element selector
        selector: String,

        /// Timeout in milliseconds
        #[arg(long)]
        timeout: Option<u64>,

        /// State to wait for (visible, enabled, gone)
        #[arg(long)]
        state: Option<String>,
    },

    /// Get automation agent status
    Status,

    /// Relaunch the UI Automation agent without a full RDP reconnect
    ///
    /// Use this when the agent died mid-session or never came up after
    /// connect, but the RDP session itself is still fine - a full
    /// disconnect+connect works too but invalidates every element ref for no
    /// reason. Requires connect to have been run with
    /// --enable-win-automation this session.
    Restart,
}

/// Locate command arguments (OCR-based text location).
///
/// The click flags form one mutually exclusive group, which `--index` requires:
/// an `--index` with nothing to click is a silent no-op otherwise, and silently
/// ignoring a targeting argument is exactly the failure mode this command
/// exists to prevent.
#[derive(Parser)]
#[command(group = ArgGroup::new("click_action")
    .args(["click", "double_click", "right_click"])
    .multiple(false)
    .conflicts_with("all"))]
pub struct LocateArgs {
    /// Text to search for on screen (searches within full lines)
    #[arg(required_unless_present = "all")]
    pub text: Option<String>,

    /// Use pattern matching (glob-style: * and ?)
    #[arg(long, short = 'p', conflicts_with = "exact")]
    pub pattern: bool,

    /// Require the whole line to equal the search text, not just contain it
    ///
    /// Default substring mode matches "Провести" against a line reading
    /// "Провести и закрыть" - usually harmless (--click already refuses to
    /// guess when a search is ambiguous), but --exact avoids the ambiguity
    /// in the first place.
    #[arg(long, short = 'e')]
    pub exact: bool,

    /// Case-sensitive matching (default is case-insensitive)
    #[arg(long, short = 'c')]
    pub case_sensitive: bool,

    /// Return all text lines on screen (ignores search text)
    #[arg(long, short = 'a')]
    pub all: bool,

    /// Search only part of the screen (X,Y,WIDTH,HEIGHT in screen pixels).
    /// Results are still reported in full-screen coordinates.
    #[arg(long, value_name = "X,Y,W,H", value_parser = crate::cli::commands::parse_region)]
    pub region: Option<agent_rdp_protocol::Region>,

    /// Keep retrying until the text appears, up to this many milliseconds
    ///
    /// Blocks server-side instead of polling `locate` in a loop from the
    /// outside. Has no target text to wait for with `--all`, so the two
    /// conflict. Composes with `--click`: wait, then click what appeared.
    #[arg(long, value_name = "MS", conflicts_with = "all")]
    pub wait: Option<u64>,

    /// Click the match instead of just printing its position
    #[arg(long)]
    pub click: bool,

    /// Double-click the match
    #[arg(long)]
    pub double_click: bool,

    /// Right-click the match
    #[arg(long)]
    pub right_click: bool,

    /// Which match to click when several are found (0-based).
    /// Without it, clicking requires an unambiguous single match.
    #[arg(long, value_name = "N", requires = "click_action")]
    pub index: Option<usize>,

    /// Only consider matches within --near-distance px of a line containing
    /// this anchor text (substring match). Useful when the same text appears
    /// in several places (a repeated column header, a label and its tooltip)
    /// - anchor to a nearby, more distinctive label instead.
    #[arg(long, value_name = "TEXT")]
    pub near: Option<String>,

    /// Max distance in pixels from the --near anchor (default: 150)
    #[arg(long, value_name = "PX", requires = "near", default_value = "150")]
    pub near_distance: u32,
}

/// File transfer arguments.
#[derive(Parser)]
pub struct FileArgs {
    #[command(subcommand)]
    pub action: FileAction,
}

#[derive(Subcommand)]
pub enum FileAction {
    /// Copy a local file to the remote machine
    ///
    /// Transfers in verified chunks over the automation channel. Requires
    /// connect with --enable-win-automation. Use this rather than pasting
    /// content through the clipboard or reaching for \\TSCLIENT paths, both
    /// of which hang on payloads of any size.
    Push {
        /// Local file to copy
        local: String,
        /// Destination path on the remote machine
        remote: String,
    },

    /// Copy a file from the remote machine
    Pull {
        /// Remote file to copy
        remote: String,
        /// Local destination path
        local: String,
    },
}

/// Click-at command arguments (geometric click-safety check).
#[derive(Parser)]
pub struct ClickAtArgs {
    /// X coordinate to click
    pub x: u16,

    /// Y coordinate to click
    pub y: u16,

    /// OCR detection window around the point, WIDTHxHEIGHT (default 400x160)
    #[arg(long, value_name = "WxH", value_parser = crate::cli::commands::parse_window)]
    pub window: Option<(u32, u32)>,

    /// Refuse the click if another detected text region is within this many
    /// pixels of the target (default: 10)
    #[arg(long, value_name = "PX", value_parser = clap::value_parser!(u32).range(0..=200))]
    pub min_gap: Option<u32>,

    /// Double-click instead of single click
    #[arg(long, conflicts_with = "right_click")]
    pub double_click: bool,

    /// Right-click instead of left click
    #[arg(long)]
    pub right_click: bool,

    /// A second, independently measured point for the same target (X,Y) -
    /// e.g. a vision model queried twice. If the two points roughly agree
    /// (within --max-divergence), their midpoint is clicked instead of the
    /// first point alone; if they diverge, the click is refused rather than
    /// picking one arbitrarily.
    #[arg(long, value_name = "X,Y", value_parser = crate::cli::commands::parse_point)]
    pub confirm: Option<(u16, u16)>,

    /// Max pixel distance between the point and --confirm before it's
    /// treated as diverging measurements rather than noise (default: 40)
    #[arg(long, value_name = "PX", requires = "confirm")]
    pub max_divergence: Option<u32>,
}
