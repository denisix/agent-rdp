---
name: agent-rdp
description: Control Windows Remote Desktop sessions for automation, testing, and remote administration. Use when the user needs to connect to Windows machines via RDP, take screenshots, click, type, or interact with remote Windows desktops.
allowed-tools: Bash(agent-rdp:*)
---

# agent-rdp

Tested against agent-rdp 0.6.5. `agent-rdp` must be on PATH.

## Quick start

```bash
agent-rdp connect --host <ip> -u <user> -p <pass> --enable-win-automation
agent-rdp automate snapshot -i              # accessibility tree, interactive elements only
agent-rdp automate click "@e5"              # click by ref
agent-rdp automate fill "@e7" "Hello"       # type into field
agent-rdp disconnect
```

Prefer `automate fill`/`automate click` over raw `keyboard type`/`mouse click` when `--enable-win-automation` is set: lossless and ref-based, no coordinate guessing.

After connecting, wait ~5s before the first input — immediate input can be dropped while the desktop stabilizes: `agent-rdp wait 5000`.

## Core workflow

1. Connect: `agent-rdp connect --host <ip> -u <user> -p <pass> --enable-win-automation`
2. Snapshot: `agent-rdp automate snapshot -i`
3. Act: `agent-rdp automate click @e5` / `agent-rdp automate fill @e7 "text"`
4. Repeat snapshot → act → snapshot → act
5. Disconnect: `agent-rdp disconnect`

## Commands

```bash
# Connection
agent-rdp connect --host <ip> -u <user> -p <pass>
agent-rdp connect --host <ip> -u <user> --password-stdin
agent-rdp connect --host <ip> --width 1920 --height 1080
agent-rdp connect --host <ip> -u <user> -p <pass> --drive /local/path:DriveName
agent-rdp disconnect
agent-rdp session list
agent-rdp session info
agent-rdp --session work connect ...       # named session
agent-rdp --session work screenshot

# Screenshot
agent-rdp screenshot -o desktop.png        # PNG, default ./screenshot.png
agent-rdp screenshot --format jpeg         # JPEG fails if the frame has alpha; prefer PNG

# Mouse
agent-rdp mouse click 500 300
agent-rdp mouse right-click 500 300
agent-rdp mouse double-click 500 300
agent-rdp mouse move 100 200
agent-rdp mouse drag 100 100 500 500

# Keyboard
agent-rdp keyboard type "Hello World"      # Unicode-safe
agent-rdp keyboard press "ctrl+c"          # use "press", not "key"
agent-rdp keyboard press "win+r"
agent-rdp keyboard press enter

# Scroll (amount is positional, not --amount)
agent-rdp scroll up 3
agent-rdp scroll down 5 --at 600 400

# Clipboard (first use can block during remote channel init; wrap in a timeout)
agent-rdp clipboard set "text"
agent-rdp clipboard get

# Drive mapping
agent-rdp connect --host <ip> -u <user> -p <pass> --drive /local/path:Share
agent-rdp drive list                       # remote path: \\tsclient\Share

# OCR / locate
agent-rdp locate "Cancel"                  # substring match, returns coords
agent-rdp locate "Save*" --pattern         # glob match
agent-rdp locate --all                     # all text on screen
```

`locate` output: `'Cancel' at (650, 420) size 45x14 - center: (672, 427)` → click with `agent-rdp mouse click 672 427`.

## UI Automation (`--enable-win-automation`)

```bash
agent-rdp automate snapshot                # full tree (refs always included)
agent-rdp automate snapshot -i -c -d 3     # interactive only, compact, depth 3
agent-rdp automate snapshot -s "~*Notepad*"

# selectors: @e5 (ref), #AutomationId, .ClassName, ~*wildcard*, exact Name
agent-rdp automate click "@e5"
agent-rdp automate click "@e5" -d          # double-click
agent-rdp automate select "@e10" --item "Option 1"
agent-rdp automate toggle "@e7" --state on
agent-rdp automate expand "@e3"
agent-rdp automate collapse "@e3"
agent-rdp automate context-menu "@e5"
agent-rdp automate focus <selector>
agent-rdp automate get <selector>
agent-rdp automate fill <selector> "text"
agent-rdp automate clear <selector>
agent-rdp automate scroll <selector> --direction down --amount 3
agent-rdp automate window list
agent-rdp automate window focus "~*Notepad*"
agent-rdp automate window maximize|minimize|restore|close
agent-rdp automate wait-for <selector> --timeout 5000 --state visible
agent-rdp automate status
```

Run commands/apps (preferred way to open apps):

```bash
agent-rdp automate run "notepad.exe"
agent-rdp automate run "Get-Process" --wait --process-timeout 5000
agent-rdp automate run "$PSVersionTable" --wait --shell pwsh.exe   # non-default shell
agent-rdp automate run "ping -t 127.0.0.1" --stream                # returns pid immediately
agent-rdp automate run-poll <pid>                                   # drain output incrementally; repeat until exited
```

There is no `session close` subcommand — use `disconnect`.

## JSON output

Add `--json` to any command for machine-readable output, e.g. `agent-rdp --json screenshot -o desktop.png`, `agent-rdp --json automate snapshot`.

## Node API

```js
import { RdpSession } from 'agent-rdp';
const session = new RdpSession({ session: 'default' });
await session.connect({ host, username, password, width: 1280, height: 800 });
const shot = await session.screenshot({ format: 'png' });      // { base64, width, height }
const drives = await session.drives.list();
await session.disconnect();
```

The process can stay alive after `disconnect()` unless the caller does explicit cleanup — call `session.close()` or exit explicitly.

## Debugging: WebSocket streaming

```bash
agent-rdp --stream-port 9224 connect --host <ip> -u <user> -p <pass>
agent-rdp view --port 9224                 # opens web viewer
# or connect directly: ws://localhost:9224 (broadcasts base64 JPEG frames, accepts mouse/keyboard/clipboard input)
```

## Requirements and limitations

- Target must have NLA enabled and support TLS 1.2+. agent-rdp uses `rustls`, which does not implement TLS 1.0/1.1 — legacy targets (e.g. Windows Server 2008 R2) are not supported and fail with a TLS handshake error.
- UI Automation cannot see WebView content (Start menu search, Edge/Electron content). Use `automate run` or Win+R to launch programs directly instead of navigating menus.
- UI Automation cannot see UAC elevation dialogs (secure desktop). Fall back to `locate` (OCR) + `mouse click`; unreliable but may work for simple Yes/No prompts.
- `locate` (OCR) can misread or miss text — use only when UI Automation can't reach an element, and verify coordinates before clicking anything destructive.
- **Claude models in non-computer-use mode (like Claude Code) are bad at estimating pixel coordinates from screenshots — do not guess coordinates by looking at an image.** Always get coordinates from `automate snapshot` refs or `locate` output. (Gemini models are generally good at this; for vision-based Claude coordinate detection, use Claude's Computer Use Tool instead.)

Recommended fallback order when an element isn't reachable via automation: `automate snapshot` (with/without `-i`) → `locate "text"` (OCR) → `mouse click` with returned coordinates. Never estimate coordinates from a screenshot.
