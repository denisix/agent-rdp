---
name: agent-rdp
description: Control Windows Remote Desktop sessions for automation, testing, and remote administration. Use when the user needs to connect to Windows machines via RDP, take screenshots, click, type, or interact with remote Windows desktops.
allowed-tools: Bash(agent-rdp:*), Bash(npm install -g @denisixnpm/agent-rdp)
---

# agent-rdp

Tested against agent-rdp 0.7.5.

If `agent-rdp` is not on PATH, install it first — this is expected on a fresh
machine and needs no confirmation:

```bash
npm install -g @denisixnpm/agent-rdp
```

## Quick start

```bash
agent-rdp connect --host <ip> -u <user> -p <pass> --enable-win-automation
agent-rdp automate snapshot -i              # accessibility tree, interactive elements only
agent-rdp automate click "@e5"              # click by ref
agent-rdp automate fill "@e7" "Hello"       # type into field
agent-rdp disconnect
```

Prefer `automate fill`/`automate click` over raw `keyboard type`/`mouse click` when `--enable-win-automation` is set: lossless and ref-based, no coordinate guessing.

After connecting, wait a moment before the first input — immediate input can be
dropped while the desktop stabilizes: `agent-rdp wait 3000`.

`connect --enable-win-automation` already waits for the automation agent (up to
~25s), so no sleep is needed for it. If the agent fails to start, connect now
says so explicitly — reconnect to retry rather than polling `automate status`.

## Reliability rules (learned from real failures)

**A DVC timeout does NOT mean the action failed.** `automation indeterminate`
means the request reached the agent but the reply was lost — it may well have
been applied. Never blindly retry: check state first, or you double-apply
(text typed twice, a button clicked twice).

```bash
agent-rdp automate get "@e2"        # read back before retrying
```

**"Channel unresponsive" is usually transient.** It recovers on its own. Re-probe
with `automate status` before reconnecting; a needless reconnect re-issues all
refs and, combined with a retry, is what corrupts state.

**Verify with `automate get`, not OCR.** `get` now returns `Value:` for text
controls including multiline editors. OCR (`locate`) misreads confidently —
it read "Hello World!" as "Hel1o Worldi", so `locate "Hello"` found nothing.
Use OCR only when UI Automation cannot see the element.

**Wildcards search the whole desktop, including the taskbar.** `~*Notepad*`
matches the taskbar button before the window. Get the exact title first:

```bash
agent-rdp automate window list              # -> "Untitled - Notepad"
agent-rdp automate snapshot -s "Untitled - Notepad"
```

**Concurrency:** parallelise only read-only commands (`session info`, `status`,
`screenshot`, `snapshot`) and keep it to ~4. Never run input events
concurrently — they race and reorder in the remote UI. Always handle
`daemon_not_running`: if the daemon exited, `<session>/daemon.log` says why.

## Speed

The dominant cost of a short command is ~140ms of process startup, not the
network (a round-trip incl. a full screenshot is ~85ms). So:

- **Make fewer tool calls.** Batch independent commands into one shell
  invocation with `;` — five commands in one call costs one round-trip, not five.
- **Block server-side.** `automate wait-for <sel> --timeout 15000` returns the
  moment the condition holds; `locate "text" --wait 15000` does the same via
  OCR when UIA can't see the element. Never poll `screenshot` in a loop.
- **Reuse refs.** They stay stable while the window is unchanged; re-snapshot
  only after the UI actually changes. They are NOT stable across reconnects.
- **Use the Node API for long sequences** — it pays process startup once.
- `-i` / `-c` on snapshots cut *output size*, not time. Use them to save
  context, not to go faster.
- `automate run --wait` beats `run` + repeated `run-poll` unless the command is
  genuinely long-running.

`keyboard type` is batched and fast (dozens of characters in one round-trip).
If a remote app drops fast input, pace it with `--delay 20`. For long or
non-Latin text, prefer `keyboard paste "text"` instead: it sets the clipboard
and pastes as one daemon-side command, so it cannot lose individual
keystrokes and there is no focus-moving gap between two separate calls.

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
agent-rdp screenshot --format jpeg         # smaller; PNG is lossless if exact pixels matter
agent-rdp screenshot --region 100,380,600,30 -o row.png  # crop; reports the offset back

# Mouse
agent-rdp mouse click 500 300
agent-rdp mouse right-click 500 300
agent-rdp mouse double-click 500 300
agent-rdp mouse move 100 200
agent-rdp mouse drag 100 100 500 500

# Keyboard
agent-rdp keyboard type "Hello World"      # Unicode-safe, batched into one round-trip
agent-rdp keyboard type "text" --delay 20  # pace it only if the remote app drops fast input
agent-rdp keyboard paste "Привет, мир!"    # clipboard + Ctrl+V as one command; most reliable for long/non-Latin text
agent-rdp keyboard press "ctrl+c"          # use "press", not "key"
agent-rdp keyboard press "win+r"
agent-rdp keyboard press enter
agent-rdp keyboard down shift              # hold, do something else, then release
agent-rdp keyboard up shift

# Scroll (amount is positional, not --amount). Default point is the SCREEN
# CENTER, not whatever pane you're working in - use --at to target one.
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
agent-rdp locate "Cancel" --click          # click the match directly - no coordinate hand-off
agent-rdp locate "OK" --wait 10000 --click # block until it appears, then click it
agent-rdp locate --all --region 100,380,600,30  # verify one row; coords stay full-screen
```

Never estimate a coordinate by reading a screenshot image - always `--click`,
or read the printed coordinate straight from `locate`/`automate get` output.

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
agent-rdp automate get <selector>            # includes Value: for text/multiline edits
agent-rdp automate focused                   # what has keyboard focus right now - verify a Tab landed correctly
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

- Target must offer TLS for RDP and support TLS 1.2+. agent-rdp uses `rustls`, which does not implement TLS 1.0/1.1 — legacy targets (e.g. Windows Server 2008 R2) are not supported and fail with a TLS handshake error. **Stock Windows defaults work as shipped.** The rule is that it works wherever the host offers TLS: with NLA on (`UserAuthentication=1`) that is always true, since NLA forces CredSSP/TLS. Only two configurations fail, both NLA-off hosts that refuse TLS: `SecurityLayer=1` (Negotiate) and `SecurityLayer=0` (legacy RC4). Fix on the host by enabling NLA, or setting `SecurityLayer=2`; neither is fixable client-side.
- UI Automation cannot see WebView content (Start menu search, Edge/Electron content). Use `automate run` or Win+R to launch programs directly instead of navigating menus.
- UI Automation cannot see UAC elevation dialogs (secure desktop). Fall back to `locate` (OCR) + `mouse click`; unreliable but may work for simple Yes/No prompts.
- `locate` (OCR) can misread or miss text — use only when UI Automation can't reach an element, and verify coordinates before clicking anything destructive.
- **Claude models in non-computer-use mode (like Claude Code) are bad at estimating pixel coordinates from screenshots — do not guess coordinates by looking at an image.** Always get coordinates from `automate snapshot` refs or `locate` output. (Gemini models are generally good at this; for vision-based Claude coordinate detection, use Claude's Computer Use Tool instead.)

Recommended fallback order when an element isn't reachable via automation: `automate snapshot` (with/without `-i`) → `locate "text"` (OCR) → `mouse click` with returned coordinates. Never estimate coordinates from a screenshot.
