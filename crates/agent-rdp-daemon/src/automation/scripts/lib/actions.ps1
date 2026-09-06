# actions.ps1 - All automation action functions using native UI Automation patterns

function Invoke-Click {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    # Verify element still exists before interacting
    try {
        $null = $element.Current.ProcessId
    } catch {
        throw "Element no longer exists (window may have closed)"
    }

    # Check if element is enabled
    if (-not $element.Current.IsEnabled) {
        throw "Element is disabled"
    }

    # Use mouse click via SendInput for reliable, non-blocking interaction.

    $doubleClick = if ($null -ne $Params.double_click) { $Params.double_click } else { $false }

    # Get the element's bounding rectangle
    $rect = $element.Current.BoundingRectangle
    if ($rect.IsEmpty -or [double]::IsInfinity($rect.X)) {
        throw "Element has no valid bounding rectangle (may be off-screen or invisible)"
    }

    $centerX = [int]($rect.X + $rect.Width / 2)
    $centerY = [int]($rect.Y + $rect.Height / 2)

    # Add Win32 mouse input type if not already defined
    if (-not ([System.Management.Automation.PSTypeName]'InvokeMouse').Type) {
        Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public class InvokeMouse {
    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll")]
    public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, UIntPtr dwExtraInfo);

    public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    public const uint MOUSEEVENTF_LEFTUP = 0x0004;
}
"@
    }

    # Move cursor to element center
    $null = [InvokeMouse]::SetCursorPos($centerX, $centerY)
    Start-Sleep -Milliseconds 30

    # Perform click(s)
    [InvokeMouse]::mouse_event([InvokeMouse]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 30
    [InvokeMouse]::mouse_event([InvokeMouse]::MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero)

    if ($doubleClick) {
        Start-Sleep -Milliseconds 50
        [InvokeMouse]::mouse_event([InvokeMouse]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 30
        [InvokeMouse]::mouse_event([InvokeMouse]::MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
    }

    $method = if ($doubleClick) { "double_click" } else { "click" }

    return @{
        clicked = $true
        method = $method
        x = $centerX
        y = $centerY
    }
}

function Invoke-Expand {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    try {
        $expandPattern = $element.GetCurrentPattern([System.Windows.Automation.ExpandCollapsePattern]::Pattern)
        if ($expandPattern) {
            $expandPattern.Expand()
            return @{ expanded = $true; method = "ExpandCollapsePattern" }
        }
    } catch {
        throw "Element does not support ExpandCollapsePattern: $($_.Exception.Message)"
    }

    throw "Element does not support ExpandCollapsePattern"
}

function Invoke-Collapse {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    try {
        $expandPattern = $element.GetCurrentPattern([System.Windows.Automation.ExpandCollapsePattern]::Pattern)
        if ($expandPattern) {
            $expandPattern.Collapse()
            return @{ collapsed = $true; method = "ExpandCollapsePattern" }
        }
    } catch {
        throw "Element does not support ExpandCollapsePattern: $($_.Exception.Message)"
    }

    throw "Element does not support ExpandCollapsePattern"
}

function Invoke-ContextMenu {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    # Get the element's bounding rectangle and calculate center point
    $rect = $element.Current.BoundingRectangle
    if ($rect.IsEmpty) {
        throw "Element has no bounding rectangle (may be off-screen or invisible)"
    }

    $centerX = [int]($rect.X + $rect.Width / 2)
    $centerY = [int]($rect.Y + $rect.Height / 2)

    # Add Win32 mouse input type if not already defined
    if (-not ([System.Management.Automation.PSTypeName]'ContextMenuMouse').Type) {
        Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public class ContextMenuMouse {
    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll")]
    public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, UIntPtr dwExtraInfo);

    public const uint MOUSEEVENTF_RIGHTDOWN = 0x0008;
    public const uint MOUSEEVENTF_RIGHTUP = 0x0010;
}
"@
    }

    # Move cursor to element center
    [ContextMenuMouse]::SetCursorPos($centerX, $centerY)
    Start-Sleep -Milliseconds 50

    # Perform right-click
    [ContextMenuMouse]::mouse_event([ContextMenuMouse]::MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 50
    [ContextMenuMouse]::mouse_event([ContextMenuMouse]::MOUSEEVENTF_RIGHTUP, 0, 0, 0, [UIntPtr]::Zero)

    return @{
        context_menu_opened = $true
        method = "mouse_right_click"
        x = $centerX
        y = $centerY
    }
}

function Invoke-Toggle {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    try {
        $togglePattern = $element.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
        if ($togglePattern) {
            $previousState = $togglePattern.Current.ToggleState

            # If a specific state is requested
            if ($null -ne $Params.state) {
                $targetState = if ($Params.state) {
                    [System.Windows.Automation.ToggleState]::On
                } else {
                    [System.Windows.Automation.ToggleState]::Off
                }

                # Toggle until we reach the target state (handles tri-state checkboxes)
                $maxAttempts = 3
                for ($i = 0; $i -lt $maxAttempts -and $togglePattern.Current.ToggleState -ne $targetState; $i++) {
                    $togglePattern.Toggle()
                    Start-Sleep -Milliseconds 50
                }
            } else {
                # Just toggle
                $togglePattern.Toggle()
            }

            return @{
                toggled = $true
                previous_state = $previousState.ToString()
                new_state = $togglePattern.Current.ToggleState.ToString()
                method = "TogglePattern"
            }
        }
    } catch {
        throw "Element does not support TogglePattern: $($_.Exception.Message)"
    }

    throw "Element does not support TogglePattern"
}

function Invoke-Focus {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    $element.SetFocus()

    return @{ focused = $true }
}

function Invoke-Get {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    $property = if ($Params.property) { $Params.property } else { "all" }
    $result = @{}

    if ($property -eq "all" -or $property -eq "name") {
        $result["name"] = $element.Current.Name
    }

    if ($property -eq "all" -or $property -eq "value") {
        $value = $null

        try {
            $valuePattern = $element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
            if ($valuePattern) {
                $value = $valuePattern.Current.Value
            }
        } catch {
            $value = $null
        }

        # Multiline edits (Notepad's editor, rich text controls) do not implement
        # ValuePattern, only TextPattern - without this fallback they report no
        # text at all and there is no way to read back what was typed.
        if ($null -eq $value) {
            try {
                $textPattern = $element.GetCurrentPattern([System.Windows.Automation.TextPattern]::Pattern)
                if ($textPattern) {
                    # -1 means the whole document.
                    $value = $textPattern.DocumentRange.GetText(-1)
                }
            } catch {
                $value = $null
            }
        }

        $result["value"] = $value
    }

    if ($property -eq "all" -or $property -eq "states") {
        $result["states"] = @(Get-ElementStates $element)
    }

    if ($property -eq "all" -or $property -eq "bounds") {
        $rect = $element.Current.BoundingRectangle
        $result["bounds"] = @{
            x = [int]$rect.X
            y = [int]$rect.Y
            width = [int]$rect.Width
            height = [int]$rect.Height
        }
    }

    return $result
}

function Invoke-Fill {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    $element.SetFocus()
    Start-Sleep -Milliseconds 50

    # Try ValuePattern first
    try {
        $valuePattern = $element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
        if ($valuePattern) {
            $valuePattern.SetValue($Params.text)
            return @{ filled = $true; text = $Params.text; method = "value_pattern" }
        }
    } catch {}

    # Fallback: select all and type
    [System.Windows.Forms.SendKeys]::SendWait("^a")
    Start-Sleep -Milliseconds 50

    # Escape special characters for SendKeys
    $escaped = $Params.text -replace '([+^%~(){}])', '{$1}'
    [System.Windows.Forms.SendKeys]::SendWait($escaped)

    return @{ filled = $true; text = $Params.text; method = "sendkeys" }
}

function Invoke-Clear {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    $element.SetFocus()
    Start-Sleep -Milliseconds 50

    # Try ValuePattern first
    try {
        $valuePattern = $element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
        if ($valuePattern) {
            $valuePattern.SetValue("")
            return @{ cleared = $true; method = "value_pattern" }
        }
    } catch {}

    # Fallback: select all and delete
    [System.Windows.Forms.SendKeys]::SendWait("^a{DEL}")

    return @{ cleared = $true; method = "sendkeys" }
}

function Invoke-Select {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    # If no item name specified, select the element directly using SelectionItemPattern
    if (-not $Params.item) {
        try {
            $selectPattern = $element.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
            if ($selectPattern) {
                $selectPattern.Select()
                return @{ selected = $true; method = "SelectionItemPattern" }
            }
        } catch {
            throw "Element does not support SelectionItemPattern: $($_.Exception.Message)"
        }
        throw "Element does not support SelectionItemPattern"
    }

    # Item name specified - find and select within container
    # Try SelectionItemPattern first
    try {
        $itemCondition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::NameProperty, $Params.item)
        $item = $element.FindFirst(
            [System.Windows.Automation.TreeScope]::Descendants, $itemCondition)

        if ($item) {
            $selectPattern = $item.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
            if ($selectPattern) {
                $selectPattern.Select()
                return @{ selected = $true; item = $Params.item; method = "SelectionItemPattern" }
            }
        }
    } catch {}

    # Try ExpandCollapsePattern for combo boxes - expand first, then find item
    try {
        $expandPattern = $element.GetCurrentPattern([System.Windows.Automation.ExpandCollapsePattern]::Pattern)
        if ($expandPattern) {
            $expandPattern.Expand()
            Start-Sleep -Milliseconds 200

            # Now find and select the item
            $itemCondition = New-Object System.Windows.Automation.PropertyCondition(
                [System.Windows.Automation.AutomationElement]::NameProperty, $Params.item)
            $item = $element.FindFirst(
                [System.Windows.Automation.TreeScope]::Descendants, $itemCondition)

            if ($item) {
                # Try SelectionItemPattern first
                try {
                    $selectPattern = $item.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
                    if ($selectPattern) {
                        $selectPattern.Select()
                        return @{ selected = $true; item = $Params.item; method = "ExpandCollapse+SelectionItemPattern" }
                    }
                } catch {}

                # Try InvokePattern
                try {
                    $invokePattern = $item.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
                    if ($invokePattern) {
                        $invokePattern.Invoke()
                        return @{ selected = $true; item = $Params.item; method = "ExpandCollapse+InvokePattern" }
                    }
                } catch {}
            }
        }
    } catch {}

    throw "Could not select item: $($Params.item)"
}

function Invoke-Scroll {
    param($Params)

    $element = Find-Element -Selector $Params.selector
    if (-not $element) { throw "Element not found: $($Params.selector)" }

    # If to_child is specified, scroll until that child is visible
    if ($Params.to_child) {
        $child = Find-Element -Selector $Params.to_child
        if ($child) {
            try {
                $scrollItemPattern = $child.GetCurrentPattern([System.Windows.Automation.ScrollItemPattern]::Pattern)
                if ($scrollItemPattern) {
                    $scrollItemPattern.ScrollIntoView()
                    return @{ scrolled = $true; to_child = $Params.to_child }
                }
            } catch {}
        }
    }

    # Try ScrollPattern
    try {
        $scrollPattern = $element.GetCurrentPattern([System.Windows.Automation.ScrollPattern]::Pattern)
        if ($scrollPattern) {
            $amount = if ($Params.amount) { [int]$Params.amount } else { 1 }
            $direction = if ($Params.direction) { $Params.direction } else { "down" }

            for ($i = 0; $i -lt $amount; $i++) {
                switch ($direction) {
                    "up" { $scrollPattern.ScrollVertical([System.Windows.Automation.ScrollAmount]::SmallDecrement) }
                    "down" { $scrollPattern.ScrollVertical([System.Windows.Automation.ScrollAmount]::SmallIncrement) }
                    "left" { $scrollPattern.ScrollHorizontal([System.Windows.Automation.ScrollAmount]::SmallDecrement) }
                    "right" { $scrollPattern.ScrollHorizontal([System.Windows.Automation.ScrollAmount]::SmallIncrement) }
                }
            }

            return @{ scrolled = $true; direction = $direction; amount = $amount }
        }
    } catch {}

    throw "Element does not support scrolling"
}

function Invoke-Window {
    param($Params)

    $action = $Params.action

    if ($action -eq "list") {
        $windows = @()
        $root = [System.Windows.Automation.AutomationElement]::RootElement

        $condition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Window)

        $windowElements = $root.FindAll(
            [System.Windows.Automation.TreeScope]::Children, $condition)

        foreach ($win in $windowElements) {
            $rect = $win.Current.BoundingRectangle
            $windows += @{
                title = $win.Current.Name
                process_id = $win.Current.ProcessId
                bounds = @{
                    x = [int]$rect.X
                    y = [int]$rect.Y
                    width = [int]$rect.Width
                    height = [int]$rect.Height
                }
            }
        }

        return @{ windows = $windows }
    }

    # For other actions, find the window
    $window = $null
    if ($Params.selector) {
        # Use window-specific search for wildcard patterns
        if ($Params.selector -match '^~(.+)$') {
            $window = Find-WindowByPattern -Pattern $Matches[1]
        } else {
            $window = Find-Element -Selector $Params.selector
        }
    } else {
        # Get foreground window
        Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
}
"@
        $hwnd = [Win32]::GetForegroundWindow()
        $window = [System.Windows.Automation.AutomationElement]::FromHandle($hwnd)
    }

    if (-not $window) {
        throw "Window not found"
    }

    switch ($action) {
        "focus" {
            $window.SetFocus()
            return @{ action = "focus"; success = $true }
        }
        "maximize" {
            $windowPattern = $window.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern)
            if ($windowPattern) {
                $windowPattern.SetWindowVisualState([System.Windows.Automation.WindowVisualState]::Maximized)
                return @{ action = "maximize"; success = $true }
            }
        }
        "minimize" {
            $windowPattern = $window.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern)
            if ($windowPattern) {
                $windowPattern.SetWindowVisualState([System.Windows.Automation.WindowVisualState]::Minimized)
                return @{ action = "minimize"; success = $true }
            }
        }
        "restore" {
            $windowPattern = $window.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern)
            if ($windowPattern) {
                $windowPattern.SetWindowVisualState([System.Windows.Automation.WindowVisualState]::Normal)
                return @{ action = "restore"; success = $true }
            }
        }
        "close" {
            $windowPattern = $window.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern)
            if ($windowPattern) {
                $windowPattern.Close()
                return @{ action = "close"; success = $true }
            }
        }
    }

    throw "Window action failed: $action"
}

function Invoke-Run {
    param($Params)

    # Whether a child process was actually started by this run. The
    # dispatch loop persists a keyed *failure* only if it was - a parse
    # error or a Process.Start failure had no side effect and must not be
    # replayed for a week. Reset first, before anything here can throw.
    $script:LastRunLaunched = $false

    $command = $Params.command
    # Quote each argument as a PowerShell single-quoted string literal (doubling
    # embedded single quotes) so args containing spaces or quotes survive as one
    # token when re-parsed by the -Command string below.
    $commandArgs = if ($Params.args) {
        ($Params.args | ForEach-Object { "'" + ($_ -replace "'", "''") + "'" }) -join " "
    } else { "" }
    $wait = if ($null -ne $Params.wait) { $Params.wait } else { $false }
    $hidden = if ($null -ne $Params.hidden) { $Params.hidden } else { $false }
    $timeoutMs = if ($Params.timeout_ms) { [int]$Params.timeout_ms } else { 10000 }
    $shell = if ($Params.shell) { $Params.shell } else { "powershell.exe" }
    $stream = if ($null -ne $Params.stream) { $Params.stream } else { $false }

    # Hand the script to PowerShell base64-encoded rather than as a quoted
    # -Command string. Interpolating the command into `-Command "..."` meant any
    # double quote in it terminated that string early, so
    # `Test-Path "C:\Program Files\x"` was torn apart before PowerShell saw it,
    # and even backtick-escaping spaces still failed on literal parentheses like
    # `(x86)`. -EncodedCommand removes the command-line quoting layer entirely.
    $userScript = if ($commandArgs) { "$command $commandArgs" } else { $command }

    # Everything from here on reports the exact text the child was given.
    # A quoting accident (the caller's local shell expanding `$_`, an
    # argument re-tokenised on the way in) otherwise surfaces as a parse
    # error quoting text the caller never typed, with no way to see what
    # actually arrived.
    try {
        $result = Start-RunChild -UserScript $userScript -Shell $shell -Wait $wait -Hidden $hidden -TimeoutMs $timeoutMs -Stream $stream
        $result.command_line = $userScript
        return $result
    } catch {
        throw "$($_.Exception.Message)`nCommand line as executed by the agent: $userScript"
    }
}

function Start-RunChild {
    param(
        [string]$UserScript,
        [string]$Shell,
        [bool]$Wait,
        [bool]$Hidden,
        [int]$TimeoutMs,
        [bool]$Stream
    )

    $shape = Get-ChildScriptShape -UserScript $UserScript
    # The agent's parser is the child's parser when the child is Windows
    # PowerShell, so a script that does not parse here will not parse there
    # either: refuse it now, with the parser's own positions, instead of
    # launching a child whose only output is a CLIXML-wrapped parse error.
    # A different shell (pwsh 7 syntax the 5.1 parser rejects) is launched
    # unwrapped as before and reports its own errors.
    if ($shape.parse_errors.Count -gt 0 -and (Test-DefaultShell -Shell $Shell)) {
        throw "parse_error: the command does not parse as Windows PowerShell: $($shape.parse_errors -join '; ')"
    }

    $script = New-ChildScript -UserScript $UserScript -Shape $shape
    $encodedCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))

    # `--wait` wins over `--stream`, as documented. The reverse order returned
    # only a pid for `--wait --stream`, and every byte of output went to files
    # the caller was never told to poll.
    if ($Stream -and -not $Wait) {
        return Start-StreamedRun -Shell $Shell -EncodedCommand $encodedCommand -Hidden $Hidden
    }

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $Shell
    $startInfo.Arguments = "-NoProfile -EncodedCommand $encodedCommand"
    $startInfo.WorkingDirectory = $env:USERPROFILE
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $Wait
    $startInfo.RedirectStandardError = $Wait
    $startInfo.CreateNoWindow = $Hidden
    # Decode the child's output as UTF-8. Left unset, .NET decodes using the
    # console's OEM codepage (cp866 on a Russian-locale host), which is where
    # Cyrillic output came back as mojibake regardless of what the child sent.
    if ($Wait) {
        # `UTF8Encoding($false)`, not `[Text.Encoding]::UTF8`: the latter is
        # the BOM-emitting instance, and it is what the child writes with.
        $startInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
        $startInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    }

    $process = [System.Diagnostics.Process]::Start($startInfo)
    $script:LastRunLaunched = $true
    $startedUnix = Get-UnixNow

    if ($Wait) {
        # Use async reading to avoid deadlock when buffer fills
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()

        $exited = $process.WaitForExit($TimeoutMs)

        if (-not $exited) {
            try { $process.Kill() } catch {}
            throw "Process timed out after $TimeoutMs ms and was killed"
        }

        # Wait for async reads to complete (with short timeout since process exited)
        [void]$stdoutTask.Wait(5000)
        [void]$stderrTask.Wait(5000)

        return @{
            exit_code = $process.ExitCode
            stdout = $stdoutTask.Result
            stderr = $stderrTask.Result
            started_unix = $startedUnix
            # The freshness marker for this output: the caller can tell a
            # reply produced now from one replayed out of the journal, and
            # correlate it with `file stat` on the same clock.
            finished_unix = Get-UnixNow
        }
    } else {
        $launch = Get-LaunchResult -Process $process
        $launch.started_unix = $startedUnix
        return $launch
    }
}

# Seconds since the Unix epoch by this machine's clock - the same clock
# `file_stat` reports, so a run can be correlated with the files it wrote.
function Get-UnixNow {
    $epoch = [datetime]::new(1970, 1, 1, 0, 0, 0, [System.DateTimeKind]::Utc)
    return [int64]([datetime]::UtcNow - $epoch).TotalSeconds
}

# ---- child script assembly ----
#
# Everything below is source code for the *child* powershell.exe, shipped via
# -EncodedCommand. Single-quoted here-strings on purpose: nothing in them may
# be expanded in the agent's own scope (a double-quoted prelude once expanded
# `$ProgressPreference` here and handed the child `Continue='SilentlyContinue'`,
# a CommandNotFoundException on every single `run`).

# Console encoding first and guarded: forcing UTF-8 keeps non-ASCII output
# intact (the default OEM codepage turns Cyrillic into mojibake), but a child
# without a console can throw on the setter - with `Stop` already in effect
# that killed the child before the user's script ran. No BOM: the old
# `[Text.Encoding]::UTF8` wrote one into the first streamed chunk.
#
# Skipped when the console is already UTF-8, which it is once the agent sets
# its own (children share it). Assigning to `Console.OutputEncoding` tears
# down and rebuilds the host's stdout writer, and this line runs immediately
# before the user's first statement - the narrow suspect for a report of a
# waited run losing its first output line. Not a proven cause: nothing in the
# capture path drops data, and the same symptom is what `Get-FileHash` on a
# wildcard that matches nothing produces (no output, no error, exit 0).
#
# `$ErrorActionPreference='Stop'` is what makes the exit code mean something:
# without it a cmdlet that fails non-terminatingly (Add-Content to a locked
# file, Set-Content to a bad path) writes to the error stream and the process
# still exits 0. A script that wants continue-on-error semantics sets the
# preference back on its first line.
$script:ChildPrelude = @'
if ([Console]::OutputEncoding.CodePage -ne 65001) { try { [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false) } catch {} }
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
'@

$script:ChildWrapperHead = @'
try {
'@

# The catch writes the whole exception chain as plain text to stderr. Plain
# text never becomes CLIXML, and the inner-exception walk is where a COM
# error's real message lives (a 1C COM failure surfaces as a bare
# NullReferenceException at the top level). `exit N` in the user's script is
# flow control, not an exception, and passes through untouched; the
# `$LASTEXITCODE` line keeps a native command's exit code, which PowerShell
# would otherwise collapse to 1.
$script:ChildWrapperTail = @'

if ($LASTEXITCODE) { exit $LASTEXITCODE }
} catch {
    $agentRdpErr = $_
    $agentRdpLines = New-Object System.Collections.Generic.List[string]
    $agentRdpEx = $agentRdpErr.Exception
    if ($null -ne $agentRdpEx) {
        $agentRdpLines.Add('ERROR: ' + $agentRdpEx.GetType().FullName + ': ' + $agentRdpEx.Message)
        $agentRdpInner = $agentRdpEx.InnerException
        while ($null -ne $agentRdpInner) {
            $agentRdpLines.Add('  caused by ' + $agentRdpInner.GetType().FullName + ': ' + $agentRdpInner.Message)
            $agentRdpInner = $agentRdpInner.InnerException
        }
    } else {
        $agentRdpLines.Add('ERROR: ' + [string]$agentRdpErr)
    }
    if ($agentRdpErr.ErrorDetails -and $agentRdpErr.ErrorDetails.Message) {
        $agentRdpLines.Add('  details: ' + $agentRdpErr.ErrorDetails.Message)
    }
    if ($agentRdpErr.InvocationInfo -and $agentRdpErr.InvocationInfo.PositionMessage) {
        $agentRdpLines.Add($agentRdpErr.InvocationInfo.PositionMessage)
    }
    if ($agentRdpErr.ScriptStackTrace) {
        $agentRdpLines.Add($agentRdpErr.ScriptStackTrace)
    }
    try { [Console]::Error.WriteLine(($agentRdpLines -join [Environment]::NewLine)) } catch {}
    exit 1
}
'@

# How the user's text parses, decided once per run. A script with a param()
# block or `using` statements cannot live inside `try { }` - those must be
# the first statements of a script - so it gets the prelude only. Parse
# errors are reported as "line:col: message" for the caller to see; whether
# they stop the launch is the caller's decision (it depends on the shell).
function Get-ChildScriptShape {
    param([string]$UserScript)

    $shape = @{ wrappable = $true; parse_errors = @() }
    try {
        $tokens = $null
        $errors = $null
        $ast = [System.Management.Automation.Language.Parser]::ParseInput($UserScript, [ref]$tokens, [ref]$errors)
        if ($errors -and $errors.Count -gt 0) {
            $shape.wrappable = $false
            $shape.parse_errors = @($errors | ForEach-Object {
                "line $($_.Extent.StartLineNumber), column $($_.Extent.StartColumnNumber): $($_.Message)"
            })
        }
        if ($null -ne $ast -and $null -ne $ast.ParamBlock) { $shape.wrappable = $false }
        if ($null -ne $ast -and $ast.UsingStatements -and $ast.UsingStatements.Count -gt 0) { $shape.wrappable = $false }
    } catch {
        # The parser itself failing is not the user's fault; wrap and run.
    }
    return $shape
}

# Whether the child is Windows PowerShell - the interpreter whose parser
# this agent runs on - rather than pwsh or another shell.
function Test-DefaultShell {
    param([string]$Shell)

    if (-not $Shell) { return $true }
    try {
        $leaf = [System.IO.Path]::GetFileName($Shell).ToLowerInvariant()
        return ($leaf -eq "powershell.exe" -or $leaf -eq "powershell")
    } catch {
        # An unparseable path is not Windows PowerShell as far as we can
        # tell; launch it unwrapped and let it report its own errors.
        return $false
    }
}

function New-ChildScript {
    param(
        [string]$UserScript,
        $Shape
    )

    if ($null -eq $Shape) { $Shape = Get-ChildScriptShape -UserScript $UserScript }

    # The agent's own pid, so a script can clean up stray powershell.exe
    # processes without killing the process that is running it.
    $prelude = $script:ChildPrelude + "`n" +
               '$env:AGENT_RDP_AGENT_PID = ''' + $PID + '''' + "`n"

    if ($Shape.wrappable) {
        return $prelude + $script:ChildWrapperHead + "`n" + $UserScript + $script:ChildWrapperTail
    }
    return $prelude + $UserScript
}

# The launch reply for a detached child. A child that dies within its first
# moments - a script that fails before its first statement, a missing
# executable behind the wrapper - used to be indistinguishable from one that
# is running: `Process.Start` had succeeded, and that was all the caller
# learned. Waiting a beat and reporting an early exit makes "it never ran"
# visible at launch time instead of at the next file pull.
$script:EarlyExitProbeMs = 250

function Get-LaunchResult {
    param([System.Diagnostics.Process]$Process)

    Start-Sleep -Milliseconds $script:EarlyExitProbeMs
    if ($Process.HasExited) {
        return @{
            pid = $Process.Id
            exit_code = $Process.ExitCode
            early_exit = $true
        }
    }
    return @{ pid = $Process.Id }
}

# ---- run --stream: file-backed capture ----
#
# The child's stdout/stderr go straight to files (Start-Process hands the
# file handles to CreateProcess), and Invoke-RunPoll reads whatever has been
# appended since the last poll. The previous design buffered output through
# OutputDataReceived events, which only fire when this runspace is idle -
# and it spends its life blocked in a native ReadFile on the DVC handle.
# Combined with HasExited being checked without WaitForExit(), the tail of
# every command's output (all of it, for `echo hello`) was stranded in a
# buffer nobody would ever read again, and the entry was already gone.

# How long a finished process stays pollable. A repeat poll after exit gets
# `exited: true` with empty chunks rather than an error, so a caller that
# lost the first "exited" reply can still learn the exit code.
$script:StreamRetentionSeconds = 600
$script:StreamMaxFinished = 32

function Get-StreamDir {
    $dir = Join-Path -Path $env:TEMP -ChildPath "agent-rdp-run"
    if (-not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }
    return $dir
}

function Remove-ExpiredStreams {
    if ($null -eq $script:StreamedProcesses) { return }

    $now = Get-Date
    $finished = @()
    foreach ($entry in @($script:StreamedProcesses.GetEnumerator())) {
        $s = $entry.Value
        if ($null -ne $s.ExitedAt) {
            $finished += [PSCustomObject]@{ Key = $entry.Key; ExitedAt = $s.ExitedAt }
        }
    }

    $toRemove = @()
    foreach ($f in $finished) {
        if (($now - $f.ExitedAt).TotalSeconds -gt $script:StreamRetentionSeconds) {
            $toRemove += $f.Key
        }
    }
    if ($finished.Count -gt $script:StreamMaxFinished) {
        $excess = $finished | Sort-Object ExitedAt | Select-Object -First ($finished.Count - $script:StreamMaxFinished)
        foreach ($e in $excess) { $toRemove += $e.Key }
    }

    foreach ($key in ($toRemove | Select-Object -Unique)) {
        $s = $script:StreamedProcesses[$key]
        if ($null -ne $s) {
            foreach ($p in @($s.StdoutPath, $s.StderrPath)) {
                try { Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue } catch {}
            }
        }
        $script:StreamedProcesses.Remove($key)
    }
}

function Start-StreamedRun {
    param(
        [string]$Shell,
        [string]$EncodedCommand,
        [bool]$Hidden
    )

    Remove-ExpiredStreams

    $dir = Get-StreamDir
    $token = [Guid]::NewGuid().ToString("N")
    $stdoutPath = Join-Path -Path $dir -ChildPath "$token.out"
    $stderrPath = Join-Path -Path $dir -ChildPath "$token.err"

    $startArgs = @{
        FilePath               = $Shell
        ArgumentList           = "-NoProfile -EncodedCommand $EncodedCommand"
        WorkingDirectory       = $env:USERPROFILE
        RedirectStandardOutput = $stdoutPath
        RedirectStandardError  = $stderrPath
        PassThru               = $true
    }
    # Without one of these, `Start-Process` creates a NEW console - a visible
    # empty window on the remote desktop for every streamed run, which also
    # takes foreground from whatever was in front. The waited path never had
    # this because it uses ProcessStartInfo directly.
    if ($Hidden) {
        $startArgs.WindowStyle = "Hidden"
    } else {
        $startArgs.NoNewWindow = $true
    }

    $process = Start-Process @startArgs
    $script:LastRunLaunched = $true
    $startedUnix = Get-UnixNow

    $state = [PSCustomObject]@{
        Process       = $process
        StdoutPath    = $stdoutPath
        StderrPath    = $stderrPath
        StdoutOffset  = [long]0
        StderrOffset  = [long]0
        # Stateful decoders: a poll can cut the file between the bytes of one
        # UTF-8 character, and a decoder carries the partial sequence over.
        StdoutDecoder = [System.Text.Encoding]::UTF8.GetDecoder()
        StderrDecoder = [System.Text.Encoding]::UTF8.GetDecoder()
        ExitedAt      = $null
        ExitedUnix    = $null
        ExitCode      = $null
    }

    if ($null -eq $script:StreamedProcesses) {
        $script:StreamedProcesses = @{}
    }
    # Windows reuses pids. Overwriting silently orphaned the previous entry's
    # undrained output and leaked its files.
    if ($script:StreamedProcesses.ContainsKey($process.Id)) {
        $old = $script:StreamedProcesses[$process.Id]
        foreach ($p in @($old.StdoutPath, $old.StderrPath)) {
            try { Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue } catch {}
        }
        $script:StreamedProcesses.Remove($process.Id)
    }
    $script:StreamedProcesses[$process.Id] = $state

    $launch = Get-LaunchResult -Process $process
    $launch.started_unix = $startedUnix
    # Output is being captured for run_poll - not a plain detached launch.
    $launch.streamed = $true
    if ($launch.early_exit) {
        # Record it now so the first poll already reports `exited`.
        $state.ExitedAt = Get-Date
        $state.ExitedUnix = Get-UnixNow
        $state.ExitCode = $launch.exit_code
    }
    return $launch
}

# Read everything appended to $Path since $Offset. Returns the decoded text,
# the new offset, and an Error message instead of throwing: a poll must never
# fail as a whole because one side's file was momentarily unreadable (the
# child's sharing mode, an AV scanner, a race with eviction), and it must not
# lose the other side's chunk - which is what an exception here did, after
# the stdout offset had already advanced. The child still has the file open
# for writing, so open with full sharing.
function Read-StreamTail {
    param(
        [string]$Path,
        [long]$Offset,
        [System.Text.Decoder]$Decoder
    )

    $fs = $null
    try {
        if (-not (Test-Path -LiteralPath $Path)) {
            return @{ Text = ""; Offset = $Offset; Error = $null }
        }

        $share = [System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete
        $fs = [System.IO.FileStream]::new($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, $share)
        if ($fs.Length -le $Offset) {
            return @{ Text = ""; Offset = $Offset; Error = $null }
        }
        [void]$fs.Seek($Offset, [System.IO.SeekOrigin]::Begin)
        $count = [int]($fs.Length - $Offset)
        $bytes = New-Object byte[] $count
        $read = 0
        while ($read -lt $count) {
            $n = $fs.Read($bytes, $read, $count - $read)
            if ($n -le 0) { break }
            $read += $n
        }
        # A UTF-8 BOM at the very start of the file is framing, not output.
        $start = 0
        if ($Offset -eq 0 -and $read -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
            $start = 3
        }
        $chars = New-Object char[] ($Decoder.GetCharCount($bytes, $start, $read - $start))
        $charCount = $Decoder.GetChars($bytes, $start, $read - $start, $chars, 0)
        return @{
            Text   = [string]::new($chars, 0, $charCount)
            Offset = $Offset + $read
            Error  = $null
        }
    } catch {
        return @{ Text = ""; Offset = $Offset; Error = $_.Exception.Message }
    } finally {
        if ($null -ne $fs) { $fs.Dispose() }
    }
}

function Invoke-RunPoll {
    param($Params)

    $targetPid = [int]$Params.pid

    Remove-ExpiredStreams

    if ($null -eq $script:StreamedProcesses -or -not $script:StreamedProcesses.ContainsKey($targetPid)) {
        throw "No streamed process with pid $targetPid (it was never started with run --stream, or it exited more than $($script:StreamRetentionSeconds)s ago and its output has been discarded)"
    }

    $state = $script:StreamedProcesses[$targetPid]
    $process = $state.Process

    $exited = $false
    $exitCode = $null
    if ($null -ne $state.ExitedAt) {
        $exited = $true
        $exitCode = $state.ExitCode
    } elseif ($process.HasExited) {
        # Parameterless WaitForExit: the process handle is signalled before
        # its stdio handles are closed, and this is what makes the files
        # complete before we read the final chunk.
        $process.WaitForExit()
        $exited = $true
        $exitCode = $process.ExitCode
        $state.ExitedAt = Get-Date
        $state.ExitedUnix = Get-UnixNow
        $state.ExitCode = $exitCode
    }

    $out = Read-StreamTail -Path $state.StdoutPath -Offset $state.StdoutOffset -Decoder $state.StdoutDecoder
    $state.StdoutOffset = $out.Offset
    $err = Read-StreamTail -Path $state.StderrPath -Offset $state.StderrOffset -Decoder $state.StderrDecoder
    $state.StderrOffset = $err.Offset

    # A read failure is reported in-band, on the stderr side, and the offset
    # was not advanced, so the next poll retries the same bytes.
    $stderrText = $err.Text
    if ($out.Error) { $stderrText += "[run-poll: stdout not readable this poll: $($out.Error)]`n" }
    if ($err.Error) { $stderrText += "[run-poll: stderr not readable this poll: $($err.Error)]`n" }

    $poll = @{
        pid = $targetPid
        stdout_chunk = $out.Text
        stderr_chunk = $stderrText
        exited = $exited
        exit_code = $exitCode
    }
    if ($exited -and $null -ne $state.ExitedUnix) { $poll.finished_unix = $state.ExitedUnix }
    return $poll
}

function Invoke-WaitFor {
    param($Params)

    $selector = $Params.selector
    $timeout = if ($Params.timeout_ms) { [int]$Params.timeout_ms } else { 30000 }
    $state = if ($Params.state) { $Params.state } else { "visible" }

    $startTime = Get-Date
    $pollInterval = 100  # ms

    while ($true) {
        $elapsed = ((Get-Date) - $startTime).TotalMilliseconds
        if ($elapsed -gt $timeout) {
            throw "Timeout waiting for element: $selector (state: $state)"
        }

        $element = $null
        try {
            $element = Find-Element -Selector $selector
        } catch {}

        switch ($state) {
            "visible" {
                if ($element -and -not $element.Current.IsOffscreen) {
                    return @{ found = $true; state = $state; elapsed_ms = [int]$elapsed }
                }
            }
            "enabled" {
                if ($element -and $element.Current.IsEnabled) {
                    return @{ found = $true; state = $state; elapsed_ms = [int]$elapsed }
                }
            }
            "gone" {
                if (-not $element) {
                    return @{ found = $true; state = $state; elapsed_ms = [int]$elapsed }
                }
            }
        }

        Start-Sleep -Milliseconds $pollInterval
    }
}

function Get-AgentStatus {
    return @{
        agent_running = $true
        agent_pid = $PID
        version = $script:Version
        # Where this agent writes its own log, so `agent-rdp diagnose` can
        # pull it into the bug-report bundle.
        log_path = $script:LocalLogPath
        capabilities = @(
            "snapshot", "invoke", "select", "toggle", "expand", "collapse",
            "context_menu", "focus", "get", "fill", "clear",
            "scroll", "window", "run", "wait_for", "status"
        )
    }
}

# ============ FILE TRANSFER ============
#
# Byte-oriented on purpose. Text-mode reads/writes re-encode the payload
# through the console codepage, which corrupts anything that isn't plain
# ASCII - the reason transferring files by pasting text through the clipboard
# or Add-Content mangled non-Latin content. Base64 keeps the DVC message
# JSON-safe; the bytes either side of it are never reinterpreted.

function Invoke-FileWriteChunk {
    param($Params)

    $path = $Params.path
    if (-not $path) { throw "file_write_chunk requires 'path'" }

    $data = [Convert]::FromBase64String($Params.data_b64)

    # Chunks land in a sidecar and the destination is replaced only once the
    # whole file verifies. Writing in place meant the first chunk truncated
    # the existing file, so a transfer that failed at chunk 2 - or two
    # transfers overlapping on one path - destroyed a good file and reported
    # a hash of whatever was left.
    $tid = if ($Params.transfer_id) { $Params.transfer_id } else { "legacy" }
    $part = "$path.agent-rdp-$tid.part"

    if ($Params.first) {
        if (Test-Path -LiteralPath $path -PathType Container) {
            throw "Cannot write '$path': it is a directory"
        }
        $dir = Split-Path -Parent $path
        if ($dir -and -not (Test-Path -LiteralPath $dir)) {
            New-Item -ItemType Directory -Path $dir -Force | Out-Null
        }
        # Sidecars from transfers that died before finishing. Nothing else
        # cleans them up, and they are named after a transfer that is over -
        # except one still being written by an overlapping push to the same
        # destination, which this transfer did not start and must not delete
        # out from under it. An age cutoff is the cheap way to tell the two
        # apart without tracking every transfer id in flight.
        $leaf = Split-Path -Leaf $path
        $searchDir = if ($dir) { $dir } else { "." }
        $staleBefore = (Get-Date).AddMinutes(-10)
        Get-ChildItem -LiteralPath $searchDir -Filter "$leaf.agent-rdp-*.part" -ErrorAction SilentlyContinue |
            Where-Object { $_.LastWriteTime -lt $staleBefore } |
            ForEach-Object { try { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue } catch {} }
        [System.IO.File]::WriteAllBytes($part, $data)
    } else {
        $stream = [System.IO.File]::Open($part, [System.IO.FileMode]::Append, [System.IO.FileAccess]::Write)
        try {
            $stream.Write($data, 0, $data.Length)
        } finally {
            $stream.Close()
        }
    }

    $result = @{
        bytes_written = $data.Length
    }

    if (-not $Params.last) {
        $result.total_size = (Get-Item -LiteralPath $part).Length
        return $result
    }

    # Final chunk: verify the assembled file, then put it in place. A hash is
    # required - skipping the check when one is absent is how a transfer could
    # report success without anything having been verified.
    try {
        if (-not $Params.sha256) {
            throw "Transfer of '$path' carried no expected hash, so it cannot be verified"
        }
        $hash = (Get-FileHash -LiteralPath $part -Algorithm SHA256).Hash.ToLower()
        if ($Params.sha256.ToLower() -ne $hash) {
            throw "Transfer verification failed for '$path': expected $($Params.sha256), got $hash"
        }

        # Replace rather than Move-Item: Move-Item is a delete followed by a
        # move, and onto an existing directory it would move the sidecar
        # inside it instead of failing.
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            [System.IO.File]::Replace($part, $path, $null)
        } else {
            [System.IO.File]::Move($part, $path)
        }
    } catch {
        try { Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue } catch {}
        throw
    }

    # Hash what is actually at the destination now, not what the sidecar was:
    # this is the number the daemon compares against the bytes it sent.
    $item = Get-Item -LiteralPath $path
    $result.sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLower()
    $result.total_size = $item.Length
    $result.path = $item.FullName

    return $result
}

function Invoke-FileReadChunk {
    param($Params)

    $path = $Params.path
    if (-not $path) { throw "file_read_chunk requires 'path'" }
    if (-not (Test-Path -LiteralPath $path)) { throw "File not found: $path" }

    $offset = [int64]$Params.offset
    $length = [int]$Params.length

    $stream = [System.IO.File]::OpenRead($path)
    try {
        $total = $stream.Length
        if ($offset -ge $total) {
            return @{ data_b64 = ""; bytes_read = 0; total_size = $total; eof = $true }
        }

        $stream.Seek($offset, [System.IO.SeekOrigin]::Begin) | Out-Null
        $remaining = $total - $offset
        if ($length -gt $remaining) { $length = [int]$remaining }

        $buffer = New-Object byte[] $length
        # A single Read can legally return fewer bytes than asked for; loop
        # until the buffer is full or the file ends, or chunks silently
        # truncate mid-transfer.
        $read = 0
        while ($read -lt $length) {
            $n = $stream.Read($buffer, $read, $length - $read)
            if ($n -le 0) { break }
            $read += $n
        }

        if ($read -lt $length) {
            $exact = New-Object byte[] $read
            [Array]::Copy($buffer, $exact, $read)
            $buffer = $exact
        }

        return @{
            data_b64 = [Convert]::ToBase64String($buffer)
            bytes_read = $read
            total_size = $total
            eof = ($offset + $read) -ge $total
        }
    } finally {
        $stream.Close()
    }
}

function Invoke-FileStat {
    param($Params)

    $path = $Params.path
    if (-not $path) { throw "file_stat requires 'path'" }

    if (-not (Test-Path -LiteralPath $path)) {
        return @{ exists = $false }
    }

    $item = Get-Item -LiteralPath $path
    if ($item.PSIsContainer) {
        return @{ exists = $true; is_directory = $true; size = 0 }
    }

    # Modification time and the current time, both from this machine's clock,
    # so the daemon can compute the file's age without any assumption about
    # clock agreement between the two hosts. Explicit UTC epoch: parsing
    # '1970-01-01Z' yields a Local-kind DateTime and subtraction ignores Kind.
    $epoch = [datetime]::new(1970, 1, 1, 0, 0, 0, [System.DateTimeKind]::Utc)

    return @{
        exists = $true
        is_directory = $false
        size = $item.Length
        sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLower()
        modified_unix = [int64]($item.LastWriteTimeUtc - $epoch).TotalSeconds
        now_unix = [int64]([datetime]::UtcNow - $epoch).TotalSeconds
    }
}

# ============ RESULT JOURNAL ============
#
# A lost DVC reply leaves the caller unable to tell "never ran" from "ran and
# the acknowledgement was lost" - the difference between safely retrying a
# mutating command and applying it twice. Keeping the last few results lets
# the daemon come back and ask what actually happened instead of guessing.
#
# Two tiers. Every request's result is kept in memory (bounded, FIFO). A
# `run` that carried a caller-chosen idempotency key is also written to disk,
# because that is the case where the key outlives this process: the caller
# retries after a reconnect or an agent relaunch, and "empty journal, so run
# it again" was the double-apply the key exists to prevent.

$script:ResultJournal = @{}
$script:ResultJournalOrder = New-Object System.Collections.ArrayList
$script:ResultJournalLimit = 64

# Disk tier bounds. Best effort: a missing or unreadable entry means "unknown",
# and an unknown key executes.
$script:JournalMaxEntries = 256
$script:JournalRetentionDays = 7
$script:JournalMaxStreamChars = 524288  # 512K characters of stdout / stderr each
$script:JournalDir = $null
$script:JournalDirResolved = $false

function Add-JournaledResult {
    param(
        [string]$Id,
        [bool]$Success,
        $Data,
        $ErrorInfo,
        [string]$Fingerprint,
        [switch]$Persist
    )

    if (-not $Id) { return }

    $entry = @{
        success = $Success
        data = $Data
        error = $ErrorInfo
        fingerprint = $Fingerprint
        at = (Get-Date).ToString("o")
        at_unix = Get-UnixNow
    }
    $script:ResultJournal[$Id] = $entry
    # Re-journaling an id must not list it twice, or the FIFO evicts a live
    # key early.
    if (-not $script:ResultJournalOrder.Contains($Id)) {
        [void]$script:ResultJournalOrder.Add($Id)
    }

    # Bounded: this is a safety net for the last few commands, not a log.
    while ($script:ResultJournalOrder.Count -gt $script:ResultJournalLimit) {
        $oldest = $script:ResultJournalOrder[0]
        $script:ResultJournalOrder.RemoveAt(0)
        $script:ResultJournal.Remove($oldest)
    }

    if ($Persist) {
        Write-PersistedJournalEntry -Id $Id -Entry $entry
    }
}

function Get-Sha256Hex {
    param([string]$Text)

    $bytes = [Text.Encoding]::UTF8.GetBytes($Text)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($sha.ComputeHash($bytes)) -replace "-", "")
    } finally {
        $sha.Dispose()
    }
}

# SHA-256 over the command name and its parameters, so a replay can tell "the
# same request again" from "a new request that happens to reuse the id".
# `timeout_ms` is left out: retrying a timed-out command with a longer
# budget is the same request, not a reuse of its key.
function Get-RequestFingerprint {
    param(
        [string]$Command,
        $Params
    )

    $json = ""
    if ($null -ne $Params) {
        $copy = $Params
        if ($Params -is [hashtable]) {
            $copy = $Params.Clone()
            $copy.Remove("timeout_ms")
        } elseif ($Params.PSObject -and $Params.PSObject.Properties["timeout_ms"]) {
            $copy = $Params.PSObject.Copy()
            $copy.PSObject.Properties.Remove("timeout_ms")
        }
        $json = $copy | ConvertTo-Json -Compress -Depth 10
    }
    return Get-Sha256Hex -Text "$Command|$json"
}

# The raw journal entry for an id (with its fingerprint), or $null. Used by
# the dispatch loop for replay; `Get-JournaledResult` below is the
# `query_result` command's view of the same data. With -IncludeDisk a key
# this process has never seen is looked up on disk, where a previous agent
# may have left it.
function Get-JournalEntry {
    param(
        [string]$Id,
        [switch]$IncludeDisk
    )

    if (-not $Id) { return $null }
    if ($script:ResultJournal.ContainsKey($Id)) { return $script:ResultJournal[$Id] }
    if (-not $IncludeDisk) { return $null }

    $entry = Get-PersistedJournalEntry -Id $Id
    if ($null -eq $entry) { return $null }
    # Cache it in memory (the file already exists; no re-persist).
    $script:ResultJournal[$Id] = $entry
    if (-not $script:ResultJournalOrder.Contains($Id)) {
        [void]$script:ResultJournalOrder.Add($Id)
    }
    return $entry
}

function Get-JournaledResult {
    param($Params)

    $id = $Params.id
    if (-not $id) { throw "query_result requires 'id'" }

    $entry = Get-JournalEntry -Id $id -IncludeDisk
    if ($null -eq $entry) {
        # Deliberately distinguishes "we never saw it" from "still running":
        # the daemon only asks once the agent is answering again, so an
        # unknown id at that point means the request never executed.
        return @{ known = $false }
    }

    return @{
        known = $true
        success = $entry.success
        data = $entry.data
        error = $entry.error
        at = $entry.at
        at_unix = $entry.at_unix
    }
}

# ---- disk tier ----
#
# Under %LOCALAPPDATA%, not %TEMP%: on a session host TEMP is per logon
# session and deleted at logoff, which is precisely when a caller comes back
# with the same key. LOCALAPPDATA is per Windows account and per host, so a
# temporary profile or a farm that lands the user on another host starts
# empty - callers verify the side effect there, as documented. Never the
# \\TSCLIENT drive (see the DVC/RDPDR deadlock note in AUTOMATION.md).

function Get-JournalDir {
    if ($script:JournalDirResolved) { return $script:JournalDir }
    $script:JournalDirResolved = $true

    # No fallback to TEMP: that is the per-logon directory the whole tier
    # exists to avoid. Without a local profile the journal is memory-only.
    $root = $env:LOCALAPPDATA
    if (-not $root) {
        Write-Log "Result journal is memory-only: LOCALAPPDATA is not set" "WARN"
        return $null
    }

    $dir = Join-Path -Path (Join-Path -Path $root -ChildPath "agent-rdp") -ChildPath "journal"
    try {
        if (-not (Test-Path -LiteralPath $dir)) {
            New-Item -ItemType Directory -Path $dir -Force | Out-Null
        }
        $script:JournalDir = $dir
    } catch {
        Write-Log "Result journal is memory-only: cannot create $dir ($($_.Exception.Message))" "WARN"
        $script:JournalDir = $null
    }
    return $script:JournalDir
}

# Keys may contain characters that are not valid in a file name (`:`), so
# the file is named by the key's hash.
function Get-JournalPath {
    param([string]$Id)

    $dir = Get-JournalDir
    if (-not $dir) { return $null }
    return Join-Path -Path $dir -ChildPath ((Get-Sha256Hex -Text $Id).ToLowerInvariant() + ".json")
}

# Recursive PSCustomObject -> hashtable, so an entry read back from JSON has
# the same shape as one recorded live (the replay path adds keys to it, and
# PowerShell 5.1's ConvertFrom-Json has no -AsHashtable). Arrays are
# returned with the unary comma so a single-element array does not unroll
# into its element on the way out of the function.
function ConvertTo-Hashtable {
    param($InputObject)

    if ($null -eq $InputObject) { return $null }
    if ($InputObject -is [string] -or $InputObject -is [ValueType]) { return $InputObject }
    if ($InputObject -is [hashtable]) {
        $copy = @{}
        foreach ($key in $InputObject.Keys) { $copy[$key] = ConvertTo-Hashtable -InputObject $InputObject[$key] }
        return $copy
    }
    if ($InputObject -is [System.Collections.IEnumerable]) {
        $list = New-Object System.Collections.ArrayList
        foreach ($item in $InputObject) { [void]$list.Add((ConvertTo-Hashtable -InputObject $item)) }
        return ,$list.ToArray()
    }
    if ($InputObject -is [System.Management.Automation.PSCustomObject]) {
        $table = @{}
        foreach ($property in $InputObject.PSObject.Properties) {
            $table[$property.Name] = ConvertTo-Hashtable -InputObject $property.Value
        }
        return $table
    }
    return $InputObject
}

# The on-disk copy of a result: the same fields, with stdout/stderr capped so
# one chatty command cannot fill the profile. A capped entry still answers
# the question the key asks - did it run, and how did it exit.
function ConvertTo-PersistedJournalData {
    param($Data)

    if ($null -eq $Data -or -not ($Data -is [hashtable])) { return $Data }
    $copy = $Data.Clone()
    foreach ($stream in @("stdout", "stderr")) {
        $text = $copy[$stream]
        if ($text -is [string] -and $text.Length -gt $script:JournalMaxStreamChars) {
            $copy[$stream] = $text.Substring(0, $script:JournalMaxStreamChars) + "`n[truncated by the result journal]"
            $copy["journal_truncated"] = $true
        }
    }
    return $copy
}

# Write-once: the first execution's result is the one that counts, and a
# second writer (two agents for the same account racing on one key) must not
# replace it. A temp file plus File.Move is atomic on NTFS; Move-Item -Force
# is a delete followed by a move and is not.
function Write-PersistedJournalEntry {
    param(
        [string]$Id,
        [hashtable]$Entry
    )

    $path = Get-JournalPath -Id $Id
    if (-not $path) { return }

    try {
        $record = @{
            id = $Id
            success = $Entry.success
            data = ConvertTo-PersistedJournalData -Data $Entry.data
            error = $Entry.error
            fingerprint = $Entry.fingerprint
            at = $Entry.at
            at_unix = $Entry.at_unix
        }
        $json = $record | ConvertTo-Json -Depth 20 -Compress
        $tmp = "$path.$PID.tmp"
        # UTF-8 without BOM through .NET: Set-Content in PowerShell 5.1 writes
        # the ANSI codepage and would corrupt non-ASCII output.
        [System.IO.File]::WriteAllText($tmp, $json, [System.Text.UTF8Encoding]::new($false))
        try {
            [System.IO.File]::Move($tmp, $path)
        } catch {
            # Destination exists: an earlier execution already owns this key.
            # Anything else (ACL, disk) is worth a log line.
            if (-not (Test-Path -LiteralPath $path)) {
                Write-Log "Could not move journal entry into place for '$Id': $($_.Exception.Message)" "WARN"
            }
            try { [System.IO.File]::Delete($tmp) } catch {}
        }
        Remove-ExpiredJournalEntries
    } catch {
        Write-Log "Could not persist journal entry for '$Id': $($_.Exception.Message)" "WARN"
    }
}

function Get-PersistedJournalEntry {
    param([string]$Id)

    $path = Get-JournalPath -Id $Id
    if (-not $path) { return $null }
    if (-not (Test-Path -LiteralPath $path)) { return $null }

    try {
        $json = [System.IO.File]::ReadAllText($path, [System.Text.UTF8Encoding]::new($false))
        $record = ConvertTo-Hashtable -InputObject ($json | ConvertFrom-Json)
        if ($null -eq $record -or -not ($record -is [hashtable])) { throw "not a JSON object" }
        if ($record.id -ne $Id -or -not $record.fingerprint) { throw "id or fingerprint missing" }
        return @{
            success = [bool]$record.success
            data = $record.data
            error = $record.error
            fingerprint = [string]$record.fingerprint
            at = $record.at
            at_unix = $record.at_unix
        }
    } catch {
        # Unreadable is unknown, and unknown executes - the same answer a
        # missing file gives. Remove the file too: a corrupt entry has no
        # value, and left in place it would block the write-once record of
        # the execution that is about to happen.
        Write-Log "Discarding unreadable journal entry for '$Id' at ${path}: $($_.Exception.Message)" "WARN"
        try { Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue } catch {}
        return $null
    }
}

function Remove-ExpiredJournalEntries {
    $dir = Get-JournalDir
    if (-not $dir) { return }

    try {
        $files = @(Get-ChildItem -LiteralPath $dir -Filter "*.json" -File -ErrorAction SilentlyContinue)
        $cutoff = (Get-Date).AddDays(-$script:JournalRetentionDays)
        $live = New-Object System.Collections.ArrayList
        foreach ($file in $files) {
            if ($file.LastWriteTime -lt $cutoff) {
                Remove-Item -LiteralPath $file.FullName -Force -ErrorAction SilentlyContinue
            } else {
                [void]$live.Add($file)
            }
        }
        if ($live.Count -gt $script:JournalMaxEntries) {
            $excess = $live | Sort-Object LastWriteTime | Select-Object -First ($live.Count - $script:JournalMaxEntries)
            foreach ($file in $excess) {
                Remove-Item -LiteralPath $file.FullName -Force -ErrorAction SilentlyContinue
            }
        }
        # Stale temp files from a writer that died mid-write.
        foreach ($tmp in @(Get-ChildItem -LiteralPath $dir -Filter "*.tmp" -File -ErrorAction SilentlyContinue)) {
            if ($tmp.LastWriteTime -lt (Get-Date).AddMinutes(-10)) {
                Remove-Item -LiteralPath $tmp.FullName -Force -ErrorAction SilentlyContinue
            }
        }
    } catch {
        Write-Log "Journal pruning failed: $($_.Exception.Message)" "WARN"
    }
}
