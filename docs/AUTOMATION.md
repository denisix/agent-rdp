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
  "version": "1.3.0",
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
13. Wait for DVC handshake message
14. Return success or timeout error

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
| `run` / `run_poll` | Launch a process; wait, or stream its output incrementally. Streamed output is captured to files under `%TEMP%\agent-rdp-run` and read back by offset, so nothing is lost if the process exits between polls; finished entries stay pollable for 10 minutes. Child stderr arrives CLIXML-serialized (PowerShell's behavior on a redirected stderr); the daemon reduces it to the text of error/warning records and drops progress noise |
| `file_write_chunk` | Append one base64 chunk to a remote file; verifies SHA-256 on the last chunk |
| `file_read_chunk` | Read a byte range of a remote file as base64 |
| `file_stat` | Existence, size and SHA-256 of a remote path |
| `query_result` | Look up the recorded outcome of an earlier request by id |
| `status` | Agent pid, version, capabilities |

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

### Error Codes

| Code | Description |
|------|-------------|
| `element_not_found` | Selector didn't match any element |
| `stale_ref` | @ref number not in current snapshot |
| `command_failed` | UI Automation operation failed |
| `timeout` | Operation exceeded timeout |
| `channel_closed` | DVC channel was closed |
| `unknown` | Unspecified error |

## Cleanup

### On Disconnect (Daemon)

The daemon removes the entire automation directory on disconnect or shutdown.

### On Channel Close (Agent)

When the DVC channel closes or errors, the PS agent:
1. Logs error details
2. Closes file handle and WTS handle
3. Exits gracefully

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
