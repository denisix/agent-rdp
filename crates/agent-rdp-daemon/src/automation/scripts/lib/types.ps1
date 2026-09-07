# types.ps1 - P/Invoke helper types for mouse input, window enumeration,
# process-tree termination, host CPU load and desktop liveness.
#
# One Add-Type for all of them on purpose: each Add-Type call compiles a C#
# assembly with csc.exe, and that compile is exactly what costs seconds on a
# CPU-saturated host. Adding a second block would pay that price again at
# every agent start. Every caller wraps these in try/catch with a fallback,
# so a marshalling mistake degrades behaviour rather than breaking the agent.

Add-Type -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public class MouseInput {
    [DllImport("user32.dll")]
    public static extern void mouse_event(int dwFlags, int dx, int dy, int dwData, int dwExtraInfo);

    public const int MOUSEEVENTF_LEFTDOWN = 0x0002;
    public const int MOUSEEVENTF_LEFTUP = 0x0004;
    public const int MOUSEEVENTF_RIGHTDOWN = 0x0008;
    public const int MOUSEEVENTF_RIGHTUP = 0x0010;
    public const int MOUSEEVENTF_MIDDLEDOWN = 0x0020;
    public const int MOUSEEVENTF_MIDDLEUP = 0x0040;

    public static void LeftClick() {
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
    }

    public static void RightClick() {
        mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, 0);
        mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
    }

    public static void MiddleClick() {
        mouse_event(MOUSEEVENTF_MIDDLEDOWN, 0, 0, 0, 0);
        mouse_event(MOUSEEVENTF_MIDDLEUP, 0, 0, 0, 0);
    }

    public static void DoubleClick() {
        LeftClick();
        System.Threading.Thread.Sleep(50);
        LeftClick();
    }
}

public class WindowEnum {
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hWnd);

    private static List<IntPtr> windowHandles;

    public static IntPtr[] GetAllWindows() {
        windowHandles = new List<IntPtr>();
        EnumWindows(EnumWindowCallback, IntPtr.Zero);
        return windowHandles.ToArray();
    }

    private static bool EnumWindowCallback(IntPtr hWnd, IntPtr lParam) {
        // Include all windows, even invisible ones (some popups may not be "visible")
        windowHandles.Add(hWnd);
        return true;
    }
}

// Job objects: how `run --wait` kills a whole process tree on timeout.
//
// Process.Kill() terminates one process. The command the caller wrote runs
// in a child powershell.exe, so anything *it* starts is a grandchild that
// survives - the agent reported "was killed" while the real work carried on
// (a watcher script fired tsdiscon two minutes after being declared dead).
// A walk of ParentProcessId cannot fix that either: it races the kill, and
// a grandchild whose parent already exited is reparented out of the tree.
// A job holds every descendant created after the assignment no matter how
// it was spawned, TerminateJobObject kills them in one call, and
// ActiveProcesses is an exact, PID-reuse-proof answer to "is it really
// gone".
//
// KILL_ON_JOB_CLOSE is deliberately NOT set: the handle dies with the
// agent, and an agent restart must not take a caller's running command
// with it.
[StructLayout(LayoutKind.Sequential)]
public struct JobBasicAccounting {
    public Int64 TotalUserTime;
    public Int64 TotalKernelTime;
    public Int64 ThisPeriodTotalUserTime;
    public Int64 ThisPeriodTotalKernelTime;
    public UInt32 TotalPageFaultCount;
    public UInt32 TotalProcesses;
    public UInt32 ActiveProcesses;
    public UInt32 TotalTerminatedProcesses;
}

public class AgentJob {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateJobObject(IntPtr lpJobAttributes, string lpName);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateJobObject(IntPtr hJob, uint uExitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool QueryInformationJobObject(IntPtr hJob, int infoClass,
        ref JobBasicAccounting info, int infoLength, IntPtr returnLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr hObject);

    private const int JobObjectBasicAccountingInformation = 1;

    public static IntPtr Create() {
        return CreateJobObject(IntPtr.Zero, null);
    }

    public static bool Assign(IntPtr job, IntPtr process) {
        if (job == IntPtr.Zero || process == IntPtr.Zero) { return false; }
        return AssignProcessToJobObject(job, process);
    }

    public static bool Terminate(IntPtr job) {
        if (job == IntPtr.Zero) { return false; }
        return TerminateJobObject(job, 1);
    }

    // -1 when the job cannot be queried, so a caller can tell "no answer"
    // from a genuine zero.
    public static int ActiveProcesses(IntPtr job) {
        JobBasicAccounting info = new JobBasicAccounting();
        if (job == IntPtr.Zero) { return -1; }
        if (!QueryInformationJobObject(job, JobObjectBasicAccountingInformation,
                ref info, Marshal.SizeOf(info), IntPtr.Zero)) {
            return -1;
        }
        return (int)info.ActiveProcesses;
    }

    // Every process the job ever held. > 1 means the command really did
    // spawn something, i.e. the shell got past starting up.
    public static int TotalProcesses(IntPtr job) {
        JobBasicAccounting info = new JobBasicAccounting();
        if (job == IntPtr.Zero) { return -1; }
        if (!QueryInformationJobObject(job, JobObjectBasicAccountingInformation,
                ref info, Marshal.SizeOf(info), IntPtr.Zero)) {
            return -1;
        }
        return (int)info.TotalProcesses;
    }

    public static void Close(IntPtr job) {
        if (job != IntPtr.Zero) { CloseHandle(job); }
    }
}

// Host CPU load, in-process. Deliberately not WMI: the first CIM call in a
// process starts a session, and under the load this is meant to measure
// that has been seen to take seconds - the timeout path is the worst place
// to pay it.
public class AgentCpu {
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetSystemTimes(out Int64 idleTime, out Int64 kernelTime, out Int64 userTime);

    // Busy percentage between two samples. kernelTime includes idle, which
    // is why busy is (kernel + user - idle) over (kernel + user).
    public static int BusyPercent(int sampleMs) {
        Int64 idle1, kernel1, user1, idle2, kernel2, user2;
        if (!GetSystemTimes(out idle1, out kernel1, out user1)) { return -1; }
        System.Threading.Thread.Sleep(sampleMs);
        if (!GetSystemTimes(out idle2, out kernel2, out user2)) { return -1; }
        Int64 total = (kernel2 - kernel1) + (user2 - user1);
        if (total <= 0) { return -1; }
        Int64 busy = total - (idle2 - idle1);
        if (busy < 0) { busy = 0; }
        return (int)((busy * 100) / total);
    }
}

// Desktop liveness. "Connected" on the RDP transport and "there is an
// interactive desktop to draw on" are different facts that drift apart: a
// field report saw the session report Connected at 14:37 and the desktop
// die at 14:39, staying dead for 25 minutes. The input desktop's *name*
// ("Default" vs "Winlogon"/"Disconnect") is the sharpest signal of which
// state the session is in.
public class AgentDesktop {
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll", SetLastError = true)]
    private static extern IntPtr OpenInputDesktop(uint dwFlags, bool fInherit, uint dwDesiredAccess);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool CloseDesktop(IntPtr hDesktop);

    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool GetUserObjectInformationW(IntPtr hObj, int nIndex,
        StringBuilder pvInfo, int nLength, out int lpnLengthNeeded);

    private const uint DESKTOP_READOBJECTS = 0x0001;
    private const int UOI_NAME = 2;

    public static bool InputDesktopOpen() {
        IntPtr desk = OpenInputDesktop(0, false, DESKTOP_READOBJECTS);
        if (desk == IntPtr.Zero) { return false; }
        CloseDesktop(desk);
        return true;
    }

    // null when the input desktop cannot be opened (which is itself the
    // answer: another session or the secure desktop owns it).
    public static string InputDesktopName() {
        IntPtr desk = OpenInputDesktop(0, false, DESKTOP_READOBJECTS);
        if (desk == IntPtr.Zero) { return null; }
        try {
            StringBuilder name = new StringBuilder(256);
            int needed = 0;
            if (!GetUserObjectInformationW(desk, UOI_NAME, name, name.Capacity * 2, out needed)) {
                return null;
            }
            return name.ToString();
        } finally {
            CloseDesktop(desk);
        }
    }
}
"@
