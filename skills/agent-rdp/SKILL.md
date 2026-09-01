---
name: agent-rdp
description: Control Windows Remote Desktop sessions for automation, testing, and remote administration. Use when the user needs to connect to Windows machines via RDP, take screenshots, click, type, or interact with remote Windows desktops.
allowed-tools: Bash(agent-rdp:*), Bash(npm install -g @denisixnpm/agent-rdp)
---

# agent-rdp

Tested against agent-rdp 0.8.0+ (transport keepalive, `automate restart`,
`locate --near`, `click-at --confirm`, `file push/pull`).

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
been applied. The agent now journals recent results, so the daemon asks it
what happened and usually returns the real outcome, or states plainly that
the request never ran. A surviving `indeterminate` means the agent is still
busy: check state first, never blindly retry, or you double-apply (text typed
twice, a button clicked twice). Read-only commands (`snapshot`, `get`,
`status`, `wait-for`, `window list`) say so in the error text — those are
always safe to retry.

**Never touch `\\TSCLIENT\...` from `automate run`.** Drive redirection is
serviced by the same task that carries the automation channel, so reading the
share from inside the agent deadlocks — the command hangs and the session
stops responding until you reconnect. Use `file push`/`file pull` instead.

**Long commands are allowed to be long, but they block the agent.** The
transport, IPC and watchdog budgets all extend to cover `--process-timeout`
and `wait-for --timeout`, so a 4-minute command is no longer cut off. But the
agent handles one command at a time, so a long `run --wait` stalls every other
`automate` call. Past ~1 minute, prefer `run --stream` + `run-poll`:

```bash
agent-rdp automate run "long-build.cmd" --stream   # returns a pid immediately
agent-rdp automate run-poll <pid>                  # drain output; repeat until exited
```

```bash
agent-rdp automate get "@e2"        # read back before retrying
```

**"Channel unresponsive" is usually transient.** It recovers on its own. Re-probe
with `automate status` before reconnecting — it now reports agent uptime,
last DVC round-trip time, and consecutive-failure count, so you can judge
"degraded but working" from "actually dead" instead of guessing. A needless
reconnect re-issues all refs and, combined with a retry, is what corrupts
state.

**Agent died or never came up? Try `automate restart` before reconnecting.**
It relaunches the PowerShell agent without touching the RDP session or
invalidating refs - a full `disconnect`+`connect` should be the last resort,
not the first response to `automate status` failing:

```bash
agent-rdp automate restart
```

**Verify with `automate get`, not OCR.** `get` now returns `Value:` for text
controls including multiline editors. OCR (`locate`) misreads confidently —
it read "Hello World!" as "Hel1o Worldi", so `locate "Hello"` found nothing.
Use OCR only when UI Automation cannot see the element.

**A screenshot is a cached frame, not a live poll.** The daemon returns the
last frame the server painted, tagged with `frame_age_ms` (how long since the
server last sent anything). A dead RDP transport is now detected within
seconds via TCP keepalive rather than sitting silent for tens of minutes, but
a large `frame_age_ms` on its own can still just mean "the desktop is
genuinely idle" — check `agent-rdp session info`'s `last_frame_age_ms` too;
if it keeps climbing indefinitely rather than resetting on real UI changes,
distrust the frame and reconnect. Every CLI command also has a hard watchdog
now (defaults to its own timeout plus a grace window) so a stuck command
exits with `watchdog_timeout` instead of hanging indefinitely. `--json`
screenshots also carry `frame_seq`/`frame_hash` - two screenshots with the
same value are guaranteed pixel-identical, the reliable way to confirm a
frame actually changed after an action, instead of hashing the saved file
yourself.

**Numbers with thousands separators can lose their leading digit group in
OCR** - `1 250,00` has been read as `2250,00`. For monetary values, crop
tight with `--region` and verify via `automate get` or a second independent
read before acting on the amount.

**Arrow-key navigation inside a panel can land on the wrong item.** Observed
in 1C side panels ("Функции" etc.): Up/Down + Enter selection is not
reliably deterministic. Prefer `automate` refs when the panel is exposed to
UI Automation; otherwise use two independent coordinate measurements with
`click-at --confirm` rather than arrow keys.

**Enter in a 1C list can mean "Create", not "Open".** With no row explicitly
selected, pressing Enter in a 1C list view often opens a new-document form
instead of the current row. Before Enter, explicitly select the row
(`locate --click` on it, or click its coordinates), and after, check the
opened form's title/header for "(создание)"/"(create)" before proceeding -
this is 1C's own behavior, not something agent-rdp can prevent.

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

# File transfer (needs --enable-win-automation). Chunked + SHA-256 verified on
# both ends; byte-exact, so it is also the safe way to place a script with
# non-ASCII content. Use instead of clipboard payloads or \\TSCLIENT access.
agent-rdp file push ./local.txt "C:\Users\Admin\remote.txt"
agent-rdp file pull "C:\Users\Admin\out.csv" ./out.csv

# OCR / locate
agent-rdp locate "Cancel"                  # substring match, returns coords
agent-rdp locate "Save*" --pattern         # glob match
agent-rdp locate "Провести" --exact        # whole-line match - won't hit "Провести и закрыть"
agent-rdp locate --all                     # all text on screen
agent-rdp locate "Cancel" --click          # click the match directly - no coordinate hand-off
agent-rdp locate "OK" --wait 10000 --click # block until it appears, then click it
agent-rdp locate --all --region 100,380,600,30  # verify one row; coords stay full-screen

# Constrain to matches near a distinctive anchor label - the same text often
# repeats (a column header in every row, a label that also appears in a
# tooltip). Anchor not found at all -> zero matches, not an error.
agent-rdp locate "Отменить" --near "Заказ №001" --click

# Safe click of an externally-computed point (vision-model bbox, manual crop):
# refuses if another detected label is within --min-gap px of the target.
# Detection-only, so it works even where OCR can't READ the text (AR-003-style
# Cyrillic custom renderers) and UIA is blind.
agent-rdp click-at 665 209
agent-rdp click-at 665 209 --min-gap 20 --double-click

# Cross-check two independent measurements of the same target (e.g. two
# vision calls): clicks the midpoint if they agree within --max-divergence
# (default 40px), refuses otherwise. This is the "click the intersection"
# technique that catches a single call's misreads/hallucinated coordinates.
agent-rdp click-at 665 209 --confirm 670,212
```

Substring matching means a query that is a prefix of a longer button label
matches both ("Провести" matches "Провести и закрыть" too). `--click` refuses
to guess between multiple matches, so the worst case is an error, not a wrong
click - but prefer `--exact` (or `--near` when several genuinely distinct
matches share a name) so the ambiguity never arises.

Never estimate a coordinate by reading a screenshot image - always `--click`,
or read the printed coordinate straight from `locate`/`automate get` output.
If the coordinate must come from a vision model (OCR/UIA both fail), click it
through `click-at` rather than raw `mouse click` - it adds the ambiguity check
you'd otherwise have to do by hand, and `--confirm` adds the two-measurement
cross-check on top of that.

**When OCR recognition mangles the text itself** (e.g. Cyrillic read as
"OTM?H?T? ?????????"), don't give up on OCR entirely - `locate --all` still
returns every detected line's *position*, even when the recognized text is
garbage. Dump it, filter for the digits/Latin substrings that usually survive
intact (order numbers, codes), and use their positions to reason about the
layout geometrically instead of by text content.

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
agent-rdp automate status                    # includes uptime, last RTT, consecutive-failure count
agent-rdp automate restart                   # relaunch the agent without a full RDP reconnect
```

Snapshots include `disabled` on interactive elements - a free pre-action
state check. A disabled "Отменить проведение" menu item tells you the
document isn't posted before you click anything.

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
