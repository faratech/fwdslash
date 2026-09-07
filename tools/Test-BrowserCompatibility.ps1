[CmdletBinding()]
param(
    [ValidateSet('Prepare', 'Launch', 'Verify', 'Cleanup')]
    [string]$Action = 'Prepare',
    [ValidateSet('Edge', 'Chrome', 'Brave', 'Firefox')]
    [string[]]$Browser = @('Edge', 'Chrome', 'Brave', 'Firefox'),
    [ValidateSet('SetValue', 'Keyboard')]
    [string]$InputMode = 'SetValue',
    [string]$BareRootUri,
    [string]$RunRoot
)

$ErrorActionPreference = 'Stop'

function Get-BrowserPath {
    param([string]$Name)

    $executable = @{
        Edge = 'msedge.exe'
        Chrome = 'chrome.exe'
        Brave = 'brave.exe'
        Firefox = 'firefox.exe'
    }[$Name]
    foreach ($hive in 'HKCU:', 'HKLM:') {
        $key = "$hive\Software\Microsoft\Windows\CurrentVersion\App Paths\$executable"
        $path = (Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue).'(default)'
        if ($path -and (Test-Path -LiteralPath $path -PathType Leaf)) {
            return (Get-Item -LiteralPath $path).FullName
        }
    }
    return $null
}

function Get-StatePath {
    param([string]$Root)
    Join-Path $Root 'state.json'
}

function Read-State {
    param([string]$Root)
    $path = Get-StatePath $Root
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "No owned compatibility run exists at '$Root'."
    }
    return Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
}

function New-Fixture {
    param([string]$Root, [string]$Nonce)

    $initial = Join-Path $Root 'initial.html'
    $slash = Join-Path $Root 'slash.html'
    "<!doctype html><meta charset=`"utf-8`"><title>FSW Browser Fixture Slash $Nonce</title><p>Slash translation target.</p>" | Set-Content -LiteralPath $slash -NoNewline
    $expectedWebDecoy = Convert-ToMntCInput $slash
    @"
<!doctype html>
<meta charset="utf-8">
<title>FSW Browser Fixture Initial $Nonce</title>
<h1>Forward Slash Windows browser compatibility fixture</h1>
<p>This window and profile are owned by the test run. Do not use a personal browser window.</p>
<p>Manual negative tests: type a slash path and press Enter in each field below. They must remain ordinary page input; this page deliberately has an address-bar-like label and ID.</p>
<label>Address and search bar <input id="urlbar-input" aria-label="Address and search bar" type="text" data-expected-value="$expectedWebDecoy"></label>
<output id="web-decoy-result" aria-label="Web-page decoy result">pending</output>
<label>Password <input id="password-decoy" aria-label="Address and search bar" type="password" data-expected-value="fixture-password-$Nonce"></label>
<output id="password-decoy-result" aria-label="Password decoy result">pending</output>
<p>Expected browser-address-bar cases are recorded in matrix.json. The fixture never exposes field values; it only exposes whether its own field received Enter.</p>
<script>
for (const [fieldId, resultId, exactResult, changedResult] of [
  ['urlbar-input', 'web-decoy-result', 'web-page-decoy-exact-enter-delivered', 'web-page-decoy-mutated'],
  ['password-decoy', 'password-decoy-result', 'password-decoy-exact-enter-delivered', 'password-decoy-mutated'],
]) {
  document.getElementById(fieldId).addEventListener('keydown', event => {
    if (event.key === 'Enter') {
      const output = document.getElementById(resultId);
      const result = event.currentTarget.value === event.currentTarget.dataset.expectedValue
        ? exactResult
        : changedResult;
      output.textContent = result;
      output.setAttribute('aria-label', result);
    }
  });
}
</script>
"@ | Set-Content -LiteralPath $initial -NoNewline
    $native = Join-Path $Root 'native.html'
    "<!doctype html><meta charset=`"utf-8`"><title>FSW Browser Fixture Native $Nonce</title><p>Native file baseline.</p>" | Set-Content -LiteralPath $native -NoNewline
    return [pscustomobject]@{ Initial = $initial; Native = $native; Slash = $slash }
}

function Convert-ToFileUri {
    param([string]$Path)
    return ([Uri]::new((Get-Item -LiteralPath $Path).FullName)).AbsoluteUri
}

function Convert-ToMntCInput {
    param([string]$Path)
    $full = (Get-Item -LiteralPath $Path).FullName
    if (-not $full.StartsWith('C:\', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "The owned fixture must be on C: to create a /mnt/c test input: '$full'."
    }
    return '/mnt/c/' + $full.Substring(3).Replace('\', '/')
}

function Write-Matrix {
    param($State)

    $matrix = foreach ($browser in $State.Browsers) {
        $isGecko = $browser.Name -eq 'Firefox'
        [pscustomobject]@{
            Browser = $browser.Name
            Executable = $browser.Path
            NativeFileBaseline = [pscustomobject]@{
                Input = $State.NativeUri
                Expected = 'Native browser file navigation; broker must not rewrite an already-native file URI.'
                Result = 'NotRun'
                Evidence = 'Manual observation in this owned browser window only.'
            }
            MntCEnter = [pscustomobject]@{
                Input = $State.MntCInput
                Expected = if ($isGecko) { 'Native Enter; Gecko profile is explicitly unverified.' } else { "One address-bar navigation to $($State.SlashUri)." }
                Result = 'NotRun'
                Evidence = 'Manual observation in this owned browser window only.'
            }
            BareSlashRoot = [pscustomobject]@{
                Input = '/'
                Expected = if ($State.BareRootUri) { "Direct and slash navigation both commit $($State.BareRootUri) to a non-error directory page." } else { 'Unsupported until Prepare receives -BareRootUri from the current CLI.' }
                Result = 'NotRun'
                Evidence = 'No directory contents are read.'
            }
            NormalUrl = [pscustomobject]@{
                Input = 'https://example.com/'
                Expected = 'Native URL behavior; no slash-path rewrite.'
                Result = 'NotRun'
                Evidence = 'Manual observation in this owned browser window only.'
            }
            WebPageDecoy = [pscustomobject]@{
                Input = $State.MntCInput
                Expected = 'Fixture urlbar-input retains the exact submitted literal, receives Enter in-page, and does not navigate.'
                Result = 'NotRun'
                Evidence = 'Manual observation in this owned browser window only.'
            }
            PasswordDecoy = [pscustomobject]@{
                Input = $State.MntCInput
                Expected = 'Fixture password-decoy retains the exact submitted literal and receives Enter in-page; no password value is read or exposed and no navigation occurs.'
                Result = 'NotRun'
                Evidence = 'Manual observation in this owned browser window only.'
            }
            ProfileStatus = if ($isGecko) { 'Unverified; failure/native pass-through is required.' } else { 'Chromium candidate; record actual UIA profile before treating success as coverage.' }
        }
    }
    $matrix | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $State.RunRoot 'matrix.json') -NoNewline
}

function Start-OwnedBrowser {
    param($BrowserState, $State)

    $arguments = if ($BrowserState.Name -eq 'Firefox') {
        @('-no-remote', '-profile', $BrowserState.Profile, '-new-window', $State.InitialUri)
    } else {
        @("--user-data-dir=$($BrowserState.Profile)", '--no-first-run', '--no-default-browser-check', '--disable-sync', '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0', '--new-window', $State.InitialUri)
    }
    $quotedArguments = ($arguments | ForEach-Object {
        '"' + $_.Replace('"', '\"') + '"'
    }) -join ' '
    $process = Start-Process -FilePath $BrowserState.Path -ArgumentList $quotedArguments -PassThru
    $BrowserState.ProcessId = $process.Id
}

function Add-CompatibilityInputType {
    if ('CompatibilityInput' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public sealed class CompatibilitySendResult { public bool Success; public uint Count; public int Error; public int InputSize; public string Detail; }
public static class CompatibilityInput {
    [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public INPUTUNION U; }
    [StructLayout(LayoutKind.Explicit)] public struct INPUTUNION { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
    [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public UIntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public UIntPtr dwExtraInfo; }
    [DllImport("user32.dll", SetLastError=true)] static extern uint SendInput(uint n, INPUT[] i, int cb);
    [DllImport("user32.dll", SetLastError=true)] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll", SetLastError=true)] static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool attach);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
    [DllImport("user32.dll")] static extern short GetAsyncKeyState(int vKey);
    static bool ModifiersHeld() { return (GetAsyncKeyState(0x10) & 0x8000) != 0 || (GetAsyncKeyState(0x11) & 0x8000) != 0 || (GetAsyncKeyState(0x12) & 0x8000) != 0 || (GetAsyncKeyState(0x5B) & 0x8000) != 0 || (GetAsyncKeyState(0x5C) & 0x8000) != 0; }
    public static bool TryActivateOwnedWindow(IntPtr target) {
        IntPtr foreground=GetForegroundWindow(); if(foreground==target) return true;
        uint ignored; uint foregroundThread=GetWindowThreadProcessId(foreground,out ignored); uint targetThread=GetWindowThreadProcessId(target,out ignored);
        bool attached=false;
        try { if(foregroundThread!=0 && targetThread!=0 && foregroundThread!=targetThread) attached=AttachThreadInput(foregroundThread,targetThread,true); SetForegroundWindow(target); return GetForegroundWindow()==target; }
        finally { if(attached) AttachThreadInput(foregroundThread,targetThread,false); }
    }
    static void Key(System.Collections.Generic.List<INPUT> input, ushort vk, ushort scan, uint flags) { INPUT down = new INPUT(); down.type=1; down.U.ki.wVk=vk; down.U.ki.wScan=scan; down.U.ki.dwFlags=flags; input.Add(down); INPUT up=down; up.U.ki.dwFlags=flags|0x0002; input.Add(up); }
    static CompatibilitySendResult Send(System.Collections.Generic.List<INPUT> input, int inputSize) { uint count=SendInput((uint)input.Count,input.ToArray(),inputSize); int error=Marshal.GetLastWin32Error(); return new CompatibilitySendResult { Success=count==(uint)input.Count, Count=count, Error=error, InputSize=inputSize, Detail=count==(uint)input.Count ? "ok" : "SendInput did not inject every event" }; }
    public static CompatibilitySendResult SendGenericEnter() {
        int inputSize = Marshal.SizeOf(typeof(INPUT));
        int expected = IntPtr.Size == 8 ? 40 : 28;
        if (inputSize != expected) return new CompatibilitySendResult { Success=false, Count=0, Error=0, InputSize=inputSize, Detail="Unexpected INPUT size; expected " + expected };
        INPUT[] input = new INPUT[2];
        input[0].type = 1; input[0].U.ki.wVk = 0x0D;
        input[1].type = 1; input[1].U.ki.wVk = 0x0D; input[1].U.ki.dwFlags = 0x0002;
        uint count = SendInput(2, input, inputSize);
        int error = Marshal.GetLastWin32Error();
        return new CompatibilitySendResult { Success=count == 2, Count=count, Error=error, InputSize=inputSize, Detail=count == 2 ? "ok" : "SendInput did not inject both events" };
    }
    public static CompatibilitySendResult SendCtrlL() {
        int inputSize=Marshal.SizeOf(typeof(INPUT)); int expected=IntPtr.Size==8 ? 40 : 28;
        if(inputSize!=expected) return new CompatibilitySendResult { Success=false, Count=0, Error=0, InputSize=inputSize, Detail="Unexpected INPUT size; expected "+expected };
        if(ModifiersHeld()) return new CompatibilitySendResult { Success=false, Count=0, Error=0, InputSize=inputSize, Detail="A modifier is physically held; Ctrl+L was not injected" };
        var input=new System.Collections.Generic.List<INPUT>();
        INPUT ctrlDown=new INPUT(); ctrlDown.type=1; ctrlDown.U.ki.wVk=0x11; input.Add(ctrlDown);
        Key(input,0x4C,0,0);
        INPUT ctrlUp=ctrlDown; ctrlUp.U.ki.dwFlags=0x0002; input.Add(ctrlUp);
        return Send(input,inputSize);
    }
    public static CompatibilitySendResult SendUnicodeAddressText(string text) {
        int inputSize=Marshal.SizeOf(typeof(INPUT)); int expected=IntPtr.Size==8 ? 40 : 28;
        if(inputSize!=expected) return new CompatibilitySendResult { Success=false, Count=0, Error=0, InputSize=inputSize, Detail="Unexpected INPUT size; expected "+expected };
        if(ModifiersHeld()) return new CompatibilitySendResult { Success=false, Count=0, Error=0, InputSize=inputSize, Detail="A modifier is physically held; keyboard injection was not attempted" };
        var input=new System.Collections.Generic.List<INPUT>();
        INPUT ctrlDown=new INPUT(); ctrlDown.type=1; ctrlDown.U.ki.wVk=0x11; input.Add(ctrlDown);
        Key(input,0x4C,0,0);
        INPUT ctrlUp=ctrlDown; ctrlUp.U.ki.dwFlags=0x0002; input.Add(ctrlUp);
        foreach(char c in text) Key(input,0,(ushort)c,0x0004);
        Key(input,0x0D,0,0);
        return Send(input,inputSize);
    }
}
'@
}

function Get-OwnedBrowserWindows {
    param($State, $BrowserState)

    $owned = Get-CimInstance Win32_Process | Where-Object {
        $_.ExecutablePath -and $_.ExecutablePath.Equals($BrowserState.Path, [System.StringComparison]::OrdinalIgnoreCase) -and
        $_.CommandLine -and $_.CommandLine.IndexOf($BrowserState.Profile, [System.StringComparison]::OrdinalIgnoreCase) -ge 0
    }
    foreach ($process in $owned) {
        $managed = Get-Process -Id $process.ProcessId -ErrorAction SilentlyContinue
        if ($managed -and $managed.MainWindowHandle -ne 0) {
            [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$managed.MainWindowHandle)
        }
    }
}

function Wait-OwnedDocument {
    param($State, $BrowserState, [string]$ExpectedTitle)

    for ($attempt = 0; $attempt -lt 15; $attempt++) {
        $window = Get-OwnedBrowserWindows $State $BrowserState | Where-Object {
            $_.Current.Name -like "*$ExpectedTitle*"
        } | Select-Object -First 1
        if ($window) { return $window }
        Start-Sleep -Seconds 1
    }
    return $null
}

function Find-OwnedField {
    param($Window, [string]$AutomationId, [string]$ClassName)

    try {
        $elements = $Window.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition
        )
        foreach ($element in $elements) {
            if ($element.Current.ProcessId -ne $Window.Current.ProcessId) { continue }
            if ($AutomationId -and $element.Current.AutomationId -eq $AutomationId) { return $element }
            if ($ClassName -and $element.Current.ClassName -eq $ClassName -and $element.Current.Name -eq 'Address and search bar') { return $element }
        }
    } catch {
        return $null
    }
    return $null
}

function Test-OwnedNativeAncestry {
    param($Window, $Element, [string]$BrowserName)

    try {
        $windowHandle = [Int64]$Window.Current.NativeWindowHandle
        $windowPid = $Window.Current.ProcessId
        $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
        $current = $Element
        $classes = New-Object System.Collections.Generic.List[string]
        $reachedOwnedWindow = $false
        for ($depth = 0; $depth -lt 16 -and $current; $depth++) {
            if ($current.Current.ProcessId -ne $windowPid) { return $false }
            $class = $current.Current.ClassName
            if ($class -eq 'Document' -or $class -match 'RenderWidget|WebView|InternetExplorer_Server' -or
                $current.Current.ControlType -eq [System.Windows.Automation.ControlType]::Document) {
                return $false
            }
            [void]$classes.Add($class)
            if ([Int64]$current.Current.NativeWindowHandle -eq $windowHandle) {
                $reachedOwnedWindow = $true
                break
            }
            $current = $walker.GetParent($current)
        }
        if (-not $reachedOwnedWindow) { return $false }
        if ($BrowserName -eq 'Brave') {
            foreach ($required in 'BraveLocationBarView', 'BraveToolbarView', 'TopContainerView', 'BraveBrowserView', 'BraveBrowserFrameViewWin', 'NonClientView', 'BraveBrowserRootView') {
                if (-not $classes.Contains($required)) { return $false }
            }
        }
        return $true
    } catch {
        return $false
    }
}

function Find-OwnedNativeOmnibox {
    param($BrowserState, $Window)

    $elements = $Window.FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        [System.Windows.Automation.Condition]::TrueCondition
    )
    foreach ($element in $elements) {
        $isMatch = switch ($BrowserState.Name) {
            'Firefox' {
                $element.Current.ControlType -eq [System.Windows.Automation.ControlType]::ComboBox -and
                $element.Current.ClassName -eq 'urlbar-input textbox-input' -and
                $element.Current.AutomationId -eq 'urlbar-input' -and
                $element.Current.Name -eq 'Search with Google or enter address'
            }
            'Brave' {
                $element.Current.ControlType -eq [System.Windows.Automation.ControlType]::Edit -and
                $element.Current.ClassName -eq 'BraveOmniboxViewViews' -and
                $element.Current.Name -eq 'Address and search bar'
            }
            default {
                $element.Current.ControlType -eq [System.Windows.Automation.ControlType]::Edit -and
                $element.Current.ClassName -eq 'OmniboxViewViews' -and
                $element.Current.Name -eq 'Address and search bar'
            }
        }
        if ($isMatch -and (Test-OwnedNativeAncestry $Window $element $BrowserState.Name)) { return $element }
    }
    return $null
}

function Get-ElementIdentity {
    param($Element)
    if (-not $Element) { return $null }
    try {
        return [pscustomobject]@{
            Class = $Element.Current.ClassName
            AutomationId = $Element.Current.AutomationId
            Name = $Element.Current.Name
            IsPassword = $Element.Current.IsPassword
            ProcessId = $Element.Current.ProcessId
            NativeWindowHandle = $Element.Current.NativeWindowHandle
        }
    } catch {
        return [pscustomobject]@{ Error = $_.Exception.ToString() }
    }
}

function Get-ControlViewAncestry {
    param($Element)
    $nodes = New-Object System.Collections.Generic.List[object]
    try {
        $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
        $current = $Element
        for ($depth = 0; $depth -lt 16 -and $current; $depth++) {
            $identity = Get-ElementIdentity $current
            $nodes.Add([pscustomobject]@{ Depth = $depth; Identity = $identity })
            $current = $walker.GetParent($current)
        }
    } catch {
        $nodes.Add([pscustomobject]@{ Error = $_.Exception.ToString() })
    }
    return $nodes.ToArray()
}

function Get-OwnedEditInventory {
    param($Window)
    $inventory = New-Object System.Collections.Generic.List[object]
    try {
        $elements = $Window.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition
        )
        foreach ($element in $elements) {
            if ($element.Current.ControlType -eq [System.Windows.Automation.ControlType]::Edit -or
                $element.Current.ControlType -eq [System.Windows.Automation.ControlType]::ComboBox) {
                $inventory.Add([pscustomobject]@{
                    Identity = Get-ElementIdentity $element
                    Ancestry = Get-ControlViewAncestry $element
                })
            }
        }
    } catch {
        $inventory.Add([pscustomobject]@{ Error = $_.Exception.ToString() })
    }
    return $inventory.ToArray()
}

function Test-OwnedFirefoxTermsOnboarding {
    param($Window)

    try {
        $ownedPid = $Window.Current.ProcessId
        $hasDialog = $false
        $hasContainer = $false
        $hasTermsScreen = $false
        $elements = $Window.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition
        )
        foreach ($element in $elements) {
            if ($element.Current.ProcessId -ne $ownedPid) { continue }
            $class = $element.Current.ClassName
            if ($class -eq 'window-modal-dialog') { $hasDialog = $true }
            if ($class -eq 'onboardingContainer' -and $element.Current.AutomationId -eq 'multi-stage-message-root') { $hasContainer = $true }
            if ($class -match '\bTOU_ONBOARDING(?:_[A-Z]+)?\b') { $hasTermsScreen = $true }
        }
        return $hasDialog -and $hasContainer -and $hasTermsScreen
    } catch {
        return $false
    }
}

function Activate-OwnedWindow {
    param($Window)

    try {
        $targetHandle = [IntPtr]$Window.Current.NativeWindowHandle
        $preForeground = [CompatibilityInput]::GetForegroundWindow()
        try {
            $windowPattern = [System.Windows.Automation.WindowPattern]$Window.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern)
            if ($windowPattern.Current.WindowVisualState -ne [System.Windows.Automation.WindowVisualState]::Normal) {
                $windowPattern.SetWindowVisualState([System.Windows.Automation.WindowVisualState]::Normal)
            }
        } catch {}
        try { $Window.SetFocus() } catch {}
        $setForegroundReturned = [CompatibilityInput]::SetForegroundWindow($targetHandle)
        Start-Sleep -Milliseconds 100
        $activationAttached = $false
        if ([CompatibilityInput]::GetForegroundWindow() -ne $targetHandle) {
            $activationAttached = $true
            [void][CompatibilityInput]::TryActivateOwnedWindow($targetHandle)
            Start-Sleep -Milliseconds 100
        }
        $foreground = [CompatibilityInput]::GetForegroundWindow()
        return [pscustomobject]@{
            Success = $foreground -eq $targetHandle
            TargetHwnd = $targetHandle
            PreInjectForegroundHwnd = $preForeground
            ForegroundHwnd = $foreground
            SetForegroundWindowReturned = $setForegroundReturned
            AttachThreadInputAttempted = $activationAttached
        }
    } catch {
        return [pscustomobject]@{ Success = $false; TargetHwnd = $null; ForegroundHwnd = $null; Detail = $_.Exception.GetType().FullName }
    }
}

function Get-FocusedSafetyMetadata {
    try {
        $element = [System.Windows.Automation.AutomationElement]::FocusedElement
        return [pscustomobject]@{
            Class = $element.Current.ClassName
            ProcessId = $element.Current.ProcessId
            NativeWindowHandle = $element.Current.NativeWindowHandle
        }
    } catch {
        return [pscustomobject]@{ Error = $_.Exception.GetType().FullName }
    }
}

function Set-OwnedValueAndGenericEnter {
    param($Window, $Element, [string]$Value, [string]$Mode, [switch]$NoValueRead, [switch]$AllowOwnedRenderer)

    try {
        if ([string]::IsNullOrWhiteSpace($Value)) {
            return [pscustomobject]@{ Success = $false; Stage = 'NavigationText'; Detail = 'Navigation text is empty; no input was injected.' }
        }
        $targetHandle = [IntPtr]$Window.Current.NativeWindowHandle
        $preForeground = [CompatibilityInput]::GetForegroundWindow()
        $pattern = [System.Windows.Automation.ValuePattern]$Element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
        if (-not $pattern -or $pattern.Current.IsReadOnly) {
            return [pscustomobject]@{ Success = $false; Stage = 'ValuePattern'; Detail = 'ValuePattern unavailable or read-only.'; TargetHwnd = $targetHandle.ToInt64(); PreInjectForegroundHwnd = $preForeground.ToInt64(); Focused = Get-FocusedSafetyMetadata }
        }
        try {
            $windowPattern = [System.Windows.Automation.WindowPattern]$Window.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern)
            if ($windowPattern.Current.WindowVisualState -ne [System.Windows.Automation.WindowVisualState]::Normal) {
                $windowPattern.SetWindowVisualState([System.Windows.Automation.WindowVisualState]::Normal)
            }
        } catch {}
        try { $Window.SetFocus() } catch {}
        $setForegroundReturned = [CompatibilityInput]::SetForegroundWindow($targetHandle)
        Start-Sleep -Milliseconds 100
        $activationAttached = $false
        if ([CompatibilityInput]::GetForegroundWindow() -ne $targetHandle) {
            $activationAttached = $true
            [void][CompatibilityInput]::TryActivateOwnedWindow($targetHandle)
            Start-Sleep -Milliseconds 100
        }
        $Element.SetFocus()
        $focused = [System.Windows.Automation.AutomationElement]::FocusedElement
        $currentForeground = [CompatibilityInput]::GetForegroundWindow()
        if ($currentForeground -ne $targetHandle -or
            ((-not $AllowOwnedRenderer) -and $focused.Current.ProcessId -ne $Window.Current.ProcessId) -or
            $focused.Current.AutomationId -ne $Element.Current.AutomationId -or
            $focused.Current.ClassName -ne $Element.Current.ClassName) {
            return [pscustomobject]@{ Success = $false; Stage = 'ForegroundFocusProof'; Detail = 'Foreground or focused element did not exactly match the owned target.'; TargetHwnd = $targetHandle.ToInt64(); PreInjectForegroundHwnd = $preForeground.ToInt64(); ForegroundHwnd = $currentForeground.ToInt64(); SetForegroundWindowReturned = $setForegroundReturned; AttachThreadInputAttempted = $activationAttached; Focused = Get-FocusedSafetyMetadata; Target = Get-ElementIdentity $Element }
        }
        $valueAfterSet = $null
        if ($Mode -eq 'SetValue') {
            $pattern.SetValue($Value)
            if (-not $NoValueRead) { $valueAfterSet = $pattern.Current.Value }
            Start-Sleep -Milliseconds 100
        }
        $preDispatchForeground = [CompatibilityInput]::GetForegroundWindow()
        $preDispatchFocused = [System.Windows.Automation.AutomationElement]::FocusedElement
        if ($preDispatchForeground -ne $targetHandle -or
            ((-not $AllowOwnedRenderer) -and $preDispatchFocused.Current.ProcessId -ne $Window.Current.ProcessId) -or
            $preDispatchFocused.Current.AutomationId -ne $Element.Current.AutomationId -or
            $preDispatchFocused.Current.ClassName -ne $Element.Current.ClassName) {
            return [pscustomobject]@{ Success = $false; Stage = 'ForegroundFocusRecheck'; Detail = 'Foreground or focus changed before input dispatch; no Enter was injected.'; Mode = $Mode; ValueAfterSet = $valueAfterSet; TargetHwnd = $targetHandle.ToInt64(); PreInjectForegroundHwnd = $preForeground.ToInt64(); ForegroundHwnd = $preDispatchForeground.ToInt64(); Focused = Get-FocusedSafetyMetadata; Target = Get-ElementIdentity $Element }
        }
        $valueBeforeSend = if ($Mode -eq 'SetValue' -and -not $NoValueRead) { $pattern.Current.Value } else { $null }
        $send = if ($Mode -eq 'Keyboard') { [CompatibilityInput]::SendUnicodeAddressText($Value) } else { [CompatibilityInput]::SendGenericEnter() }
        return [pscustomobject]@{ Success = $send.Success; Stage = 'SendInput'; Detail = $send.Detail; Mode = $Mode; ValueAfterSet = $valueAfterSet; ValueBeforeSend = $valueBeforeSend; TargetHwnd = $targetHandle.ToInt64(); PreInjectForegroundHwnd = $preForeground.ToInt64(); ForegroundHwnd = $preDispatchForeground.ToInt64(); Focused = Get-ElementIdentity $preDispatchFocused; Target = Get-ElementIdentity $Element; SendCount = $send.Count; SendError = $send.Error; InputSize = $send.InputSize }
    } catch {
        return [pscustomobject]@{ Success = $false; Stage = 'Exception'; Detail = $_.Exception.ToString(); TargetHwnd = $null; PreInjectForegroundHwnd = $null; Focused = Get-FocusedSafetyMetadata }
    }
}

function New-CaseResult {
    param([string]$Browser, [string]$Case, [string]$Result, [string]$Detail, $Evidence)
    [pscustomobject]@{ Browser = $Browser; Case = $Case; Result = $Result; Detail = $Detail; Evidence = $Evidence }
}

function Test-UriMatch {
    param([string]$Actual, [string]$Expected)
    try {
        return ([Uri]$Actual).AbsoluteUri -eq ([Uri]$Expected).AbsoluteUri
    } catch {
        return $false
    }
}

function Get-OwnedFieldValue {
    param($Element)
    try {
        return ([System.Windows.Automation.ValuePattern]$Element.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)).Current.Value
    } catch {
        return $null
    }
}

function Get-OwnedFixtureSignal {
    param($Window, [string]$AutomationId)
    $signal = Find-OwnedField $Window $AutomationId ''
    if (-not $signal) { return $null }
    try { return $signal.Current.Name } catch { return $null }
}

function Get-OwnedNavigationState {
    param($State, $BrowserState)
    $observations = New-Object System.Collections.Generic.List[object]
    foreach ($window in (Get-OwnedBrowserWindows $State $BrowserState)) {
        try {
            $address = Find-OwnedNativeOmnibox $BrowserState $window
            $observations.Add([pscustomobject]@{
                Hwnd = $window.Current.NativeWindowHandle
                Title = $window.Current.Name
                AddressValue = if ($address) { Get-OwnedFieldValue $address } else { $null }
            })
        } catch {
            $observations.Add([pscustomobject]@{ Error = $_.Exception.ToString() })
        }
    }
    return $observations.ToArray()
}

function Invoke-OwnedNavigationCase {
    param($State, $BrowserState, $Window, [string]$NavigationText, [string]$ExpectedTitle, [string]$ExpectedUri, [string]$Case)

    $address = Find-OwnedNativeOmnibox $BrowserState $Window
    if (-not $address) {
        return New-CaseResult $BrowserState.Name $Case 'Unsupported' 'No strict native browser address-bar profile was observed in this owned window.'
    }
    $injection = Set-OwnedValueAndGenericEnter $Window $address $NavigationText $InputMode
    if (-not $injection -or -not $injection.Success) {
        $detail = if ($injection) { "$($injection.Stage): $($injection.Detail); count=$($injection.SendCount), error=$($injection.SendError), size=$($injection.InputSize)" } else { 'Owned foreground/focus proof or ValuePattern setup failed before injection.' }
        return New-CaseResult $BrowserState.Name $Case 'Blocked' $detail $injection
    }
    $committedWindow = Wait-OwnedDocument $State $BrowserState $ExpectedTitle
    if (-not $committedWindow) {
        return New-CaseResult $BrowserState.Name $Case 'Fail' 'The expected distinct target document title did not commit.' ([pscustomobject]@{ Injection = $injection; Observed = Get-OwnedNavigationState $State $BrowserState })
    }
    $committedValue = Get-OwnedCommittedUri $State $BrowserState $committedWindow
    if (-not (Test-UriMatch $committedValue $ExpectedUri)) {
        return New-CaseResult $BrowserState.Name $Case 'Fail' "Expected committed URI '$ExpectedUri'; observed '$committedValue'." ([pscustomobject]@{ Injection = $injection; Observed = Get-OwnedNavigationState $State $BrowserState })
    }
    return New-CaseResult $BrowserState.Name $Case 'Pass' "Distinct document and committed URI matched '$ExpectedUri'."
}

function Get-OwnedDevToolsPages {
    param($State, $BrowserState)
    if ($BrowserState.Name -eq 'Firefox' -or -not (Get-OwnedBrowserWindows $State $BrowserState | Select-Object -First 1)) {
        return @()
    }
    $portFile = Join-Path $BrowserState.Profile 'DevToolsActivePort'
    if (-not (Test-Path -LiteralPath $portFile -PathType Leaf)) { return @() }
    try {
        $lines = Get-Content -LiteralPath $portFile -ErrorAction Stop
        $port = 0
        if (-not [int]::TryParse($lines[0], [ref]$port) -or $port -lt 1 -or $port -gt 65535) { return @() }
        $response = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$port/json/list" -TimeoutSec 2 -ErrorAction Stop
        $pages = @($response.Content | ConvertFrom-Json | Where-Object { $_.type -eq 'page' })
        return $pages
    } catch {
        return @()
    }
}

function Get-OwnedDevToolsPage {
    param($State, $BrowserState, [string]$ExpectedUri, [string]$ExpectedTitle)
    $pages = Get-OwnedDevToolsPages $State $BrowserState
    if ($BrowserState.DevToolsTargetId) {
        return $pages | Where-Object { $_.id -eq $BrowserState.DevToolsTargetId } | Select-Object -First 1
    }
    return $pages | Where-Object {
        (Test-UriMatch $_.url $ExpectedUri) -and $_.title -like "*$ExpectedTitle*"
    } | Select-Object -First 1
}

function Get-OwnedCommittedUri {
    param($State, $BrowserState, $Window)
    $page = Get-OwnedDevToolsPage $State $BrowserState '' ''
    if ($page) { return $page.url }
    $address = Find-OwnedNativeOmnibox $BrowserState $Window
    return if ($address) { Get-OwnedFieldValue $address } else { $null }
}

function Wait-OwnedUri {
    param($State, $BrowserState, [string]$ExpectedUri)
    for ($attempt = 0; $attempt -lt 15; $attempt++) {
        $page = Get-OwnedDevToolsPage $State $BrowserState '' ''
        if ($page -and (Test-UriMatch $page.url $ExpectedUri)) {
            return Get-OwnedBrowserWindows $State $BrowserState | Select-Object -First 1
        }
        foreach ($window in (Get-OwnedBrowserWindows $State $BrowserState)) {
            $address = Find-OwnedNativeOmnibox $BrowserState $window
            if ($address -and (Test-UriMatch (Get-OwnedFieldValue $address) $ExpectedUri)) {
                return $window
            }
        }
        Start-Sleep -Seconds 1
    }
    return $null
}

function Test-NonErrorDirectoryWindow {
    param($State, $BrowserState, $Window)
    try {
        $page = Get-OwnedDevToolsPage $State $BrowserState '' ''
        $title = if ($page) { $page.title } else { $Window.Current.Name }
        if (-not $title) { return $false }
        $normalized = $title.ToLowerInvariant()
        return -not ($normalized.Contains('error') -or $normalized.Contains('search') -or $normalized.Contains('not found') -or $normalized.Contains('can''t reach') -or $normalized.Contains('failed'))
    } catch {
        return $false
    }
}

function Invoke-OwnedBareRootCase {
    param($State, $BrowserState, $Window, [string]$NavigationText, [string]$Case)
    $address = Find-OwnedNativeOmnibox $BrowserState $Window
    if (-not $address) { return New-CaseResult $BrowserState.Name $Case 'Unsupported' 'No strict native browser address-bar profile was observed in this owned window.' }
    $injection = Set-OwnedValueAndGenericEnter $Window $address $NavigationText $InputMode
    if (-not $injection -or -not $injection.Success) {
        return New-CaseResult $BrowserState.Name $Case 'Blocked' "$($injection.Stage): $($injection.Detail)" $injection
    }
    $directory = Wait-OwnedUri $State $BrowserState $State.BareRootUri
    if (-not $directory) {
        return New-CaseResult $BrowserState.Name $Case 'Fail' "Expected committed root URI '$($State.BareRootUri)' was not observed." ([pscustomobject]@{ Injection = $injection; Observed = Get-OwnedNavigationState $State $BrowserState })
    }
    if (-not (Test-NonErrorDirectoryWindow $State $BrowserState $directory)) {
        $page = Get-OwnedDevToolsPage $State $BrowserState '' ''
        return New-CaseResult $BrowserState.Name $Case 'Fail' 'The committed root URI showed an empty or error/search-like title; no directory contents were read.' ([pscustomobject]@{ Injection = $injection; Title = if ($page) { $page.title } else { $directory.Current.Name }; Url = if ($page) { $page.url } else { Get-OwnedCommittedUri $State $BrowserState $directory } })
    }
    return New-CaseResult $BrowserState.Name $Case 'Pass' "Committed '$($State.BareRootUri)' with a non-error directory title; no directory contents were read."
}

function Invoke-OwnedBrowserVerification {
    param($State)

    Add-Type -AssemblyName UIAutomationClient
    Add-CompatibilityInputType
    $results = New-Object System.Collections.Generic.List[object]
    $inventories = New-Object System.Collections.Generic.List[object]
    foreach ($browserState in $State.Browsers) {
        $initialTitle = "FSW Browser Fixture Initial $($State.Nonce)"
        $nativeTitle = "FSW Browser Fixture Native $($State.Nonce)"
        $slashTitle = "FSW Browser Fixture Slash $($State.Nonce)"
        $window = Wait-OwnedDocument $State $browserState $initialTitle
        if (-not $window) {
            $results.Add((New-CaseResult $browserState.Name 'FixtureGate' 'Blocked' 'Owned initial fixture title was not visible. This may be onboarding; no UI was accepted or changed.'))
            continue
        }
        $inventories.Add([pscustomobject]@{
            Browser = $browserState.Name
            Window = Get-ElementIdentity $window
            Edits = Get-OwnedEditInventory $window
        })
        if ($browserState.Name -eq 'Firefox') {
            if (Test-OwnedFirefoxTermsOnboarding $window) {
                $results.Add((New-CaseResult $browserState.Name 'GeckoProfile' 'Blocked' 'Owned Firefox profile is covered by its first-run Terms onboarding. Accepting or bypassing that legal flow is outside this harness; no URL-bar input was attempted.'))
                continue
            }
            $activation = Activate-OwnedWindow $window
            if (-not $activation.Success) {
                $results.Add((New-CaseResult $browserState.Name 'GeckoProfile' 'Blocked' 'Owned Firefox fixture window could not be proven foreground before URL-bar focus.' $activation))
                continue
            }
            $focus = [CompatibilityInput]::SendCtrlL()
            if (-not $focus.Success) {
                $results.Add((New-CaseResult $browserState.Name 'GeckoProfile' 'Blocked' 'Owned Firefox URL-bar focus shortcut was not injected.' $focus))
                continue
            }
            Start-Sleep -Milliseconds 300
            if (-not (Find-OwnedNativeOmnibox $browserState $window)) {
                $results.Add((New-CaseResult $browserState.Name 'GeckoProfile' 'Unsupported' 'Fixture loaded, but the strict observed native Firefox URL-bar profile was not present. No URL-bar input was attempted.'))
                continue
            }
            $results.Add((New-CaseResult $browserState.Name 'GeckoProfile' 'Pass' 'Observed the strict native Firefox ComboBox URL-bar profile in the owned fixture window.'))
        } else {
            $devToolsPage = Get-OwnedDevToolsPage $State $browserState $State.InitialUri $initialTitle
            if (-not $devToolsPage) {
                $results.Add((New-CaseResult $browserState.Name 'DevToolsMetadata' 'Unsupported' 'Owned profile did not expose a matching localhost DevTools page target.'))
                continue
            }
            $browserState | Add-Member -NotePropertyName DevToolsTargetId -NotePropertyValue $devToolsPage.id -Force
        }

        $baseline = Invoke-OwnedNavigationCase $State $browserState $window $State.NativeUri $nativeTitle $State.NativeUri 'NativeFileBaseline'
        $results.Add($baseline)
        if ($baseline.Result -ne 'Pass') {
            $results.Add((New-CaseResult $browserState.Name 'MntCEnter' 'Blocked' 'Native file baseline/injection precondition did not pass.'))
            continue
        }

        $window = Wait-OwnedDocument $State $browserState $nativeTitle
        if ($State.BareRootUri) {
            $directRoot = Invoke-OwnedBareRootCase $State $browserState $window $State.BareRootUri 'BareRootDirect'
            $results.Add($directRoot)
            if ($directRoot.Result -eq 'Pass') {
                $rootWindow = Wait-OwnedUri $State $browserState $State.BareRootUri
                $returnInitial = Invoke-OwnedNavigationCase $State $browserState $rootWindow $State.InitialUri $initialTitle $State.InitialUri 'BareRootReturnInitial'
                $results.Add($returnInitial)
                if ($returnInitial.Result -eq 'Pass') {
                    $initialWindow = Wait-OwnedDocument $State $browserState $initialTitle
                    $bareSlash = Invoke-OwnedBareRootCase $State $browserState $initialWindow '/' 'BareSlashRoot'
                    $results.Add($bareSlash)
                    if ($bareSlash.Result -eq 'Pass') {
                        $rootWindow = Wait-OwnedUri $State $browserState $State.BareRootUri
                        $restoreNative = Invoke-OwnedNavigationCase $State $browserState $rootWindow $State.NativeUri $nativeTitle $State.NativeUri 'BareRootRestoreNative'
                        $results.Add($restoreNative)
                    }
                } else {
                    $results.Add((New-CaseResult $browserState.Name 'BareSlashRoot' 'Blocked' 'Could not return to the distinct initial fixture after direct root baseline.'))
                }
            } else {
                $results.Add((New-CaseResult $browserState.Name 'BareSlashRoot' 'Blocked' 'Direct native root baseline did not pass.'))
            }
        } else {
            $results.Add((New-CaseResult $browserState.Name 'BareSlashRoot' 'Unsupported' 'Prepare did not receive an explicit -BareRootUri from the current CLI.'))
        }

        $window = Wait-OwnedDocument $State $browserState $nativeTitle
        if (-not $window) {
            $results.Add((New-CaseResult $browserState.Name 'MntCEnter' 'Blocked' 'Native fixture was not restored after root comparison.'))
            continue
        }
        $slash = Invoke-OwnedNavigationCase $State $browserState $window $State.MntCInput $slashTitle $State.SlashUri 'MntCEnter'
        $results.Add($slash)
        if ($slash.Result -ne 'Pass') { continue }

        $window = Wait-OwnedDocument $State $browserState $slashTitle
        $address = if ($window) { Find-OwnedNativeOmnibox $browserState $window } else { $null }
        $normalInjection = if ($address) { Set-OwnedValueAndGenericEnter $window $address 'https://example.com/' $InputMode } else { $false }
        if ($normalInjection -and $normalInjection.Success) {
            $normalWindow = Wait-OwnedUri $State $browserState 'https://example.com/'
            if ($normalWindow) {
                $results.Add((New-CaseResult $browserState.Name 'NormalUrl' 'Pass' 'Observed the exact expected ordinary URL after bounded completion polling.' $normalInjection))
            } else {
                $observed = Get-OwnedNavigationState $State $browserState
                $results.Add((New-CaseResult $browserState.Name 'NormalUrl' 'Fail' 'The exact expected ordinary URL was not observed before the bounded completion timeout.' ([pscustomobject]@{ Injection = $normalInjection; Observed = $observed })))
            }
        } else {
            $results.Add((New-CaseResult $browserState.Name 'NormalUrl' 'Blocked' 'Owned foreground/focus proof or generic injection failed.'))
        }

        $normalWindow = Get-OwnedBrowserWindows $State $browserState | Select-Object -First 1
        $normalAddress = if ($normalWindow) { Find-OwnedNativeOmnibox $browserState $normalWindow } else { $null }
        $restore = if ($normalAddress) { Set-OwnedValueAndGenericEnter $normalWindow $normalAddress $State.InitialUri $InputMode } else { $false }
        $window = if ($restore -and $restore.Success) { Wait-OwnedDocument $State $browserState $initialTitle } else { $null }
        if (-not $window) {
            $results.Add((New-CaseResult $browserState.Name 'NegativeFixtureGate' 'Blocked' 'Could not return to the distinct initial fixture document.'))
            continue
        }
        if ($InputMode -eq 'Keyboard') {
            $results.Add((New-CaseResult $browserState.Name 'WebPageDecoy' 'NotRun' 'Keyboard mode intentionally sends Ctrl+L and therefore cannot prove page-field behavior.'))
            $results.Add((New-CaseResult $browserState.Name 'PasswordDecoy' 'NotRun' 'Keyboard mode intentionally sends Ctrl+L and therefore cannot prove password-field behavior.'))
            continue
        }
        $decoy = if ($window) { Find-OwnedField $window 'urlbar-input' '' } else { $null }
        $decoyInjection = if ($decoy) { Set-OwnedValueAndGenericEnter $window $decoy $State.MntCInput $InputMode -NoValueRead -AllowOwnedRenderer } else { $false }
        if ($decoyInjection -and $decoyInjection.Success) {
            $fixtureWindow = Wait-OwnedDocument $State $browserState $initialTitle
            $signal = if ($fixtureWindow) { Get-OwnedFixtureSignal $fixtureWindow 'web-decoy-result' } else { $null }
            if ($signal -eq 'web-page-decoy-exact-enter-delivered' -and $fixtureWindow) {
                $results.Add((New-CaseResult $browserState.Name 'WebPageDecoy' 'Pass' 'Owned fixture retained the exact non-secret literal, received Enter in its page field, and remained the initial document.'))
            } else {
                $results.Add((New-CaseResult $browserState.Name 'WebPageDecoy' 'Fail' 'Fixture did not both retain the exact non-secret literal and expose its Enter-delivery signal while remaining the initial document.'))
            }
        } else {
            $results.Add((New-CaseResult $browserState.Name 'WebPageDecoy' 'Blocked' 'Fixture decoy foreground/focus proof or generic injection failed.'))
        }

        $window = Wait-OwnedDocument $State $browserState $initialTitle
        $password = if ($window) { Find-OwnedField $window 'password-decoy' '' } else { $null }
        $secret = 'fixture-password-' + $State.Nonce
        $passwordInjection = if ($password) { Set-OwnedValueAndGenericEnter $window $password $secret $InputMode -NoValueRead -AllowOwnedRenderer } else { $false }
        if ($passwordInjection -and $passwordInjection.Success) {
            $fixtureWindow = Wait-OwnedDocument $State $browserState $initialTitle
            $signal = if ($fixtureWindow) { Get-OwnedFixtureSignal $fixtureWindow 'password-decoy-result' } else { $null }
            if ($signal -eq 'password-decoy-exact-enter-delivered' -and $fixtureWindow) {
                $results.Add((New-CaseResult $browserState.Name 'PasswordDecoy' 'Pass' 'Owned fixture retained its exact password literal, received Enter in its password field, and remained the initial document; no password value was read or exposed.'))
            } else {
                $results.Add((New-CaseResult $browserState.Name 'PasswordDecoy' 'Fail' 'Fixture did not both retain its exact password literal and expose its non-secret Enter-delivery signal while remaining the initial document.'))
            }
        } else {
            $results.Add((New-CaseResult $browserState.Name 'PasswordDecoy' 'Blocked' 'Fixture password foreground/focus proof or generic injection failed.'))
        }
    }
    $results | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $State.RunRoot 'results.json') -NoNewline
    $inventories | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $State.RunRoot 'uia-edit-inventory.json') -NoNewline
    $State | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Get-StatePath $State.RunRoot) -NoNewline
    return $results
}

function Remove-OwnedBrowsers {
    param($State)

    $ownedPaths = @($State.Browsers | ForEach-Object { $_.Path })
    $ownedProfiles = @($State.Browsers | ForEach-Object { $_.Profile })
    Get-CimInstance Win32_Process | ForEach-Object {
        $process = $_
        $commandLine = $process.CommandLine
        if (-not $commandLine) { return }
        $profileMatch = $ownedProfiles | Where-Object { $commandLine.IndexOf($_, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 } | Select-Object -First 1
        $pathMatch = $ownedPaths | Where-Object { $_.Equals($process.ExecutablePath, [System.StringComparison]::OrdinalIgnoreCase) } | Select-Object -First 1
        if ($profileMatch -and $pathMatch) {
            Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
    foreach ($profile in $ownedProfiles) {
        $exited = $false
        for ($attempt = 0; $attempt -lt 20; $attempt++) {
            $remaining = @(Get-CimInstance Win32_Process | Where-Object {
                $process = $_
                $process.ExecutablePath -and
                ($ownedPaths | Where-Object { $_.Equals($process.ExecutablePath, [System.StringComparison]::OrdinalIgnoreCase) } | Select-Object -First 1) -and
                $process.CommandLine -and $process.CommandLine.IndexOf($profile, [System.StringComparison]::OrdinalIgnoreCase) -ge 0
            })
            if ($remaining.Count -eq 0) { $exited = $true; break }
            Start-Sleep -Milliseconds 500
        }
        if (-not $exited) { return $false }
    }
    return $true
}

if ($Action -eq 'Prepare') {
    if (-not $RunRoot) {
        $RunRoot = Join-Path $env:TEMP ("fsw-browser-compat-" + [guid]::NewGuid().ToString('N'))
    }
    New-Item -ItemType Directory -Path $RunRoot -Force | Out-Null
    $nonce = [guid]::NewGuid().ToString('N')
    $fixture = New-Fixture $RunRoot $nonce
    $browsers = foreach ($name in $Browser) {
        $path = Get-BrowserPath $name
        if ($path) {
            [pscustomobject]@{
                Name = $name
                Path = $path
                Profile = Join-Path $RunRoot ("profile-" + $name.ToLowerInvariant())
                ProcessId = $null
                DevToolsTargetId = $null
            }
        }
    }
    $state = [pscustomobject]@{
        RunRoot = $RunRoot
        Nonce = $nonce
        Initial = $fixture.Initial
        InitialUri = Convert-ToFileUri $fixture.Initial
        Native = $fixture.Native
        NativeUri = Convert-ToFileUri $fixture.Native
        Slash = $fixture.Slash
        SlashUri = Convert-ToFileUri $fixture.Slash
        MntCInput = Convert-ToMntCInput $fixture.Slash
        BareRootUri = $BareRootUri
        DiagnosticLog = Join-Path $RunRoot 'broker.log'
        Browsers = @($browsers)
    }
    $state | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Get-StatePath $RunRoot) -NoNewline
    Write-Matrix $state
    Write-Output "Prepared owned fixture: $RunRoot"
    Write-Output "Matrix: $(Join-Path $RunRoot 'matrix.json')"
    Write-Output "Coordinated unpackaged broker invocation (root owns this test PID; do not run while the packaged broker owns the mutex):"
    Write-Output "  `$env:__COMPAT_LAYER = 'RunAsInvoker'"
    Write-Output "  `$env:FSW_DIAGNOSTIC_LOG = '$($state.DiagnosticLog)'"
    Write-Output "  & '<unpackaged-built-fswbroker.exe>'"
    Write-Output 'After the coordinator stops only that unpackaged test PID, restore the installed broker with the product''s existing normal broker-start mechanism. This harness never changes package files, registry settings, scheduled tasks, or normal browser windows.'
    exit 0
}

if (-not $RunRoot) { throw '-RunRoot is required for Launch and Cleanup.' }
$state = Read-State $RunRoot

if ($Action -eq 'Launch') {
    foreach ($browserState in $state.Browsers) {
        New-Item -ItemType Directory -Path $browserState.Profile -Force | Out-Null
        Start-OwnedBrowser $browserState $state
    }
    $state | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Get-StatePath $RunRoot) -NoNewline
    Write-Output 'Owned browser windows launched. Perform only the matrix.json cases in these profiles; this command did not start or stop any broker.'
    exit 0
}

if ($Action -eq 'Verify') {
    Invoke-OwnedBrowserVerification $state | Format-Table -AutoSize
    Write-Output "Results: $(Join-Path $state.RunRoot 'results.json')"
    exit 0
}

if (-not (Remove-OwnedBrowsers $state)) {
    throw 'A verified owned browser process remained after the bounded shutdown wait; its profile was not deleted.'
}
Remove-Item -LiteralPath $RunRoot -Recurse -Force
Write-Output "Removed only the owned browser processes and fixture at $RunRoot."
