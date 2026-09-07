# Requires -Version 5.1
<#
Exercises the broker's foreground-surface attestation against a real
low-integrity UIA address-bar impersonator.

The script compiles two private C# executables under a unique temporary
directory. The helper creates a restricted child with a Low mandatory label;
that child exposes a writable WinForms TextBox named "Address Bar" and focuses
it. In the default Attestation mode, the script supplies the fake surface's
verified PID and foreground HWND to the built broker test executable. That
ignored test must call the production attest_foreground_surface() path and
return success only after it rejects the surface.

SyntheticInjection is retained as a deliberately bounded compatibility mode.
It uses keybd_event to synthesize Enter and checks that a marker executable was
not launched, but it is not evidence that physical input would be rejected and
does not attest the broker's foreground-surface policy.

No elevation is used. An absent medium-integrity broker or GUI desktop is a
clear SKIP, not a passing security result.
#>
[CmdletBinding()]
param(
    [string] $BrokerProcessName = 'fswbroker',
    [ValidateSet('Attestation', 'SyntheticInjection')]
    [string] $Mode = 'Attestation',
    [string] $AttestationTestExecutable,
    [switch] $KeepArtifacts
)

$ErrorActionPreference = 'Stop'
$work = $null
$runFailure = $null

function Skip-Harness([string] $Reason) {
    Write-Host "SKIP: $Reason"
    exit 77
}

function Quote-CSharpVerbatim([string] $Value) {
    return $Value.Replace('"', '""')
}

function Wait-ReadyMetadata {
    param(
        [string] $Path,
        [int] $TimeoutMilliseconds = 10000
    )

    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMilliseconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $Path) {
            $metadata = @{}
            foreach ($line in @(Get-Content -LiteralPath $Path -ErrorAction SilentlyContinue)) {
                $pair = $line.Split('=', 2)
                if ($pair.Count -eq 2) { $metadata[$pair[0]] = $pair[1] }
            }
            if ($metadata.ContainsKey('pid') -and $metadata.ContainsKey('hwnd') -and $metadata.ContainsKey('edit') -and $metadata.ContainsKey('class') -and $metadata.ContainsKey('integrity')) {
                return $metadata
            }
        }
        Start-Sleep -Milliseconds 100
    }
    throw 'the low-integrity fake UIA child did not publish readiness metadata.'
}

function Invoke-AttestationTest {
    param(
        [string] $Executable,
        [Int64] $Hwnd,
        [int] $FakeProcessId,
        [Int64] $EditHwnd,
        [string] $Marker,
        [string] $Input,
        [string] $ExpectedVersion
    )

    if (-not $Executable) {
        Skip-Harness 'Attestation mode requires -AttestationTestExecutable pointing to a built fswbroker test binary.'
    }
    $resolved = Resolve-Path -LiteralPath $Executable -ErrorAction Stop
    if ((Get-Item -LiteralPath $resolved).PSIsContainer) {
        throw '-AttestationTestExecutable must name an executable file.'
    }
    $testVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo($resolved.Path).ProductVersion
    if (-not $testVersion -or $testVersion -ne $ExpectedVersion) {
        throw "The attestation test binary version '$testVersion' does not match the running broker version '$ExpectedVersion'."
    }

    $start = New-Object System.Diagnostics.ProcessStartInfo
    $start.FileName = $resolved.Path
    $start.Arguments = '--exact tests::reject_low_integrity_foreground --ignored --nocapture'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.WindowStyle = [System.Diagnostics.ProcessWindowStyle]::Hidden
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.EnvironmentVariables['FSW_ATTESTATION_TEST_HWND'] = $Hwnd.ToString([Globalization.CultureInfo]::InvariantCulture)
    $start.EnvironmentVariables['FSW_ATTESTATION_TEST_PID'] = $FakeProcessId.ToString([Globalization.CultureInfo]::InvariantCulture)
    $start.EnvironmentVariables['FSW_ATTESTATION_TEST_EDIT_HWND'] = $EditHwnd.ToString([Globalization.CultureInfo]::InvariantCulture)
    $start.EnvironmentVariables['FSW_ATTESTATION_MARKER'] = $Marker
    $start.EnvironmentVariables['FSW_ATTESTATION_INPUT'] = $Input
    $start.EnvironmentVariables['FSW_ATTESTATION_EXPECTED_VERSION'] = $ExpectedVersion
    $test = [System.Diagnostics.Process]::Start($start)
    if ($null -eq $test) { throw 'could not start the broker attestation test executable.' }
    if (-not $test.WaitForExit(15000)) {
        try { $test.Kill() } catch { }
        throw 'FAIL: tests::reject_low_integrity_foreground did not finish within 15 seconds.'
    }
    $testOutput = $test.StandardOutput.ReadToEnd()
    $testError = $test.StandardError.ReadToEnd()
    if ($test.ExitCode -ne 0) {
        throw "FAIL: tests::reject_low_integrity_foreground exited with $($test.ExitCode); Low-IL foreground rejection was not proven.`n$testOutput$testError"
    }
}

function Stop-PrivateHarnessProcesses {
    param([string[]] $ExecutablePaths)

    $wanted = @{}
    foreach ($path in $ExecutablePaths) {
        if ($path) { $wanted[$path.ToLowerInvariant()] = $true }
    }
    foreach ($process in @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue)) {
        if ($process.ExecutablePath -and $wanted.ContainsKey($process.ExecutablePath.ToLowerInvariant())) {
            Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
}

try {
    if ($env:OS -ne 'Windows_NT') {
        Skip-Harness 'Windows is required.'
    }
    if (-not [Environment]::UserInteractive) {
        Skip-Harness 'an interactive Windows desktop is required.'
    }

    $brokers = @(Get-Process -Name $BrokerProcessName -ErrorAction SilentlyContinue)
    if ($brokers.Count -ne 1) {
        Skip-Harness "exactly one running $BrokerProcessName.exe is required."
    }

    $work = Join-Path $env:TEMP ("fsw-low-il-uia-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work | Out-Null
    # Mandatory integrity is checked before the user's normal directory DACL.
    # Give Low write access only to this unique test directory and its children;
    # never alter the ambient Temp directory's label.
    $icacls = Join-Path $env:SystemRoot 'System32\icacls.exe'
    if (-not (Test-Path -LiteralPath $icacls)) { throw 'icacls.exe is unavailable.' }
    $currentSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    & $icacls $work '/grant:r' ("*{0}:(OI)(CI)(F)" -f $currentSid) | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls could not grant the current user access to $work." }
    & $icacls $work '/setintegritylevel' '(OI)(CI)L' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls could not grant Low integrity access to $work." }
    $helper = Join-Path $work 'low-il-uia-helper.exe'
    $marker = Join-Path $work 'fsw-low-il-marker.exe'
    $ready = Join-Path $work 'fake-ready.txt'
    $fired = Join-Path $work 'marker-fired.txt'

    $helperSource = @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows.Forms;

internal static class LowIntegrityUiaHelper
{
    const uint TOKEN_ASSIGN_PRIMARY = 0x0001;
    const uint TOKEN_DUPLICATE = 0x0002;
    const uint TOKEN_QUERY = 0x0008;
    const uint TOKEN_ADJUST_DEFAULT = 0x0080;
    const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    const uint DISABLE_MAX_PRIVILEGE = 0x1;
    const int TokenIntegrityLevel = 25;
    const uint LOW_INTEGRITY_RID = 0x1000;
    const byte VK_RETURN = 0x0D;
    const uint KEYEVENTF_KEYUP = 0x0002;

    [StructLayout(LayoutKind.Sequential)] struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }
    [StructLayout(LayoutKind.Sequential)] struct TOKEN_MANDATORY_LABEL { public SID_AND_ATTRIBUTES Label; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct STARTUPINFO {
        public int cb; public string lpReserved; public string lpDesktop; public string lpTitle;
        public int dwX; public int dwY; public int dwXSize; public int dwYSize; public int dwXCountChars; public int dwYCountChars;
        public int dwFillAttribute; public int dwFlags; public short wShowWindow; public short cbReserved2;
        public IntPtr lpReserved2; public IntPtr hStdInput; public IntPtr hStdOutput; public IntPtr hStdError;
    }
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION {
        public IntPtr hProcess; public IntPtr hThread; public int dwProcessId; public int dwThreadId;
    }

    [DllImport("advapi32.dll", SetLastError = true)] static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)] static extern bool CreateRestrictedToken(IntPtr token, uint flags, uint disableCount, IntPtr disableSids, uint deletePrivilegeCount, IntPtr privileges, uint restrictCount, IntPtr restrictSids, out IntPtr restricted);
    [DllImport("advapi32.dll", SetLastError = true)] static extern bool SetTokenInformation(IntPtr token, int tokenClass, IntPtr information, int informationLength);
    [DllImport("advapi32.dll", SetLastError = true)] static extern bool GetTokenInformation(IntPtr token, int tokenClass, IntPtr information, int informationLength, out int returnLength);
    [DllImport("advapi32.dll", SetLastError = true)] static extern bool CreateProcessAsUser(IntPtr token, string app, string command, IntPtr processAttributes, IntPtr threadAttributes, bool inheritHandles, uint flags, IntPtr environment, string currentDirectory, ref STARTUPINFO startup, out PROCESS_INFORMATION process);
    [DllImport("advapi32.dll", SetLastError = true)] static extern bool ConvertStringSidToSid(string value, out IntPtr sid);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr process, uint exitCode);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr memory);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("advapi32.dll", SetLastError = true)] static extern IntPtr GetSidSubAuthorityCount(IntPtr sid);
    [DllImport("advapi32.dll", SetLastError = true)] static extern IntPtr GetSidSubAuthority(IntPtr sid, uint index);
    [DllImport("user32.dll")] static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();

    static int Integrity(IntPtr process) {
        IntPtr token = IntPtr.Zero;
        if (!OpenProcessToken(process, TOKEN_QUERY, out token)) return -1;
        try {
            int bytes;
            GetTokenInformation(token, TokenIntegrityLevel, IntPtr.Zero, 0, out bytes);
            IntPtr buffer = Marshal.AllocHGlobal(bytes);
            try {
                if (!GetTokenInformation(token, TokenIntegrityLevel, buffer, bytes, out bytes)) return -1;
                IntPtr sid = Marshal.ReadIntPtr(buffer);
                IntPtr countPtr = GetSidSubAuthorityCount(sid);
                if (countPtr == IntPtr.Zero) return -1;
                byte count = Marshal.ReadByte(countPtr);
                if (count == 0) return -1;
                IntPtr rid = GetSidSubAuthority(sid, (uint)(count - 1));
                return rid == IntPtr.Zero ? -1 : Marshal.ReadInt32(rid);
            } finally { Marshal.FreeHGlobal(buffer); }
        } finally { CloseHandle(token); }
    }

    static string Quote(string value) { return "\"" + value.Replace("\"", "\\\"") + "\""; }

    static int SpawnLow(string[] childArgs) {
        IntPtr current = IntPtr.Zero, restricted = IntPtr.Zero, lowSid = IntPtr.Zero, label = IntPtr.Zero;
        try {
            // The source token is our own primary token. CreateRestrictedToken
            // makes a restricted derivative of it, which is the documented
            // CreateProcessAsUser case that does not require
            // SeAssignPrimaryTokenPrivilege. TOKEN_ADJUST_DEFAULT is required
            // before applying the Low mandatory label to that derivative.
            if (!OpenProcessToken(GetCurrentProcess(), TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT, out current)) throw new Win32Exception(Marshal.GetLastWin32Error());
            if (!CreateRestrictedToken(current, DISABLE_MAX_PRIVILEGE, 0, IntPtr.Zero, 0, IntPtr.Zero, 0, IntPtr.Zero, out restricted)) throw new Win32Exception(Marshal.GetLastWin32Error());
            if (!ConvertStringSidToSid("S-1-16-4096", out lowSid)) throw new Win32Exception(Marshal.GetLastWin32Error());
            TOKEN_MANDATORY_LABEL mandatory = new TOKEN_MANDATORY_LABEL { Label = new SID_AND_ATTRIBUTES { Sid = lowSid, Attributes = 0x20 } };
            label = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(TOKEN_MANDATORY_LABEL)));
            Marshal.StructureToPtr(mandatory, label, false);
            if (!SetTokenInformation(restricted, TokenIntegrityLevel, label, Marshal.SizeOf(typeof(TOKEN_MANDATORY_LABEL)))) throw new Win32Exception(Marshal.GetLastWin32Error());
            // Keep the caller's already-authorized interactive desktop. An
            // explicit winsta0\\default request asks the Low token to reopen
            // desktop objects and can fail with ERROR_ACCESS_DENIED before the
            // child starts.
            STARTUPINFO startup = new STARTUPINFO { cb = Marshal.SizeOf(typeof(STARTUPINFO)) };
            PROCESS_INFORMATION child;
            string command = Quote(Application.ExecutablePath) + " " + String.Join(" ", childArgs.Select(Quote));
            if (!CreateProcessAsUser(restricted, null, command, IntPtr.Zero, IntPtr.Zero, false, 0, IntPtr.Zero, null, ref startup, out child)) throw new Win32Exception(Marshal.GetLastWin32Error());
            try {
                if (WaitForSingleObject(child.hProcess, 30000) != 0) {
                    TerminateProcess(child.hProcess, 22);
                    throw new TimeoutException("Low-integrity fake UIA child did not exit.");
                }
                uint code;
                if (!GetExitCodeProcess(child.hProcess, out code)) throw new Win32Exception(Marshal.GetLastWin32Error());
                return unchecked((int)code);
            }
            finally { CloseHandle(child.hThread); CloseHandle(child.hProcess); }
        } finally {
            if (label != IntPtr.Zero) Marshal.FreeHGlobal(label);
            if (lowSid != IntPtr.Zero) LocalFree(lowSid);
            if (restricted != IntPtr.Zero) CloseHandle(restricted);
            if (current != IntPtr.Zero) CloseHandle(current);
        }
    }

    const uint WS_OVERLAPPEDWINDOW = 0x00CF0000;
    const uint WS_VISIBLE = 0x10000000;
    const uint WS_CHILD = 0x40000000;
    const uint WS_TABSTOP = 0x00010000;
    const uint ES_AUTOHSCROLL = 0x0080;
    const uint WM_CLOSE = 0x0010;
    const uint WM_DESTROY = 0x0002;
    const uint WM_COMMAND = 0x0111;
    const int EN_CHANGE = 0x0300;
    const string SearchWindowClass = "Windows.UI.Core.CoreWindow";

    [UnmanagedFunctionPointer(CallingConvention.Winapi)]
    delegate IntPtr WindowProc(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct WNDCLASSEX {
        public uint cbSize, style;
        public WindowProc lpfnWndProc;
        public int cbClsExtra, cbWndExtra;
        public IntPtr hInstance, hIcon, hCursor, hbrBackground;
        [MarshalAs(UnmanagedType.LPWStr)] public string lpszMenuName;
        [MarshalAs(UnmanagedType.LPWStr)] public string lpszClassName;
        public IntPtr hIconSm;
    }
    [StructLayout(LayoutKind.Sequential)] struct POINT { public int x, y; }
    [StructLayout(LayoutKind.Sequential)] struct MSG {
        public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam;
        public uint time; public POINT pt;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)] static extern IntPtr GetModuleHandle(string name);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern ushort RegisterClassEx(ref WNDCLASSEX windowClass);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)] static extern IntPtr CreateWindowEx(uint exStyle, string className, string title, uint style, int x, int y, int width, int height, IntPtr parent, IntPtr menu, IntPtr instance, IntPtr parameter);
    [DllImport("user32.dll")] static extern IntPtr DefWindowProc(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll")] static extern bool UpdateWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern IntPtr SetFocus(IntPtr hwnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool SetWindowText(IntPtr hwnd, string text);
    [DllImport("user32.dll")] static extern int GetMessage(out MSG message, IntPtr window, uint minimum, uint maximum);
    [DllImport("user32.dll")] static extern bool TranslateMessage(ref MSG message);
    [DllImport("user32.dll")] static extern IntPtr DispatchMessage(ref MSG message);
    [DllImport("user32.dll")] static extern void PostQuitMessage(int exitCode);
    [DllImport("user32.dll")] static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr hwnd, System.Text.StringBuilder buffer, int maximum);

    static IntPtr fakeWindow, fakeEdit;
    static string initialText, markerExecutable;
    static bool markerArmed;

    static string WindowText(IntPtr hwnd) {
        var text = new System.Text.StringBuilder(32768);
        GetWindowText(hwnd, text, text.Capacity);
        return text.ToString();
    }

    static IntPtr FakeSearchWindowProc(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam) {
        // The canary is armed only after the ready record is atomically
        // published. A broker-side UIA SetValue would change this real native
        // EDIT control and start the marker; rejected surfaces must leave it
        // untouched. No marker is expected on a passing run.
        if (message == WM_COMMAND && lParam == fakeEdit && ((wParam.ToInt64() >> 16) & 0xffff) == EN_CHANGE && markerArmed && WindowText(fakeEdit) != initialText) {
            markerArmed = false;
            try { Process.Start(markerExecutable); } catch { }
        }
        if (message == WM_DESTROY) PostQuitMessage(0);
        return DefWindowProc(hwnd, message, wParam, lParam);
    }

    static int FakeAddressBar(string slashMarker, string readyPath, string markerPath, bool injectEnter, int lifetimeMilliseconds) {
        if (Integrity(GetCurrentProcess()) != LOW_INTEGRITY_RID) return 17;
        try {
            var proc = new WindowProc(FakeSearchWindowProc);
            var windowClass = new WNDCLASSEX {
                cbSize = (uint)Marshal.SizeOf(typeof(WNDCLASSEX)), lpfnWndProc = proc,
                hInstance = GetModuleHandle(null), lpszClassName = SearchWindowClass
            };
            if (RegisterClassEx(ref windowClass) == 0)
                throw new Win32Exception(Marshal.GetLastWin32Error(), "could not register the Search-compatible window class");
            fakeWindow = CreateWindowEx(0, SearchWindowClass, "Search", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100, 640, 160, IntPtr.Zero, IntPtr.Zero, windowClass.hInstance, IntPtr.Zero);
            if (fakeWindow == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "could not create the Search-compatible top-level window");
            initialText = slashMarker;
            markerExecutable = markerPath;
            fakeEdit = CreateWindowEx(0, "EDIT", slashMarker, WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL, 12, 12, 600, 28, fakeWindow, IntPtr.Zero, windowClass.hInstance, IntPtr.Zero);
            if (fakeEdit == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "could not create the writable native EDIT control");
            if (!SetWindowText(fakeEdit, slashMarker)) throw new Win32Exception(Marshal.GetLastWin32Error(), "could not seed the native EDIT control");
            ShowWindow(fakeWindow, 5);
            UpdateWindow(fakeWindow);
            SetForegroundWindow(fakeWindow);
            SetFocus(fakeEdit);
            string metadata = "pid=" + Process.GetCurrentProcess().Id + Environment.NewLine
                + "hwnd=" + fakeWindow.ToInt64() + Environment.NewLine
                + "edit=" + fakeEdit.ToInt64() + Environment.NewLine
                + "class=" + SearchWindowClass + Environment.NewLine
                + "integrity=" + Integrity(GetCurrentProcess()) + Environment.NewLine;
            string temporaryReady = readyPath + ".tmp";
            File.WriteAllText(temporaryReady, metadata);
            File.Move(temporaryReady, readyPath);
            markerArmed = true;
            new Thread(() => {
                try {
                    if (injectEnter) {
                        Thread.Sleep(300);
                        keybd_event(VK_RETURN, 0, 0, UIntPtr.Zero);
                        keybd_event(VK_RETURN, 0, KEYEVENTF_KEYUP, UIntPtr.Zero);
                    }
                    Thread.Sleep(lifetimeMilliseconds);
                    PostMessage(fakeWindow, WM_CLOSE, IntPtr.Zero, IntPtr.Zero);
                } catch { PostMessage(fakeWindow, WM_CLOSE, IntPtr.Zero, IntPtr.Zero); }
            }) { IsBackground = true }.Start();
            MSG message;
            while (GetMessage(out message, IntPtr.Zero, 0, 0) > 0) {
                TranslateMessage(ref message);
                DispatchMessage(ref message);
            }
            return 0;
        } catch (Exception error) { Console.Error.WriteLine(error); return 21; }
    }

    [STAThread] public static int Main(string[] args) {
        try {
            if (args.Length == 2 && args[0] == "--integrity") {
                IntPtr process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, Int32.Parse(args[1]));
                if (process == IntPtr.Zero) return 18;
                try { Console.WriteLine(Integrity(process)); return 0; } finally { CloseHandle(process); }
            }
            if (args.Length >= 2 && args[0] == "--spawn-low") return SpawnLow(args.Skip(1).ToArray());
            if (args.Length == 6 && args[0] == "--fake") return FakeAddressBar(args[1], args[2], args[3], args[4] == "--inject-enter", Int32.Parse(args[5]));
            if (args.Length == 1 && args[0] == "--foreground-hwnd") { Console.WriteLine(GetForegroundWindow().ToInt64()); return 0; }
            return 19;
        } catch (Exception error) { Console.Error.WriteLine(error); return 20; }
    }
}
'@
    Add-Type -TypeDefinition $helperSource -OutputAssembly $helper -OutputType ConsoleApplication -ReferencedAssemblies @('System.Windows.Forms.dll')

    # Do not interpolate a Windows path into C# source: both PowerShell and C#
    # have quoting rules, and a verbatim literal is easy to generate wrongly.
    # A Base64 UTF-16 value makes the generated source independent of path
    # separators, apostrophes, quotes, and spaces.
    $markerSignalBase64 = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($fired))
    $markerSource = @"
using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;
internal static class Marker {
    public static void Main() {
        File.WriteAllText(Encoding.Unicode.GetString(Convert.FromBase64String("$markerSignalBase64")), Process.GetCurrentProcess().Id.ToString());
        Thread.Sleep(5000);
    }
}
"@
    Add-Type -TypeDefinition $markerSource -OutputAssembly $marker -OutputType ConsoleApplication

    $brokerIntegrity = (& $helper --integrity $brokers[0].Id | Select-Object -Last 1)
    if ($LASTEXITCODE -ne 0 -or $brokerIntegrity -ne '8192') {
        Skip-Harness "$BrokerProcessName.exe must run at medium integrity (8192); found '$brokerIntegrity'."
    }

    $drive = $marker.Substring(0, 1).ToLowerInvariant()
    $slashMarker = '/mnt/' + $drive + $marker.Substring(2).Replace('\', '/')
    # The fake surface is deliberately a separate process. The runner waits
    # for it so the finally block can clean both the medium parent and its
    # Low-IL child, while this process verifies the actual foreground HWND.
    $fakeBehavior = if ($Mode -eq 'SyntheticInjection') { '--inject-enter' } else { '--attestation' }
    $fakeLifetime = if ($Mode -eq 'SyntheticInjection') { 1800 } else { 20000 }
    $spawnArguments = '--spawn-low --fake "{0}" "{1}" "{2}" {3} {4}' -f $slashMarker, $ready, $marker, $fakeBehavior, $fakeLifetime
    $lowRunner = Start-Process -FilePath $helper -ArgumentList $spawnArguments -PassThru
    $metadata = Wait-ReadyMetadata -Path $ready
    if ($metadata['integrity'] -ne '4096') {
        throw "the fake Search/UIA child did not prove Low integrity (4096); found '$($metadata['integrity'])'."
    }
    $fakePid = 0
    $fakeHwnd = [Int64]0
    $editHwnd = [Int64]0
    if (-not [Int32]::TryParse($metadata['pid'], [ref] $fakePid) -or -not [Int64]::TryParse($metadata['hwnd'], [ref] $fakeHwnd) -or -not [Int64]::TryParse($metadata['edit'], [ref] $editHwnd) -or $fakeHwnd -eq 0 -or $editHwnd -eq 0) {
        throw 'the fake Search/UIA readiness metadata contains an invalid PID, top-level HWND, or EDIT HWND.'
    }
    if ($metadata['class'] -ne 'Windows.UI.Core.CoreWindow') {
        throw "the fake top-level window did not use the Search-compatible class; found '$($metadata['class'])'."
    }
    $foregroundHwnd = (& $helper --foreground-hwnd | Select-Object -Last 1)
    if ($LASTEXITCODE -ne 0 -or $foregroundHwnd -ne $fakeHwnd.ToString([Globalization.CultureInfo]::InvariantCulture)) {
        throw "the Low-IL fake UIA surface is not the actual foreground window (expected HWND $fakeHwnd, found '$foregroundHwnd')."
    }

    if ($Mode -eq 'Attestation') {
        $brokerVersion = $brokers[0].MainModule.FileVersionInfo.ProductVersion
        if (-not $brokerVersion) { throw 'The running broker has no embedded product version.' }
        Invoke-AttestationTest -Executable $AttestationTestExecutable -Hwnd $fakeHwnd -FakeProcessId $fakePid -EditHwnd $editHwnd -Marker $marker -Input $slashMarker -ExpectedVersion $brokerVersion
        if (Test-Path -LiteralPath $fired) {
            throw 'FAIL: production attestation/keyboard handling mutated the rejected Low-IL UIA edit and launched its marker.'
        }
        Write-Output 'PASS: production keyboard attestation rejected the actual Low-IL Search-class UIA surface without mutating its editable control.'
    }
    else {
        try {
            Wait-Process -Id $lowRunner.Id -Timeout 15 -ErrorAction Stop
        }
        catch {
            throw 'the restricted low-integrity synthetic-input runner did not finish within 15 seconds.'
        }
        $lowRunner.Refresh()
        if ($lowRunner.ExitCode -ne 0) {
            throw "the restricted low-integrity UIA child failed with exit code $($lowRunner.ExitCode)."
        }
        $markerName = [IO.Path]::GetFileName($marker)
        $markerProcesses = @(Get-CimInstance Win32_Process -Filter "Name='$markerName'" -ErrorAction SilentlyContinue)
        if ((Test-Path -LiteralPath $fired) -or $markerProcesses.Count -ne 0) {
            throw 'FAIL: the low-integrity fake UIA surface caused the marker executable to start.'
        }
        Write-Output 'PASS: bounded synthetic-input mode did not start a medium-integrity marker child; this is not physical-input or attestation evidence.'
    }
}
catch {
    $runFailure = $_
    throw
}
finally {
    Stop-PrivateHarnessProcesses @($helper, $marker)
    if ($work -and -not $KeepArtifacts -and (Test-Path -LiteralPath $work)) {
        foreach ($attempt in 1..5) {
            Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
            if (-not (Test-Path -LiteralPath $work)) { break }
            Start-Sleep -Milliseconds 250
        }
        if (Test-Path -LiteralPath $work) {
            $message = "could not clean private harness directory $work after five attempts."
            if ($runFailure) { Write-Warning $message } else { throw $message }
        }
    }
}
