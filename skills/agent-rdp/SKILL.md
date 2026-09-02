---
name: agent-rdp
description: Control Windows Remote Desktop sessions for automation, testing, and remote administration. Use when the user needs to connect to Windows machines via RDP, take screenshots, click, type, or interact with remote Windows desktops.
allowed-tools: Bash(agent-rdp:*), Bash(npm install -g @denisixnpm/agent-rdp)
---

# agent-rdp

Tested against agent-rdp 0.7.11.

If `agent-rdp` is not on PATH, install it first — expected on a fresh machine,
needs no confirmation:

```bash
npm install -g @denisixnpm/agent-rdp
```

## Quick start

```bash
agent-rdp connect --host <ip> -u <user> -p <pass> --enable-win-automation
agent-rdp automate snapshot -i              # accessibility tree, interactive only
agent-rdp automate click "@e5"              # click by ref
agent-rdp automate fill "@e7" "Hello"       # type into field
agent-rdp disconnect
```

Loop: snapshot → act → snapshot → act. Prefer `automate fill`/`automate click`
over raw `keyboard type`/`mouse click` — lossless, ref-based, no coordinate
guessing.

After connecting, `agent-rdp wait 3000` before the first input: immediate input
can be dropped while the desktop stabilizes. `connect --enable-win-automation`
already waits for the agent (~25s), and says so explicitly if it fails to
start — reconnect rather than polling `automate status`.

## Reliability rules (learned from real failures)

**A DVC timeout does NOT mean the action failed.** `automation indeterminate`
means the request reached the agent but the reply was lost. The agent journals
recent results, so the daemon asks what happened and usually returns the real
outcome, or states that the request never ran. A surviving `indeterminate`
means the agent is still busy: check state first (`automate get "@e2"`), never
blindly retry, or you double-apply — text typed twice, a button clicked twice.
Read-only commands (`snapshot`, `get`, `status`, `wait-for`, `window list`) say
so in the error text; those are always safe to retry.

**Never touch `\\TSCLIENT\...` from `automate run`.** Drive redirection is
serviced by the same task that carries the automation channel, so reading the
share from inside the agent deadlocks — the command hangs and the session stops
responding until you reconnect. Use `file push`/`file pull` instead.

**Long commands are allowed to be long, but they block the agent.** Transport,
IPC and watchdog budgets all extend to cover `--process-timeout` and `wait-for
--timeout`, so a 4-minute command is not cut off. But the agent handles one
command at a time, so a long `run --wait` stalls every other `automate` call.
Past ~1 minute prefer streaming:

```bash
agent-rdp automate run "long-build.cmd" --stream   # returns a pid immediately
agent-rdp automate run-poll <pid>                  # drain output; repeat until exited
```

Output is captured to files on the remote side, so nothing is lost if the
process exits between polls; the final poll returns the tail plus
`exited: true`, and a repeat poll within 10 minutes returns `exited: true`
again with empty chunks rather than an error. If you redirect inside the
command instead (`*> out.txt`), remember Windows PowerShell 5.1 writes UTF-16LE
— use `| Out-File -Encoding utf8` before `file pull`.

**"Channel unresponsive" is usually transient.** Re-probe with `automate
status` — it reports agent uptime, last DVC round-trip and consecutive-failure
count, so you can tell "degraded but working" from "dead". If the agent really
is gone, `automate restart` relaunches it without touching the RDP session or
invalidating refs. A full `disconnect`+`connect` is the last resort: it
re-issues every ref, and combined with a retry is what corrupts state.

**Verify with `automate get`, not OCR.** `get` returns `Value:` for text
controls including multiline editors. OCR misreads confidently — it read
"Hello World!" as "Hel1o Worldi", so `locate "Hello"` found nothing.

**A screenshot is a cached frame, not a live poll.** Each carries
`frame_age_ms` (time since the server last sent anything); `--json` adds
`frame_seq`/`frame_hash`, and two screenshots with the same value are
guaranteed pixel-identical — the reliable way to confirm a frame changed after
an action. A large age usually just means an idle desktop; a dead transport is
detected within seconds by TCP keepalive. Every command also has a watchdog, so
a stuck one exits with `watchdog_timeout` instead of hanging.

**OCR loses the leading digit group in numbers with thousands separators** —
`1 250,00` has been read as `2250,00`. For monetary values crop tight with
`--region` and verify via `automate get` or a second independent read.

**Arrow-key navigation inside a panel can land on the wrong item.** Observed in
1C side panels ("Функции"): Up/Down + Enter is not deterministic. Prefer
`automate` refs; otherwise use two independent measurements with `click-at
--confirm`.

**Enter in a 1C list can mean "Create", not "Open".** With no row explicitly
selected, Enter often opens a new-document form instead of the current row.
Select the row first (`locate --click`), and check the opened form's title for
"(создание)" before proceeding. This is 1C's behavior, not something agent-rdp
can prevent.

**Wildcards search the whole desktop, including the taskbar.** `~*Notepad*`
matches the taskbar button before the window. Get the exact title first:

```bash
agent-rdp automate window list              # -> "Untitled - Notepad"
agent-rdp automate snapshot -s "Untitled - Notepad"
```

**Concurrency:** parallelise only read-only commands (`session info`, `status`,
`screenshot`, `snapshot`), ~4 at most. Never run input events concurrently —
they race and reorder in the remote UI.

**`daemon_not_running` and `daemon_unresponsive` want opposite reactions.**
`daemon_not_running` means there is no process — reconnect, and read
`<session>/daemon.log` for why it exited. `daemon_unresponsive` means the
process is alive but did not answer a health check within 10s — it is busy
(a long `run --wait`, a file transfer). Wait a few seconds and retry the same
command; do **not** reconnect, that throws away a working session. The message
quotes the last lines of daemon.log so you can see what it is doing. Only if it
stays unresponsive for over a minute does `connect` replace it (killing the
stuck one first). Cold `connect` with automation can take up to ~2.5 minutes;
its timeout now covers that.

**OCR coordinates are top-left pixels.** `locate` returns `x/y/width/height`
with the origin at the top-left of the desktop and `center_x/center_y` as the
click point; no conversion is needed (the bottom-left-origin issue exists in
Apple Vision, which agent-rdp does not use).

## Speed

A short command costs ~140ms of process startup, dwarfing the network (~85ms
round-trip including a full screenshot). So:

- **Make fewer tool calls.** Batch independent commands into one shell
  invocation with `;` — five commands cost one round-trip, not five.
- **Block server-side.** `automate wait-for <sel> --timeout 15000` returns the
  moment the condition holds; `locate "text" --wait 15000` does the same via
  OCR. Never poll `screenshot` in a loop.
- **Reuse refs.** Stable while the window is unchanged; re-snapshot only after
  the UI actually changes. NOT stable across reconnects.
- **Use the Node API for long sequences** — it pays startup once.
- `-i`/`-c` on snapshots cut *output size*, not time. Use them to save context.
- `automate run --wait` beats `run` + repeated `run-poll` unless the command is
  genuinely long-running.

## Commands

```bash
# Connection
agent-rdp connect --host <ip> -u <user> -p <pass>
agent-rdp connect --host <ip> -u <user> --password-stdin
agent-rdp connect --host <ip> --width 1920 --height 1080
agent-rdp connect --host <ip> -u <user> -p <pass> --drive /local/path:DriveName
agent-rdp disconnect                       # there is no `session close`
agent-rdp session list
agent-rdp session info
agent-rdp --session work connect ...       # named session
agent-rdp --session work screenshot

# Screenshot
agent-rdp screenshot -o desktop.png        # PNG, default ./screenshot.png
agent-rdp screenshot --format jpeg         # smaller; PNG if exact pixels matter
agent-rdp screenshot --region 100,380,600,30 -o row.png  # crop; reports offset back

# Mouse
agent-rdp mouse click 500 300              # also right-click, double-click, move
agent-rdp mouse drag 100 100 500 500

# Keyboard
agent-rdp keyboard type "Hello World"      # Unicode-safe, batched into one round-trip
agent-rdp keyboard type "text" --delay 20  # pace only if the remote app drops input
agent-rdp keyboard paste "Привет, мир!"    # clipboard + Ctrl+V; best for long/non-Latin
agent-rdp keyboard press "ctrl+c"          # use "press", not "key"; also win+r, enter
agent-rdp keyboard down shift              # hold, do something else, then release
agent-rdp keyboard up shift

# Scroll (amount is positional, not --amount). Default point is the SCREEN
# CENTER, not whatever pane you're working in - use --at to target one.
agent-rdp scroll up 3
agent-rdp scroll down 5 --at 600 400

# Clipboard (first use can block during remote channel init; wrap in a timeout).
# Line endings become CRLF on the Windows side, so a multi-line script survives
# `Get-Clipboard | Set-Content`; `get` returns Windows text with CRLF as-is.
agent-rdp clipboard set "text"
agent-rdp clipboard set --file ./script.ps1   # no shell quoting; `-` reads stdin
agent-rdp clipboard get

# Drive mapping
agent-rdp connect --host <ip> -u <user> -p <pass> --drive /local/path:Share
agent-rdp drive list                       # remote path: \\tsclient\Share

# File transfer (needs --enable-win-automation). Chunked + SHA-256 verified on
# both ends; byte-exact, so it is also the safe way to place a script with
# non-ASCII content. Use instead of clipboard payloads or \\TSCLIENT access.
agent-rdp file push ./local.txt "C:\Users\Admin\remote.txt"
agent-rdp file pull "C:\Users\Admin\out.csv" ./out.csv

# OCR / locate
agent-rdp locate "Cancel"                  # substring match, returns coords
agent-rdp locate "Save*" --pattern         # glob match
agent-rdp locate "Провести" --exact        # whole-line - won't hit "Провести и закрыть"
agent-rdp locate --all                     # all text on screen
agent-rdp locate "Cancel" --click          # click directly - no coordinate hand-off
agent-rdp locate "OK" --wait 10000 --click # block until it appears, then click
agent-rdp locate --all --region 100,380,600,30  # verify one row; coords full-screen
agent-rdp locate "Отменить" --near "Заказ №001" --click  # anchor to a nearby label

# Safe click of an externally-computed point (vision-model bbox, manual crop):
# refuses if another detected label is within --min-gap px. Detection-only, so
# it works even where OCR can't READ the text and UIA is blind.
agent-rdp click-at 665 209
agent-rdp click-at 665 209 --min-gap 20 --double-click
agent-rdp click-at 665 209 --confirm 670,212   # two measurements -> click midpoint
```

Substring matching means a prefix matches longer labels too ("Провести" also
matches "Провести и закрыть"). `--click` refuses to guess between multiple
matches, so the worst case is an error, not a wrong click — but prefer
`--exact`, or `--near` when several genuinely distinct matches share a name.

Never estimate a coordinate by reading a screenshot image — use `--click`, or
read the printed coordinate from `locate`/`automate get`. If a coordinate must
come from a vision model (OCR and UIA both failed), click it through `click-at`
rather than raw `mouse click`: it adds the ambiguity check, and `--confirm`
adds a two-measurement cross-check.

**When OCR mangles the text itself** (Cyrillic read as "OTM?H?T? ?????????"),
don't abandon OCR — `locate --all` still returns every detected line's
*position*. Dump it, filter for digits/Latin substrings that survive intact
(order numbers, codes), and reason about the layout geometrically.

## UI Automation (`--enable-win-automation`)

```bash
agent-rdp automate snapshot                # full tree (refs always included)
agent-rdp automate snapshot -i -c -d 3     # interactive only, compact, depth 3
agent-rdp automate snapshot -s "~*Notepad*"

# selectors: @e5 (ref), #AutomationId, .ClassName, ~*wildcard*, exact Name
agent-rdp automate click "@e5"             # -d for double-click
agent-rdp automate select "@e10" --item "Option 1"
agent-rdp automate toggle "@e7" --state on
agent-rdp automate expand "@e3"            # also collapse, context-menu, focus, clear
agent-rdp automate get <selector>          # includes Value: for text/multiline edits
agent-rdp automate focused                 # verify a Tab landed where you expected
agent-rdp automate fill <selector> "text"
agent-rdp automate scroll <selector> --direction down --amount 3
agent-rdp automate window list
agent-rdp automate window focus "~*Notepad*"
agent-rdp automate window maximize|minimize|restore|close
agent-rdp automate wait-for <selector> --timeout 5000 --state visible
agent-rdp automate status                  # uptime, last RTT, consecutive failures
agent-rdp automate restart                 # relaunch agent without an RDP reconnect

# Run commands/apps (preferred way to open apps)
agent-rdp automate run "notepad.exe"
agent-rdp automate run "Get-Process" --wait --process-timeout 5000
agent-rdp automate run "$PSVersionTable" --wait --shell pwsh.exe   # non-default shell
agent-rdp automate run "ping -t 127.0.0.1" --stream                # returns pid
agent-rdp automate run-poll <pid>                                  # drain incrementally
```

Snapshots include `disabled` on interactive elements — a free pre-action state
check. A disabled "Отменить проведение" tells you the document isn't posted
before you click anything.

## JSON output

Add `--json` to any command, e.g. `agent-rdp --json automate snapshot`.

## Node API

```js
import { RdpSession } from 'agent-rdp';
const session = new RdpSession({ session: 'default' });
await session.connect({ host, username, password, width: 1280, height: 800 });
const shot = await session.screenshot({ format: 'png' });   // { base64, width, height }
await session.files.push('./local.txt', 'C:\\remote.txt');
await session.disconnect();
```

The process can stay alive after `disconnect()` unless the caller cleans up —
call `session.close()` or exit explicitly.

## Debugging: WebSocket streaming

```bash
agent-rdp --stream-port 9224 connect --host <ip> -u <user> -p <pass>
agent-rdp view --port 9224                 # opens web viewer
# or connect directly to ws://localhost:9224 (base64 JPEG frames, accepts input)
```

## Requirements and limitations

- Target must offer TLS for RDP (TLS 1.2+). `rustls` doesn't implement TLS
  1.0/1.1, so legacy targets (e.g. Windows Server 2008 R2) fail with a
  handshake error. **Stock Windows defaults work as shipped.** It works
  wherever the host offers TLS — with NLA on (`UserAuthentication=1`) that is
  always true, since NLA forces CredSSP/TLS. Only two configs fail, both
  NLA-off hosts that refuse TLS: `SecurityLayer=1` (Negotiate) and
  `SecurityLayer=0` (legacy RC4). Fix on the host by enabling NLA or setting
  `SecurityLayer=2`; not fixable client-side.
- UI Automation cannot see WebView content (Start menu search, Edge/Electron).
  Use `automate run` or Win+R to launch programs instead of navigating menus.
- UI Automation cannot see UAC dialogs (secure desktop). Fall back to `locate`
  + `mouse click`; unreliable but may work for simple Yes/No prompts.
- `locate` (OCR) can misread or miss text — use only when UI Automation can't
  reach an element, and verify before clicking anything destructive.
- **Claude models in non-computer-use mode (like Claude Code) are bad at
  estimating pixel coordinates from screenshots — never guess from an image.**
  Get coordinates from `automate snapshot` refs or `locate` output. (Gemini
  models are generally good at this; for Claude use the Computer Use Tool.)

Fallback order when an element isn't reachable: `automate snapshot` →
`locate "text"` (OCR) → `click-at` with the returned coordinates.
