# agent-rdp

[![npm](https://img.shields.io/npm/v/@denisixnpm/agent-rdp.svg)](https://www.npmjs.com/package/@denisixnpm/agent-rdp)
[![CI](https://github.com/denisix/agent-rdp/actions/workflows/ci.yml/badge.svg)](https://github.com/denisix/agent-rdp/actions/workflows/ci.yml)
[![Release](https://github.com/denisix/agent-rdp/actions/workflows/release-please.yml/badge.svg)](https://github.com/denisix/agent-rdp/actions/workflows/release-please.yml)
[![license](https://img.shields.io/npm/l/@denisixnpm/agent-rdp.svg)](#license)
[![platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows-informational)](#installation)

A CLI tool for AI agents to control Windows Remote Desktop sessions, built on [IronRDP](https://github.com/Devolutions/IronRDP).

Screenshots, mouse/keyboard input, clipboard sync, drive mapping, verified file
transfer, Windows UI Automation, OCR text location, and JSON output for every
command — across named sessions with automatic daemon lifecycle.

## Demo

Claude Code automating SQLite database and table creation via RDP:

https://github.com/user-attachments/assets/91892b39-4edb-412b-b265-55ccd75d7421

## Installation

```bash
npm install -g @denisixnpm/agent-rdp        # CLI
npx add-skill https://github.com/denisix/agent-rdp   # as a Claude Code skill
```

<details>
<summary>From a GitHub release (standalone binary)</summary>

Each release attaches a binary per platform plus the OCR models as
`agent-rdp-models.tar.gz` / `.zip`. The models are architecture-independent, so
they ship once. The binary alone covers everything except `locate`, which needs
the models — extract them and point `AGENT_RDP_MODELS_DIR` at them.

```bash
tar -xzf agent-rdp-linux-x64.tar.gz -C ~/.local/bin      # or agent-rdp-darwin-arm64.tar.gz
chmod +x ~/.local/bin/agent-rdp
mkdir -p ~/.local/share/agent-rdp/models
tar -xzf agent-rdp-models.tar.gz -C ~/.local/share/agent-rdp/models
export AGENT_RDP_MODELS_DIR="$HOME/.local/share/agent-rdp/models"   # add to your shell rc
```

```powershell
Expand-Archive agent-rdp-win32-x64.zip -DestinationPath "$env:LOCALAPPDATA\agent-rdp"
Expand-Archive agent-rdp-models.zip -DestinationPath "$env:LOCALAPPDATA\agent-rdp\models"
setx AGENT_RDP_MODELS_DIR "$env:LOCALAPPDATA\agent-rdp\models"   # restart the terminal after
```

macOS release binaries are unsigned. If Gatekeeper blocks it with *"cannot be
opened because the developer cannot be verified"*:
`xattr -d com.apple.quarantine ~/.local/bin/agent-rdp`

Installing from npm needs none of this — the models ship in the package.
</details>

<details>
<summary>From source</summary>

```bash
git clone https://github.com/denisix/agent-rdp && cd agent-rdp
bun install
bun run build      # native binary
bun run build:ts   # TypeScript
```
</details>

## Using with AI coding agents

**Claude Code** — `npx add-skill https://github.com/denisix/agent-rdp` installs
the [SKILL.md](skills/agent-rdp/SKILL.md) workflow so Claude knows the commands,
flags and gotchas without you explaining them. It installs the CLI on first use
if it isn't on PATH. Then ask in plain language:

```
Connect to 192.168.1.100 as Administrator (password: secret), open Notepad,
type "hello from Claude", and take a screenshot.
```

**Codex** reads `AGENTS.md` instead. Add a section pointing at the tool:

```bash
cat >> AGENTS.md <<'EOF'

## Remote Windows control

Use the `agent-rdp` CLI (npm i -g @denisixnpm/agent-rdp) to control Windows machines via RDP:
connect, screenshot, mouse/keyboard input, and UI Automation. See
https://github.com/denisix/agent-rdp for the full command reference.
EOF
```

## Usage

### Connect

```bash
agent-rdp connect --host 192.168.1.100 --username Administrator --password 'secret'

# Environment variables (recommended - keeps the password out of the process list)
export AGENT_RDP_USERNAME=Administrator AGENT_RDP_PASSWORD=secret
agent-rdp connect --host 192.168.1.100

# stdin (most secure)
echo 'secret' | agent-rdp connect --host 192.168.1.100 -u Administrator --password-stdin

# Only when the session's daemon has stayed unresponsive for over a minute:
# stop it (gracefully, then by force) and start a fresh one. Plain `connect`
# refuses an unresponsive daemon rather than killing one that is merely busy.
agent-rdp connect --replace --host 192.168.1.100

agent-rdp disconnect
```

### Screenshot

```bash
agent-rdp screenshot --output desktop.png            # default ./screenshot.png
agent-rdp --json screenshot --output desktop.png     # metadata; image always goes to disk
agent-rdp screenshot --region 100,380,600,30 -o row.png   # crop; reports the offset back
```

A screenshot is the last frame the server painted, not a live poll. Each one
reports `frame_age_ms` (time since the server last sent anything), and `--json`
adds `frame_seq` and `frame_hash` — two screenshots with the same value are
guaranteed pixel-identical, which is how you confirm a frame actually changed
after an action rather than hashing the saved file yourself. A large
`frame_age_ms` usually just means an idle desktop, since RDP servers send
nothing when nothing changes; a genuinely dead connection is detected within
seconds by TCP keepalive and reported as disconnected.

> The CLI has no `screenshot --base64`. In Node, `rdp.screenshot({ path })`
> writes to disk and returns `{ path, width, height }` — prefer it over the
> base64-returning form when the caller doesn't need the bytes, since echoing a
> base64 image into an LLM context is expensive.

### Getting coordinates right

**Never estimate a coordinate by looking at a screenshot.** Screenshot pixels,
OCR boxes and click coordinates share one space, so a coordinate the tool
returns is exact — one guessed from an image is not. Images get downscaled on
their way into a vision model, and a click 30px off lands on the wrong row.

Ask for the target by name and let the tool find it:

```bash
agent-rdp locate "Добавить" --click          # coordinate never passes through your hands
agent-rdp locate "Добавить" --click --index 1  # several matches: choosing is explicit
agent-rdp automate click "#SaveButton"       # preferred when UI Automation can see it
agent-rdp automate focused                   # confirm which field has focus before typing
```

`agent-rdp mouse click X Y` remains available for coordinates you got from
`locate`, `automate get`, or a deliberate calculation.

### Mouse and keyboard

```bash
agent-rdp mouse click 500 300
agent-rdp mouse right-click 500 300
agent-rdp mouse double-click 500 300
agent-rdp mouse move 100 200
agent-rdp mouse drag 100 100 500 500

agent-rdp keyboard type "Hello, World!"        # Unicode, batched into one round-trip
agent-rdp keyboard type "text" --delay 20      # pace it if the remote app drops fast input
agent-rdp keyboard paste "Привет, мир!"        # clipboard + Ctrl+V as one command
agent-rdp keyboard press "ctrl+c"              # also alt+tab, enter, escape, f5, win+r
agent-rdp keyboard down shift                  # hold across other commands
agent-rdp keyboard up shift
```

Prefer `keyboard paste` for long or non-Latin text: it cannot lose individual
keystrokes, and leaves no gap for focus to move between setting the clipboard
and pasting.

### Scroll

Amount is positional (not `--amount`), and the default point is the **screen
center** rather than whatever pane you're working in — use `--at` to target one.

```bash
agent-rdp scroll up 3
agent-rdp scroll down 5 --at 600 400
agent-rdp scroll left            # also: right
```

### Clipboard and drive mapping

```bash
agent-rdp clipboard set "Hello from CLI"
agent-rdp clipboard set --file ./script.ps1   # or `--file -` to read stdin
agent-rdp clipboard get

# Drives must be mapped at connect time; multiple --drive flags are allowed
agent-rdp connect --host 192.168.1.100 -u Administrator -p secret \
  --drive /home/user/documents:Documents --drive /tmp/shared:Shared
agent-rdp drive list
```

Mapped drives appear on the remote as network locations. Clipboard text is
sent with CRLF line endings, so multi-line content survives
`Get-Clipboard | Set-Content`; `clipboard get` returns Windows text with its
CRLF intact.

> **Do not access `\\TSCLIENT\...` from `automate run`.** Drive redirection is
> serviced by the same task that carries the automation channel, so reading the
> share from inside the agent deadlocks — the command never returns and the
> session stops responding until you reconnect. Use `file push`/`file pull`.

### File transfer

Requires `--enable-win-automation`. Copies in chunks over the automation
channel, with a SHA-256 computed independently on each end, so a truncated or
corrupted transfer fails loudly instead of leaving a plausible-looking file.

```bash
agent-rdp file push ./report.xlsx "C:\\Users\\Admin\\report.xlsx"
agent-rdp file pull "C:\\Users\\Admin\\export.csv" ./export.csv
agent-rdp file pull "C:\\out\\result.json" ./result.json --max-age 120   # stale_file if older
agent-rdp file stat "C:\\scripts\\job.ps1"    # exists, size, SHA-256, modified/age - no transfer
```

Transfers are byte-exact, which also makes this the reliable way to place a
script with non-ASCII content on the remote — writing one through the clipboard
or `Add-Content` re-encodes it and mangles anything outside ASCII. Limit: 128MB.

`pull` reports the remote file's last-write time (`Modified: … (Ns ago by the
remote clock)`; `modified`/`modified_unix`/`age_secs` in JSON), so a result
file can be told from yesterday's without reading it; `--max-age <secs>`
refuses a stale one outright. The age is computed from the remote machine's
own clock, so it is right even when the two hosts disagree on the time.

A pull hashes the file and then reads it, so a file being rewritten underneath
it (a status file its producer replaces every few seconds) can change in
between. A file of 8MB or smaller gets up to 3 attempts (and only while they
stay quick) before that is reported, as `file_changed_during_transfer` rather
than a generic internal error. Note that
Windows PowerShell writes UTF-8 *with* a BOM and `pull` returns the bytes
verbatim, so a client-side parser needs to expect it (Python:
`encoding="utf-8-sig"`).

### Locate (OCR)

Finds text on screen with [ocrs](https://github.com/robertknight/ocrs). Use it
when UI Automation can't reach an element (WebView content, some dialogs).

```bash
agent-rdp locate "Cancel"                    # substring match, returns coordinates
agent-rdp locate "Save*" --pattern           # glob match
agent-rdp locate "Провести" --exact          # whole-line match
agent-rdp locate --all                       # all text on screen
agent-rdp locate "Cancel" --click            # also --double-click, --right-click
agent-rdp locate "OK" --wait 10000 --click   # block until it appears, then click
agent-rdp locate --all --region 100,380,600,30   # coords stay full-screen
agent-rdp locate "Отменить" --near "Заказ №001" --click   # anchor to a nearby label
```

Output carries coordinates ready to click:

```
Found 1 line(s) containing 'Cancel':
  'Cancel Button' at (650, 420) size 80x14 - center: (690, 427)
```

Clicking is deliberately strict: no match, or several matches without
`--index`, is an error rather than a guess. Default matching is substring
containment, so `"Провести"` also matches `"Провести и закрыть"` — use
`--exact` to avoid the ambiguity, `--near` when the same text genuinely repeats
across rows, or narrow with `--region`.

**Numbers with thousands separators can lose their leading digit group** — OCR
has been observed reading `1 250,00` as `2250,00`. For monetary values, crop
tight with `--region` and verify with `automate get` or a second independent
read before acting on the amount.

### Click-at (safe clicking of externally-computed coordinates)

When the click point comes from outside agent-rdp — a vision model reading a
screenshot, a manual crop — `locate --click` can't help and a raw `mouse click`
has no safety net. `click-at` clicks only if the point isn't ambiguously close
to more than one detected text region:

```bash
agent-rdp click-at 665 209
agent-rdp click-at 665 209 --window 400x160 --min-gap 20   # tune detection window/gap
agent-rdp click-at 665 209 --double-click                  # also --right-click

# Cross-check two independent measurements: clicks their midpoint if they agree
# within --max-divergence (default 40px), refuses if they don't.
agent-rdp click-at 665 209 --confirm 670,212 --max-divergence 20
```

The check uses OCR *detection* only (bounding boxes, script-agnostic), not
recognition, so it works even for text OCR can't read — custom-rendered
Cyrillic UIs where both UI Automation and `locate` fail. On refusal it lists
the nearby regions and exits non-zero; nothing is clicked.

### UI Automation

Drives Windows applications through the UI Automation API using native patterns
(Invoke, SelectionItem, Toggle, ExpandCollapse). A PowerShell agent is injected
into the remote session and speaks to the daemon over a Dynamic Virtual Channel.
Full protocol details in [AUTOMATION.md](docs/AUTOMATION.md).

```bash
agent-rdp connect --host 192.168.1.100 -u Admin -p secret --enable-win-automation

agent-rdp automate snapshot                 # full tree (refs always included)
agent-rdp automate snapshot -i -c -d 3      # interactive only, compact, depth 3
agent-rdp automate snapshot -s "~*Notepad*" # scope to a window
agent-rdp automate focused                  # what has keyboard focus right now

agent-rdp automate click "@e5"              # also: "#SaveButton", ".Edit", "~*wild*"
agent-rdp automate click "@e5" -d           # double-click
agent-rdp automate select "@e10" --item "Option 1"
agent-rdp automate toggle "@e7" --state on
agent-rdp automate expand "@e3"             # also: collapse, context-menu, focus, clear
agent-rdp automate get "@e2"                # includes Value: for text/multiline edits
agent-rdp automate fill ".Edit" "Hello World"
agent-rdp automate scroll "@e4" --direction down --amount 3
agent-rdp automate wait-for "#SaveButton" --timeout 5000 --state visible

agent-rdp automate window list
agent-rdp automate window focus "~*Notepad*"
agent-rdp automate window maximize|minimize|restore|close

agent-rdp automate run "Get-Process" --wait --process-timeout 5000
agent-rdp automate run "$PSVersionTable" --wait --shell pwsh.exe
agent-rdp automate run "ping -t 127.0.0.1" --stream   # returns a pid immediately
agent-rdp automate run-poll <pid>                     # drain output; repeat until exited
agent-rdp automate run-poll <pid> --json              # `pending: true` = alive, nothing new yet
agent-rdp automate run "Add-Content C:\log.txt x" --wait --idempotency-key step-07
                                            # a retry with the same key replays, never re-runs

agent-rdp automate status                   # health: RTT, failures, relaunches; works while the agent is down
agent-rdp automate restart                  # relaunch the agent now, keeping the RDP session
```

**Selectors:** `@e5` (snapshot ref), `#SaveButton` (automation ID), `.Edit`
(Win32 class), `~*pattern*` (wildcard name), `File` (exact name).

**Snapshot format**, with `disabled` shown on interactive elements — a free
pre-action state check (a disabled "Отменить проведение" tells you the document
isn't posted before you click anything):

```
- Window "Notepad" [ref=e1, id=Notepad]
  - MenuBar "Application" [ref=e2]
    - MenuItem "File" [ref=e3]
  - Edit "Text Editor" [ref=e5, value="Hello"]
```

**`automation indeterminate`** means the reply was lost, not that the action
failed. The agent journals recent results, so the daemon asks what actually
happened and usually returns the real outcome — or reports that the request
never ran and is safe to retry. A surviving `indeterminate` means the agent is
still busy: check state before retrying, or a click/fill can apply twice.
Read-only commands (`snapshot`, `get`, `status`, `wait-for`, `window list`) say
so explicitly, since those are always safe to retry.

**Retrying `run` safely.** Give a mutating `run` an `--idempotency-key`. A
retry that reuses the key — after `indeterminate`, an IPC timeout or
`daemon_unresponsive` — gets the recorded result of the first execution back
(`replayed: true`, with `replayed_at_unix`) instead of running the command
again; reusing a key for a *different* command is refused
(`idempotency_key_reused`). Keyed results are journaled on the remote host
under `%LOCALAPPDATA%\agent-rdp\journal` (7 days / 256 keys, per Windows
account), so a replay survives a reconnect, an agent relaunch and a logoff.
A different host or profile (an RDS farm, a temporary profile) starts empty —
verify the side effect there instead of retrying blindly. A retry with a
longer `--process-timeout` still replays — including a run that timed out
after starting (it may have had effects; verify, then use a new key). A parse
error or a launch that never started is not persisted, so a retry executes.

**What did it actually run?** Every `run` reply carries `command_line`: the
exact text the agent handed to the child shell (the command, then each
argument as a single-quoted literal). A command that does not parse as
Windows PowerShell is refused before anything launches (`parse_error`, with
line and column) and the error ends with that same command line — so a `$_`
your local shell expanded away, or a `1,2` argument re-tokenised, is visible
rather than guessed at. Waited runs also report `started_unix` and
`finished_unix` on the remote clock, print `(no stdout captured: the command
printed nothing)` when that is the case, and `(detached: output is not captured)` for a
launch without `--wait`/`--stream`.

**If the agent fails to start**, `connect` says so and the daemon keeps
retrying in the background (backoff 60s → 5 min, only once no input has been
sent for 2 minutes, at most 3 relaunch attempts per 10 minutes — each up to 3
typed launches — giving up after 6 consecutive failures). `automate status` answers while the agent is down with
`last_error` and `next_retry_secs`; `automate restart` retries at once;
`AGENT_RDP_NO_AUTO_RELAUNCH=1` disables the automatic retries.

**`run` exit codes mean something, and errors arrive as text.** The child
runs with `$ErrorActionPreference='Stop'` inside a `try/catch` the agent adds:
a cmdlet that fails non-terminatingly — `Add-Content` to a locked file,
`Set-Content` to a bad path — exits 1, and stderr carries the whole exception
chain as plain text (`ERROR: <type>: <message>`, then `caused by …` for each
inner exception, the failing line, and the script stack trace). The inner
exceptions are where a COM error's real message lives — a 1C failure that
used to surface as a bare `NullReferenceException` now shows the COM text
under it. Native commands keep their own exit code. Scripts that start with
`param(...)` or `using` get the prelude but no wrapper (those must be a
script's first statements). A script that wants continue-on-error sets
`$ErrorActionPreference='Continue'` on its first line; note that with `Stop`,
a native command's stderr merged via `2>&1` is an error. Do not redirect `*>`
into a file the script itself writes: the child holds it open and the script's
own writes fail with "being used by another process" — `run --wait` captures
output without a file.

**Detached launches report an immediate death.** `run` without `--wait`
returns the pid; if the process has already exited ~250ms later the reply
also carries `early_exit: true` and its `exit_code` — the "it said it started
but nothing happened" case made visible at launch time.

**Do not `Stop-Process -Name powershell` from a script.** The automation agent
is a `powershell.exe` process, and so are `run --stream` children; that kills
them all (`automate restart` brings the agent back, streamed pids are gone).
Children see the agent's pid as `$env:AGENT_RDP_AGENT_PID`:
`Get-Process powershell | Where-Object { $_.Id -notin $PID, [int]$env:AGENT_RDP_AGENT_PID } | Stop-Process`.

**Long-running commands** get the time they ask for: the transport deadline,
the CLI socket timeout and the watchdog all extend to cover
`--process-timeout`/`--timeout`. But the agent handles one command at a time,
so a long `run --wait` blocks every other `automate` call — past about a
minute, prefer `run --stream` plus `run-poll` — that is the detached mode:
`run-poll <pid> --follow [--follow-timeout <ms>]` keeps polling until the
process exits, printing output as it arrives and the exit code at the end, so
one command collects everything (a single plain poll before the child has
flushed prints nothing, which looks like lost output). Streamed output is
captured to files on the remote side, so nothing is lost if the process exits
between polls; a finished process stays pollable for 10 minutes and repeat
polls return `exited: true` with empty chunks. `--wait --stream` waits (the
stream flag is ignored, with a note). (If you redirect inside the command
instead, note that Windows PowerShell 5.1's `>` writes UTF-16LE.)

**`daemon_not_running` vs `daemon_unresponsive`.** The first means no daemon
process exists — reconnect, and `<session>/daemon.log` says why it exited. The
second means the process is alive but did not answer a health check within
10s: it is busy (a long `run --wait`, a file transfer). Wait and retry; do not
reconnect, that discards a working session. The message includes the tail of
daemon.log. Plain `connect` also refuses an unresponsive daemon (after
re-pinging it for ~30s) — it never kills one that may just be busy serving
another command; `connect --replace` is the explicit way to stop it and start
afresh, recorded in the transcript. A `not_connected` after the RDP transport
dropped says so ("The RDP transport dropped Ns ago (<reason>); the daemon
itself is alive"), and `session info` shows the last drop — that is a
`connect`, not a daemon restart. Read-only requests that lose their connection
mid-flight are retried once automatically; mutating ones never are.

**Idle sessions are kept alive.** RDP only ships screen deltas, so a session
nobody is driving sends nothing in either direction and a NAT or firewall on
the path eventually drops it — seen in the field after as little as ~5 minutes
of idling. The daemon sends a Refresh Rect PDU every 45s to keep the path warm
(`connect --keep-alive-secs <n>`, `0` disables). It carries no input, focus or
lock-key semantics, so it cannot disturb the remote desktop.

**A dead transport is detected in about a minute.** The RDP socket is
configured to give up on unacknowledged data after 30s, so a black-holed path
(no RST/FIN, just silence) surfaces within roughly one keep-alive interval plus
that. Before this it fell back to the OS retransmission timeout — four to five
minutes during which `screenshot` kept succeeding against a stale frame.

**The automation agent survives a reconnect.** When the transport drops, the
agent keeps re-opening its channel for about 10 minutes rather than exiting, so
a `connect` in that window adopts the running agent instead of launching one —
no Win+R, no foreground change on a desktop someone else may be using.
`automate status` reports `adopted` when that happened, `total_launches` (every
launch that did type Win+R, including each `connect`'s bootstrap) and
`relaunches` (self-heal restarts since the last connect). `connect
--defer-agent` skips the launch entirely and leaves the agent to `automate
restart`.

**The automation agent heals itself.** If its DVC channel closes while the
RDP session is alive, the daemon relaunches it (at most 3 times per 10
minutes; `automate status` reports `relaunches`), and the agent no longer
exits on the transient read errors a CPU-starved host produces. Cold
`connect --enable-win-automation` on such a host is given up to ~5 minutes:
three launch attempts with handshake windows of 25/45/75s, each extended when
the agent is visibly still starting.

**`daemon_version_mismatch`** means the daemon was started by a different
agent-rdp version than the CLI — it kept running across an upgrade, and is
still serving the old code, including the automation agent it embeds. Run
`agent-rdp connect ...` again: it replaces the daemon (and the SDK does the
same on its own). `session info` shows both versions. This is worth knowing
about because it is how "upgraded, but the old bug still reproduces" happens.

**Arrow-key navigation inside a panel can land on the wrong item.** Observed in
1C side panels: Up/Down then Enter is not reliably deterministic. Prefer
`automate` refs; when the panel isn't exposed to UI Automation, use two
independent measurements with [`click-at --confirm`](#click-at-safe-clicking-of-externally-computed-coordinates).

### Sessions and the web viewer

```bash
agent-rdp session list
agent-rdp session info
agent-rdp --session work connect --host work-pc.local ...   # named session
agent-rdp --session work screenshot

agent-rdp wait 3000    # pause; useful right after connect, before the first input

# Web viewer (needs streaming enabled at connect)
agent-rdp --stream-port 9224 connect --host 192.168.1.100 -u Admin -p secret
agent-rdp view --port 9224
```

There is no `session close` — use `disconnect`.

### Diagnostics and bug reports

```bash
agent-rdp diagnose                       # -> ./agent-rdp-diagnostics-<session>-<ts>.zip
agent-rdp diagnose --output report.zip
```

The zip holds `daemon.log` (+ `.prev`), `transcript.jsonl` (one redacted line
per request: what was asked, outcome, duration), `diagnostics/` (failure
captures), a current screenshot, the remote automation agent's own log, and
`info.json` (versions, OS, daemon state, which `AGENT_RDP_*` variables are
set). It is built from disk first and the daemon second, so it works when the
daemon is dead or unresponsive — those parts are listed as skipped. The RDP
password is never logged, and is blanked if it appears anyway.

Failure captures are automatic: when `locate` finds nothing, `click-at` refuses,
an `automate` command errors, or a waited `run` exits non-zero, the daemon saves
`diagnostics/<ts>-<kind>-<code>.png` plus a `.json` with the request, the error
and — for OCR misses — every line OCR *did* read. At most one capture per 5s,
the newest 20 kept. Set `AGENT_RDP_DIAGNOSTICS=0` to turn the transcript and
captures off.

## JSON output

Add `--json` to any command:

```json
{ "success": true,  "data": { "type": "screenshot", "path": "desktop.png", "width": 1920, "height": 1080 } }
{ "success": false, "error": { "code": "not_connected", "message": "Not connected to an RDP server" } }
```

## Environment variables

| Variable | Description |
|----------|-------------|
| `AGENT_RDP_HOST` | RDP server hostname or IP |
| `AGENT_RDP_PORT` | RDP server port (default: 3389) |
| `AGENT_RDP_USERNAME` | RDP username |
| `AGENT_RDP_PASSWORD` | RDP password |
| `AGENT_RDP_SESSION` | Session name (default: "default") |
| `AGENT_RDP_STREAM_PORT` | WebSocket streaming port (0 = disabled) |
| `AGENT_RDP_MODELS_DIR` | OCR models directory (set automatically by the npm wrapper; needed for standalone binary installs) |
| `AGENT_RDP_DIAGNOSTICS` | Set to `0` to disable the request transcript and failure captures |
| `AGENT_RDP_NO_AUTO_RELAUNCH` | Set to `1` before `connect` to stop the daemon relaunching the automation agent on its own (`automate restart` still works) |

## Node.js API

```typescript
import { RdpSession } from 'agent-rdp';

const rdp = new RdpSession({ session: 'default' });
await rdp.connect({
  host: '192.168.1.100', username: 'Administrator', password: 'secret',
  width: 1280, height: 800,
  drives: [{ path: '/tmp/share', name: 'Share' }],
  enableWinAutomation: true,
});

// Screenshot - prefer `path` so a large base64 string is never held in memory
const { path, width, height } = await rdp.screenshot({ format: 'png', path: 'shot.png' });
const { base64 } = await rdp.screenshot({ format: 'png' });   // or raw, for in-process use

await rdp.mouse.click({ x: 100, y: 200 });      // also rightClick, doubleClick, move
await rdp.mouse.drag({ from: { x: 100, y: 100 }, to: { x: 500, y: 500 } });

await rdp.keyboard.type({ text: 'Hello World' });
await rdp.keyboard.paste('Привет, мир!');       // reliable for long/non-Latin text
await rdp.keyboard.press({ keys: 'ctrl+c' });
await rdp.keyboard.down('shift'); await rdp.keyboard.up('shift');

await rdp.scroll.down({ amount: 5 });           // default 3; { x, y } to target a point
await rdp.clipboard.set({ text: 'text to copy' });
const text = await rdp.clipboard.get();

// OCR - click directly so the coordinate never leaves the process
await rdp.locate({ text: 'Cancel', click: 'left' });
await rdp.locate({ text: 'Провести', exact: true, click: 'left' });
await rdp.locate({ text: 'OK', waitMs: 10000, click: 'left' });   // block until it appears
const allText = await rdp.locate({ all: true });

// Safe click of an externally-computed point (e.g. a vision-model bbox)
const result = await rdp.clickAt(665, 209);
if (!result.clicked) console.log('Ambiguous:', result.nearby);

// File transfer (needs enableWinAutomation)
await rdp.files.push('./report.xlsx', 'C:\\Users\\Admin\\report.xlsx');
await rdp.files.pull('C:\\Users\\Admin\\export.csv', './export.csv');

// UI Automation
const snapshot = await rdp.automation.snapshot({ interactive: true });
const focused = await rdp.automation.focused();
await rdp.automation.click('@e5', { doubleClick: true });
await rdp.automation.fill('#input', 'text');
await rdp.automation.run('notepad.exe');
await rdp.automation.waitFor('#SaveButton', { timeout: 5000 });
await rdp.automation.focusWindow('~*Notepad*');

const drives = await rdp.drives.list();
const info = await rdp.getInfo();
await rdp.disconnect();
```

The process can stay alive after `disconnect()` unless the caller cleans up —
call `rdp.close()` or exit explicitly.

**WebSocket streaming** for real-time capture: construct with
`{ streamPort: 9224 }`, then `rdp.getStreamUrl()` returns `ws://localhost:9224`.
Message types, clipboard flow and input handling are specified in
[WEBSOCKET.md](docs/WEBSOCKET.md).

## Architecture

A daemon per session: the **CLI** parses commands and talks to a **daemon** over
Unix sockets (macOS/Linux) or TCP (Windows); the daemon owns the RDP connection
and processes commands. It starts on the first command and persists until
closed. See [CLAUDE.md](CLAUDE.md) for the internals.

## Limitations

- **WebViews** — UI Automation cannot see WebView content (Start menu search,
  Edge, Electron apps). Launch programs with `automate run` or Win+R instead of
  clicking through menus.
- **UAC dialogs** run on a secure desktop and are invisible to UI Automation.
  There is no good workaround short of the remote user acting manually.
- **OCR** can misread characters, miss text, or return imprecise coordinates.
  Use it only when UI Automation can't reach an element, and verify before
  clicking anything destructive.
- **Claude models in non-computer-use mode** (Claude Code included) are poor at
  estimating pixel coordinates from screenshots — don't ask for a guess from an
  image. Gemini models are generally good at it; for Claude, use the
  [Computer Use Tool](https://docs.anthropic.com/en/docs/agents-and-tools/computer-use).

## Requirements

Rust 1.75+. The target must offer **TLS** for RDP — agent-rdp uses `rustls`,
which doesn't implement TLS 1.0/1.1, so legacy hosts (e.g. Windows Server
2008 R2) fail with a handshake error. **Stock Windows defaults work as
shipped.**

<details>
<summary>RDP security layer settings</summary>

All six combinations measured against Windows Server 2022, varying only these
two values under
`HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp`:

| `SecurityLayer` | `UserAuthentication` | Meaning | Result |
|---|---|---|---|
| `1` (Negotiate) | `1` | **Windows default** | **works** |
| `2` (TLS) | `1` | TLS + NLA (most explicit) | **works** |
| `2` (TLS) | `0` | TLS, no NLA | **works** |
| `0` (RDP) | `1` | NLA forces CredSSP/TLS | **works** |
| `1` (Negotiate) | `0` | server picks, and declines TLS | fails |
| `0` (RDP) | `0` | legacy RC4 only | fails |

The rule: **agent-rdp works wherever the host offers TLS.** With NLA on that is
always the case, since NLA forces CredSSP/TLS regardless of `SecurityLayer`.
Both failing rows are NLA-off hosts that refuse TLS outright.

If `connect` reports *"server only supports Standard RDP Security"*, the host is
in one of those rows. Enable NLA (preferred — it also gives pre-authentication)
or force TLS:

```powershell
$k = 'HKLM:\System\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp'
Set-ItemProperty -Path $k -Name UserAuthentication -Value 1   # enable NLA, or:
Set-ItemProperty -Path $k -Name SecurityLayer      -Value 2   # force TLS
```

New connections normally pick this up immediately; restart `TermService` only if
they don't (that drops existing sessions).

Neither failing case is fixable client-side. With `SecurityLayer=1` and NLA off
the server returns `SSL_NOT_ALLOWED_BY_SERVER` even though the client advertises
`PROTOCOL_SSL` — verified by testing a build that requested SSL *only*, rejected
identically. Standard RDP Security (`SecurityLayer=0`, NLA off) is refused by
IronRDP itself, which implements no RC4 transport; it uses a well-known key
derivation, so credentials sent over it are recoverable in transit.
</details>

## Credits

Originally created by [Nick Yu](https://github.com/thisnick)
([thisnick/agent-rdp](https://github.com/thisnick/agent-rdp)). This fork
([denisix/agent-rdp](https://github.com/denisix/agent-rdp), published as
[`@denisixnpm/agent-rdp`](https://www.npmjs.com/package/@denisixnpm/agent-rdp))
is maintained independently; see
[CHANGELOG.md](packages/agent-rdp/CHANGELOG.md) for what's changed.

## License

MIT OR Apache-2.0 (same as IronRDP)
