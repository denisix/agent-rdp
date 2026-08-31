# agent-rdp

[![npm](https://img.shields.io/npm/v/@denisixnpm/agent-rdp.svg)](https://www.npmjs.com/package/@denisixnpm/agent-rdp)
[![CI](https://github.com/denisix/agent-rdp/actions/workflows/ci.yml/badge.svg)](https://github.com/denisix/agent-rdp/actions/workflows/ci.yml)
[![Release](https://github.com/denisix/agent-rdp/actions/workflows/release-please.yml/badge.svg)](https://github.com/denisix/agent-rdp/actions/workflows/release-please.yml)
[![license](https://img.shields.io/npm/l/@denisixnpm/agent-rdp.svg)](#license)
[![platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-informational)](#installation)

A CLI tool for AI agents to control Windows Remote Desktop sessions, built on [IronRDP](https://github.com/Devolutions/IronRDP).

## Demo

Claude Code automating SQLite database and table creation via RDP:

https://github.com/user-attachments/assets/91892b39-4edb-412b-b265-55ccd75d7421

## Features

- **Connect to RDP servers** - Full RDP protocol support with TLS and CredSSP authentication
- **Take screenshots** - Capture the remote desktop as PNG or JPEG
- **Mouse control** - Click, double-click, right-click, drag, scroll
- **Keyboard input** - Type text, press key combinations (Ctrl+C, Alt+Tab, etc.)
- **Clipboard sync** - Copy/paste text between local machine and remote Windows
- **Drive mapping** - Map local directories as network drives on the remote machine
- **UI Automation** - Interact with Windows applications via accessibility API (click, select, toggle, expand)
- **OCR text location** - Find text on screen using OCR when UI Automation isn't available
- **JSON output** - Structured output for AI agent consumption
- **Session management** - Multiple named sessions with automatic daemon lifecycle

## Installation

### From npm

```bash
npm install -g @denisixnpm/agent-rdp
```

### As a Claude Code skill

```bash
npx add-skill https://github.com/denisix/agent-rdp
```

### From a GitHub release

Each release attaches a standalone binary per platform, plus the OCR models as
`agent-rdp-models.tar.gz` / `agent-rdp-models.zip`. The models are
architecture-independent, so they ship once rather than inside every archive.

The binary alone covers everything except `locate`; that command needs the
models, so extract them too and point `AGENT_RDP_MODELS_DIR` at them.

**macOS / Linux**

```bash
tar -xzf agent-rdp-linux-x64.tar.gz -C ~/.local/bin      # or agent-rdp-darwin-arm64.tar.gz
chmod +x ~/.local/bin/agent-rdp
mkdir -p ~/.local/share/agent-rdp/models
tar -xzf agent-rdp-models.tar.gz -C ~/.local/share/agent-rdp/models
export AGENT_RDP_MODELS_DIR="$HOME/.local/share/agent-rdp/models"   # add to your shell rc to persist
```

On macOS the release binaries are unsigned, so Gatekeeper quarantines anything
downloaded from a browser. If you get *"cannot be opened because the developer
cannot be verified"*, clear the flag:

```bash
xattr -d com.apple.quarantine ~/.local/bin/agent-rdp
```

**Windows (PowerShell)**

```powershell
Expand-Archive agent-rdp-win32-x64.zip -DestinationPath "$env:LOCALAPPDATA\agent-rdp"
Expand-Archive agent-rdp-models.zip -DestinationPath "$env:LOCALAPPDATA\agent-rdp\models"
# persist for future sessions (restart the terminal afterwards)
setx AGENT_RDP_MODELS_DIR "$env:LOCALAPPDATA\agent-rdp\models"
```

Installing from npm needs none of this — the models ship inside
`@denisixnpm/agent-rdp` and are located automatically.

### From source

```bash
git clone https://github.com/denisix/agent-rdp
cd agent-rdp
bun install
bun run build      # Build native binary
bun run build:ts   # Build TypeScript
```

## Using with AI Coding Agents

### Claude Code

One command — installs the [SKILL.md](skills/agent-rdp/SKILL.md) workflow so Claude knows the commands, flags, and gotchas without you explaining them:

```bash
npx add-skill https://github.com/denisix/agent-rdp
```

You don't need to install the CLI separately: the skill installs
`@denisixnpm/agent-rdp` on first use if it isn't already on PATH.

Or install manually:

```bash
mkdir -p .claude/skills/agent-rdp
curl -o .claude/skills/agent-rdp/SKILL.md \
  https://raw.githubusercontent.com/denisix/agent-rdp/main/skills/agent-rdp/SKILL.md
```

Then just ask Claude Code, in plain language:

```
Connect to 192.168.1.100 as Administrator (password: secret), open Notepad,
type "hello from Claude", and take a screenshot.
```

Claude will run the underlying `agent-rdp connect`, `automate run`, `keyboard type`, and `screenshot` commands on its own.

### Codex

Codex doesn't have a skill-install mechanism, but it reads `AGENTS.md` for project instructions. Point it at this tool by adding a section to your `AGENTS.md`:

```bash
cat >> AGENTS.md <<'EOF'

## Remote Windows control

Use the `agent-rdp` CLI (npm i -g @denisixnpm/agent-rdp) to control Windows machines via RDP:
connect, screenshot, mouse/keyboard input, and UI Automation. See
https://github.com/denisix/agent-rdp for the full command reference.
EOF
```

Then prompt Codex the same way:

```
codex "Connect to the Windows VM at 192.168.1.100 (user Administrator, password
secret) using agent-rdp, open the Run dialog, launch calc.exe, and confirm it's
open with a screenshot."
```

Codex will call `agent-rdp` as a regular shell command, same as any other CLI tool.

## Usage

### Connect to an RDP Server

```bash
# Using command line (password visible in process list - not recommended)
agent-rdp connect --host 192.168.1.100 --username Administrator --password 'secret'

# Using environment variables (recommended)
export AGENT_RDP_USERNAME=Administrator
export AGENT_RDP_PASSWORD=secret
agent-rdp connect --host 192.168.1.100

# Using stdin (most secure)
echo 'secret' | agent-rdp connect --host 192.168.1.100 --username Administrator --password-stdin
```

### Take a Screenshot

```bash
# Save to file (default: ./screenshot.png)
agent-rdp screenshot --output desktop.png

# JSON metadata (path/width/height — image is always written to disk)
agent-rdp --json screenshot --output desktop.png

# Capture just part of the screen (X,Y,WIDTH,HEIGHT in screen pixels).
# The result reports the offset, so a coordinate read off the crop can be
# turned back into a clickable one.
agent-rdp screenshot --region 100,380,600,30 --output row.png
```

> Note: the CLI no longer has `screenshot --base64`. For agent pipelines, write a file and encode it yourself, or use the Node.js API's `rdp.screenshot({ path })`, which writes to disk and returns `{ path, width, height }` without materializing base64 — prefer this over the default `rdp.screenshot()` (which returns `{ base64, width, height }`) when the caller doesn't need the raw bytes, since echoing a base64 image into an LLM context is expensive.

Every screenshot also reports `frame_age_ms` (`--json`) / a printed warning
(human mode, when large): milliseconds since the RDP server last sent any
data. An idle-but-alive desktop can legitimately sit at a large age — RDP
servers send nothing when nothing changes — but combined with `agent-rdp
session info`'s `last_frame_age_ms`, a value that never resets on an
otherwise-idle session is the signal to check the connection rather than
trust the frame. The daemon also enables TCP keepalive on the RDP socket, so
a genuinely dead connection (cable pull, black-holed network path) is now
detected and disconnected within seconds instead of the OS's own multi-minute
retransmission timeout — previously `screenshot` could keep returning a
stale, byte-identical frame under a fresh filename for the entire time the
transport was silently dead.

### Getting Coordinates Right

**Never estimate a coordinate by looking at a screenshot.** Screenshot pixels,
OCR boxes and click coordinates are all the same space, so a coordinate that
comes out of the tool is exact — but one guessed from an image is not. Images
get downscaled on their way into a vision model, and a click that is 30px off
lands on the wrong row, the wrong cell, or a search box.

Ask for the target by name and let the tool find it:

```bash
# Click text, without a coordinate ever passing through your hands
agent-rdp locate "Добавить" --click

# Several matches? Choosing is explicit - it will not guess for you
agent-rdp locate "Добавить" --click --index 1

# Click a button by accessibility selector (preferred when UIA can see it)
agent-rdp automate click "#SaveButton"
```

To verify a value in place, read the region rather than cropping an image
yourself — coordinates still come back in full-screen space:

```bash
# Check one table row
agent-rdp locate --all --region 100,380,600,30
agent-rdp screenshot --region 100,380,600,30 -o row.png

# Confirm which field the keyboard is actually in before typing
agent-rdp automate focused
```

`agent-rdp mouse click X Y` remains available for coordinates you got from
`locate`, `automate get`, or a deliberate calculation.

### Mouse Operations

```bash
# Click at position
agent-rdp mouse click 500 300

# Right-click
agent-rdp mouse right-click 500 300

# Double-click
agent-rdp mouse double-click 500 300

# Move cursor
agent-rdp mouse move 100 200

# Drag from (100,100) to (500,500)
agent-rdp mouse drag 100 100 500 500
```

### Keyboard Operations

```bash
# Type text (supports Unicode)
agent-rdp keyboard type "Hello, World!"

# Paste instead of type for long or non-Latin text: clipboard + Ctrl+V as one
# command. More reliable than `type` - it cannot lose individual keystrokes,
# and there's no gap between setting the clipboard and pasting for focus to
# move in.
agent-rdp keyboard paste "Привет, мир! A very long string types can drop pieces of."

# Press key combinations
agent-rdp keyboard press "ctrl+c"
agent-rdp keyboard press "alt+tab"
agent-rdp keyboard press "ctrl+shift+esc"

# Press single keys (use press command)
agent-rdp keyboard press enter
agent-rdp keyboard press escape
agent-rdp keyboard press f5

# Hold a key across other commands (shift-click, hold-and-drag, ...)
agent-rdp keyboard down shift
agent-rdp mouse click 200 300
agent-rdp keyboard up shift
```

### Scroll

Position defaults to the **screen center**, not whatever pane you're working
in — pass `--at` to target a specific window or control.

Amount is a positional argument, not `--amount`:

```bash
agent-rdp scroll up 3
agent-rdp scroll down 5
agent-rdp scroll left
agent-rdp scroll right
agent-rdp scroll down 5 --at 600 400
```

### Locate (OCR)

Find text on screen using OCR (powered by [ocrs](https://github.com/robertknight/ocrs)). Useful when UI Automation can't access certain elements (WebView content, some dialogs).

```bash
# Find lines containing text
agent-rdp locate "Cancel"

# Pattern matching (glob-style)
agent-rdp locate "Save*" --pattern

# Exact whole-line matching. Default mode is substring containment, so
# "Провести" also matches a button reading "Провести и закрыть" - two matches,
# and --click refuses to guess between them. --exact matches only the line
# that reads exactly "Провести".
agent-rdp locate "Провести" --exact --click

# Get all text on screen
agent-rdp locate --all

# JSON output
agent-rdp locate "OK" --json

# Click the match directly (see "Getting Coordinates Right" above)
agent-rdp locate "Cancel" --click
agent-rdp locate "Cancel" --double-click
agent-rdp locate "Cancel" --right-click

# Search only part of the screen. Results stay in full-screen coordinates,
# so --region composes with --click. A tight region also reads small text
# more reliably than a full-screen pass.
agent-rdp locate --all --region 100,380,600,30

# Block until text appears (e.g. a dialog finishing its animation), instead
# of polling `locate` in a loop from the outside. Composes with --click.
agent-rdp locate "OK" --wait 10000 --click

# Constrain matches to those near a distinctive anchor label - useful when
# the same text repeats on screen (a column header in every row, a label
# that also appears in a tooltip). Anchor is matched by substring; if it
# isn't found at all, this returns zero matches rather than an error.
agent-rdp locate "Отменить" --near "Заказ №001" --click
```

Returns text lines with coordinates for clicking:
```
Found 1 line(s) containing 'Cancel':
  'Cancel Button' at (650, 420) size 80x14 - center: (690, 427)

To click it: agent-rdp locate 'Cancel' --click
```

Clicking is deliberately strict: no match, or several matches without
`--index`, is an error rather than a guess. Prefer narrowing with `--region`
over picking an index. When two buttons share a prefix ("Провести" /
"Провести и закрыть"), use `--exact` so the ambiguity never arises.

### Click-at (safe clicking of externally-computed coordinates)

When the click point comes from outside agent-rdp — a vision model reading a
screenshot, a manual crop — `locate --click` can't help, and a raw
`mouse click X Y` has no safety net. `click-at` clicks the point only if it
isn't ambiguously close to more than one detected text region:

```bash
# Click, refusing if another label is within 10px of the target
agent-rdp click-at 665 209

# Tune the detection window and gap
agent-rdp click-at 665 209 --window 400x160 --min-gap 20

# Variants
agent-rdp click-at 665 209 --double-click
agent-rdp click-at 665 209 --right-click

# Cross-check: a second independent measurement for the same target (e.g. a
# vision model queried twice). Clicks the midpoint if the two agree within
# --max-divergence (default 40px); refuses if they don't. Formalizes the
# "click the intersection of two measurements" technique that catches a
# single call's misreads.
agent-rdp click-at 665 209 --confirm 670,212
agent-rdp click-at 665 209 --confirm 670,212 --max-divergence 20
```

The check uses OCR *detection* only (bounding boxes, script-agnostic), not
recognition — so it works even for text OCR can't read, e.g. custom-rendered
Cyrillic UIs where both UI Automation and `locate` text search fail. On
refusal it lists the nearby regions and exits non-zero; nothing is clicked.

### Clipboard

```bash
# Set clipboard text (available when you paste on Windows)
agent-rdp clipboard set "Hello from CLI"

# Get clipboard text (after copying on Windows)
agent-rdp clipboard get

# With JSON output
agent-rdp --json clipboard get
```

### Drive Mapping

Map local directories as network drives on the remote Windows machine. Drives must be mapped at connect time. Multiple drives can be specified.

```bash
# Map local directories during connection
agent-rdp connect --host 192.168.1.100 -u Administrator -p secret \
  --drive /home/user/documents:Documents \
  --drive /tmp/shared:Shared

# List mapped drives
agent-rdp drive list
```

On the remote Windows machine, mapped drives appear in File Explorer as network locations.

### UI Automation

Interact with Windows applications programmatically via the Windows UI Automation API using native patterns (InvokePattern, SelectionItemPattern, TogglePattern, etc.). When enabled, a PowerShell agent is injected into the remote session that captures the accessibility tree and performs actions. Communication between the CLI and the agent uses a Dynamic Virtual Channel (DVC) for fast bidirectional IPC.

For detailed documentation, see [AUTOMATION.md](https://github.com/denisix/agent-rdp/blob/main/docs/AUTOMATION.md).

```bash
# Connect with automation enabled
agent-rdp connect --host 192.168.1.100 -u Admin -p secret --enable-win-automation

# Take an accessibility tree snapshot (refs are always included)
agent-rdp automate snapshot

# Snapshot filtering options (like agent-browser)
agent-rdp automate snapshot -i              # Interactive elements only
agent-rdp automate snapshot -c              # Compact (remove empty structural elements)
agent-rdp automate snapshot -d 3            # Limit depth to 3 levels
agent-rdp automate snapshot -s "~*Notepad*" # Scope to a window/element
agent-rdp automate snapshot -i -c -d 5      # Combine options

# Which control has keyboard focus right now?
# Use this after Tab/Enter to confirm where your keystrokes will go, instead
# of typing into a cell you assume is focused.
agent-rdp automate focused
# edit 'Количество' = "5,000" at (110, 391) 80x18 [focusable]

# Pattern-based element operations (refs use @eN format)
agent-rdp automate click "#SaveButton"     # Click button
agent-rdp automate click "@e5"             # Click by ref number from snapshot
agent-rdp automate click "@e5" -d          # Double-click (for file list items)
agent-rdp automate select "@e10"           # Select item (SelectionItemPattern)
agent-rdp automate toggle "@e7"            # Toggle checkbox (TogglePattern)
agent-rdp automate expand "@e3"            # Expand menu (ExpandCollapsePattern)
agent-rdp automate context-menu "@e5"      # Open context menu (Shift+F10)

# Fill text fields
agent-rdp automate fill ".Edit" "Hello World"

# Window operations
agent-rdp automate window list
agent-rdp automate window focus "~*Notepad*"

# Run PowerShell commands
agent-rdp automate run "Get-Process" --wait
agent-rdp automate run "Get-Process" --wait --process-timeout 5000  # With 5s timeout
agent-rdp automate run "$PSVersionTable" --wait --shell pwsh.exe    # Run through PowerShell 7 instead of Windows PowerShell

# Stream output from a long-running command instead of waiting for it to exit
agent-rdp automate run "ping -t 127.0.0.1" --stream   # Returns immediately with a pid
agent-rdp automate run-poll <pid>                      # Repeat to drain output incrementally; reports exit once the process ends

# Diagnostics: agent uptime, last DVC round-trip time, consecutive failures
agent-rdp automate status

# Relaunch the agent without a full RDP reconnect - use when the agent died
# mid-session or never came up after connect, but the RDP session is fine.
# Requires --enable-win-automation to have been passed at connect time.
agent-rdp automate restart
```

An `automation indeterminate` error means the DVC reply was lost, not that
the action necessarily failed or succeeded - check state before retrying, or
a retried click/fill can apply twice. For read-only commands (`snapshot`,
`get`, `status`, `wait-for`, `window list`) the error text now says so
explicitly ("This command is read-only - retrying is safe"), since those can
always be retried safely.

**Selector Types:**
- `@e5` or `@5` - Reference number from snapshot (e prefix recommended)
- `#SaveButton` - Automation ID
- `.Edit` - Win32 class name
- `~*pattern*` - Wildcard name match
- `File` - Element name (exact match)

**Snapshot Output Format:**
```
- Window "Notepad" [ref=e1, id=Notepad]
  - MenuBar "Application" [ref=e2]
    - MenuItem "File" [ref=e3]
  - Edit "Text Editor" [ref=e5, value="Hello"]
```

### Session Management

```bash
# List active sessions
agent-rdp session list

# Get current session info
agent-rdp session info

# Close a session
agent-rdp session close

# Use a named session
agent-rdp --session work connect --host work-pc.local ...
agent-rdp --session work screenshot
```

### Disconnect

```bash
agent-rdp disconnect
```

### Web Viewer

Open the web-based viewer to see the remote desktop in your browser:

```bash
# Open viewer (connects to default streaming port 9224)
agent-rdp view

# Specify a different port
agent-rdp view --port 9224
```

The viewer requires WebSocket streaming to be enabled. Start a session with streaming:

```bash
agent-rdp --stream-port 9224 connect --host 192.168.1.100 -u Admin -p secret
agent-rdp view
```

## JSON Output

All commands support `--json` for structured output:

```bash
agent-rdp --json screenshot --output desktop.png
```

**Success response:**
```json
{
  "success": true,
  "data": {
    "type": "screenshot",
    "path": "desktop.png",
    "width": 1920,
    "height": 1080
  }
}
```

**Error response:**
```json
{
  "success": false,
  "error": {
    "code": "not_connected",
    "message": "Not connected to an RDP server"
  }
}
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `AGENT_RDP_HOST` | RDP server hostname or IP |
| `AGENT_RDP_PORT` | RDP server port (default: 3389) |
| `AGENT_RDP_USERNAME` | RDP username |
| `AGENT_RDP_PASSWORD` | RDP password |
| `AGENT_RDP_SESSION` | Session name (default: "default") |
| `AGENT_RDP_STREAM_PORT` | WebSocket streaming port (0 = disabled) |
| `AGENT_RDP_MODELS_DIR` | Override the OCR models directory (set automatically by the npm wrapper; useful for standalone binary installs) |

## Node.js API

Use agent-rdp programmatically from Node.js/TypeScript:

```typescript
import { RdpSession } from 'agent-rdp';

const rdp = new RdpSession({ session: 'default' });

await rdp.connect({
  host: '192.168.1.100',
  username: 'Administrator',
  password: 'secret',
  width: 1280,
  height: 800,
  drives: [{ path: '/tmp/share', name: 'Share' }],
  enableWinAutomation: true,  // Enable UI Automation
});

// Screenshot - prefer `path` so a large base64 string never has to be
// held in memory or echoed into an agent's context
const { path, width, height } = await rdp.screenshot({ format: 'png', path: 'screenshot.png' });

// Or get raw base64 directly (e.g. for further in-process processing)
const { base64 } = await rdp.screenshot({ format: 'png' });

// Mouse
await rdp.mouse.click({ x: 100, y: 200 });
await rdp.mouse.rightClick({ x: 100, y: 200 });
await rdp.mouse.doubleClick({ x: 100, y: 200 });
await rdp.mouse.move({ x: 150, y: 250 });
await rdp.mouse.drag({ from: { x: 100, y: 100 }, to: { x: 500, y: 500 } });

// Keyboard
await rdp.keyboard.type({ text: 'Hello World' });
await rdp.keyboard.paste('Привет, мир!');     // Reliable for long/non-Latin text
await rdp.keyboard.press({ keys: 'ctrl+c' });
await rdp.keyboard.press({ keys: 'enter' });  // Single keys use press()
await rdp.keyboard.down('shift');             // Hold across other commands
await rdp.keyboard.up('shift');

// Scroll
await rdp.scroll.up();                    // Default amount: 3
await rdp.scroll.down({ amount: 5 });     // Custom amount
await rdp.scroll.up({ x: 500, y: 300 });  // Scroll at position

// Clipboard
await rdp.clipboard.set({ text: 'text to copy' });
const text = await rdp.clipboard.get();

// Locate text using OCR
const matches = await rdp.locate({ text: 'Cancel' });
if (matches.length > 0) {
  await rdp.mouse.click({ x: matches[0].center_x, y: matches[0].center_y });
}

// Get all text on screen
const allText = await rdp.locate({ all: true });

// Click a match directly - the coordinate never leaves the process
await rdp.locate({ text: 'Cancel', click: 'left' });

// Exact whole-line match - won't also hit "Провести и закрыть"
await rdp.locate({ text: 'Провести', exact: true, click: 'left' });

// Block until text appears, instead of polling in a loop
await rdp.locate({ text: 'OK', waitMs: 10000, click: 'left' });

// Safe click of an externally-computed point (e.g. a vision-model bbox):
// refuses if it's ambiguously close to more than one detected label
const result = await rdp.clickAt(665, 209);
if (!result.clicked) console.log('Ambiguous:', result.nearby);

// Automation (requires --enable-win-automation at connect)
const snapshot = await rdp.automation.snapshot({ interactive: true });
const focused = await rdp.automation.focused();  // What has keyboard focus right now
await rdp.automation.click('@e5');           // Click button by ref
await rdp.automation.click('@e5', { doubleClick: true }); // Double-click
await rdp.automation.select('@e10');         // Select item
await rdp.automation.toggle('@e7');          // Toggle checkbox
await rdp.automation.expand('@e3');          // Expand menu
await rdp.automation.contextMenu('@e5');     // Open context menu
await rdp.automation.fill('#input', 'text'); // Fill text field
await rdp.automation.run('notepad.exe');     // Run command
await rdp.automation.waitFor('#SaveButton', { timeout: 5000 });

// Window management
const windows = await rdp.automation.listWindows();
await rdp.automation.focusWindow('~*Notepad*');
await rdp.automation.maximizeWindow();

// Drives
const drives = await rdp.drives.list();

// Session info
const info = await rdp.getInfo();

// Disconnect
await rdp.disconnect();
```

### WebSocket Streaming

Enable WebSocket streaming for real-time screen capture and bidirectional clipboard support:

```typescript
const rdp = new RdpSession({
  session: 'viewer',
  streamPort: 9224,  // Enable streaming
});

await rdp.connect({...});

// Connect your WebSocket client to receive JPEG frames
const streamUrl = rdp.getStreamUrl(); // "ws://localhost:9224"
```

For the complete WebSocket protocol specification (message types, clipboard flow, input handling), see [WEBSOCKET.md](https://github.com/denisix/agent-rdp/blob/main/docs/WEBSOCKET.md).

## Architecture

agent-rdp uses a daemon-per-session architecture:

1. **CLI** (`agent-rdp`) - Parses commands and communicates with the daemon
2. **Daemon** - Maintains the RDP connection and processes commands
3. **IPC** - Unix sockets (macOS/Linux) or TCP (Windows)

The daemon is automatically started on the first command and persists until explicitly closed or the session times out.

## Limitations

### UI Automation

- **WebViews**: UI Automation cannot interact with WebView content (e.g., Windows Start menu search, Edge browser content, Electron apps). Use `Win+R` or `automate run` to launch programs directly instead of clicking through menus.
- **UAC Dialogs**: User Account Control elevation prompts run on a secure desktop and are not accessible via UI Automation. There is no good workaround - the remote user must interact with UAC manually, or UAC must be disabled (not recommended for security reasons).

### OCR Fallback

When UI Automation cannot access certain elements, the `locate` command provides OCR-based text detection:

```bash
agent-rdp locate "Button Text"    # Find text and get coordinates
agent-rdp mouse click <x> <y>     # Click at returned coordinates
```

This is not highly reliable (OCR can misread characters, miss text, or return imprecise coordinates), but may work for simple cases like dialog buttons.

### Screenshot Coordinate Detection

**Claude models** (in non-computer-use mode, such as Claude Code) are poor at estimating pixel coordinates from screenshots. Do not ask Claude to look at a screenshot and guess where to click - it will likely be inaccurate.

**Gemini models** are generally good at pixel coordinate estimation from images.

If you need vision-based coordinate detection with Claude, implement your own harness using Claude's [Computer Use Tool](https://docs.anthropic.com/en/docs/agents-and-tools/computer-use) which is specifically designed for this purpose.

## Requirements

- Rust 1.75 or later
- Target RDP server must offer **TLS** for RDP. agent-rdp uses `rustls`, which does not implement TLS 1.0/1.1, so the server must support TLS 1.2 or later — legacy targets (e.g. Windows Server 2008 R2) are not supported and fail with a TLS handshake error.
- NLA (`UserAuthentication=1`) is **recommended but not required** — TLS is what matters. The stock Windows defaults work as shipped.

### RDP security layer settings

All six combinations measured against Windows Server 2022, varying only these two
values under `HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp`:

| `SecurityLayer` | `UserAuthentication` | Meaning | Result |
|---|---|---|---|
| `1` (Negotiate) | `1` | **Windows default** | **works** |
| `2` (TLS) | `1` | TLS + NLA (most explicit) | **works** |
| `2` (TLS) | `0` | TLS, no NLA | **works** |
| `0` (RDP) | `1` | NLA forces CredSSP/TLS | **works** |
| `1` (Negotiate) | `0` | server picks, and declines TLS | fails |
| `0` (RDP) | `0` | legacy RC4 only | fails |

The rule: **agent-rdp works wherever the host offers TLS.** With NLA on that is
always the case, because NLA forces CredSSP/TLS regardless of `SecurityLayer`.
Both failing rows are NLA-off hosts that refuse TLS outright.

If `connect` reports *"server only supports Standard RDP Security"*, the host is
in one of those two rows. Either enable NLA (preferred — it also gives you
pre-authentication) or force TLS:

```powershell
$k = 'HKLM:\System\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp'
Set-ItemProperty -Path $k -Name UserAuthentication -Value 1   # enable NLA, or:
Set-ItemProperty -Path $k -Name SecurityLayer      -Value 2   # force TLS
```

New connections normally pick this up immediately; restart `TermService` only if
they don't (a restart drops existing sessions).

Neither failing case is fixable client-side. With `SecurityLayer=1` and NLA off
the server returns `SSL_NOT_ALLOWED_BY_SERVER` even though the client advertises
`PROTOCOL_SSL` — verified by testing a build that requested SSL *only*, which was
rejected identically. And Standard RDP Security (`SecurityLayer=0`, NLA off) is
refused by IronRDP itself, which implements no RC4 transport layer; it uses a
well-known key derivation, so credentials sent over it are recoverable in transit.

## Credits

Originally created by [Nick Yu](https://github.com/thisnick) ([thisnick/agent-rdp](https://github.com/thisnick/agent-rdp)). This fork ([denisix/agent-rdp](https://github.com/denisix/agent-rdp), published to npm as [`@denisixnpm/agent-rdp`](https://www.npmjs.com/package/@denisixnpm/agent-rdp)) is maintained independently with additional fixes and features; see [CHANGELOG.md](packages/agent-rdp/CHANGELOG.md) for what's changed.

## License

MIT OR Apache-2.0 (same as IronRDP)
