#![windows_subsystem = "windows"]

use fsw_core::*;
use fsw_path::{RenderBuf, eq_ignore_case};
use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::sync::{mpsc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
    CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationLegacyIAccessiblePattern,
    IUIAutomationValuePattern, UIA_ComboBoxControlTypeId, UIA_EditControlTypeId,
    UIA_LegacyIAccessiblePatternId, UIA_ValuePatternId, UIA_ValueValuePropertyId,
};
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
use windows::core::{BOOL, BSTR, Interface};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM,
    LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentThreadId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYEVENTF_KEYUP, SendInput, VK_ESCAPE, VK_RETURN,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    SEE_MASK_ASYNCOK, SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW, Shell_NotifyIconW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, FindWindowExW, GetClassNameW, GetCursorPos, GetDlgItem,
    GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, HHOOK, HICON, HWND_MESSAGE,
    IDC_ARROW, KBDLLHOOKSTRUCT, KillTimer, LLKHF_UP, LoadCursorW, LoadIconW, MF_CHECKED, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, PostThreadMessageW,
    RegisterClassExW, RegisterWindowMessageW, SW_SHOWNORMAL, SetForegroundWindow,
    SetMenuDefaultItem, SetTimer, SetWindowsHookExW, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_KEYUP, WM_LBUTTONDBLCLK,
    WM_LBUTTONUP, WM_NULL, WM_QUERYENDSESSION, WM_QUIT, WM_RBUTTONUP, WM_SYSKEYUP, WM_TIMER,
    WNDCLASSEXW, WNDPROC, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

const MUTEX_NAME: &str = "Local\\ForwardSlashWindows.Broker";
/// Class of the worker window. Never discovered by anyone: the worker HWND
/// travels through `WORKER_WINDOW`, not `FindWindowW`.
const WORKER_WINDOW_CLASS: &str = "ForwardSlashWindows.BrokerWorker";

/// Tray callback (broker window).
const TRAY_MESSAGE: u32 = WM_APP + 1;
/// Hook -> worker: `wParam` is a [`SurfaceKind`], `lParam` the foreground HWND.
const PROCESS_ENTER: u32 = WM_APP + 2;
/// UI thread -> worker: `lParam` owns a `Box<String>` with the path to open.
const WORKER_OPEN_PATH: u32 = WM_APP + 3;
/// Persist thread -> broker window: show the "could not be saved" balloon on
/// the thread that owns the icon.
const PERSIST_FAILED: u32 = WM_APP + 4;

const TRAY_ID: usize = 1;
const HEALTH_TIMER: usize = 1;
/// Tick interval while a driver is on the other end of the filter port.
const HEALTH_INTERVAL_CONNECTED_MS: u32 = 5_000;
/// Tick interval with no driver — the shipping configuration. Nothing on the
/// tick is urgent: a reconnect probe, the tray-icon retry and the hook re-arm.
const HEALTH_INTERVAL_IDLE_MS: u32 = 60_000;
/// Minimum spacing of the tray-icon retry and the hook re-arm, independent of
/// the tick interval so a connected driver does not re-arm the hook every 5 s.
const MAINTENANCE_INTERVAL_MS: u64 = 60_000;
const REPLAY_MARKER: usize = 0x4653_572F;

/// Shutdown grace for the Enter worker: 50 polls, 10 ms apart.
const WORKER_STOP_ATTEMPTS: u32 = 50;
const WORKER_STOP_POLL_MS: u64 = 10;

/// Ceiling on one `fwdslash integration <id> enable` child. The transaction it
/// runs is a directory copy plus a handful of registry writes; anything past
/// this is a hang (a locked payload file, a wedged `reg.exe`), not slowness,
/// and the sweep must not leave a child of the resident broker running for the
/// rest of the session.
const ADAPTER_UPGRADE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// `try_wait` spacing while an upgrade child runs.
const ADAPTER_UPGRADE_POLL_MS: u64 = 250;
/// Balloon retry spacing and count (~10 s) for the sweep's one notification.
const ADAPTER_UPGRADE_NOTIFY_INTERVAL_MS: u64 = 500;
const ADAPTER_UPGRADE_NOTIFY_ATTEMPTS: u32 = 20;
/// Pause before the sweep's single retry of a failed adapter (issue #56).
///
/// The failures this sweep sees right after an MSIX update are timing: the old
/// payload tree still has a `fwdslash.exe` open in a console that just ran a
/// `doskey` macro, or the copy out of `WindowsApps` was still being staged.
/// Both clear in seconds, and a retry that lands is a balloon the user never
/// has to read.
const ADAPTER_RETRY_DELAY_MS: u64 = 5_000;

/// How often the broker even *considers* an update cycle. The real cadence is
/// the CLI's (`check_is_due`, 24 h): this only decides how often it is asked,
/// so a machine that is up for a week still checks daily and one that is up
/// for an hour costs nothing.
const UPDATE_CONSIDER_INTERVAL_MS: u64 = 6 * 60 * 60 * 1_000;
/// Nothing update-related happens for the first five minutes of a broker's
/// life. Logon is the busiest moment on the machine, the adapter sweep is
/// already running, and an update that force-closes the package seconds after
/// the user signed in is the worst possible moment for one.
const UPDATE_FIRST_DELAY_MS: u64 = 5 * 60 * 1_000;
/// Ceiling on `fwdslash update check`. A Store round trip on a bad network,
/// or `curl.exe` against GitHub, both answer or give up well inside this.
const UPDATE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
/// Ceiling on `fwdslash update install`. The child only *starts* the install —
/// the Store's own download runs in the Store's service, and the relaunch is a
/// scheduled task — so this bounds a handful of WinRT calls, not a download.
const UPDATE_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Consecutive `update install` errors before the broker says so out loud.
/// One is noise (the Store was mid-something); two in a row is a state the
/// user's own click can get out of.
const UPDATE_FAILURES_BEFORE_BALLOON: u32 = 2;
/// `CREATE_NO_WINDOW`. `fwdslash.exe` is a console binary, and the sweep runs
/// unattended at logon: without this every outdated adapter flashes a console
/// window on the user's desktop.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const MENU_SETTINGS: u32 = 1001;
const MENU_PAUSE: u32 = 1002;
const MENU_EXIT: u32 = 1003;
const MENU_OPEN_ROOT: u32 = 1004;
const MENU_WINDOWS: u32 = 1005;
const MENU_CMD: u32 = 1006;
const MENU_WINDOWS_POWERSHELL: u32 = 1007;
const MENU_POWERSHELL: u32 = 1008;
const MENU_VERSION: u32 = 1009;
/// First id of the "Open distribution" submenu; item *i* is `BASE + i`.
const MENU_DISTRO_BASE: u32 = 1100;
/// The submenu is a convenience, not an inventory: cap it so the id range
/// stays private to the submenu no matter how many distributions exist.
const MENU_DISTRO_MAX: usize = 64;

/// `cmb13` and `edt1`: the path combo and file-name edit of the classic
/// common-item dialog. Their presence is what separates an Open/Save dialog
/// from every other `#32770` (a Find box, a property sheet, a message box).
const DIALOG_PATH_COMBO: i32 = 0x47C;
const DIALOG_FILE_NAME_EDIT: i32 = 0x480;

/// Icon resource id, kept in step with `include/fsw_resources.h`.
const IDI_FSW_APP: u16 = 101;

// Port, protocol version and distribution capacity come from `fsw_core`
// (hand copies of `include/fsw_filter_protocol.h`).
const FSW_OPERATION_REPLACE_MAPPINGS: u32 = 1;
const FSW_MAX_DISTRIBUTION_NAME: usize = 128;

#[repr(C)]
struct FswMappingMessage {
    version: u32,
    size: u32,
    operation: u32,
    reserved: u32,
    generation: u64,
    distribution_count: u32,
    distributions: [[u16; FSW_MAX_DISTRIBUTION_NAME]; FSW_FILTER_MAX_DISTRIBUTIONS],
}

#[cfg(windows)]
#[link(name = "fltlib", kind = "raw-dylib")]
unsafe extern "system" {
    fn FilterConnectCommunicationPort(
        lpPortName: *const u16,
        dwOptions: u32,
        lpContext: *const std::ffi::c_void,
        wSizeOfContext: u16,
        lpSecurityAttributes: *mut std::ffi::c_void,
        hPort: *mut HANDLE,
    ) -> i32;

    fn FilterSendMessage(
        hPort: HANDLE,
        lpInBuffer: *const std::ffi::c_void,
        dwInBufferSize: u32,
        lpOutBuffer: *mut std::ffi::c_void,
        dwOutBufferSize: u32,
        lpBytesReturned: *mut u32,
    ) -> i32;
}

/// The classification the hook made, carried to the worker in the
/// `PROCESS_ENTER` `wParam` so the worker never re-runs `classify_surface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum SurfaceKind {
    Unknown = 0,
    Explorer = 1,
    Run = 2,
    Search = 3,
    CommonDialog = 4,
}

impl SurfaceKind {
    const fn from_wparam(value: usize) -> Self {
        match value {
            1 => Self::Explorer,
            2 => Self::Run,
            3 => Self::Search,
            4 => Self::CommonDialog,
            _ => Self::Unknown,
        }
    }
}

static PAUSED: AtomicBool = AtomicBool::new(false);
/// Pause writes started but not yet finished, so [`reload_settings`] can tell
/// "the registry disagrees with us" from "the registry has not caught up with
/// us yet".
///
/// The tray toggle changes `PAUSED` in memory and persists it off-thread (see
/// [`request_persist_disabled`]), which leaves a window in which the stored
/// value is still the old one. A state-changed broadcast landing inside that
/// window — anyone's, including the one this broker's own write posts — would
/// otherwise be read as an external change and revert the toggle.
static PERSIST_IN_FLIGHT: AtomicU32 = AtomicU32::new(0);
static ENTER_DOWN: AtomicBool = AtomicBool::new(false);
static SUPPRESS_ENTER_UP: AtomicBool = AtomicBool::new(false);

static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);
static BROKER_WINDOW: AtomicIsize = AtomicIsize::new(0);
static FILTER_PORT: AtomicIsize = AtomicIsize::new(-1);

/// The worker's message-only window and thread id. The hook reads the window
/// on every swallowed Enter, so it lives in an atomic rather than behind a
/// lock: a keyboard hook must never wait on anything.
static WORKER_WINDOW: AtomicIsize = AtomicIsize::new(0);
static WORKER_THREAD: AtomicU32 = AtomicU32::new(0);
static WORKER_STOPPED: AtomicBool = AtomicBool::new(false);
static WORKER_JOIN: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// One FIFO writer prevents rapid pause/resume toggles from completing their
/// registry writes out of order. The sender is intentionally unbounded: a
/// tray command must never wait for a slow `reg.exe` child.
static PERSIST_QUEUE: Mutex<Option<mpsc::Sender<bool>>> = Mutex::new(None);

/// Whether `Shell_NotifyIconW(NIM_ADD)` has actually succeeded. It fails with
/// `ERROR_TIMEOUT` when the shell is busy — precisely the logon moment the
/// MSIX startup task launches us — and every later `NIM_MODIFY` (tooltip,
/// balloon) is silently discarded until an add lands.
static ICON_ADDED: AtomicBool = AtomicBool::new(false);

/// Whether the adapter-upgrade sweep has already been started. One pass per
/// process: the broker is restarted by the logon task and by every product
/// update, which is exactly when the payload can be stale.
static ADAPTER_UPGRADE_STARTED: AtomicBool = AtomicBool::new(false);

/// Current `SetTimer` interval, so the timer is only re-created when the
/// wanted interval actually changes.
static HEALTH_INTERVAL_MS: AtomicU32 = AtomicU32::new(HEALTH_INTERVAL_IDLE_MS);
/// `GetTickCount64` of the last tray/hook maintenance pass.
static LAST_MAINTENANCE_MS: AtomicU64 = AtomicU64::new(0);

/// `GetTickCount64` when this broker started, so the first update cycle can be
/// held off for [`UPDATE_FIRST_DELAY_MS`] without a second timer.
static BROKER_START_MS: AtomicU64 = AtomicU64::new(0);
/// `GetTickCount64` of the last update cycle's *start*; `0` means none has run
/// in this process.
static LAST_UPDATE_TICK_MS: AtomicU64 = AtomicU64::new(0);
/// An update cycle is on the `fsw-update` thread right now. Cleared by
/// [`UpdateCycleGuard`], so no early return can strand it.
static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);
/// The Enter worker is inside a request. An install that force-closes the
/// package while the worker is rewriting an address bar would take the user's
/// keystroke with it, so the update cycle never starts while this is set.
static WORKER_BUSY: AtomicBool = AtomicBool::new(false);
/// The version the last update balloon was about, so one available update
/// produces one balloon however many cycles see it.
static UPDATE_NOTIFIED_TAG: Mutex<Option<String>> = Mutex::new(None);
/// Consecutive `update install` failures; any other outcome resets it.
static UPDATE_INSTALL_FAILURES: AtomicU32 = AtomicU32::new(0);

/// The distribution list the "Open distribution" submenu was built from, so a
/// click resolves to the name that was on screen rather than to a re-read of
/// the registry.
static MENU_DISTRIBUTIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

thread_local! {
    /// Owned by the worker thread for its whole life. The UI thread never
    /// creates or releases it: a second STA is exactly what keeps UIA,
    /// `ShellExecuteExW` and `Navigate2` off the thread that owns the hook.
    static AUTOMATION: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

fn to_u16_vec(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = OsStr::new(s).encode_wide().collect();
    v.push(0);
    v
}

fn from_u16_slice(slice: &[u16]) -> String {
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    std::ffi::OsString::from_wide(&slice[..len])
        .to_string_lossy()
        .into_owned()
}

fn log_diagnostic(msg: &str) {
    if let Ok(path) = std::env::var("FSW_DIAGNOSTIC_LOG") {
        if !path.is_empty() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{msg}");
            }
        }
    }
}

/// Sign-extends one 16-bit word of a packed `WPARAM`/`LPARAM` coordinate pair,
/// the `GET_X_LPARAM`/`GET_Y_LPARAM` macros. A tray icon near the right or
/// bottom edge of a secondary monitor has negative coordinates, and a plain
/// mask would put the menu on the wrong screen.
fn signed_word(value: usize, shift: u32) -> i32 {
    let word = u16::try_from((value >> shift) & 0xFFFF).unwrap_or(0);
    i32::from(i16::from_ne_bytes(word.to_ne_bytes()))
}

fn process_name(process_id: u32) -> String {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return String::new();
        }
        // 1024 units, not MAX_PATH-extended: this runs behind the class check
        // now, but it still runs on the hook thread. A path longer than the
        // buffer fails with ERROR_INSUFFICIENT_BUFFER and is treated as an
        // unnamed process, which classifies as Unknown — a pass-through.
        let mut image = [0u16; 1024];
        let mut length = image.len() as u32;
        let success = QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &mut length);
        CloseHandle(process);
        if success == 0 {
            return String::new();
        }
        let full_path = from_u16_slice(&image[..length as usize]);
        Path::new(&full_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    }
}

fn window_class(window: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 256];
        let len = GetClassNameW(window, buf.as_mut_ptr(), buf.len() as i32);
        if len <= 0 {
            return String::new();
        }
        from_u16_slice(&buf[..len as usize])
    }
}

/// Whether a `#32770` looks like a file-picker rather than any other dialog:
/// either the modern common-item dialog's `DirectUI` view, or one of the two
/// classic dialog controls that carry a path.
fn dialog_has_path_control(dialog: HWND) -> bool {
    unsafe {
        let dui = to_u16_vec("DUIViewWndClassName");
        if !FindWindowExW(
            dialog,
            std::ptr::null_mut(),
            dui.as_ptr(),
            std::ptr::null(),
        )
        .is_null()
        {
            return true;
        }
        !GetDlgItem(dialog, DIALOG_PATH_COMBO).is_null()
            || !GetDlgItem(dialog, DIALOG_FILE_NAME_EDIT).is_null()
    }
}

/// Runs inside the low-level hook on every Enter in every application, so the
/// window class — a fixed-size read of the calling process's own memory —
/// gates everything else. Only when the class is one of the four the product
/// supports is the process image worth an `OpenProcess`.
fn classify_surface(foreground: HWND) -> SurfaceKind {
    if foreground.is_null() {
        return SurfaceKind::Unknown;
    }
    let class = window_class(foreground);
    let is_browser = eq_ignore_case(&class, "CabinetWClass") || eq_ignore_case(&class, "ExploreWClass");
    let is_dialog = eq_ignore_case(&class, "#32770");
    let is_core_window = eq_ignore_case(&class, "Windows.UI.Core.CoreWindow");
    if !is_browser && !is_dialog && !is_core_window {
        return SurfaceKind::Unknown;
    }

    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(foreground, &mut process_id);
    }
    let proc = process_name(process_id);

    if eq_ignore_case(&proc, "SearchHost.exe")
        || eq_ignore_case(&proc, "SearchApp.exe")
        || eq_ignore_case(&proc, "StartMenuExperienceHost.exe")
    {
        return SurfaceKind::Search;
    }

    if eq_ignore_case(&proc, "explorer.exe") {
        if is_browser {
            return SurfaceKind::Explorer;
        }
        if is_dialog {
            return SurfaceKind::Run;
        }
    }

    // Every Win32 dialog is a `#32770`. Claiming them all made the broker
    // swallow Enter in Find boxes and rewrite their search text; only a
    // dialog that actually carries a path control qualifies.
    if is_dialog && dialog_has_path_control(foreground) {
        return SurfaceKind::CommonDialog;
    }

    SurfaceKind::Unknown
}

fn send_virtual_key(key: u16) -> bool {
    unsafe {
        let mut inputs: [INPUT; 2] = std::mem::zeroed();
        inputs[0].r#type = INPUT_KEYBOARD;
        inputs[0].Anonymous.ki.wVk = key;
        inputs[0].Anonymous.ki.dwExtraInfo = REPLAY_MARKER;

        inputs[1].r#type = INPUT_KEYBOARD;
        inputs[1].Anonymous.ki.wVk = key;
        inputs[1].Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
        inputs[1].Anonymous.ki.dwExtraInfo = REPLAY_MARKER;

        SendInput(2, inputs.as_mut_ptr(), std::mem::size_of::<INPUT>() as i32) == 2
    }
}

fn replay_enter() {
    if !send_virtual_key(VK_RETURN) {
        log_diagnostic("event=replay_enter_failed");
    }
}

fn read_focused_value(focused: &IUIAutomationElement) -> Option<String> {
    unsafe {
        if let Ok(variant) = focused.GetCurrentPropertyValue(UIA_ValueValuePropertyId) {
            if variant.vt() == VT_BSTR {
                let s = variant.Anonymous.Anonymous.Anonymous.bstrVal.to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }

        if let Ok(legacy) = focused.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
            UIA_LegacyIAccessiblePatternId,
        ) {
            if let Ok(bstr) = legacy.CurrentValue() {
                let s = bstr.to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }
}

/// The focused element as a writable, non-password text field, or `None`.
///
/// Window-class detection cannot tell a file dialog's path box from a Find
/// box that happens to live in a `#32770`; this can. Requiring the pattern up
/// front also means the broker never reads text it could not have written
/// back, which is the promise PRIVACY.md makes. Applied to every surface —
/// Explorer, Run and Search all focus an edit control by construction, so the
/// gate costs three property reads and rejects nothing there.
fn editable_value_pattern(focused: &IUIAutomationElement) -> Option<IUIAutomationValuePattern> {
    unsafe {
        let control_type = focused.CurrentControlType().ok()?;
        if control_type != UIA_EditControlTypeId && control_type != UIA_ComboBoxControlTypeId {
            return None;
        }
        let Ok(password) = focused.CurrentIsPassword() else {
            return None;
        };
        if password.as_bool() {
            return None;
        }
        let pattern = focused
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()?;
        let Ok(read_only) = pattern.CurrentIsReadOnly() else {
            return None;
        };
        if read_only.as_bool() {
            return None;
        }
        Some(pattern)
    }
}

fn set_pattern_value(pattern: &IUIAutomationValuePattern, value: &str) -> bool {
    let bstr = BSTR::from(value);
    unsafe { pattern.SetValue(&bstr) }.is_ok()
}

/// Opens a resolved location. Always called from the worker: binding
/// `\\wsl.localhost\<distro>` boots a stopped distribution, which takes
/// seconds. `SEE_MASK_ASYNCOK` lets the shell finish the launch on its own
/// thread and `SEE_MASK_FLAG_NO_UI` keeps a failure from parking a modal
/// error box on a window nobody can see.
fn open_resolved_path(path: &str) -> bool {
    unsafe {
        let wide_verb = to_u16_vec("open");
        let wide_file = to_u16_vec(path);
        let mut exec: SHELLEXECUTEINFOW = std::mem::zeroed();
        exec.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        exec.fMask = SEE_MASK_ASYNCOK | SEE_MASK_FLAG_NO_UI;
        exec.lpVerb = wide_verb.as_ptr();
        exec.lpFile = wide_file.as_ptr();
        exec.nShow = SW_SHOWNORMAL;

        if ShellExecuteExW(&mut exec) != 0 {
            true
        } else {
            log_diagnostic(&format!("event=shell_open_failed error={}", GetLastError()));
            false
        }
    }
}

/// Hands a path to the worker to open. Menu commands arrive on the UI thread,
/// which is also the hook thread, and must not sit inside `ShellExecuteExW`.
fn request_open_path(path: String) {
    let worker = WORKER_WINDOW.load(Ordering::Relaxed) as HWND;
    let owned = Box::into_raw(Box::new(path));
    if !worker.is_null()
        && unsafe { PostMessageW(worker, WORKER_OPEN_PATH, 0, owned as LPARAM) } != 0
    {
        // The worker owns the allocation now and frees it in `worker_proc`.
        return;
    }
    // Never run ShellExecuteEx on the broker (hook-owning) thread. A null
    // worker HWND can make PostMessageW target this thread's queue, and a WSL
    // path can block while its distribution starts, so dropping is safer than
    // a late inline fallback. Reclaim the ownership we could not hand over.
    drop(unsafe { Box::from_raw(owned) });
    log_diagnostic("event=worker_open_path_dropped");
    show_notification("The location could not be opened right now.", NIIF_ERROR);
}

fn navigate_explorer_window(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    foreground: HWND,
    path: &str,
) -> bool {
    if !request_control_is_current(automation, focused, foreground) {
        return false;
    }
    unsafe {
        let Ok(shell_windows) =
            CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_LOCAL_SERVER)
        else {
            return false;
        };
        let Ok(count) = shell_windows.Count() else {
            return false;
        };

        for i in 0..count {
            let item_var = VARIANT::from(i);
            if let Ok(disp) = shell_windows.Item(&item_var) {
                if let Ok(browser) = disp.cast::<IWebBrowser2>() {
                    if let Ok(hwnd_num) = browser.HWND() {
                        if hwnd_num.0 == foreground as isize {
                            // Every call above can cross into Explorer's COM
                            // apartment. Do not navigate a window whose focus
                            // or foreground changed while it was answering.
                            if !request_control_is_current(automation, focused, foreground) {
                                return false;
                            }
                            let target_var = VARIANT::from(path);
                            let empty = VARIANT::default();
                            let res = browser.Navigate2(
                                &target_var,
                                Some(&empty),
                                Some(&empty),
                                Some(&empty),
                                Some(&empty),
                            );
                            if res.is_ok() {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }
}

fn show_notification(message: &str, flags: u32) {
    let broker_wnd = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if broker_wnd.is_null() || !ICON_ADDED.load(Ordering::Relaxed) {
        // NIM_MODIFY against an icon the shell never accepted just fails; the
        // balloon would be lost either way.
        return;
    }
    unsafe {
        let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
        icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = broker_wnd;
        icon.uID = TRAY_ID as u32;
        icon.uFlags = NIF_INFO;
        icon.dwInfoFlags = flags;

        let title = to_u16_vec("Forward Slash Windows");
        let msg_wide = to_u16_vec(message);

        let title_len = title.len().min(icon.szInfoTitle.len() - 1);
        icon.szInfoTitle[..title_len].copy_from_slice(&title[..title_len]);

        let msg_len = msg_wide.len().min(icon.szInfo.len() - 1);
        icon.szInfo[..msg_len].copy_from_slice(&msg_wide[..msg_len]);

        Shell_NotifyIconW(NIM_MODIFY, &icon);
    }
}

/// Runs on the worker thread. Everything here can block for seconds — UIA
/// cross-process calls, a WSL bind, a shell navigation — which is why the
/// hook thread only classifies and posts.
fn request_window_is_current(foreground: HWND) -> bool {
    if !request_target_is_current(foreground, unsafe { GetForegroundWindow() }) {
        // The user moved on while this request was queued. Replaying Enter now
        // would inject it into whatever they switched to — a half-written chat
        // message sent, a half-typed command run. Drop it instead.
        log_diagnostic("event=enter_dropped_foreground_changed");
        return false;
    }
    true
}

#[must_use]
fn request_target_is_current(expected: HWND, current: HWND) -> bool {
    expected == current
}

#[must_use]
fn should_swallow_enter(worker_present: bool, post_succeeded: bool) -> bool {
    worker_present && post_succeeded
}

/// UIA calls can block while the target application is busy. Do not let the
/// request resume against a new focused field when they return: both the
/// captured foreground window and the exact focused element must still match.
fn request_control_is_current(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    foreground: HWND,
) -> bool {
    if !request_window_is_current(foreground) {
        return false;
    }
    let Ok(current) = (unsafe { automation.GetFocusedElement() }) else {
        log_diagnostic("event=enter_dropped_control_changed");
        return false;
    };
    if !unsafe { automation.CompareElements(focused, &current) }.is_ok_and(BOOL::as_bool) {
        log_diagnostic("event=enter_dropped_control_changed");
        return false;
    }
    // GetFocusedElement and CompareElements are cross-process operations too;
    // check the owning window once more immediately before touching the field.
    request_window_is_current(foreground)
}

fn replay_enter_if_current(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    foreground: HWND,
) {
    if request_control_is_current(automation, focused, foreground) {
        replay_enter();
    }
}

fn process_enter_request(surface: SurfaceKind, foreground: HWND) {
    // This check deliberately precedes the paused branch: a request queued
    // while active must not replay Enter into a window the user selected while
    // the worker was busy.
    if !request_window_is_current(foreground) {
        return;
    }

    if PAUSED.load(Ordering::Relaxed) {
        // There is no captured UIA control in this branch, but foreground must
        // still be current immediately before injecting Enter.
        if request_window_is_current(foreground) {
            replay_enter();
        }
        return;
    }

    if surface == SurfaceKind::Unknown {
        if request_window_is_current(foreground) {
            replay_enter();
        }
        return;
    }

    let Some(automation) = AUTOMATION.with_borrow(Option::clone) else {
        if request_window_is_current(foreground) {
            replay_enter();
        }
        return;
    };

    let Ok(focused) = (unsafe { automation.GetFocusedElement() }) else {
        if request_window_is_current(foreground) {
            replay_enter();
        }
        return;
    };

    if !request_control_is_current(&automation, &focused, foreground) {
        return;
    }

    let Some(value_pattern) = editable_value_pattern(&focused) else {
        log_diagnostic("event=surface_rejected");
        replay_enter_if_current(&automation, &focused, foreground);
        return;
    };

    // The value read is private input. Revalidate before reading and again
    // after the potentially blocking UIA property calls complete.
    if !request_control_is_current(&automation, &focused, foreground) {
        return;
    }
    let Some(input) = read_focused_value(&focused) else {
        replay_enter_if_current(&automation, &focused, foreground);
        return;
    };

    if !request_control_is_current(&automation, &focused, foreground) {
        return;
    }

    if !input.starts_with('/') {
        replay_enter_if_current(&automation, &focused, foreground);
        return;
    }

    let snap = Snapshot::current();
    let mut buf = RenderBuf::new();
    let resolved = match resolve_user_slash_path(&input, &snap, &mut buf) {
        Ok(r) => r,
        Err(err) => {
            log_diagnostic(&format!("event=path_rejected reason={}", err.name()));
            let err_msg = format_resolve_error(err, &snap.distributions);
            show_notification(&err_msg, NIIF_WARNING);
            return;
        }
    };

    log_diagnostic(if resolved.is_provider_root() {
        "event=route_wsl_root"
    } else if resolved.distribution().is_none() {
        "event=route_folder"
    } else {
        "event=route_distribution"
    });

    // Win32 strips a trailing `.` or space from the last component — but only
    // when it is the end of the string. ext4 allows both, so a separator is
    // appended to keep `\\wsl.localhost\Ubuntu\tmp\dir.` addressable.
    let resolved_display = resolved.unc_display();
    let owned_with_separator;
    let unc_path: &str =
        if resolved.has_win32_normalization_hazard() && !resolved_display.ends_with('\\') {
            log_diagnostic("event=win32_normalization_hazard");
            owned_with_separator = format!("{resolved_display}\\");
            &owned_with_separator
        } else {
            resolved_display
        };

    if surface == SurfaceKind::Search {
        if !request_control_is_current(&automation, &focused, foreground) {
            return;
        }
        if !open_resolved_path(unc_path) {
            show_notification("Windows could not open the location.", NIIF_ERROR);
        }
        if request_control_is_current(&automation, &focused, foreground) {
            send_virtual_key(VK_ESCAPE);
        }
        return;
    }

    if surface == SurfaceKind::Explorer
        && resolved.is_provider_root()
        && request_control_is_current(&automation, &focused, foreground)
        && navigate_explorer_window(&automation, &focused, foreground, unc_path)
    {
        return;
    }

    if request_control_is_current(&automation, &focused, foreground)
        && set_pattern_value(&value_pattern, unc_path)
    {
        replay_enter_if_current(&automation, &focused, foreground);
        return;
    }

    if !request_control_is_current(&automation, &focused, foreground) {
        return;
    }
    if !open_resolved_path(unc_path) {
        show_notification("Windows could not open the location.", NIIF_ERROR);
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code != 0 || lparam == 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    if key.vkCode != u32::from(VK_RETURN) || key.dwExtraInfo == REPLAY_MARKER {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let key_up = wparam == WM_KEYUP as usize
        || wparam == WM_SYSKEYUP as usize
        || (key.flags & LLKHF_UP) != 0;

    if key_up {
        ENTER_DOWN.store(false, Ordering::Relaxed);
        if SUPPRESS_ENTER_UP.swap(false, Ordering::Relaxed) {
            return 1;
        }
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    if ENTER_DOWN.swap(true, Ordering::Relaxed) {
        return if SUPPRESS_ENTER_UP.load(Ordering::Relaxed) {
            1
        } else {
            unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
        };
    }

    let foreground = unsafe { GetForegroundWindow() };
    if PAUSED.load(Ordering::Relaxed) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }
    let surface = classify_surface(foreground);
    if surface == SurfaceKind::Unknown {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    // Everything past the classification happens on the worker: a low-level
    // hook that takes longer than LowLevelHooksTimeout is removed by Windows
    // without notice, and the removal is invisible to us.
    let worker = WORKER_WINDOW.load(Ordering::Relaxed) as HWND;
    // PostMessageW(NULL, ...) targets this thread's message queue and succeeds,
    // so it is not evidence that a worker will process the request. Pass both
    // edges through natively until a real worker window exists.
    let posted = !worker.is_null()
        && unsafe { PostMessageW(worker, PROCESS_ENTER, surface as usize, foreground as LPARAM) } != 0;
    if !should_swallow_enter(!worker.is_null(), posted) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    SUPPRESS_ENTER_UP.store(true, Ordering::Relaxed);
    1
}

fn install_hook() -> bool {
    let cur_hook = KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK;
    if !cur_hook.is_null() {
        return true;
    }

    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    KEYBOARD_HOOK.store(hook as isize, Ordering::Relaxed);
    if hook.is_null() {
        log_diagnostic(&format!("event=debug_hook_failed error={}", unsafe {
            GetLastError()
        }));
    }
    !hook.is_null()
}

fn remove_hook() {
    let hook = KEYBOARD_HOOK.swap(0, Ordering::Relaxed) as HHOOK;
    if !hook.is_null() {
        unsafe {
            UnhookWindowsHookEx(hook);
        }
    }
    ENTER_DOWN.store(false, Ordering::Relaxed);
    SUPPRESS_ENTER_UP.store(false, Ordering::Relaxed);
}

/// Replaces a live hook with a fresh one, keeping the old handle until the
/// replacement exists.
///
/// Windows silently unhooks a low-level hook whose owning thread exceeded
/// `LowLevelHooksTimeout`, and nothing tells the process: `fwdslash status`
/// keeps reporting `running (active)` while `/` does nothing anywhere. Since
/// there is no way to ask whether a hook handle is still live, the broker
/// re-arms on a slow timer instead.
fn rearm_hook(window: HWND) {
    if PAUSED.load(Ordering::Relaxed) {
        return;
    }
    let old = KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK;
    if old.is_null() {
        // Nothing to replace — the hook never installed, so the tooltip reads
        // "hook unavailable". A retry can clear that.
        if install_hook() {
            update_tray_tooltip(window);
        }
        return;
    }

    let fresh = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_keyboard_proc),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    if fresh.is_null() {
        // Keep the incumbent: a failed re-arm must never leave the product
        // with no hook at all.
        return;
    }
    KEYBOARD_HOOK.store(fresh as isize, Ordering::Relaxed);
    unsafe {
        UnhookWindowsHookEx(old);
    }
    log_diagnostic("event=hook_rearmed");
}

fn disconnect_filter() {
    // The next connection starts with an empty driver-side table, so drop the
    // cache to guarantee the following publish actually sends.
    if let Ok(mut published) = PUBLISHED_DISTRIBUTIONS.lock() {
        *published = None;
    }
    let port = FILTER_PORT.swap(-1, Ordering::Relaxed) as HANDLE;
    if port != INVALID_HANDLE_VALUE && !port.is_null() {
        unsafe {
            CloseHandle(port);
        }
    }
}

/// The distribution list most recently accepted by the driver.
///
/// Mirrors `g_published_distributions` (`src/broker/main.cpp:54`).
static PUBLISHED_DISTRIBUTIONS: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// The list most recently enumerated for publication, accepted or not.
///
/// `PUBLISHED_DISTRIBUTIONS` alone can never short-circuit anything while no
/// driver is loaded — the shipping configuration — because it is only written
/// after a successful `FilterSendMessage`. Recording the attempt is what makes
/// the compare-only path engage with the port absent.
static ATTEMPTED_DISTRIBUTIONS: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// Opens the filter port if it is not open already. Returns whether a port is
/// available afterwards.
fn ensure_filter_port() -> bool {
    let port = FILTER_PORT.load(Ordering::Relaxed) as HANDLE;
    if port != INVALID_HANDLE_VALUE && !port.is_null() {
        return true;
    }

    let port_name = to_u16_vec(FSW_FILTER_PORT_NAME);
    let mut connected_port = INVALID_HANDLE_VALUE;
    let hr = unsafe {
        FilterConnectCommunicationPort(
            port_name.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            &mut connected_port,
        )
    };
    if hr < 0 || connected_port == INVALID_HANDLE_VALUE {
        return false;
    }
    FILTER_PORT.store(connected_port as isize, Ordering::Relaxed);
    true
}

/// Re-times the health timer. `SetTimer` with an existing id replaces the
/// interval in place, so there is only ever one timer.
fn set_health_interval(connected: bool) {
    let wanted = if connected {
        HEALTH_INTERVAL_CONNECTED_MS
    } else {
        HEALTH_INTERVAL_IDLE_MS
    };
    if HEALTH_INTERVAL_MS.swap(wanted, Ordering::Relaxed) == wanted {
        return;
    }
    let window = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if !window.is_null() {
        unsafe {
            SetTimer(window, HEALTH_TIMER, wanted, None);
        }
    }
}

fn publish_filter_mappings(force: bool) {
    // The connect attempt comes first: whether anything is listening decides
    // both the tick interval and whether enumerating Lxss buys anything.
    let connected = ensure_filter_port();
    set_health_interval(connected);
    if !connected && !force {
        // Idle tick with no driver. The registry read, the sort and the
        // kernel round-trip would all be discarded; only the probe above is
        // worth doing.
        return;
    }

    let distributions = if PAUSED.load(Ordering::Relaxed) {
        Vec::new()
    } else {
        let mut distros = list_registered_distributions();
        // Ordinal case-insensitive, matching the C++ `CompareStringOrdinal`
        // sort. The driver receives this array in order, so the comparison has
        // to agree.
        distros.sort_by(|a, b| {
            let a_folded: Vec<char> = a.chars().flat_map(char::to_uppercase).collect();
            let b_folded: Vec<char> = b.chars().flat_map(char::to_uppercase).collect();
            a_folded.cmp(&b_folded)
        });
        distros
    };

    let unchanged = match ATTEMPTED_DISTRIBUTIONS.lock() {
        Ok(mut attempted) => {
            let same = attempted.as_ref() == Some(&distributions);
            if !same {
                *attempted = Some(distributions.clone());
            }
            same
        }
        Err(_) => false,
    };

    if !connected {
        // A forced publish with no driver: the attempt is recorded, and there
        // is nobody to send it to.
        return;
    }

    // Nothing changed and nobody asked for a resend, so skip the kernel
    // round-trip.
    if !force
        && unchanged
        && PUBLISHED_DISTRIBUTIONS
            .lock()
            .is_ok_and(|published| published.as_ref() == Some(&distributions))
    {
        return;
    }

    let port = FILTER_PORT.load(Ordering::Relaxed) as HANDLE;
    unsafe {
        let mut msg: FswMappingMessage = std::mem::zeroed();
        msg.version = FSW_FILTER_PROTOCOL_VERSION;
        msg.size = std::mem::size_of::<FswMappingMessage>() as u32;
        msg.operation = FSW_OPERATION_REPLACE_MAPPINGS;
        msg.reserved = 0; // the driver requires zero; explicit, not padding luck
        msg.generation = GetTickCount64();
        let count = distributions.len().min(FSW_FILTER_MAX_DISTRIBUTIONS);
        msg.distribution_count = count as u32;

        for (i, d) in distributions[..count].iter().enumerate() {
            let wide = to_u16_vec(d);
            let copy_len = wide.len().min(FSW_MAX_DISTRIBUTION_NAME - 1);
            msg.distributions[i][..copy_len].copy_from_slice(&wide[..copy_len]);
        }

        let mut returned = 0u32;
        let sent = FilterSendMessage(
            port,
            (&raw const msg).cast(),
            std::mem::size_of::<FswMappingMessage>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
        );
        if sent < 0 {
            disconnect_filter();
            return;
        }
    }

    if let Ok(mut published) = PUBLISHED_DISTRIBUTIONS.lock() {
        *published = Some(distributions);
    }
}

/// The tooltip is the only place the product reports its own health, so it
/// distinguishes a deliberate pause from a hook that failed to install.
fn tray_tip() -> &'static str {
    if PAUSED.load(Ordering::Relaxed) {
        "Forward Slash Windows \u{2014} paused"
    } else if (KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK).is_null()
        || (WORKER_WINDOW.load(Ordering::Acquire) as HWND).is_null()
    {
        "Forward Slash Windows \u{2014} processing unavailable"
    } else {
        "Forward Slash Windows \u{2014} active"
    }
}

fn tray_icon_data(window: HWND) -> NOTIFYICONDATAW {
    unsafe {
        let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
        icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = window;
        icon.uID = TRAY_ID as u32;
        icon
    }
}

/// Adds the notification icon, reporting whether the shell took it.
///
/// `Shell_NotifyIcon` fails with `ERROR_TIMEOUT` while the shell is busy, and
/// the MSIX startup task launches the broker at exactly that moment. An
/// unchecked add costs the user the icon, the menu and every balloon for the
/// whole session, so the result drives `ICON_ADDED` and the health-timer retry.
fn add_tray_icon(window: HWND) -> bool {
    unsafe {
        let mut icon = tray_icon_data(window);
        icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        icon.uCallbackMessage = TRAY_MESSAGE;
        icon.hIcon = LoadIconW(GetModuleHandleW(std::ptr::null()), IDI_FSW_APP as *const u16);

        let tip = to_u16_vec(tray_tip());
        let tip_len = tip.len().min(icon.szTip.len() - 1);
        icon.szTip[..tip_len].copy_from_slice(&tip[..tip_len]);

        if Shell_NotifyIconW(NIM_ADD, &icon) == 0 {
            ICON_ADDED.store(false, Ordering::Relaxed);
            log_diagnostic("event=tray_icon_add_failed");
            return false;
        }
        ICON_ADDED.store(true, Ordering::Relaxed);

        // Version 4 only after the icon exists: it is a property of an icon
        // the shell already knows about.
        icon.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &icon);
        true
    }
}

fn remove_tray_icon(window: HWND) {
    unsafe {
        let icon = tray_icon_data(window);
        Shell_NotifyIconW(NIM_DELETE, &icon);
    }
    ICON_ADDED.store(false, Ordering::Relaxed);
}

/// Health-timer retry for an add the shell refused.
fn ensure_tray_icon(window: HWND) {
    if !ICON_ADDED.load(Ordering::Relaxed) {
        add_tray_icon(window);
    }
}

/// Re-announces the tooltip (NIM_MODIFY) without touching the icon itself.
fn update_tray_tooltip(window: HWND) {
    if !ICON_ADDED.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let mut icon = tray_icon_data(window);
        icon.uFlags = NIF_TIP;
        let tip = to_u16_vec(tray_tip());
        let tip_len = tip.len().min(icon.szTip.len() - 1);
        icon.szTip[..tip_len].copy_from_slice(&tip[..tip_len]);
        Shell_NotifyIconW(NIM_MODIFY, &icon);
    }
}

/// `TaskbarCreated` is broadcast when the shell restarts; broadcasts never
/// reach a message-only window, which is why the broker window is a real
/// (never-shown) top-level window. Without the re-add, an explorer.exe
/// restart would leave the resident broker with no tray icon at all.
fn taskbar_created_message() -> u32 {
    static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *TASKBAR_CREATED.get_or_init(|| unsafe {
        RegisterWindowMessageW(to_u16_vec("TaskbarCreated").as_ptr())
    })
}

/// Drains pause writes in submission order on one background thread.
///
/// `persist_disabled` shells out to `reg.exe` (see its doc comment for why a
/// child process and not a direct write). That is a process creation plus a
/// wait — unbounded under load — and the caller here is the thread that owns
/// the low-level keyboard hook, where any wait freezes every keystroke on the
/// machine. The pause therefore takes effect in memory immediately and the
/// persistence is reported asynchronously: `FSW_WM_SET_PAUSED` replies from
/// in-memory state plus the hook result, and a failed write surfaces later as
/// a balloon plus `event=persist_disabled_failed`.
fn report_persist_failure() {
    log_diagnostic("event=persist_disabled_failed");
    let window = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if !window.is_null() {
        unsafe {
            PostMessageW(window, PERSIST_FAILED, 0, 0);
        }
    }
}

fn drain_persist_queue<F>(receiver: mpsc::Receiver<bool>, mut persist: F)
where
    F: FnMut(bool),
{
    while let Ok(disabled) = receiver.recv() {
        persist(disabled);
    }
}

fn persist_worker_main(receiver: mpsc::Receiver<bool>) {
    drain_persist_queue(receiver, |disabled| {
        let failed = persist_disabled(disabled).is_err();
        PERSIST_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        if failed {
            report_persist_failure();
        }
    });
}

fn request_persist_disabled(disabled: bool) {
    PERSIST_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    let Ok(mut queue) = PERSIST_QUEUE.lock() else {
        PERSIST_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        log_diagnostic("event=persist_queue_unavailable");
        report_persist_failure();
        return;
    };
    if queue.is_none() {
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("fsw-persist".to_owned())
            .spawn(move || persist_worker_main(receiver))
        {
            Ok(_) => *queue = Some(sender),
            Err(_) => {
                PERSIST_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
                log_diagnostic("event=persist_queue_start_failed");
                report_persist_failure();
                return;
            }
        }
    }
    let Some(sender) = queue.as_ref() else { return };
    if sender.send(disabled).is_err() {
        *queue = None;
        PERSIST_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        log_diagnostic("event=persist_queue_unavailable");
        report_persist_failure();
    }
}

/// The shell adapters, in the order the tray "Integrations" submenu lists
/// them: the CLI verb id, the name a balloon may show, and the marker key
/// that records the payload version currently deployed.
fn adapter_upgrade_targets() -> [(&'static str, &'static str, String); 3] {
    [
        ("cmd", "Command Prompt", CMD_ADAPTER_KEY.to_owned()),
        (
            "windows-powershell",
            "Windows PowerShell",
            format!("{POWERSHELL_ADAPTER_ROOT}WindowsPowerShell"),
        ),
        (
            "powershell",
            "PowerShell 7",
            format!("{POWERSHELL_ADAPTER_ROOT}PowerShell"),
        ),
    ]
}

/// Runs one `fwdslash.exe` invocation to completion, bounded by `timeout`, and
/// reports the exit code it finished with.
///
/// `None` is deliberately *not* "failed": it means the child never got to
/// answer — it could not be spawned, the wait itself errored, or it blew the
/// deadline and was killed. Every caller here treats that differently from a
/// child that ran and refused, because the first retries itself for free (the
/// marker key still reads the old version, the update cadence comes round
/// again) and the second needs a person.
///
/// Never call this on the hook thread or the window thread: it parks the
/// calling thread for as long as the child takes.
fn run_cli_bounded(cli: &Path, args: &[&str], timeout: std::time::Duration) -> Option<i32> {
    let spawned = std::process::Command::new(cli)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let Ok(mut child) = spawned else {
        return None;
    };

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            // `code()` is `None` only for a signal, which Windows has no
            // concept of; the fallback keeps it total.
            Ok(Some(status)) => return Some(status.code().unwrap_or(1)),
            Ok(None) => {}
            Err(_) => return None,
        }
        if std::time::Instant::now() >= deadline {
            // Half-applied is the transaction's problem, not ours: the CLI
            // rolls its own snapshot back, and the marker key still reads the
            // old version, so the next launch tries again.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(ADAPTER_UPGRADE_POLL_MS));
    }
}

/// Runs one `fwdslash integration <id> enable`, bounded by
/// [`ADAPTER_UPGRADE_TIMEOUT`].
///
/// The CLI does the transactional uninstall+install itself and is idempotent
/// once the recorded version already matches, so racing a manual enable from
/// the settings app costs at worst a redundant reinstall.
fn run_adapter_upgrade(cli: &Path, id: &str) -> Option<i32> {
    run_cli_bounded(cli, &["integration", id, "enable"], ADAPTER_UPGRADE_TIMEOUT)
}

/// Runs one `fwdslash repair-adapters` to completion, bounded by
/// [`ADAPTER_UPGRADE_TIMEOUT`]. Fire-and-forget: a repair that fails or times
/// out just retries next launch, and the guarded profile block means an
/// un-repaired orphan is silent, never a red shell error (#37).
fn run_adapter_repair(cli: &Path) {
    let _ = run_cli_bounded(cli, &["repair-adapters"], ADAPTER_UPGRADE_TIMEOUT);
}

/// `show_notification` silently drops a balloon while the shell has not
/// accepted `NIM_ADD`, and at logon the add can be waiting on the 60 s
/// health tick to retry. Give the icon ~10 s to appear before spending the
/// one balloon this sweep is allowed.
fn notify_when_icon_ready(message: &str, flags: u32) {
    for _ in 0..ADAPTER_UPGRADE_NOTIFY_ATTEMPTS {
        if ICON_ADDED.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(
            ADAPTER_UPGRADE_NOTIFY_INTERVAL_MS,
        ));
    }
    show_notification(message, flags);
}

/// What the sweep decided about one adapter, after up to two attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterOutcome {
    /// The payload is now on this build's version.
    Upgraded,
    /// Neither attempt got an answer out of the CLI — the child could not
    /// start, or blew its budget and was killed. The marker key still reads
    /// the old version, so the next broker start or settings launch tries
    /// again. Silent by design: nothing the user does changes this.
    Deferred,
    /// The CLI ran and refused. A third-party-modified profile, a missing
    /// `pwsh.exe`, a Controlled Folder Access block — the failures that stay
    /// failed until somebody acts.
    NeedsUser,
}

/// Classifies one adapter from its two attempts' exit codes (issue #56).
///
/// `None` means the attempt never produced an exit code (see
/// [`run_cli_bounded`]); `Some(0)` is success and any other `Some` is a
/// refusal. The retry is only consulted when the first attempt did not
/// succeed, which is also why the caller may pass `None` for it unconditionally
/// when the first attempt already won.
#[must_use]
fn adapter_outcome(first: Option<i32>, retry: Option<i32>) -> AdapterOutcome {
    if first == Some(0) {
        return AdapterOutcome::Upgraded;
    }
    match retry {
        Some(0) => AdapterOutcome::Upgraded,
        // Two attempts, neither of which the CLI answered: transient by every
        // available signal. Balloon nothing.
        None => AdapterOutcome::Deferred,
        Some(_) => AdapterOutcome::NeedsUser,
    }
}

/// Holds [`FSW_ADAPTER_SWEEP_MUTEX`] for the length of a sweep.
///
/// `None` from [`SweepLock::acquire`] means the settings window is already
/// sweeping (issue #56): skip rather than fight it for the payload tree, since
/// whoever holds it is running the identical work.
struct SweepLock(HANDLE);

impl SweepLock {
    fn acquire() -> Option<Self> {
        let name = to_u16_vec(FSW_ADAPTER_SWEEP_MUTEX);
        // SAFETY: a named mutex with a static, NUL-terminated name; the handle
        // is closed exactly once, in `Drop`.
        unsafe {
            let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
            if handle.is_null() {
                // Nobody can tell whether a sweep is running; err toward doing
                // the work, which is idempotent, rather than never doing it.
                return Some(Self(std::ptr::null_mut()));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return None;
            }
            Some(Self(handle))
        }
    }
}

impl Drop for SweepLock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from `CreateMutexW` above and is closed
            // once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Brings every installed shell adapter whose recorded payload version is not
/// this build's back up to date, with no click from the user.
///
/// An adapter's payload — the scripts plus a private copy of `fwdslash.exe` —
/// is copied into `%LOCALAPPDATA%` at install time, so after a product update
/// the previous copy keeps serving every console until someone re-runs
/// `fwdslash integration <id> enable`. The broker is the one component that
/// starts at every logon and after every update, so the check belongs here.
fn adapter_upgrade_sweep() {
    // One sweeper at a time (issue #56). The settings window runs the same
    // work at launch, and an update that restarts the app starts both within
    // seconds of each other; the loser of that race deletes a payload
    // directory the winner's child is running out of and reports a failure
    // that was never real.
    let Some(_lock) = SweepLock::acquire() else {
        log_diagnostic("event=adapter_sweep_busy");
        return;
    };

    // Before anything else: mirror the settings this packaged process holds
    // into the real hive, because the adapters this sweep is about to look
    // after read them from an unpackaged shell and would otherwise still see
    // whatever the install started with (issue #52). A no-op unpackaged, and
    // a no-op packaged once the hives agree. It logs `event=settings_synced`
    // itself when it repairs something, so nothing is logged here.
    let _ = sync_settings_to_real_hive();

    // Beside the broker, never from PATH: an appExecutionAlias or a stale
    // directory on PATH could resolve to a different install entirely.
    let Ok(directory) = executable_directory() else {
        log_diagnostic("event=adapter_upgrade_skipped");
        return;
    };
    let cli = directory.join("fwdslash.exe");
    if !cli.is_file() {
        log_diagnostic("event=adapter_upgrade_skipped");
        return;
    }

    // The version-bump upgrade first, so its "integrations were updated"
    // balloon still fires. Repair runs afterward regardless, cleaning any
    // orphaned or duplicated block the version match alone would miss (#37).
    let targets = adapter_upgrade_targets();
    let outdated: Vec<(&'static str, &'static str)> = targets
        .iter()
        .filter(|(_, _, marker_key)| adapter_outdated(marker_key, FSW_VERSION))
        .map(|&(id, label, _)| (id, label))
        .collect();
    if !outdated.is_empty() {
        let mut upgraded: Vec<&'static str> = Vec::new();
        let mut needs_user = false;
        for (id, label) in outdated {
            let first = run_adapter_upgrade(&cli, id);
            // One retry, after a pause (issue #56). Right after an MSIX update
            // the first attempt fails for reasons that are gone seconds later:
            // a console still holding the old payload's `fwdslash.exe`, a copy
            // out of `WindowsApps` competing with the package still being
            // staged.
            let retry = if first == Some(0) {
                None
            } else {
                log_diagnostic("event=adapter_upgrade_retry");
                std::thread::sleep(std::time::Duration::from_millis(ADAPTER_RETRY_DELAY_MS));
                run_adapter_upgrade(&cli, id)
            };
            match adapter_outcome(first, retry) {
                AdapterOutcome::Upgraded => {
                    log_diagnostic("event=adapter_upgraded");
                    upgraded.push(label);
                }
                AdapterOutcome::Deferred => log_diagnostic("event=adapter_upgrade_deferred"),
                AdapterOutcome::NeedsUser => {
                    log_diagnostic("event=adapter_upgrade_failed");
                    needs_user = true;
                }
            }
        }

        // Exactly one balloon, whatever the mix: a per-adapter notification
        // would stack three toasts on top of a logon the user did not ask
        // about. A deferral alone is silent — it retries itself.
        if needs_user {
            notify_when_icon_ready(
                "Some terminal integrations could not be updated automatically. \
                 Open Settings and choose Repair integrations.",
                NIIF_WARNING,
            );
        } else if !upgraded.is_empty() {
            notify_when_icon_ready(
                &format!(
                    "Terminal integrations were updated to {FSW_VERSION}: {}.",
                    upgraded.join(", ")
                ),
                NIIF_INFO,
            );
        }
    }

    // Detect-and-repair every adapter's profile/AutoRun hygiene: an orphaned or
    // duplicated block self-heals even when the recorded version already
    // matches and nothing was "outdated" (#37). Silent — the guarded block
    // means an un-repaired orphan is never a red shell error anyway.
    run_adapter_repair(&cli);
}

/// Starts the adapter-upgrade sweep on a thread of its own, once per process.
///
/// Fire-and-forget by design. The work is process creation plus a wait, both
/// unbounded under load, so it may touch neither the hook/UI thread nor the
/// Enter worker; and `WM_DESTROY` deliberately does not join it, because a
/// child `fwdslash.exe` already mid-transaction owns its own rollback and
/// finishes whether the broker is still there or not.
fn start_adapter_upgrade() {
    if ADAPTER_UPGRADE_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    if std::thread::Builder::new()
        .name("fsw-adapter-upgrade".to_owned())
        .spawn(adapter_upgrade_sweep)
        .is_err()
    {
        // Out of threads. Running it inline would park the thread that owns
        // the keyboard hook for as long as three CLI transactions take, so the
        // upgrade waits for the next launch instead.
        log_diagnostic("event=adapter_upgrade_skipped");
    }
}

// ---------------------------------------------------------------------------
// The update cycle
// ---------------------------------------------------------------------------

/// Exit codes of `fwdslash update`, mirrored from `crates/fsw-cli/src/update`.
/// Named here too, because a bare `10` in a match arm reads as nothing.
const UPDATE_EXIT_AVAILABLE: i32 = 10;
const UPDATE_EXIT_NEEDS_USER: i32 = 11;
const UPDATE_EXIT_ERROR: i32 = 1;

/// Clears [`UPDATE_RUNNING`] however [`update_cycle`] ends.
struct UpdateCycleGuard;

impl Drop for UpdateCycleGuard {
    fn drop(&mut self) {
        UPDATE_RUNNING.store(false, Ordering::Release);
    }
}

/// How old the update cycle is at `now`, in milliseconds.
///
/// Before the first cycle of this process (`last == 0`) the age is a step
/// function of uptime rather than a duration: nothing until the broker has been
/// up [`UPDATE_FIRST_DELAY_MS`], a full interval after that. It keeps the two
/// thresholds — the first delay and the recurring interval — from needing two
/// timers or a signed clock.
#[must_use]
fn update_cycle_age_ms(now: u64, start: u64, last: u64) -> u64 {
    if last == 0 {
        if now.saturating_sub(start) >= UPDATE_FIRST_DELAY_MS {
            UPDATE_CONSIDER_INTERVAL_MS
        } else {
            0
        }
    } else {
        now.saturating_sub(last)
    }
}

/// Whether an update cycle may start. Pure, so the four gates are one truth
/// table instead of four early returns spread through a side-effecting
/// function.
///
/// `allowed` is `fsw_core::update::update_check_allowed(packaged, auto_update)`:
/// an unpackaged build has nothing it could install, and the Automatic updates
/// switch is the user's answer for both flavors.
#[must_use]
fn update_cycle_due(running: bool, age_ms: u64, worker_busy: bool, allowed: bool) -> bool {
    !running && age_ms >= UPDATE_CONSIDER_INTERVAL_MS && !worker_busy && allowed
}

/// Whether a balloon about `tag` is new information, given the version the last
/// balloon was about.
///
/// One available update produces one balloon, however many six-hour cycles see
/// it. A cycle that cannot name a version at all (the CLI answered but the
/// registry had no tag) is announced once and then stays quiet, rather than
/// once per cycle forever.
#[must_use]
fn should_balloon_update(tag: Option<&str>, notified: Option<&str>) -> bool {
    match (tag, notified) {
        (Some(current), Some(last)) => current != last,
        (Some(_), None) | (None, None) => true,
        (None, Some(_)) => false,
    }
}

/// Whether repeated install failures have earned the warning balloon.
///
/// Store flavor only: the GitHub flavor's failed install leaves the downloaded
/// bundle in place and applies it at the next logon on its own, so telling the
/// user about it would be asking for a click that changes nothing.
#[must_use]
fn should_balloon_install_failure(consecutive_failures: u32, store_flavor: bool) -> bool {
    store_flavor && consecutive_failures >= UPDATE_FAILURES_BEFORE_BALLOON
}

/// Shows one update balloon, at most once per version.
///
/// The dedupe key is whatever `cached_update_tag()` holds *now* — the value the
/// CLI just wrote — so a second update replacing the first is announced again.
fn notify_update_once(message: &str, flags: u32) {
    let tag = update::cached_update_tag();
    let Ok(mut notified) = UPDATE_NOTIFIED_TAG.lock() else {
        return;
    };
    if !should_balloon_update(tag.as_deref(), notified.as_deref()) {
        return;
    }
    *notified = tag;
    // The lock is held across a call that can park for ~10 s waiting for the
    // shell to accept the icon. Nothing else ever takes it except another
    // cycle, and `UPDATE_RUNNING` already makes those mutually exclusive.
    drop(notified);
    notify_when_icon_ready(message, flags);
}

/// One check, and — when the CLI says there is something to install — one
/// install. Runs on the `fsw-update` thread and nowhere else: every step is a
/// child process wait.
fn update_cycle() {
    let _guard = UpdateCycleGuard;
    log_diagnostic("event=update_cycle_started");

    // Beside the broker, never from PATH: an appExecutionAlias or a stale
    // directory on PATH could resolve to a different install entirely.
    let Ok(directory) = executable_directory() else {
        log_diagnostic("event=update_cycle_failed");
        return;
    };
    let cli = directory.join("fwdslash.exe");
    if !cli.is_file() {
        log_diagnostic("event=update_cycle_failed");
        return;
    }

    let Some(code) = run_cli_bounded(&cli, &["update", "check", "--json"], UPDATE_CHECK_TIMEOUT)
    else {
        log_diagnostic("event=update_cycle_failed");
        return;
    };
    if code != UPDATE_EXIT_AVAILABLE {
        // Up to date, not due, disabled, or a check that could not run: all
        // silent, all retried at the next cycle.
        return;
    }
    log_diagnostic("event=update_available");

    // `--relaunch broker` and no `--force`: the CLI's own moment gate declines
    // while a settings window is open, and what has to come back afterwards is
    // the resident broker, not a window nobody asked for.
    log_diagnostic("event=update_installing");
    let Some(code) = run_cli_bounded(
        &cli,
        &["update", "install", "--relaunch", "broker", "--json"],
        UPDATE_INSTALL_TIMEOUT,
    ) else {
        log_diagnostic("event=update_cycle_failed");
        return;
    };

    match code {
        UPDATE_EXIT_NEEDS_USER => {
            UPDATE_INSTALL_FAILURES.store(0, Ordering::Relaxed);
            notify_update_once(
                "An update to fwdslash is available in the Microsoft Store. \
                 Open Settings to install it.",
                NIIF_INFO,
            );
        }
        UPDATE_EXIT_ERROR => {
            let failures = UPDATE_INSTALL_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            log_diagnostic("event=update_cycle_failed");
            if should_balloon_install_failure(failures, is_store_flavor()) {
                notify_update_once("fwdslash could not update itself automatically.", NIIF_WARNING);
            }
        }
        // 0 (install started), 10 (deferred — a settings window is open, or
        // the download is still coming) and 12 (nothing to install) are all
        // silent: the next cycle picks the story up.
        _ => UPDATE_INSTALL_FAILURES.store(0, Ordering::Relaxed),
    }
}

/// Starts an update cycle if this is a moment to have one. Called from the
/// health tick, on the window thread, so it may do nothing but read atomics,
/// read two registry values and spawn.
fn maybe_start_update_cycle() {
    // SAFETY: no preconditions.
    let now = unsafe { GetTickCount64() };
    let age = update_cycle_age_ms(
        now,
        BROKER_START_MS.load(Ordering::Relaxed),
        LAST_UPDATE_TICK_MS.load(Ordering::Relaxed),
    );
    let allowed = update::update_check_allowed(
        has_package_identity(),
        update::read_auto_update_enabled(),
    );
    if !update_cycle_due(
        UPDATE_RUNNING.load(Ordering::Acquire),
        age,
        WORKER_BUSY.load(Ordering::Acquire),
        allowed,
    ) {
        return;
    }

    // Claim the slot before spawning, so a tick that arrives while the thread
    // is still starting cannot start a second one.
    if UPDATE_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    LAST_UPDATE_TICK_MS.store(now, Ordering::Relaxed);
    if std::thread::Builder::new()
        .name("fsw-update".to_owned())
        .spawn(update_cycle)
        .is_err()
    {
        // Never inline: this thread owns the low-level keyboard hook, and
        // Windows silently removes a hook whose thread stops pumping.
        UPDATE_RUNNING.store(false, Ordering::Release);
        log_diagnostic("event=update_cycle_skipped");
    }
}

/// Applies a pause/resume to the *running* broker and reports whether the
/// keyboard hook ended up armed as the new state requires.
///
/// Persistence is deliberately not part of it: this is also the path a
/// state-changed broadcast takes, where the value has already been written by
/// somebody else and writing it again would be a loop.
fn apply_paused(paused: bool) -> bool {
    PAUSED.store(paused, Ordering::Relaxed);
    if paused {
        remove_hook();
        true
    } else {
        install_hook()
    }
}

/// Re-reads the settings another component just changed and catches the
/// running broker up (issue #55).
///
/// The tray tooltip, the keyboard hook and the mapping published to the driver
/// are all derived from state that `fwdslash pause`, the settings window or a
/// shell adapter can change from another process. Before this they caught up
/// at the next health tick — a minute — or, for the pause flag, not at all.
///
/// Runs on the broker's window thread, which also owns the low-level keyboard
/// hook, so it stays a registry read plus, at most, exactly the work a tray
/// pause already does. The menus need nothing: they are built from live state
/// when the tray menu opens.
fn reload_settings(window: HWND) {
    // A pause of our own is mid-flight: `PAUSED` is ahead of the stored value
    // on purpose, and re-reading now would undo it.
    if PERSIST_IN_FLIGHT.load(Ordering::Acquire) != 0 {
        return;
    }
    let disabled = is_disabled();
    if disabled == PAUSED.load(Ordering::Relaxed) {
        // Nothing the tray shows changed. The distribution list still might
        // have, and this is how the driver hears about it; with no driver
        // connected it costs one failed connect attempt.
        publish_filter_mappings(false);
        return;
    }
    log_diagnostic("event=state_changed");
    let hook_ok = apply_paused(disabled);
    update_tray_tooltip(window);
    publish_filter_mappings(true);
    if !hook_ok {
        // No balloon: the user did not ask *this* process for anything, and
        // the tooltip already reads "hook unavailable". The health timer
        // re-arms on its own.
        log_diagnostic("event=hook_unavailable");
    }
}

/// Applies a pause/resume and reports the state the broker ended up in.
///
/// `Err` means the resume could not arm the keyboard hook, i.e. the broker is
/// `Unavailable`. The persistence result is deliberately *not* part of it —
/// see [`request_persist_disabled`].
///
/// No broadcast from here: the write this schedules broadcasts when it lands
/// (`fsw_core::settings_write`), and announcing the change before the value is
/// stored would have every listener — this broker included — re-read the old
/// one.
fn set_paused(paused: bool) -> Result<BrokerState, ()> {
    // Unhook before persisting: the write is off-thread now, but the ordering
    // is what guarantees a pause stops swallowing Enter immediately.
    let hook_ok = apply_paused(paused);

    request_persist_disabled(paused);

    let window = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if !window.is_null() {
        update_tray_tooltip(window);
    }
    publish_filter_mappings(true);

    if !hook_ok {
        show_notification(
            "The shell keyboard hook could not be installed.",
            NIIF_ERROR,
        );
        return Err(());
    }

    Ok(if paused {
        BrokerState::Paused
    } else {
        BrokerState::Active
    })
}

fn open_settings_section(section: &str) {
    let Ok(dir) = executable_directory() else {
        show_notification("The settings application could not be located.", NIIF_ERROR);
        return;
    };
    let exe = dir.join("fswsettings.exe");
    let arg = format!("fwdslash://settings/{section}");

    unsafe {
        let wide_verb = to_u16_vec("open");
        let wide_file = to_u16_vec(&exe.to_string_lossy());
        let wide_arg = to_u16_vec(&arg);

        let mut exec: SHELLEXECUTEINFOW = std::mem::zeroed();
        exec.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        exec.fMask = SEE_MASK_FLAG_NO_UI;
        exec.lpVerb = wide_verb.as_ptr();
        exec.lpFile = wide_file.as_ptr();
        exec.lpParameters = wide_arg.as_ptr();
        exec.nShow = SW_SHOWNORMAL;

        if ShellExecuteExW(&mut exec) == 0 {
            show_notification(
                "The WinUI settings application could not be opened.",
                NIIF_ERROR,
            );
        }
    }
}

/// Builds the "Open distribution" submenu and records the list it was built
/// from, so a click resolves to the name that was on screen.
fn build_distributions_menu() -> windows_sys::Win32::UI::WindowsAndMessaging::HMENU {
    let submenu = unsafe { CreatePopupMenu() };
    let distributions = list_registered_distributions();
    let listed = distributions.len().min(MENU_DISTRO_MAX);
    if listed == 0 {
        let s_none = to_u16_vec("No distributions registered");
        unsafe {
            AppendMenuW(submenu, MF_STRING | MF_GRAYED, 0, s_none.as_ptr());
        }
    } else {
        for (index, name) in distributions[..listed].iter().enumerate() {
            let label = to_u16_vec(name);
            let id = MENU_DISTRO_BASE as usize + index;
            unsafe {
                AppendMenuW(submenu, MF_STRING, id, label.as_ptr());
            }
        }
    }
    if let Ok(mut cached) = MENU_DISTRIBUTIONS.lock() {
        *cached = distributions;
    }
    submenu
}

fn build_integrations_menu() -> windows_sys::Win32::UI::WindowsAndMessaging::HMENU {
    let s_windows = to_u16_vec("Windows surfaces");
    let s_cmd = to_u16_vec("Command Prompt");
    let s_win_ps = to_u16_vec("Windows PowerShell");
    let s_ps7 = to_u16_vec("PowerShell 7");
    unsafe {
        let submenu = CreatePopupMenu();
        AppendMenuW(
            submenu,
            MF_STRING,
            MENU_WINDOWS as usize,
            s_windows.as_ptr(),
        );
        AppendMenuW(submenu, MF_STRING, MENU_CMD as usize, s_cmd.as_ptr());
        AppendMenuW(
            submenu,
            MF_STRING,
            MENU_WINDOWS_POWERSHELL as usize,
            s_win_ps.as_ptr(),
        );
        AppendMenuW(submenu, MF_STRING, MENU_POWERSHELL as usize, s_ps7.as_ptr());
        submenu
    }
}

fn show_tray_menu(window: HWND, anchor: POINT) {
    unsafe {
        let menu = CreatePopupMenu();

        let s_settings = to_u16_vec("Open settings");
        let s_enabled = to_u16_vec("Enabled");
        let s_open_root = to_u16_vec("Open WSL root");
        let s_open_distro = to_u16_vec("Open distribution");
        let s_integrations = to_u16_vec("Integrations");
        let version = package_version().unwrap_or_else(|| FSW_VERSION.to_owned());
        let s_version = to_u16_vec(&format!("Forward Slash Windows {version}"));
        let s_exit = to_u16_vec("Exit");

        AppendMenuW(menu, MF_STRING, MENU_SETTINGS as usize, s_settings.as_ptr());
        // Left click and Enter both land on this one.
        SetMenuDefaultItem(menu, MENU_SETTINGS, 0);
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());

        let enabled_flags = if PAUSED.load(Ordering::Relaxed) {
            MF_STRING
        } else {
            MF_STRING | MF_CHECKED
        };
        AppendMenuW(menu, enabled_flags, MENU_PAUSE as usize, s_enabled.as_ptr());
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_OPEN_ROOT as usize,
            s_open_root.as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_POPUP,
            build_distributions_menu() as usize,
            s_open_distro.as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_POPUP,
            build_integrations_menu() as usize,
            s_integrations.as_ptr(),
        );

        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(
            menu,
            MF_STRING | MF_GRAYED,
            MENU_VERSION as usize,
            s_version.as_ptr(),
        );
        AppendMenuW(menu, MF_STRING, MENU_EXIT as usize, s_exit.as_ptr());

        SetForegroundWindow(window);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            anchor.x,
            anchor.y,
            0,
            window,
            std::ptr::null(),
        );
        // Documented Shell_NotifyIcon requirement: without it the menu owner
        // keeps a stale foreground state and the *next* right-click flashes a
        // menu that dismisses itself.
        PostMessageW(window, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

fn open_menu_distribution(index: usize) {
    let name = MENU_DISTRIBUTIONS
        .lock()
        .ok()
        .and_then(|names| names.get(index).cloned());
    if let Some(name) = name {
        request_open_path(format!("\\\\wsl.localhost\\{name}"));
    }
}

fn handle_menu_command(window: HWND, id: u32) {
    match id {
        MENU_SETTINGS => open_settings_section("general"),
        MENU_WINDOWS => open_settings_section("windows"),
        MENU_CMD => open_settings_section("cmd"),
        MENU_WINDOWS_POWERSHELL => open_settings_section("windows-powershell"),
        MENU_POWERSHELL => open_settings_section("powershell"),
        MENU_OPEN_ROOT => request_open_path("\\\\wsl.localhost".to_owned()),
        MENU_PAUSE => {
            // The item is checked while enabled, so clicking it toggles.
            let _ = set_paused(!PAUSED.load(Ordering::Relaxed));
        }
        MENU_EXIT => unsafe {
            DestroyWindow(window);
        },
        _ => {
            if let Some(index) = id.checked_sub(MENU_DISTRO_BASE) {
                if (index as usize) < MENU_DISTRO_MAX {
                    open_menu_distribution(index as usize);
                }
            }
        }
    }
}

/// Tray-icon retry and hook re-arm, spaced by `MAINTENANCE_INTERVAL_MS` so
/// they run once a minute whatever the tick interval is.
fn health_tick(window: HWND) {
    publish_filter_mappings(false);

    let now = unsafe { GetTickCount64() };
    let last = LAST_MAINTENANCE_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < MAINTENANCE_INTERVAL_MS {
        return;
    }
    LAST_MAINTENANCE_MS.store(now, Ordering::Relaxed);
    ensure_tray_icon(window);
    rearm_hook(window);
    // Last, and only ever a spawn: the cycle itself waits on child processes.
    maybe_start_update_cycle();
}

/// Asks the worker to quit, waits ~500 ms for it to signal that it has, and
/// joins it only if it made that deadline.
///
/// The worker may be parked inside a multi-second `ShellExecuteExW` — binding
/// `\\wsl.localhost\<distro>` boots a stopped distribution — and this runs
/// inside `WM_DESTROY`, ahead of the tray-icon removal and the process exit.
/// An unbounded join would keep a ghost icon on screen for as long as the bind
/// takes, so a worker that misses the deadline is detached on purpose.
fn stop_worker() {
    let thread_id = WORKER_THREAD.swap(0, Ordering::Relaxed);
    if thread_id == 0 {
        return;
    }
    unsafe {
        PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
    }

    let mut stopped = false;
    for _ in 0..WORKER_STOP_ATTEMPTS {
        if WORKER_STOPPED.load(Ordering::Acquire) {
            stopped = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(WORKER_STOP_POLL_MS));
    }

    // Take the handle either way: leaving it parked in the static is what
    // makes the timeout path look like a leak.
    let handle = WORKER_JOIN.lock().ok().and_then(|mut slot| slot.take());
    let Some(handle) = handle else {
        return;
    };

    if stopped {
        // It is out of its message loop and past `CoUninitialize`, so the join
        // is immediate and reaps the thread properly.
        let _ = handle.join();
        return;
    }

    // Deliberate detach, not a dropped error: the worker is still inside
    // something that cannot be interrupted, and the process is exiting anyway.
    // The handle goes out of scope here, which detaches the thread and lets
    // process teardown reap it — `WM_DESTROY` continues on to remove the icon.
    log_diagnostic("event=worker_detached");
    drop(handle);
}

/// Raises [`WORKER_BUSY`] for the length of one worker request and lowers it
/// again however the handler returns.
struct WorkerBusy;

impl WorkerBusy {
    fn mark() -> Self {
        WORKER_BUSY.store(true, Ordering::Release);
        Self
    }
}

impl Drop for WorkerBusy {
    fn drop(&mut self) {
        WORKER_BUSY.store(false, Ordering::Release);
    }
}

unsafe extern "system" fn worker_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        PROCESS_ENTER => {
            // `WORKER_BUSY` is the update cycle's veto: an install that
            // force-closes the package while this handler is mid-rewrite would
            // take the user's keystroke down with it.
            let _busy = WorkerBusy::mark();
            process_enter_request(SurfaceKind::from_wparam(wparam), lparam as HWND);
            0
        }
        WORKER_OPEN_PATH => {
            // The tail of the same request: `request_open_path` posts this from
            // inside `process_enter_request`, and the shell navigation it runs
            // is exactly as bad a moment to be terminated in.
            let _busy = WorkerBusy::mark();
            if lparam != 0 {
                // Ownership was handed over by `request_open_path`.
                let path = unsafe { Box::from_raw(lparam as *mut String) };
                if !open_resolved_path(&path) {
                    show_notification("Windows could not open the location.", NIIF_ERROR);
                }
            }
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        FSW_WM_QUERY_STATE => {
            if PAUSED.load(Ordering::Relaxed) {
                BrokerState::Paused as isize
            } else if (KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK).is_null()
                || (WORKER_WINDOW.load(Ordering::Acquire) as HWND).is_null()
            {
                BrokerState::Unavailable as isize
            } else {
                BrokerState::Active as isize
            }
        }
        // Replies with the resulting `BrokerState` (Active=1 / Paused=2), or 0
        // when the request could not be honoured. The old unconditional 1 made
        // a failed resume indistinguishable from a successful one.
        FSW_WM_SET_PAUSED => match set_paused(wparam != 0) {
            Ok(state) => state as isize,
            Err(()) => 0,
        },
        FSW_WM_SHOW_SETTINGS => {
            open_settings_section("general");
            1
        }
        PERSIST_FAILED => {
            show_notification("The pause setting could not be saved.", NIIF_ERROR);
            0
        }
        WM_TIMER => {
            if wparam == HEALTH_TIMER {
                health_tick(window);
            }
            0
        }
        message if message == taskbar_created_message() => {
            ICON_ADDED.store(false, Ordering::Relaxed);
            add_tray_icon(window);
            0
        }
        // Somebody changed shared state (issue #55). It is a broadcast, so it
        // arrives whoever the writer was — the CLI, a shell adapter's staged
        // copy of it, the settings window — and including this process's own
        // writes, which `reload_settings` recognizes as already applied.
        // `message != 0` because a failed registration answers 0, and 0 is
        // WM_NULL — which arrives whenever anything probes this window.
        message if message != 0 && message == state_changed_message() => {
            reload_settings(window);
            0
        }
        // Session end: Windows destroys the window without WM_DESTROY running
        // our cleanup, so remove the icon now or it lingers as a ghost.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            remove_tray_icon(window);
            0
        }
        WM_COMMAND => {
            handle_menu_command(window, u32::try_from(wparam & 0xFFFF).unwrap_or(0));
            0
        }
        TRAY_MESSAGE => {
            // NOTIFYICON_VERSION_4: the notification is the low word of
            // lParam and the anchor point rides in wParam.
            let event = u32::try_from(lparam & 0xFFFF).unwrap_or(0);
            match event {
                WM_CONTEXTMENU => {
                    let anchor = POINT {
                        x: signed_word(wparam, 0),
                        y: signed_word(wparam, 16),
                    };
                    show_tray_menu(window, anchor);
                }
                WM_RBUTTONUP => {
                    // Legacy path: reached only if NIM_SETVERSION never took,
                    // where wParam is the icon id and not a point.
                    let mut cursor: POINT = unsafe { std::mem::zeroed() };
                    unsafe {
                        GetCursorPos(&mut cursor);
                    }
                    show_tray_menu(window, cursor);
                }
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => open_settings_section("general"),
                _ => {}
            }
            0
        }
        WM_CLOSE => unsafe {
            DestroyWindow(window);
            0
        },
        WM_DESTROY => {
            unsafe {
                KillTimer(window, HEALTH_TIMER);
            }
            remove_tray_icon(window);
            remove_hook();
            stop_worker();
            disconnect_filter();
            BROKER_WINDOW.store(0, Ordering::Relaxed);
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn format_resolve_error(err: fsw_path::ResolveError, distributions: &[String]) -> String {
    let mut message = err.message().to_string();
    if err.hint_lists_distributions() && !distributions.is_empty() {
        message.push_str(" Try ");
        let count = distributions.len().min(3);
        for (idx, d) in distributions[..count].iter().enumerate() {
            if idx != 0 {
                message.push_str(", ");
            }
            message.push('/');
            message.push_str(d);
        }
        message.push('.');
    }
    message
}

/// Registers a window class. Both windows of the process go through this,
/// so the `WNDCLASSEXW` layout is described once.
fn register_window_class(name: &[u16], proc: WNDPROC, icon: HICON) -> bool {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = proc;
        wc.hInstance = instance;
        wc.hIcon = icon;
        wc.hIconSm = icon;
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        wc.lpszClassName = name.as_ptr();
        RegisterClassExW(&wc) != 0
    }
}

fn pump_messages() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

/// The worker thread: a second STA that owns the UI Automation object and a
/// message-only window, and does every piece of Enter handling that can block.
///
/// `HWND_MESSAGE` is right here precisely because it receives no broadcasts —
/// unlike the broker window, which needs `TaskbarCreated`.
fn worker_thread_main(ready: &std::sync::mpsc::SyncSender<()>) {
    unsafe {
        if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
            let _ = ready.send(());
            return;
        }

        let class_name = to_u16_vec(WORKER_WINDOW_CLASS);
        if !register_window_class(&class_name, Some(worker_proc), std::ptr::null_mut()) {
            CoUninitialize();
            let _ = ready.send(());
            return;
        }

        let title = to_u16_vec("fwdslash broker worker");
        let worker_wnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null_mut(),
        );
        if worker_wnd.is_null() {
            CoUninitialize();
            let _ = ready.send(());
            return;
        }

        // Created once and kept for the thread's life. A failure here is not
        // fatal: every request then falls through to a plain Enter replay.
        match CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(automation) => AUTOMATION.with_borrow_mut(|slot| *slot = Some(automation)),
            Err(err) => log_diagnostic(&format!("event=debug_uia_failed code={}", err.code().0)),
        }

        WORKER_THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
        WORKER_WINDOW.store(worker_wnd as isize, Ordering::Release);
        let _ = ready.send(());

        pump_messages();

        WORKER_WINDOW.store(0, Ordering::Release);
        AUTOMATION.with_borrow_mut(|slot| *slot = None);
        DestroyWindow(worker_wnd);
        CoUninitialize();
        WORKER_STOPPED.store(true, Ordering::Release);
    }
}

/// Starts the worker and waits for it to publish its window, so the very first
/// Enter after startup already has somewhere to go.
fn start_worker() {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let Ok(handle) = std::thread::Builder::new()
        .name("fsw-worker".to_owned())
        .spawn(move || worker_thread_main(&ready_tx))
    else {
        log_diagnostic("event=worker_start_failed");
        return;
    };
    let _ = ready_rx.recv_timeout(std::time::Duration::from_secs(5));
    if let Ok(mut slot) = WORKER_JOIN.lock() {
        *slot = Some(handle);
    }
}

fn main() {
    unsafe {
        let wide_mutex = to_u16_vec(MUTEX_NAME);
        let mutex = CreateMutexW(std::ptr::null_mut(), 0, wide_mutex.as_ptr());
        if mutex.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            if !mutex.is_null() {
                CloseHandle(mutex);
            }
            return;
        }

        if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
            CloseHandle(mutex);
            return;
        }

        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = to_u16_vec(FSW_BROKER_WINDOW_CLASS);
        let icon = LoadIconW(instance, IDI_FSW_APP as *const u16);
        if !register_window_class(&class_name, Some(window_proc), icon) {
            CoUninitialize();
            CloseHandle(mutex);
            return;
        }

        // Not "Forward Slash Windows": the settings window carried that title
        // too, and a title-based raise could match this one instead.
        let title = to_u16_vec("fwdslash broker");
        // A top-level never-shown tool window, not a message-only one:
        // message-only windows are skipped by HWND_BROADCAST, so
        // TaskbarCreated and WM_ENDSESSION would never reach the icon
        // lifecycle below. Divergence from the C++ broker (HWND_MESSAGE);
        // discovery via FindWindowW on the class is unaffected.
        let broker_wnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );

        if broker_wnd.is_null() {
            CoUninitialize();
            CloseHandle(mutex);
            return;
        }

        BROKER_WINDOW.store(broker_wnd as isize, Ordering::Relaxed);
        // Paused state first: the tray tooltip reflects it at NIM_ADD time.
        PAUSED.store(is_disabled(), Ordering::Relaxed);
        add_tray_icon(broker_wnd);
        // The worker has to exist before the hook does, or the first Enter is
        // classified with nowhere to post it.
        start_worker();
        let hook_installed = PAUSED.load(Ordering::Relaxed) || install_hook();
        update_tray_tooltip(broker_wnd);
        let start_tick = GetTickCount64();
        LAST_MAINTENANCE_MS.store(start_tick, Ordering::Relaxed);
        // The update cycle measures its first delay from here, not from boot.
        BROKER_START_MS.store(start_tick, Ordering::Relaxed);
        SetTimer(broker_wnd, HEALTH_TIMER, HEALTH_INTERVAL_IDLE_MS, None);
        // Switches the timer to 5 s if a driver actually answers.
        publish_filter_mappings(true);

        if !hook_installed {
            show_notification(
                "The shell keyboard hook could not be installed.",
                NIIF_ERROR,
            );
        }

        // Last, and off this thread: the icon and the hook are what the user
        // notices missing, and the sweep may take minutes.
        start_adapter_upgrade();

        pump_messages();

        CoUninitialize();
        CloseHandle(mutex);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Only the pure decisions are covered here: everything else in this binary is
// a Win32 call, a child process or a thread, and none of those has a meaning
// outside a running broker. The cost of getting one of these three wrong is a
// balloon at every logon, an update that never runs, or one that runs while
// the user is typing in an address bar.
#[cfg(test)]
mod tests {
    use super::{
        AdapterOutcome, UPDATE_CONSIDER_INTERVAL_MS, UPDATE_FIRST_DELAY_MS, adapter_outcome,
        drain_persist_queue, request_target_is_current, should_balloon_install_failure,
        should_balloon_update, should_swallow_enter, update_cycle_age_ms, update_cycle_due,
    };

    // -- the cycle gate ----------------------------------------------------

    #[test]
    fn a_due_idle_allowed_broker_starts_a_cycle() {
        assert!(update_cycle_due(
            false,
            UPDATE_CONSIDER_INTERVAL_MS,
            false,
            true
        ));
    }

    #[test]
    fn each_gate_alone_stops_the_cycle() {
        // Already running.
        assert!(!update_cycle_due(
            true,
            UPDATE_CONSIDER_INTERVAL_MS,
            false,
            true
        ));
        // Not due yet.
        assert!(!update_cycle_due(
            false,
            UPDATE_CONSIDER_INTERVAL_MS - 1,
            false,
            true
        ));
        // The Enter worker is mid-request.
        assert!(!update_cycle_due(
            false,
            UPDATE_CONSIDER_INTERVAL_MS,
            true,
            true
        ));
        // Unpackaged, or Automatic updates off.
        assert!(!update_cycle_due(
            false,
            UPDATE_CONSIDER_INTERVAL_MS,
            false,
            false
        ));
    }

    #[test]
    fn the_first_cycle_waits_out_the_startup_delay() {
        // Ticks are ms since boot; a broker started an hour in.
        const START: u64 = 3_600_000;
        assert_eq!(update_cycle_age_ms(START, START, 0), 0);
        assert_eq!(
            update_cycle_age_ms(START + UPDATE_FIRST_DELAY_MS - 1, START, 0),
            0
        );
        assert_eq!(
            update_cycle_age_ms(START + UPDATE_FIRST_DELAY_MS, START, 0),
            UPDATE_CONSIDER_INTERVAL_MS
        );
    }

    #[test]
    fn later_cycles_are_a_full_interval_apart() {
        const START: u64 = 3_600_000;
        let last = START + UPDATE_FIRST_DELAY_MS;
        assert_eq!(update_cycle_age_ms(last, START, last), 0);
        assert_eq!(
            update_cycle_age_ms(last + UPDATE_CONSIDER_INTERVAL_MS, START, last),
            UPDATE_CONSIDER_INTERVAL_MS
        );
        // A tick count that went backwards must not read as an enormous age.
        assert_eq!(update_cycle_age_ms(last - 1_000, START, last), 0);
    }

    // -- balloon dedupe ----------------------------------------------------

    #[test]
    fn one_version_produces_one_balloon() {
        assert!(should_balloon_update(Some("0.0.5"), None));
        assert!(!should_balloon_update(Some("0.0.5"), Some("0.0.5")));
        // A second update replacing the first is news again.
        assert!(should_balloon_update(Some("0.0.6"), Some("0.0.5")));
    }

    #[test]
    fn a_nameless_update_is_announced_once() {
        assert!(should_balloon_update(None, None));
        assert!(!should_balloon_update(None, Some("0.0.5")));
    }

    #[test]
    fn only_the_store_flavor_reports_a_failed_install_and_only_on_the_second() {
        assert!(!should_balloon_install_failure(1, true));
        assert!(should_balloon_install_failure(2, true));
        assert!(should_balloon_install_failure(7, true));
        // The GitHub flavor's bundle applies itself at the next logon.
        assert!(!should_balloon_install_failure(2, false));
    }

    // -- #56: retry and deferral ------------------------------------------

    #[test]
    fn a_first_pass_success_never_retries() {
        // The caller passes `None` for the retry it did not run.
        assert_eq!(adapter_outcome(Some(0), None), AdapterOutcome::Upgraded);
    }

    #[test]
    fn a_retry_that_lands_is_silent_success() {
        assert_eq!(adapter_outcome(Some(1), Some(0)), AdapterOutcome::Upgraded);
        assert_eq!(adapter_outcome(None, Some(0)), AdapterOutcome::Upgraded);
    }

    #[test]
    fn two_unanswered_attempts_defer_instead_of_ballooning() {
        // Killed at the deadline, or never spawned: the marker key still reads
        // the old version, so the next launch tries again.
        assert_eq!(adapter_outcome(None, None), AdapterOutcome::Deferred);
        assert_eq!(adapter_outcome(Some(1), None), AdapterOutcome::Deferred);
    }

    #[test]
    fn a_second_refusal_is_the_one_the_user_must_see() {
        assert_eq!(adapter_outcome(Some(1), Some(1)), AdapterOutcome::NeedsUser);
        assert_eq!(adapter_outcome(None, Some(2)), AdapterOutcome::NeedsUser);
    }

    // -- #65: pause persistence ordering ----------------------------------

    #[test]
    fn persistence_queue_keeps_rapid_toggles_in_order() {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(true).unwrap();
        sender.send(false).unwrap();
        sender.send(true).unwrap();
        drop(sender);

        let mut persisted = Vec::new();
        drain_persist_queue(receiver, |disabled| persisted.push(disabled));
        assert_eq!(persisted, [true, false, true]);
    }

    // -- #7/#64: hook admission and stale replay --------------------------

    #[test]
    fn null_worker_never_admits_an_enter_for_suppression() {
        // PostMessageW(NULL, ...) may report success, but it is not a worker.
        assert!(!should_swallow_enter(false, true));
        assert!(!should_swallow_enter(false, false));
        assert!(!should_swallow_enter(true, false));
        assert!(should_swallow_enter(true, true));
    }

    #[test]
    fn stale_request_never_replays_into_a_new_foreground_window() {
        let original = 101isize as HWND;
        assert!(request_target_is_current(original, original));
        assert!(!request_target_is_current(original, 202isize as HWND));
    }
}
