#Requires -Version 5.1
# agent-rdp Windows UI Automation Agent
# Communicates via Dynamic Virtual Channel (DVC) with the Rust daemon

# BasePath kept for reference/logging (RDPDR drive still mapped for future file transfer).
# BuildId is only ever read back out as $BuildId in a cmdlet argument
# (-BuildId $BuildId below), which this rule does not recognise as a use.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', 'BasePath')]
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', 'BuildId')]
param(
    [string]$BasePath = "\\TSCLIENT\agent-automation",
    # Hash of every script the launching daemon deployed, echoed back in the
    # handshake. Lets a later connect tell this agent apart from one left
    # over from a daemon whose scripts have since changed, even when
    # $script:Version did not move - library-only changes are exactly the
    # case a version string alone would miss.
    [string]$BuildId = ""
)

# ============ SETUP ============

# Set window title for easy identification
$Host.UI.RawUI.WindowTitle = "agent-rdp automation"

# Every `run` child shares this console, so setting it once here means the
# children inherit UTF-8 and their own prelude has nothing left to change.
# The agent never writes to stdout itself; this is purely for what it starts.
try { [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false) } catch {}

# Load UI Automation assemblies
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms

# Global state
$script:RefMap = @{}  # ref number -> AutomationElement mapping
$script:SnapshotId = $null
$script:Version = "1.8.0"  # survives a transport drop and is adopted by the next connect; shutdown command
# Local log path on Windows machine (RDPDR not used for logging anymore)
$script:LocalLogPath = "$env:TEMP\agent-rdp-automation.log"
$script:DvcHandle = [IntPtr]::Zero
# Set by the `shutdown` command: tells the outer loop this exit was asked for
# and must not be retried.
$script:ShutdownRequested = $false
# How long to keep trying to re-open the channel after it dies, measured from
# the first failure. The client going away is the common case - a dropped
# transport, or a laptop closing - and the same Windows session is still there
# when it comes back. Staying alive across that window is what lets the next
# connect adopt this agent instead of typing Win+R on the desktop.
$script:ReconnectWindowSec = 600
$script:ReconnectDelaySec = 3
# Whether a client has connected since the last channel failure; resets the
# reconnect window so it is measured per outage, not per process.
$script:HandshakeSinceFailure = $false

# ============ LOGGING ============

function Write-Log {
    param([string]$Message, [string]$Level = "INFO")
    $timestamp = Get-Date -Format "yyyy-MM-dd HH:mm:ss.fff"
    $logEntry = "[$timestamp] [$Level] $Message"

    # Write to local log only (reliable, on Windows machine)
    try {
        Add-Content -Path $script:LocalLogPath -Value $logEntry -ErrorAction SilentlyContinue
    } catch {}
}

# ============ LOAD LIBRARY FILES ============

$scriptDir = $PSScriptRoot
. "$scriptDir\lib\types.ps1"
. "$scriptDir\lib\snapshot.ps1"
. "$scriptDir\lib\selectors.ps1"
. "$scriptDir\lib\actions.ps1"
. "$scriptDir\lib\dvc.ps1"

# ============ MAIN LOOP ============

function Start-Agent {
    Write-Log "Agent starting with DVC transport"
    Write-Log "Local log path: $script:LocalLogPath"
    Write-Log "BasePath (for reference): $BasePath"

    # Open DVC channel
    Write-Log "Opening DVC channel..."
    try {
        $script:DvcHandle = Open-DvcChannel
        Write-Log "DVC channel opened successfully"
    } catch {
        Write-Log "Failed to open DVC channel: $($_.Exception.Message)" "ERROR"
        throw
    }

    # Send handshake
    $capabilities = @(
        "snapshot", "click", "select", "toggle", "expand", "collapse",
        "context_menu", "focus", "get", "fill", "clear",
        "scroll", "window", "run", "run_poll", "wait_for", "status",
        "file_write_chunk", "file_read_chunk", "file_stat", "query_result",
        "persistent_journal", "shutdown", "survives_reconnect"
    )

    try {
        Send-DvcHandshake -Handle $script:DvcHandle -Version $script:Version -Capabilities $capabilities -BuildId $BuildId
        Write-Log "DVC handshake sent: version=$($script:Version)"
        # A client took us: the reconnect window starts over from the next
        # failure, so an agent that has served several sessions still gets a
        # full window each time rather than a shrinking one.
        $script:HandshakeSinceFailure = $true
    } catch {
        Write-Log "Failed to send handshake: $($_.Exception.Message)" "ERROR"
        throw
    }

    Write-Log "Entering main DVC loop"

    $loopCount = 0

    while ($true) {
        $loopCount++

        # Log every 1000 loops to show we're alive
        if ($loopCount % 1000 -eq 0) {
            Write-Log "Loop #$loopCount - still running via DVC..."
        }

        try {
            # Read request from DVC (with short timeout for polling)
            # Rust sends requests proactively, we just need to read them
            $request = Read-DvcMessage -Handle $script:DvcHandle

            if ($null -eq $request) {
                # No message available, continue polling
                continue
            }

            # Validate message type
            if ($request.type -ne "request") {
                Write-Log "Ignoring non-request message: type=$($request.type)" "WARN"
                continue
            }

            Write-Log "Processing DVC request: id=$($request.id), command=$($request.command)"

            # Idempotent replay. A request id we have already answered is a
            # retry whose reply was lost (or a caller reusing its
            # idempotency key on purpose): hand back the recorded result
            # instead of executing again. The fingerprint guards against the
            # other case - a key reused for a *different* command - which is
            # refused rather than silently answered with a stale result.
            $fingerprint = Get-RequestFingerprint -Command $request.command -Params $request.params
            # A run with a caller-chosen key is the one case where the id
            # outlives this process, so it is the one case that reaches the
            # disk tier (see the journal section of actions.ps1).
            $keyed = ($request.command -eq "run" -and [bool]$request.params.idempotency_key)
            if ($request.command -ne "query_result") {
                $replay = Get-JournalEntry -Id $request.id -IncludeDisk:$keyed
                if ($null -ne $replay) {
                    if ($replay.fingerprint -eq $fingerprint) {
                        Write-Log "Replaying journaled result for request $($request.id) (not re-executed)"
                        # Work on copies: the journal entry itself must stay
                        # as recorded, or every replay would stack another
                        # marker onto it.
                        $replayData = $replay.data
                        if ($replayData -is [hashtable]) {
                            $replayData = $replayData.Clone()
                            $replayData["replayed"] = $true
                            if ($null -ne $replay.at_unix) { $replayData["replayed_at_unix"] = $replay.at_unix }
                        }
                        $replayError = $replay.error
                        if ($replayError -is [hashtable]) {
                            $replayError = $replayError.Clone()
                            $when = ""
                            if ($null -ne $replay.at_unix) {
                                $epoch = [datetime]::new(1970, 1, 1, 0, 0, 0, [System.DateTimeKind]::Utc)
                                $when = " at " + $epoch.AddSeconds([double]$replay.at_unix).ToString("yyyy-MM-ddTHH:mm:ssZ")
                            }
                            $replayError["message"] = "$($replayError.message) (replayed from the journal: this idempotency key already ran$when; use a new key to run it again)"
                        }
                        Send-DvcResponse -Handle $script:DvcHandle -Id $request.id -Success $replay.success -Data $replayData -ErrorInfo $replayError
                    } else {
                        Write-Log "Request id $($request.id) reused for a different command; refusing" "WARN"
                        $reuseError = @{
                            code = "idempotency_key_reused"
                            message = "Request id '$($request.id)' was already used for a different command; pick a new idempotency key"
                        }
                        Send-DvcResponse -Handle $script:DvcHandle -Id $request.id -Success $false -Data $null -ErrorInfo $reuseError
                    }
                    continue
                }
            }

            $responseData = $null
            $responseError = $null
            $success = $true

            try {
                $responseData = switch ($request.command) {
                    "snapshot"     { Invoke-Snapshot -Params $request.params }
                    "click"        { Invoke-Click -Params $request.params }
                    "select"       { Invoke-Select -Params $request.params }
                    "toggle"       { Invoke-Toggle -Params $request.params }
                    "expand"       { Invoke-Expand -Params $request.params }
                    "collapse"     { Invoke-Collapse -Params $request.params }
                    "context_menu" { Invoke-ContextMenu -Params $request.params }
                    "focus"        { Invoke-Focus -Params $request.params }
                    "get"          { Invoke-Get -Params $request.params }
                    "fill"         { Invoke-Fill -Params $request.params }
                    "clear"        { Invoke-Clear -Params $request.params }
                    "scroll"       { Invoke-Scroll -Params $request.params }
                    "window"       { Invoke-Window -Params $request.params }
                    "run"          { Invoke-Run -Params $request.params }
                    "run_poll"     { Invoke-RunPoll -Params $request.params }
                    "wait_for"     { Invoke-WaitFor -Params $request.params }
                    "status"       { Get-AgentStatus }
                    "file_write_chunk" { Invoke-FileWriteChunk -Params $request.params }
                    "file_read_chunk"  { Invoke-FileReadChunk -Params $request.params }
                    "file_stat"        { Invoke-FileStat -Params $request.params }
                    "query_result"     { Get-JournaledResult -Params $request.params }
                    "shutdown"         {
                        # Answer first, exit after: the daemon waits for the
                        # channel to close as its signal that the old agent
                        # really is gone before launching a replacement.
                        $script:ShutdownRequested = $true
                        @{ stopping = $true }
                    }
                    default        { throw "Unknown command: $($request.command)" }
                }
                Write-Log "Command succeeded: $($request.command)"
            } catch {
                Write-Log "Command failed: $($_.Exception.Message)" "ERROR"
                $success = $false
                $responseError = @{
                    code = "command_failed"
                    message = $_.Exception.Message
                }
            }

            # Journal before sending: if the send fails or the reply is lost
            # in transit, the result is still here to be queried back.
            # `query_result` itself is not journaled - it is a lookup, and
            # recording lookups would evict the results being looked up.
            # A keyed run reaches the disk tier if it succeeded or if a child
            # process was started (a timeout after launch may have had side
            # effects; a parse error or start failure had none and must not
            # be replayed for a week).
            $persist = $keyed -and ($success -or [bool]$script:LastRunLaunched)
            if ($request.command -ne "query_result") {
                Add-JournaledResult -Id $request.id -Success $success -Data $responseData -ErrorInfo $responseError -Fingerprint $fingerprint -Persist:$persist
            }

            # Send response via DVC
            try {
                Send-DvcResponse -Handle $script:DvcHandle -Id $request.id -Success $success -Data $responseData -ErrorInfo $responseError
                Write-Log "Response sent for request $($request.id)"
                if ($script:ShutdownRequested) {
                    Write-Log "Shutdown requested; exiting after replying"
                    return
                }
            } catch {
                Write-Log "Failed to send response: $($_.Exception.Message)" "ERROR"
                # If we can't send response, channel may be dead
                throw
            }

        } catch {
            $errorMsg = $_.Exception.Message
            Write-Log "DVC error: $errorMsg" "ERROR"

            if ($script:ShutdownRequested) {
                # Asked to exit, and then failed to tell the daemon so (the
                # send itself is what can throw here). Exiting is still the
                # right call - staying up after a shutdown request, even on a
                # failed reply, means never leaving when asked to.
                Write-Log "Shutdown requested; exiting despite the send failure" "WARN"
                return
            }

            # Only an explicitly fatal transport error ends the agent. The
            # old substring test ("Win32 error" / "channel") also matched
            # every transient read error a CPU-starved host produces, so the
            # agent exited under load and the daemon reported channel_closed.
            if ($errorMsg.StartsWith($script:DvcFatalPrefix)) {
                Write-Log "DVC channel is gone" "WARN"
                throw
            }

            # For other errors, try to continue
            Start-Sleep -Milliseconds 100
        }
    }
}

# ============ CLEANUP ============

function Stop-Agent {
    Write-Log "Stopping agent..."

    if ($script:DvcHandle -ne [IntPtr]::Zero) {
        try {
            Close-DvcChannel
            Write-Log "DVC channel closed"
        } catch {
            Write-Log "Error closing DVC channel: $($_.Exception.Message)" "WARN"
        }
        $script:DvcHandle = [IntPtr]::Zero
    }
}

# ============ ENTRY POINT ============

# Handle clean shutdown
$exitHandler = {
    Stop-Agent
}
Register-EngineEvent -SourceIdentifier PowerShell.Exiting -Action $exitHandler | Out-Null

# Keep re-opening the channel while the client is away.
#
# The channel dies whenever the RDP transport does, and the agent used to give
# up after three attempts two seconds apart - always losing the race against a
# reconnect, so every reconnect had to launch a fresh agent by typing Win+R on
# the remote desktop. The Windows session outlives the transport, so this
# process can too: staying available for a few minutes turns the common
# reconnect into one nobody sitting at that desktop notices.
#
# A session that is ended for real (logoff, or a policy that closes
# disconnected sessions) takes this process with it regardless.
$attempt = 0
$firstFailure = $null

while ($true) {
    $attempt++
    try {
        Write-Log "=== Agent process starting (PID: $PID, attempt: $attempt) ==="
        Start-Agent
        # Start-Agent returns only when a shutdown was requested.
        Write-Log "Agent exiting normally"
        Stop-Agent
        exit 0
    } catch {
        Write-Log "Channel error (attempt $attempt): $($_.Exception.Message)" "ERROR"
        Write-Log $_.ScriptStackTrace "ERROR"

        Stop-Agent

        if ($script:HandshakeSinceFailure) {
            $script:HandshakeSinceFailure = $false
            $firstFailure = Get-Date
        } elseif ($null -eq $firstFailure) {
            $firstFailure = Get-Date
        }
        $waited = ((Get-Date) - $firstFailure).TotalSeconds
        if ($waited -ge $script:ReconnectWindowSec) {
            Write-Log "No client for $([int]$waited)s; agent exiting" "WARN"
            exit 1
        }

        # One line per attempt for ten minutes would bury the log.
        if ($attempt % 10 -eq 0) {
            Write-Log "Still waiting for a client ($([int]$waited)s of $($script:ReconnectWindowSec)s)"
        }
        Start-Sleep -Seconds $script:ReconnectDelaySec
    }
}
