---
name: agent-rdp
description: Control Windows Remote Desktop sessions for automation, testing, and remote administration. Use when the user needs to connect to Windows machines via RDP, take screenshots, click, type, or interact with remote Windows desktops.
allowed-tools: Bash(agent-rdp:*), Bash(npm install -g @denisixnpm/agent-rdp)
---

# agent-rdp

Tested against agent-rdp 0.7.17. Check with `agent-rdp session info` (shows
both CLI and daemon versions, also in `--json` as `cli_version` /
`daemon_version`) — a `daemon_version_mismatch` error means an older daemon
survived an upgrade; run `connect` again to replace it.

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
already waits for the agent (~25s, up to ~5 minutes on a starved host) and
says so explicitly if it fails to start. Do **not** reconnect for that: the
daemon keeps retrying the launch in the background (with backoff, only once
nobody has sent input for 2 minutes), and `automate status` works while the
agent is down — it shows `last_error` and `next_retry_secs`. `automate
restart` forces a retry immediately.

## Reliability rules (learned from real failures)

**A DVC timeout does NOT mean the action failed.** `automation indeterminate`
means the request reached the agent but the reply was lost. The agent journals
recent results, so the daemon asks what happened and usually returns the real
outcome, or states that the request never ran. A surviving `indeterminate`
means the agent is still busy: check state first (`automate get "@e2"`), never
blindly retry, or you double-apply — text typed twice, a button clicked twice.
Read-only commands (`snapshot`, `get`, `status`, `wait-for`, `window list`) say
so in the error text; those are always safe to retry.

**Retry a mutating `run` only with the same `--idempotency-key`.** Pick a key
per logical step (`--idempotency-key step-07`); a retry that reuses it after
`indeterminate`, an IPC timeout or `daemon_unresponsive` returns the recorded
result (`replayed: true`) instead of executing again — this is what stops
`Add-Content` from being applied twice. Keyed results are journaled on the
remote host (`%LOCALAPPDATA%\agent-rdp\journal`, 7 days / 256 keys, per
Windows account), so the replay survives a reconnect, an agent relaunch and a
logoff; a replayed result says when it originally ran (`replayed_at_unix`),
and a replayed *error* says so in its message. A retry with a longer
`--process-timeout` still replays (the budget is not part of the key) — so a
keyed run that *timed out* replays the timeout too: the process did start and
may have had effects; verify, then use a new key. A parse error or a launch
that never started is not persisted and a retry executes. The
journal is empty again only on a different host or profile (RDS farm,
temporary profile) — verify the side effect (`file stat`, `Test-Path`) there
rather than retrying.

**`run` exit codes are trustworthy; still verify side effects.** The child
runs with `$ErrorActionPreference='Stop'` inside an agent-added `try/catch`,
so a failed cmdlet (locked file, bad path, access denied) exits non-zero and
stderr carries the full exception chain as plain text: `ERROR: <type>:
<message>`, `caused by <inner type>: <message>` for each inner exception (this
is where a 1C COM error's real text is), the failing line, the stack trace.
Native executables keep their own exit codes. A detached `run` (no `--wait`)
that dies within ~250ms comes back with `early_exit: true` + `exit_code` —
treat that as "never ran". For anything that matters, confirm the effect
(`automate run "Test-Path ..." --wait`, or `file pull --max-age`) before
building on it. Never redirect `*>` into a file the script itself writes — the
child holds it open and the script fails with "being used by another
process"; `run --wait` captures output without any file. Scripts starting
with `param(`/`using` are run unwrapped.

**Never `Stop-Process -Name powershell` in a script.** The agent is a
`powershell.exe`, and so are streamed children: that kills them all. Exclude
the agent: `Get-Process powershell | Where-Object { $_.Id -notin $PID, [int]$env:AGENT_RDP_AGENT_PID } | Stop-Process`.
If it happens, `automate restart` relaunches the agent (streamed pids are lost).

**1C COM from PowerShell: a `NullReferenceException` with source
`System.Management.Automation` is PowerShell's COM binder, not 1C.** Seen with
`ПОДОБНО`/`LIKE` in a query text (`=` works). Call through reflection instead:
`[System.__ComObject].InvokeMember('Execute', 'InvokeMethod', $null, $query, $null)`
— the same trick that also recovers the real 1C error text inside `catch`.

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
agent-rdp automate run "long-build.cmd" --stream                     # returns a pid immediately
agent-rdp automate run-poll <pid> --follow --follow-timeout 600000   # collect until exit (or 10 min)
agent-rdp automate run-poll <pid>                                    # single poll; repeat until exited
```

**Inline `run` quoting: where each layer stops.** Three layers touch the
command text, and the reply tells you what survived them (`command_line` in
`--json`, and on stderr whenever the exit code is non-zero):

1. *Your local shell.* In bash/zsh double quotes, `$_`, `$var` and backticks
   are expanded **before** agent-rdp sees the text — `"... | ForEach-Object {
   $_.Line.Trim() }"` arrives as `{ .Line.Trim() }`. Use single quotes
   locally, or `--` is not enough: the expansion already happened. The CLI
   prints a note when the text looks like that.
2. *Argument quoting.* Everything after the command is sent as a separate
   argument and appended as a single-quoted PowerShell literal: `,`, `$` and
   wildcards in an argument are literal, and there is no way to pass
   PowerShell syntax through arguments. Put the syntax in the command string
   (`automate run "powershell -File x.ps1 -Param '1,2'"`).
3. *The agent's parser.* The command string is parsed as Windows PowerShell
   5.1 source before the wrapper is added. A text that does not parse is
   refused with `parse_error: … line N, column M: …` **without launching
   anything**, and the message ends with `Command line as executed by the
   agent:` — the exact text, so you can see which layer changed it. (With
   `--shell pwsh.exe` the 5.1 parser is skipped and pwsh reports its own
   errors.)

Anything with pipelines, `$_`, here-strings or more than one line: `file
push` a `.ps1` and run it with `-File`. That path has no quoting layer at all.

**Under CPU saturation** (seen at 100% CPU with 7 parallel 1C sessions on 2
vCPU): the transport and the agent stay up since 0.7.14, but anything that
*spawns a process* can take 30s+ instead of 1s. Give such commands
`--process-timeout 60000` or more (the IPC and watchdog budgets follow it);
a process that overruns is killed and reported as `command_failed: Process
timed out`, with the command line. Health check with `automate status
--json`: `last_rtt_ms` (round trip of the last request; hundreds of ms under
load is normal), `consecutive_failures` (non-zero = degraded), `relaunches`
(the agent died and was brought back), `last_error` / `next_retry_secs`
(the agent is down and when the daemon tries again). Screenshots carry
`frame_age_ms`; a frame 30-60s old with a live channel is the server being
slow to paint, not a dead session.

**`file push`/`pull` take absolute paths.** Both are executed by the daemon and
the remote agent, neither of which shares your working directory. The CLI and
SDK make the local path absolute for you; a relative *remote* path is refused
rather than written somewhere nobody looks. A push is staged beside the
destination and swapped in only after its hash is checked on both ends, so a
failed transfer leaves the previous file intact and `transfer_verification_failed`
means nothing was replaced.

**Pulling a file while something rewrites it.** `file pull` hashes the remote
file and then reads it — two separate remote operations, so a file being
replaced every few seconds can change in between. The pull now retries that
pair (3 attempts, 150ms then 300ms apart) for files of 8MB or smaller, and
only then fails, with `file_changed_during_transfer` rather than a generic
`internal_error`.
Cheaper still: have the producer write to a temp name and copy a snapshot,
then pull the snapshot.

**Files the remote wrote may carry a UTF-8 BOM.** Windows PowerShell 5.1
writes UTF-8 *with* BOM, and `file pull` returns bytes verbatim (as it
should), so client-side parsers need to expect it — Python wants
`encoding="utf-8-sig"`, or `json.load` fails on the first character.

**Freshness.** Every waited `run` reports `started_unix` and
`finished_unix` (remote clock), `file stat`/`file pull` report `modified_unix`
and `age_secs` on the same clock, and `file pull --max-age` refuses a stale
file. An empty waited stdout is printed as `(no stdout captured: the command
printed nothing)`; a detached launch says `(detached: output is not
captured)`. In JSON, `stdout: ""` is "ran, printed nothing" and a missing
`stdout` is "not captured".

This is the detached mode — use it instead of hand-rolled WMI
`Win32_Process.Create` + out-file polling. `--follow` prints output as it
arrives and the exit code at the end; a single plain poll issued before the
child has flushed reports that the process is running with no output yet
(`pending: true` in `--json`, which is a global flag and works on `run-poll`
like everywhere else) — not lost, just not written yet. Output is captured to
files on the remote side, so nothing is lost if the process exits between
polls; the final poll returns the tail plus `exited: true`, and a repeat poll
within 10 minutes returns `exited: true` again with empty chunks rather than
an error. `--wait --stream` waits. Put
agent-rdp options *before* the command: anything after it (or after `--`)
goes to the remote shell, and a `--wait` there is refused rather than
silently launching detached. If you redirect inside the command instead
(`*> out.txt`), remember Windows PowerShell 5.1 writes UTF-16LE — use
`| Out-File -Encoding utf8` before `file pull`.

**"Channel unresponsive" is usually transient.** Re-probe with `automate
status` — it reports agent uptime, last DVC round-trip, consecutive-failure
count and `relaunches`, so you can tell "degraded but working" from "dead".
If the agent really is gone, the daemon relaunches it by itself when the
channel closes; `automate restart` does the same on demand without touching
the RDP session or invalidating refs (it reports "already in progress" if the
automatic relaunch is running). A full `disconnect`+`connect` is the last
resort: it re-issues every ref, and combined with a retry is what corrupts
state.

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
quotes the last lines of daemon.log so you can see what it is doing. Plain
`connect` also refuses an unresponsive daemon (after re-pinging it for ~30s)
rather than killing it; only if it stays unresponsive for over a minute use
`connect --replace`, which stops it gracefully and starts a fresh one. Cold
`connect` with automation can take up to ~5 minutes on a CPU-starved host;
its timeout covers that. A third verdict, `daemon_version_mismatch`, means
the daemon is from a different agent-rdp version than the CLI (it outlived an
upgrade): run `connect` again, which replaces it — every other command refuses
rather than silently driving old code.

**Reconnecting usually leaves the remote desktop alone now.** The Windows agent
outlives a transport drop: it keeps re-opening its channel for about 10 minutes,
so a `connect` within that window adopts the running agent and types nothing.
`automate status` shows `adopted: yes` when that happened, and `total_launches`
counts the launches that did type Win+R (every agent launch against this host,
`connect`'s bootstrap included) next to `relaunches`, which only counts
self-heal restarts since the last connect. Launching still costs a real
foreground change — Win+R, paste, Enter into the Run dialog — so if someone
else's automation shares that desktop it will notice. Two ways to avoid it:
reconnect promptly (a fast reconnect finds the agent still there), and keep
sessions alive so the drop does not happen (`connect --keep-alive-secs`,
default 45s, 0 disables). `connect --defer-agent` skips the launch entirely: it
still adopts a surviving agent, and otherwise leaves the agent down until you
run `automate restart`.

**A dead transport is noticed in about a minute.** The socket is configured to
give up on unacknowledged data after 30s, so a black-holed path surfaces within
roughly one keep-alive interval plus that — not the 4-5 minutes an OS
retransmission timeout takes. Until it surfaces, `screenshot` keeps returning
the last frame it has; `session info` reports the frame age against the
keep-alive interval, and a frame much older than the interval on a live
session usually points at the transport rather than an idle desktop.

**Never use `connect` as a health check or a retry reflex.** `connect` is a
session action: it tears down and rebuilds the RDP session (and, before
0.7.14, killed a daemon that missed one ping — with a "reconnect before every
command" habit that was the main cause of daemons dying between commands and
of `EOF while parsing a value` on the command running at the time). Probe
with `session info` (cheap, never destructive) or `automate status`; on
`daemon_not_running` reconnect once; on `daemon_unresponsive` wait and retry
the same command. Reconnect only when the session is actually gone.

**`not_connected` after a transport drop names the drop.** If the RDP
transport fell over while the daemon stayed up (seen under 100% CPU on the
server), errors say so: "The RDP transport dropped Ns ago (<reason>); the
daemon itself is alive" — and `session info` shows `Last transport drop`.
That is a `connect` (the automation agent is relaunched by it), not a
daemon problem. A read-only request that lost its connection mid-flight is
retried once automatically; a mutating one reports the drop and is yours to
verify.

**`channel_closed` means the agent process ended — and it comes back on its
own.** The daemon relaunches the agent when its DVC channel closes while the
RDP session is alive, and keeps retrying a launch that failed (60s, 120s,
240s, then every 5 minutes; at most 3 relaunch attempts per 10 minutes, each
up to 3 typed launches; gives up after 6 consecutive failures until `automate
restart`). Because a launch types
Win+R and pastes into the session, an automatic one waits until no input has
been sent for 2 minutes — so it never interrupts a `keyboard`/`mouse`
sequence you are driving. `automate status` shows `relaunches`, `last_error`
and `next_retry_secs` (and works while the agent is down). Set
`AGENT_RDP_NO_AUTO_RELAUNCH=1` before `connect` to turn automatic relaunches
off. The agent itself rides out the transient read errors a CPU-starved host
produces instead of exiting on them. If `relaunches` keeps climbing,
something on the server is killing PowerShell — `agent-rdp diagnose` pulls
the remote agent log.

**Check a pushed file before launching it detached:** `agent-rdp file stat
"C:\path\script.ps1"` reports existence, size, SHA-256 and age by the remote
clock in one round trip; `run` results carry `started_unix`/`finished_unix`
on the same clock, so a run can be matched to the files it produced.

**When something fails, run `agent-rdp diagnose` before reporting it.** It
writes a zip with `daemon.log`, `transcript.jsonl` (every request with outcome
and timing), the `diagnostics/` failure captures, a current screenshot and the
remote agent's log; it works even when the daemon is dead. Captures are
automatic: a `locate` with no match, a refused `click-at`, an `automate` error
or a waited `run` with non-zero exit each save a screenshot plus a `.json`
with the request, the error and (for OCR misses) every line OCR did read, into
`<session>/diagnostics/`. Include the zip in any bug report, together with the
exact commands that led there.

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
agent-rdp connect --replace --host <ip> ... # only for a daemon that stays unresponsive > 1 min
agent-rdp disconnect                       # there is no `session close`
agent-rdp session list
agent-rdp session info                     # includes daemon version vs CLI version
agent-rdp --session work connect ...       # named session
agent-rdp --session work screenshot
agent-rdp diagnose                         # zip of logs, transcript, captures, screenshot

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
agent-rdp file pull "C:\out\result.json" ./r.json --max-age 120  # stale_file if older; prints Modified/age
agent-rdp file stat "C:\scripts\job.ps1"   # exists? size, SHA-256, age - before a detached launch

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
agent-rdp automate status                  # works while the agent is down: last_error, next_retry_secs
agent-rdp automate restart                 # relaunch agent now, without an RDP reconnect

# Run commands/apps (preferred way to open apps)
agent-rdp automate run "notepad.exe"
agent-rdp automate run "Get-Process" --wait --process-timeout 5000
agent-rdp automate run '$PSVersionTable' --wait --shell pwsh.exe   # single quotes locally: $ survives
agent-rdp automate run "ping -t 127.0.0.1" --stream                # returns pid
agent-rdp automate run-poll <pid>                                  # drain incrementally
agent-rdp automate run "Add-Content C:\l.txt x" --wait --idempotency-key step-07
                                          # same key on retry -> replayed, not re-run (survives reconnect)
# every waited run: started_unix/finished_unix (remote clock), command_line (what the agent ran)
```

Snapshots include `disabled` on interactive elements — a free pre-action state
check. A disabled "Отменить проведение" tells you the document isn't posted
before you click anything.

**1C Enterprise (Taxi interface) — what UIA exposes.** Only named form
`Pane` titles plus the toolbar and menus are exported; table rows, form
fields and form buttons are not. `automate click` on such elements throws
`NotImplementedException` (no InvokePattern) — click by coordinates from the
snapshot's `bounds` instead (`agent-rdp mouse click x y`, or `click-at`).
Double-click a list row to open the document; **Enter in a list means
"Create"**, not "Open". `Ctrl+F4` does not close a list form — use the ✕ in
its title bar. Side panels ("Функции"): Up/Down + Enter is not deterministic;
prefer typing into the search field. A `NullReferenceException` from
`System.Management.Automation` in COM calls is the binder, not 1C (see above).

**Foreground lock from a background child.** A process started by `automate
run` is not the foreground process, so `SetForegroundWindow` from it fails
silently (Windows' foreground lock). The ladder that works, ready to paste
into a pushed script:

```powershell
$sig = @'
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
[DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
[DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
'@
$U = Add-Type -MemberDefinition $sig -Name Win -Namespace Fg -PassThru
function Set-Foreground([IntPtr]$hwnd) {
    [System.Windows.Forms.SendKeys]::SendWait('%')        # 1. the Alt trick unlocks the foreground
    if ($U::SetForegroundWindow($hwnd)) { return $true }
    $fg = $U::GetForegroundWindow()                        # 2. attach to the foreground thread's input
    $fgT = $U::GetWindowThreadProcessId($fg, [IntPtr]::Zero); $me = $U::GetCurrentThreadId()
    [void]$U::AttachThreadInput($me, $fgT, $true)
    try { $ok = $U::SetForegroundWindow($hwnd) } finally { [void]$U::AttachThreadInput($me, $fgT, $false) }
    if ($ok) { return $true }
    [void]$U::ShowWindow($hwnd, 6); [void]$U::ShowWindow($hwnd, 9)   # 3. minimize/restore forces it
    return $U::SetForegroundWindow($hwnd)
}
```

`automate window focus <selector>` does the equivalent from the agent, which
is usually simpler when the window is visible to UIA.

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
