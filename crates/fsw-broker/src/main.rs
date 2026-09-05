#![windows_subsystem = "windows"]

use fsw_core::*;
use fsw_path::{RenderBuf, eq_ignore_case};
use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::sync::Mutex;
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
        if focused.CurrentIsPassword().is_ok_and(BOOL::as_bool) {
            return None;
        }
        let pattern = focused
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()?;
        if pattern.CurrentIsReadOnly().is_ok_and(BOOL::as_bool) {
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
    // No worker to hand it to: reclaim the allocation and open inline.
    let path = unsafe { Box::from_raw(owned) };
    if !open_resolved_path(&path) {
        show_notification("Windows could not open the location.", NIIF_ERROR);
    }
}

fn navigate_explorer_window(foreground: HWND, path: &str) -> bool {
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
fn process_enter_request(surface: SurfaceKind, foreground: HWND) {
    if PAUSED.load(Ordering::Relaxed) {
        replay_enter();
        return;
    }

    if foreground != unsafe { GetForegroundWindow() } {
        // The user moved on while this request was queued. Replaying Enter now
        // would inject it into whatever they switched to — a half-written chat
        // message sent, a half-typed command run. Drop it instead.
        log_diagnostic("event=enter_dropped_foreground_changed");
        return;
    }

    if surface == SurfaceKind::Unknown {
        replay_enter();
        return;
    }

    let Some(automation) = AUTOMATION.with_borrow(Option::clone) else {
        replay_enter();
        return;
    };

    let Ok(focused) = (unsafe { automation.GetFocusedElement() }) else {
        replay_enter();
        return;
    };

    let Some(value_pattern) = editable_value_pattern(&focused) else {
        log_diagnostic("event=surface_rejected");
        replay_enter();
        return;
    };

    let Some(input) = read_focused_value(&focused) else {
        replay_enter();
        return;
    };

    if !input.starts_with('/') {
        replay_enter();
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
        if !open_resolved_path(unc_path) {
            show_notification("Windows could not open the location.", NIIF_ERROR);
        }
        send_virtual_key(VK_ESCAPE);
        return;
    }

    if surface == SurfaceKind::Explorer
        && resolved.is_provider_root()
        && navigate_explorer_window(foreground, unc_path)
    {
        return;
    }

    if set_pattern_value(&value_pattern, unc_path) {
        replay_enter();
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
    if unsafe { PostMessageW(worker, PROCESS_ENTER, surface as usize, foreground as LPARAM) } == 0 {
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
    } else if (KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK).is_null() {
        "Forward Slash Windows \u{2014} hook unavailable"
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

/// Writes the pause flag from a thread of its own.
///
/// `persist_disabled` shells out to `reg.exe` (see its doc comment for why a
/// child process and not a direct write). That is a process creation plus a
/// wait — unbounded under load — and the caller here is the thread that owns
/// the low-level keyboard hook, where any wait freezes every keystroke on the
/// machine. The pause therefore takes effect in memory immediately and the
/// persistence is reported asynchronously: `FSW_WM_SET_PAUSED` replies from
/// in-memory state plus the hook result, and a failed write surfaces later as
/// a balloon plus `event=persist_disabled_failed`.
fn request_persist_disabled(disabled: bool) {
    let spawned = std::thread::Builder::new()
        .name("fsw-persist".to_owned())
        .spawn(move || {
            if persist_disabled(disabled).is_err() {
                log_diagnostic("event=persist_disabled_failed");
                let window = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
                if !window.is_null() {
                    unsafe {
                        PostMessageW(window, PERSIST_FAILED, 0, 0);
                    }
                }
            }
        });
    if spawned.is_err() {
        // Out of threads: the write is the whole point of the setting, so do
        // it inline rather than silently skip it.
        if persist_disabled(disabled).is_err() {
            log_diagnostic("event=persist_disabled_failed");
            show_notification("The pause setting could not be saved.", NIIF_ERROR);
        }
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

/// Runs one `fwdslash integration <id> enable` to completion, bounded by
/// [`ADAPTER_UPGRADE_TIMEOUT`]. Reports whether the CLI exited successfully.
///
/// The CLI does the transactional uninstall+install itself and is idempotent
/// once the recorded version already matches, so racing a manual enable from
/// the settings app costs at worst a redundant reinstall.
fn run_adapter_upgrade(cli: &Path, id: &str) -> bool {
    let spawned = std::process::Command::new(cli)
        .arg("integration")
        .arg(id)
        .arg("enable")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let Ok(mut child) = spawned else {
        return false;
    };

    let deadline = std::time::Instant::now() + ADAPTER_UPGRADE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => return false,
        }
        if std::time::Instant::now() >= deadline {
            // Half-applied is the transaction's problem, not ours: the CLI
            // rolls its own snapshot back, and the marker key still reads the
            // old version, so the next launch tries again.
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(ADAPTER_UPGRADE_POLL_MS));
    }
}

/// Runs one `fwdslash repair-adapters` to completion, bounded by
/// [`ADAPTER_UPGRADE_TIMEOUT`]. Fire-and-forget: a repair that fails or times
/// out just retries next launch, and the guarded profile block means an
/// un-repaired orphan is silent, never a red shell error (#37).
fn run_adapter_repair(cli: &Path) {
    let spawned = std::process::Command::new(cli)
        .arg("repair-adapters")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    let Ok(mut child) = spawned else {
        return;
    };
    let deadline = std::time::Instant::now() + ADAPTER_UPGRADE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => return,
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(ADAPTER_UPGRADE_POLL_MS));
    }
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

/// Brings every installed shell adapter whose recorded payload version is not
/// this build's back up to date, with no click from the user.
///
/// An adapter's payload — the scripts plus a private copy of `fwdslash.exe` —
/// is copied into `%LOCALAPPDATA%` at install time, so after a product update
/// the previous copy keeps serving every console until someone re-runs
/// `fwdslash integration <id> enable`. The broker is the one component that
/// starts at every logon and after every update, so the check belongs here.
fn adapter_upgrade_sweep() {
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
        let mut failed = false;
        for (id, label) in outdated {
            if run_adapter_upgrade(&cli, id) {
                log_diagnostic("event=adapter_upgraded");
                upgraded.push(label);
            } else {
                log_diagnostic("event=adapter_upgrade_failed");
                failed = true;
            }
        }

        // Exactly one balloon, whatever the mix: a per-adapter notification
        // would stack three toasts on top of a logon the user did not ask
        // about.
        if failed {
            notify_when_icon_ready(
                "Some terminal integrations could not be updated automatically. Open Settings to retry.",
                NIIF_WARNING,
            );
        } else {
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

/// Applies a pause/resume and reports the state the broker ended up in.
///
/// `Err` means the resume could not arm the keyboard hook, i.e. the broker is
/// `Unavailable`. The persistence result is deliberately *not* part of it —
/// see [`request_persist_disabled`].
fn set_paused(paused: bool) -> Result<BrokerState, ()> {
    PAUSED.store(paused, Ordering::Relaxed);

    // Unhook before persisting: the write is off-thread now, but the ordering
    // is what guarantees a pause stops swallowing Enter immediately.
    let hook_ok = if paused {
        remove_hook();
        true
    } else {
        install_hook()
    };

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

unsafe extern "system" fn worker_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        PROCESS_ENTER => {
            process_enter_request(SurfaceKind::from_wparam(wparam), lparam as HWND);
            0
        }
        WORKER_OPEN_PATH => {
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
            } else if (KEYBOARD_HOOK.load(Ordering::Relaxed) as HHOOK).is_null() {
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
        LAST_MAINTENANCE_MS.store(GetTickCount64(), Ordering::Relaxed);
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
