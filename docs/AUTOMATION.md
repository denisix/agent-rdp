# Windows UI Automation - Implementation Specification

This document describes the architecture, protocols, and implementation details of agent-rdp's UI automation system.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Host Machine                            │
│                                                                 │
│  CLI ──► Daemon ──► DvcIpc ──► DVC Channel ──► RDP Connection   │
│                                 "AgentRdp::Automation"          │
└─────────────────────────────────────────────────────────────────┘
                              │ RDP Protocol (Dynamic Virtual Channel)
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Remote Windows Machine                       │
│                                                                 │
│  agent.ps1 ◄──► WTS File Handle ◄──► Windows UI Automation API  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Dynamic Virtual Channel (DVC) IPC

Communication between host and remote uses a **Dynamic Virtual Channel (DVC)** named `AgentRdp::Automation`. This provides low-latency, bidirectional communication directly through the RDP protocol.

### Rust Side - IronRDP Integration

The Rust daemon implements `DvcProcessor` trait for the automation channel:

```rust
pub struct AutomationDvc {
    state: SharedDvcState,
    handshake_tx: Option<mpsc::UnboundedSender<DvcHandshake>>,
}

impl DvcProcessor for AutomationDvc {
    fn channel_name(&self) -> &str { "AgentRdp::Automation" }
    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>>;
    fn close(&mut self, channel_id: u32);
}
```

For proactive sending (Rust → PowerShell), we use `encode_dvc_messages()` with a command channel:

```rust
// In background frame processor
dvc_cmd = dvc_command_rx.recv() => {
    let data_pdu = DrdynvcDataPdu::Data(DataPdu::new(cmd.channel_id, cmd.data));
    let frame = active_stage.encode_dvc_messages(vec![SvcMessage::from(data_pdu)])?;
    framed.write_all(&frame).await?;
}
```

### PowerShell Side - WTS API

The PowerShell agent uses the WTS API with file handle approach (recommended by Microsoft for DVC):

```powershell
# 1. Open channel
$wtsHandle = [WtsApi]::WTSVirtualChannelOpenEx(
    [WtsApi]::WTS_CURRENT_SESSION,
    "AgentRdp::Automation",
    [WtsApi]::WTS_CHANNEL_OPTION_DYNAMIC)

# 2. Query for file handle
[WtsApi]::WTSVirtualChannelQuery($wtsHandle, $WTSVirtualFileHandle, [ref]$ptr, [ref]$len)

# 3. Duplicate handle for use with ReadFile/WriteFile
[Kernel32]::DuplicateHandle(..., [ref]$fileHandle, ...)

# 4. Read/Write using standard file I/O
[Kernel32]::ReadFile($fileHandle, $buffer, ...)
[Kernel32]::WriteFile($fileHandle, $buffer, ...)
```

**Framing**: each `ReadFile` on the channel handle returns one fragment: an
8-byte `CHANNEL_PDU_HEADER` (`length` = total bytes of the whole message, the
same on every fragment; `flags` = `CHANNEL_FLAG_FIRST` 0x1 / `CHANNEL_FLAG_LAST`
0x2) followed by up to 1600 bytes of data. A message under ~1.6KB is a single
fragment flagged FIRST|LAST; larger ones (every `file_write_chunk`) span
several, and `Read-DvcMessage` concatenates the data parts until LAST. On the
daemon side, `automation::encode_dvc_data` splits outbound messages into
`DataFirst`/`Data` PDUs at 1590 bytes, as the DVC spec requires. Both halves
are needed: with either missing, requests over ~1.6KB reach the agent as
unparseable pieces and get no reply.

## Message Protocol

Messages are JSON documents, one per DVC message (framing as above).

### Message Types

**Handshake** (PowerShell → Rust, sent on channel open):
```json
{
  "type": "handshake",
  "version": "1.7.0",
  "agent_pid": 12345,
  "capabilities": ["snapshot", "click", "select", "toggle", ...]
}
```

The daemon feature-detects from `capabilities` — `query_result` and the
`file_*` commands are only used when the connected agent advertises them.

**Request** (Rust → PowerShell):
```json
{
  "type": "request",
  "id": "a1b2c3d4",
  "command": "snapshot",
  "params": {
    "interactive_only": true,
    "max_depth": 10
  }
}
```

**Response** (PowerShell → Rust):
```json
{
  "type": "response",
  "id": "a1b2c3d4",
  "success": true,
  "data": { ... },
  "error": null
}
```

### Request/Response Flow

```
1. CLI sends Request to Daemon via Unix socket/TCP
2. Daemon generates unique request ID (8-char UUID prefix)
3. Daemon sends JSON request via DVC command channel
4. IronRDP encodes and sends via RDP connection
5. PowerShell agent receives via ReadFile on DVC file handle
6. PS agent executes UI Automation command
7. PS agent sends JSON response via WriteFile
8. IronRDP receives in DvcProcessor::process()
9. Daemon routes response to waiting request by ID
10. Daemon returns response to CLI
```

### Error Response Format

```json
{
  "type": "response",
  "id": "a1b2c3d4",
  "success": false,
  "data": null,
  "error": {
    "code": "element_not_found",
    "message": "Element not found: #SaveButton"
  }
}
```

## Bootstrap Sequence

When `--enable-win-automation` is specified:

1. Generate UUID for this connection
2. Create automation directory: `<session>/automation-<uuid>/`
3. Create subdirectories: `scripts/`
4. Write embedded `agent.ps1` to `scripts/agent.ps1`
5. Register drive mapping with RDPDR channel as `agent-automation`
6. Set up DVC channel (`AgentRdp::Automation`) via DrdynvcClient
7. Wait 2-3 seconds for Windows desktop stabilization
8. Set the PowerShell launch command on the remote clipboard:
   ```
   powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File "\\TSCLIENT\agent-automation\scripts\agent.ps1"
   ```
9. Send Win+R keystroke to open Run dialog
10. Wait 2000ms for the Run dialog to appear and grab focus (a foreground app such as a maximized browser or game can otherwise steal keystrokes typed too early)
11. Paste the launch command into the Run dialog via Ctrl+V, instead of typing it character-by-character
12. Press Enter
13. Wait for the DVC handshake, polling the shared DVC state without holding
    the automation lock. The window grows per attempt — 25s, 45s, 75s — and
    is extended once when the agent is visibly still starting (channel open
    without a handshake, or a `.ps1` read off the mapped drive in the last
    30s). Attempt 2+ skips Win+R if the previous launch is still starting.
14. Return success or timeout error. Worst case ≈ 5 minutes
    (`launch_and_wait_worst_case()`), which the CLI's `connect` and
    `automate restart` budgets are tested to exceed.

**Note**: RDPDR drive mapping is still used for bootstrapping (launching the agent), but all subsequent IPC uses DVC.

## PowerShell Agent

The agent script is embedded in the Rust binary via `include_str!()` and written to disk at runtime.

### Main Loop

1. Open DVC channel via `WTSVirtualChannelOpenEx`
2. Get file handle via `WTSVirtualChannelQuery`
3. Send handshake message
4. Loop:
   - ReadFile (blocking) until a fragment flagged CHANNEL_FLAG_LAST arrives,
     concatenating the data after each 8-byte CHANNEL_PDU_HEADER
   - Parse JSON request
   - Dispatch to command handler
   - Build response
   - WriteFile to send response
5. On error or channel close, exit gracefully

### Ref Mapping

For `@ref` selectors, the agent maintains a hashtable mapping ref numbers to `AutomationElement` objects:

- Refs are always assigned during snapshot (no `--refs` flag needed)
- On each `snapshot` command, the ref map is cleared and rebuilt
- Refs are assigned incrementally during tree traversal (depth-first)
- Ref 1 is always the root element
- Refs are only valid until the next snapshot
- Refs are displayed with "e" prefix in output: `ref=e123`
- Both `@e123` and `@123` work as selectors (e prefix recommended)

### Snapshot Filtering

The snapshot command supports filtering options (similar to agent-browser):

| Flag | Name | Description |
|------|------|-------------|
| `-i` | `--interactive` | Include only interactive elements (buttons, inputs, focusable) |
| `-c` | `--compact` | Remove empty structural elements (Pane, Group, Custom) |
| `-d N` | `--depth N` | Limit tree depth to N levels (default: 10) |
| `-s SEL` | `--selector SEL` | Scope to a specific element via selector |

**Interactive elements** are those that:
- Have `IsKeyboardFocusable = true`
- Support interactive patterns: Invoke, Value, Toggle, SelectionItem, ExpandCollapse, RangeValue, Scroll

**Compact mode** removes elements that:
- Have a structural role (Pane, Group, Custom, Document, ScrollBar, Thumb)
- Have no name, no value, and no children

### Selector Resolution

| Prefix | Type | Resolution |
|--------|------|------------|
| `@eN` or `@N` | Reference | Hashtable lookup by ref number |
| `#id` | AutomationId | PropertyCondition on AutomationIdProperty |
| `.class` | ClassName | PropertyCondition on ClassNameProperty |
| `~pattern` | Pattern | Name property with wildcard matching |
| (none) | Name | PropertyCondition on NameProperty (exact match) |

### Pattern-based Commands

Commands use native Windows UI Automation patterns for reliable interaction:

| Command | UI Automation Pattern | Use Case |
|---------|----------------------|----------|
| `invoke` | InvokePattern.Invoke() | Buttons, hyperlinks, menu items |
| `select` | SelectionItemPattern.Select() | List items, radio buttons |
| `toggle` | TogglePattern.Toggle() | Checkboxes |
| `expand` | ExpandCollapsePattern.Expand() | Menus, tree items, combo boxes |
| `collapse` | ExpandCollapsePattern.Collapse() | Menus, tree items, combo boxes |
| `context_menu` | Focus + Shift+F10 (keyboard) | Opening context menus |
| `fill` | ValuePattern.SetValue() | Text fields |

### Non-UI Commands

| Command | Purpose |
|---------|---------|
| `run` / `run_poll` | Launch a process; wait, or stream its output incrementally. The child script is assembled by `New-ChildScript`: a prelude (UTF-8 console output without BOM, guarded so a console-less child cannot die on it; `$ProgressPreference='SilentlyContinue'`; `$ErrorActionPreference='Stop'`; `$env:AGENT_RDP_AGENT_PID`), then the user script inside `try { … } catch { … }` whose catch prints the exception chain as plain text to stderr and exits 1, with `$LASTEXITCODE` preserved for native commands. Scripts with a `param` block or `using` statements (detected with the PowerShell parser, `Get-ChildScriptShape`) get the prelude only; a script with parse errors is refused before launch with `parse_error` (line/column per error) when the child shell is Windows PowerShell — the agent's own parser — and launched unwrapped for any other `shell`. Every reply carries `command_line` (the command plus each argument as a single-quoted literal, i.e. exactly what the child parses) and every error thrown by `Invoke-Run` ends with the same text. Waited runs add `finished_unix`; a finished stream poll adds it too. `wait` takes precedence over `stream`. A detached launch reports `early_exit`/`exit_code` if the child is gone ~250ms after start. Streamed output is captured to files under `%TEMP%\agent-rdp-run` and read back by offset (a read failure is reported in-band and retried next poll); finished entries stay pollable for 10 minutes. Remaining CLIXML on stderr (parse errors, unwrapped scripts) is reduced by the daemon to the text of error/warning records, including `<Obj>` records' `ToString`. A `run` carrying `idempotency_key` uses it as the request id (see Indeterminate Results) |
| `file_write_chunk` | Append one base64 chunk to a remote file; verifies SHA-256 on the last chunk |
| `file_read_chunk` | Read a byte range of a remote file as base64 |
| `file_stat` | Existence, size, SHA-256, `modified_unix` (last write, UTC) and `now_unix` (the remote clock) of a remote path — the daemon derives the file's age from the two without assuming the hosts' clocks agree. Exposed as `agent-rdp file stat` |
| `query_result` | Look up the recorded outcome of an earlier request by id |
| `status` | Agent pid, version, capabilities, `log_path` (the agent's own log, pulled by `agent-rdp diagnose`) |

File transfer is byte-oriented on both sides (`[IO.File]` plus base64 on the
wire): text-mode I/O would re-encode the payload through the console codepage
and corrupt anything that isn't ASCII.

**Why patterns instead of mouse clicks?**
- **Reliability**: Patterns interact directly with the control, not via coordinates
- **Speed**: No need to calculate positions or simulate mouse movement
- **Consistency**: Works regardless of window position or overlapping elements

## Timeout and Error Handling

### Response Timeout

Default: 10 seconds, awaited on a oneshot channel. Commands that carry their
own budget get that budget plus the default instead — `run --wait` and
`wait-for` would otherwise be cut off while the agent was still working. After
3 consecutive failures the channel is treated as dead and the error suggests
reconnecting.

### Transient vs Fatal Transport Errors (Agent)

`Read-DvcMessage` treats only a fixed set of Win32 errors as the channel
being gone — 6 `ERROR_INVALID_HANDLE`, 109 `ERROR_BROKEN_PIPE`, 232
`ERROR_NO_DATA`, 233 `ERROR_PIPE_NOT_CONNECTED`, 1167
`ERROR_DEVICE_NOT_CONNECTED` — and reports them with a `DVC_FATAL:` message
prefix, which is the only thing the main loop exits on. Every other read
failure (a CPU-starved host produces `ERROR_OPERATION_ABORTED`,
`ERROR_NO_SYSTEM_RESOURCES`, short and zero-byte reads) is logged, the
partial message is dropped, the loop pauses 200ms and reads again; more than
20 in a row is treated as fatal. A fragment without `CHANNEL_FLAG_FIRST`
arriving while nothing is accumulated is the tail of a dropped message and is
skipped, so the reader resynchronises instead of failing the next JSON parse.
Write failures are always fatal (`DVC_FATAL:` too): a reply that cannot be
sent is a reply the daemon times out on anyway.

### Surviving a Reconnect (Agent + Daemon)

The agent does not exit when its channel dies. It re-opens every 3s for
`$script:ReconnectWindowSec` (600s, measured per outage), because the Windows
session outlives the RDP transport: a reconnect inside that window finds the
agent already there, and `launch_and_wait(.., adopt_first: true)` adopts it as
soon as a handshake appears, waiting up to `SURVIVOR_WAIT` (6s) before typing
Win+R. Adoption sets `AutomationStatus.adopted` and, unlike a launch, does not
increment `total_launches`. A survivor is checked against
`expected_build_id()` - a hash over every embedded script, not just
`$script:Version` - so a library-only change with no version bump still
replaces it; a mismatch is sent `shutdown`. The build id is passed to the
launched agent as `-BuildId` and echoed back in its handshake, since the agent
cannot compute it about itself.

Two agents can therefore be alive at once — a survivor reattaching while a
freshly launched one starts. `dvc_channel.rs` keeps whichever opened the
channel first and answers the other's handshake with a `shutdown` request;
`close()` is id-guarded so the rejected agent exiting cannot clear the live
agent's handshake or wake the supervisor. This is also why the channel is
registered with `DrdynvcClient::with_listener` rather than
`with_dynamic_channel`: the latter hands its processor out exactly once per
RDP session (`OnceListener::create` is a `take`), so from the IronRDP 0.17
upgrade until 0.7.17 a relaunched agent was answered with `NO_LISTENER` and
neither the supervisor nor `automate restart` could reattach within a session.
`automate restart` now also asks the running agent to exit first, so the
replacement is the agent that survives the rule above.

### Relaunch Supervisor (Daemon)

A per-session supervisor task (spawned by `connect`, bound to that
session's generation and DVC state) owns every automatic launch. It wakes
on two things:

- the DVC processor's `close()` callback (the agent process ended while
  the RDP session is alive), which arms an immediate retry;
- a 30s tick, which checks whether a scheduled retry is due.

Every launch — `connect`'s bootstrap, `automate restart`, and the
supervisor's — goes through `launch_guarded`, which sets
`relaunch_in_flight` for its duration and records the outcome: success
clears `last_error`; failure stores it, increments `launch_failures` and
schedules `next_retry_at` with backoff (60s, 120s, 240s, then 300s), or
gives up after `MAX_LAUNCH_FAILURES` (6) until a manual restart resets the
count. A retry is armed **only** by a recorded failure or a close, never by
a bootstrap in progress, so the tick cannot double a `connect` that is still
waiting for its first handshake. The pure `should_retry` decision also
requires: RDP session alive, automation enabled, no handshake, no agent
visibly starting (`agent_is_starting`), the `RelaunchBudget` (3 attempts
per 10 minutes, each attempt being one `launch_and_wait` of up to
`LAUNCH_ATTEMPTS` typed launches; a refusal reschedules a minute out), and **no input sent by this
daemon for `RETRY_INPUT_QUIET` (120s)** — a launch types Win+R and pastes
into the focused window, which must not happen under an operator's
`keyboard`/`mouse` sequence. `AGENT_RDP_NO_AUTO_RELAUNCH=1` in the daemon's
environment disables automatic launches (captured into `AutomationState`
at `initialize()`; `next_retry_secs` is then never reported). The
bootstrap's own keystrokes do not count as input: `launch_agent` restores
the pre-launch `input_activity` mark afterwards.

IronRDP also fires `close()` on an ordinary disconnect, which is why the
supervisor is scoped to one session and exits when `cleanup()` drops the
DVC state. `automate status` reports `relaunches`, `last_error` and
`next_retry_secs`, and is answered from daemon state alone
(`offline_status`) while the agent cannot be reached.

`relaunches` counts only the supervisor's and `automate restart`'s launches,
and `initialize()` zeroes it on every `connect` — so on its own it cannot
distinguish "the agent has been up all day" from "the session was rebuilt an
hour ago". `total_launches` (incremented in `record_launch_outcome`, so it
covers `connect`'s bootstrap too, and deliberately *not* reset by
`initialize()`/`cleanup()`) answers that. It resets only when a `connect`
targets a different `host:port`, since one count spanning two machines would
be worse than none. `status` also carries `daemon_version` and, filled in
CLI-side, `cli_version`, so one call answers which three versions are running.

Every launch types Win+R and pastes into the Run dialog, which takes
foreground on the remote desktop. That is unavoidable once the previous agent
is gone — a dead transport invalidates its channel handle, and the read error
is in `DvcFatalWin32Errors`, so the agent exits on its own. The mitigation is
to need fewer reconnects: the daemon keeps idle sessions alive with a Refresh
Rect PDU every `keep_alive_secs` (default 45, `0` disables). Refresh Rect
carries no input, focus or lock-key semantics. A Synchronize input event would
also have kept the path warm, but it makes the server adopt the lock-key state
it carries, which would switch Num Lock off on the remote desktop every tick.

### Indeterminate Results

A lost reply is not a failed action: the agent may have applied it and only the
acknowledgement went missing. The agent journals its last 64 responses by
request id, so on a timeout the daemon issues `query_result` and resolves the
outcome — returning the real result if the request completed, or reporting that
it never ran (and is therefore safe to retry) if the agent has no record. Only
when the agent is still busy does an indeterminate result survive.

Because the agent's loop is strictly serial, a `query_result` sent while the
original command is still executing queues behind it and is answered once the
agent frees up.

The same journal makes retries idempotent. Before dispatching any request the
agent checks whether it has already answered that id: if so, and the request's
fingerprint (SHA-256 of command + params) matches the recorded one, it replays
the journaled result — with `replayed: true` added to the data — instead of
executing again; a matching id with a different fingerprint is refused with
`idempotency_key_reused`. The daemon's own ids are random, so this only fires
for a caller-supplied `idempotency_key` on `run` (which becomes the request id
verbatim; the daemon rejects a key that is still in flight). The fingerprint
leaves out `timeout_ms`, so a retry with a longer budget is the same request.

The journal has two tiers. Every result is kept in memory (64 entries,
FIFO). A `run` that carried an `idempotency_key` (the dispatch loop's
`$keyed`) is also written to disk if it succeeded or its child process was
started (`$script:LastRunLaunched`; a parse error or a `Process.Start`
failure had no side effect and is kept in memory only), because that is the one case where the id
outlives the agent process: `%LOCALAPPDATA%\agent-rdp\journal\<sha256(id)>.json`
(`Write-PersistedJournalEntry`), one JSON object with `id`, `success`,
`data` (stdout/stderr capped at 512K characters each, `journal_truncated`
when cut),
`error`, `fingerprint`, `at`, `at_unix`. Written UTF-8 without BOM via
`[IO.File]::WriteAllText` to a temp file and `[IO.File]::Move`d into place
— write-once, so the first execution's record wins over a racing writer.
Pruned on write to 256 entries / 7 days. Lookups (`Get-JournalEntry
-IncludeDisk`, `query_result`) fall through to disk and restore the object
through `ConvertTo-Hashtable` (PowerShell 5.1 has no `-AsHashtable`), so a
restored entry replays exactly like a live one: `replayed: true` plus
`replayed_at_unix`, and a replayed *error* carries a "replayed from the
journal … use a new key" suffix. An unreadable file is treated as unknown
and the request executes. Not `%TEMP%`: on a session host that is
per-logon and deleted at logoff. Per Windows account and per host — a
temporary profile or another farm member starts empty.

### Error Codes

| Code | Description |
|------|-------------|
| `element_not_found` | Selector didn't match any element |
| `stale_ref` | @ref number not in current snapshot |
| `command_failed` | UI Automation operation failed |
| `idempotency_key_reused` | Request id already journaled for a different command; not executed |
| `parse_error` (`command_failed` from the daemon's view) | `run` text does not parse as Windows PowerShell; nothing was launched. Message lists line/column per error and ends with the command line as assembled |
| `timeout` | Operation exceeded timeout |
| `channel_closed` | DVC channel was closed |
| `transfer_verification_failed` | `file push`: the file the agent assembled does not match the bytes sent (or it reported no hash at all). The transfer is staged in a sidecar and swapped in only after it verifies, so the destination was left untouched |
| `file_changed_during_transfer` | `file pull`: the file was rewritten between the hash and the read on every attempt (3 attempts, 150ms/300ms apart, for files under 8MB). Distinct from `internal_error` so a caller polling a file its producer rewrites can retry |
| `unknown` | Unspecified error |

## Cleanup

### On Disconnect (Daemon)

The daemon removes the entire automation directory on disconnect or shutdown.

### On Channel Close (Agent)

When the DVC channel is gone (a `DVC_FATAL:` error), the PS agent:
1. Logs error details
2. Closes file handle and WTS handle
3. Exits gracefully

The daemon then relaunches it if the RDP session is still alive (see
Relaunch Supervisor above).

## Key Implementation Files

| File | Purpose |
|------|---------|
| `automation/mod.rs` | Module exports |
| `automation/bootstrap.rs` | Agent launch sequence |
| `automation/dvc_channel.rs` | DVC processor implementation |
| `automation/dvc_ipc.rs` | IPC client (request/response handling) |
| `automation/scripts/agent.ps1` | Embedded PowerShell agent |
| `automation/scripts/lib/dvc.ps1` | WTS API P/Invoke for DVC |
| `automation/scripts/lib/actions.ps1` | Command implementations, file transfer, result journal |
| `handlers/automate.rs` | CLI command dispatch, per-request timeouts, indeterminate resolution |
| `handlers/file_transfer.rs` | Chunked file push/pull with SHA-256 verification |
| `rdp_session.rs` | DrdynvcClient setup and DVC command handling |

## Limitations

1. **Single Session**: One automation agent per RDP connection
2. **Serial execution**: One command at a time; a long `run --wait` blocks every
   other automate command until it returns
3. **UAC**: Cannot automate elevated (admin) windows from non-elevated context
4. **WebViews**: Cannot access content inside WebView controls (Edge, Electron apps)
5. **Bootstrap via RDPDR**: Initial agent launch still uses drive mapping — but
   the agent must never *itself* read `\\TSCLIENT\...`. Drive I/O is serviced
   by the same frame-processor task that carries this DVC channel, so the agent
   would be waiting on a reply that cannot be produced until it stops waiting.
   Use the `file_*` commands instead.
6. **Journal scope**: the persistent idempotency journal is per Windows
   account and per host. A temporary profile (`C:\Users\TEMP`) is discarded
   at logoff, and an RDS farm may place the next logon on another member;
   both look like "unknown key" and the command executes again.
7. **Launching the agent disturbs the desktop**: a launch types Win+R there,
   so any other automation sharing that interactive session can lose
   foreground at that moment. A reconnect within the agent's ~10 minute
   reconnect window adopts the running agent instead and types nothing;
   keep-alive makes the drop itself rarer; `connect --defer-agent` withholds
   the launch until you ask. A launch that does have to happen cannot be made
   invisible.
8. **The survivor depends on the Windows session**: a policy that ends
   disconnected sessions after N minutes takes the agent with it, and the
   next connect launches a new one.
9. **A launched agent with no client waits the full 10 minutes before
   exiting**: previously it gave up after three tries (~6s). If the daemon
   that launched it is gone for good (killed, or the CLI's watchdog ended the
   connect before a handshake), the process sits on the desktop for the rest
   of that window rather than exiting quickly - nothing else cleans it up.
10. **Keep-alive silence is treated as death**: three keep-alive periods with
   no inbound PDU at all (`KEEP_ALIVE_MISSED_LIMIT`) end the session, because
   a server whose TCP stack still ACKs but whose RDP service is gone answers
   nothing and trips no socket-level timeout. A server that never answers a
   Refresh Rect on an idle desktop would be misjudged; `AGENT_RDP_NO_SILENCE_DROP=1`
   disables the verdict while keeping the traffic, and `--keep-alive-secs 0`
   disables both. Strikes are counted per send, so a local stall of any length
   (RDPDR I/O blocks this loop) adds at most one.
