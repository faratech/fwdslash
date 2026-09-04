#![windows_subsystem = "windows"]

use fsw_core::*;
use fsw_path::{RenderBuf, eq_ignore_case};
use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
    CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationLegacyIAccessiblePattern,
    IUIAutomationValuePattern, UIA_LegacyIAccessiblePatternId, UIA_ValuePatternId,
    UIA_ValueValuePropertyId,
};
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
use windows::core::{BSTR, Interface};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM,
    LRESULT, POINT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, KEYEVENTF_KEYUP, SendInput, VK_ESCAPE, VK_RETURN,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, SHELLEXECUTEINFOW,
    Shell_NotifyIconW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetClassNameW, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, HHOOK, IDC_ARROW, KBDLLHOOKSTRUCT, KillTimer,
    LLKHF_UP, LoadCursorW, LoadIconW, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SW_SHOWNORMAL,
    SetForegroundWindow, SetTimer, SetWindowsHookExW, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_KEYUP, WM_LBUTTONDBLCLK, WM_RBUTTONUP,
    WM_QUERYENDSESSION, WM_SYSKEYUP, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

const MUTEX_NAME: &str = "Local\\ForwardSlashWindows.Broker";
const TRAY_MESSAGE: u32 = WM_APP + 1;
const PROCESS_ENTER: u32 = WM_APP + 2;
const TRAY_ID: usize = 1;
const HEALTH_TIMER: usize = 1;
const REPLAY_MARKER: usize = 0x4653_572F;

const MENU_SETTINGS: u32 = 1001;
const MENU_PAUSE: u32 = 1002;
const MENU_EXIT: u32 = 1003;
const MENU_OPEN_ROOT: u32 = 1004;
const MENU_WINDOWS: u32 = 1005;
const MENU_CMD: u32 = 1006;
const MENU_WINDOWS_POWERSHELL: u32 = 1007;
const MENU_POWERSHELL: u32 = 1008;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceKind {
    Unknown,
    Explorer,
    Run,
    Search,
    CommonDialog,
}

static PAUSED: AtomicBool = AtomicBool::new(false);
static ENTER_DOWN: AtomicBool = AtomicBool::new(false);
static SUPPRESS_ENTER_UP: AtomicBool = AtomicBool::new(false);

/// Registry state re-read on the Enter hot path, cached briefly: five registry
/// opens per keystroke buy nothing when a settings change is visible within a
/// quarter second anyway. The broker-local cache lives here (not in
/// `fsw-core`) so the funnel stays a pure pass-through.
static SNAPSHOT_CACHE: Mutex<Option<(u64, fsw_core::Snapshot)>> = Mutex::new(None);
const SNAPSHOT_CACHE_TTL_MS: u64 = 250;

/// Returns the current registry snapshot, serving repeats within the TTL from
/// the cache. Falls back to a fresh read whenever the mutex is poisoned —
/// a cache must never block resolution.
fn current_snapshot() -> fsw_core::Snapshot {
    if let Ok(guard) = SNAPSHOT_CACHE.lock() {
        if let Some((stamp, snapshot)) = guard.as_ref() {
            if stamp.wrapping_add(SNAPSHOT_CACHE_TTL_MS) > unsafe { GetTickCount64() } {
                return snapshot.clone();
            }
        }
    }
    let snapshot = Snapshot::current();
    if let Ok(mut guard) = SNAPSHOT_CACHE.lock() {
        let now = unsafe { GetTickCount64() };
        *guard = Some((now, snapshot.clone()));
    }
    snapshot
}

static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);
static BROKER_WINDOW: AtomicIsize = AtomicIsize::new(0);
static FILTER_PORT: AtomicIsize = AtomicIsize::new(-1);

thread_local! {
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
                let _ = writeln!(f, "{}", msg);
            }
        }
    }
}

fn process_name(process_id: u32) -> String {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return String::new();
        }
        let mut image = [0u16; 32768];
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

fn classify_surface(foreground: HWND) -> SurfaceKind {
    if foreground.is_null() {
        return SurfaceKind::Unknown;
    }
    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(foreground, &mut process_id);
    }
    let proc = process_name(process_id);
    let class = window_class(foreground);

    if eq_ignore_case(&proc, "SearchHost.exe")
        || eq_ignore_case(&proc, "SearchApp.exe")
        || eq_ignore_case(&proc, "StartMenuExperienceHost.exe")
    {
        return SurfaceKind::Search;
    }

    if eq_ignore_case(&proc, "explorer.exe") {
        if eq_ignore_case(&class, "CabinetWClass") || eq_ignore_case(&class, "ExploreWClass") {
            return SurfaceKind::Explorer;
        }
        if eq_ignore_case(&class, "#32770") {
            return SurfaceKind::Run;
        }
    }

    if eq_ignore_case(&class, "#32770") {
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

fn set_focused_value(focused: &IUIAutomationElement, value: &str) -> bool {
    unsafe {
        let pattern =
            match focused.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
                Ok(p) => p,
                Err(_) => return false,
            };
        let bstr = BSTR::from(value);
        let hr = pattern.SetValue(&bstr);
        hr.is_ok()
    }
}

fn open_resolved_path(path: &str) -> bool {
    unsafe {
        let wide_verb = to_u16_vec("open");
        let wide_file = to_u16_vec(path);
        let mut exec: SHELLEXECUTEINFOW = std::mem::zeroed();
        exec.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
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

fn navigate_explorer_window(foreground: HWND, path: &str) -> bool {
    unsafe {
        let shell_windows: IShellWindows =
            match CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) {
                Ok(sw) => sw,
                Err(_) => return false,
            };
        let count = match shell_windows.Count() {
            Ok(c) => c,
            Err(_) => return false,
        };

        for i in 0..count {
            let item_var = VARIANT::from(i as i32);
            if let Ok(disp) = shell_windows.Item(&item_var) {
                if let Ok(browser) = disp.cast::<IWebBrowser2>() {
                    if let Ok(hwnd_num) = browser.HWND() {
                        if hwnd_num.0 as isize == foreground as isize {
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
    if broker_wnd.is_null() {
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

fn process_enter_request(foreground: HWND) {
    if PAUSED.load(Ordering::Relaxed) || foreground != unsafe { GetForegroundWindow() } {
        replay_enter();
        return;
    }

    let surface = classify_surface(foreground);
    if surface == SurfaceKind::Unknown {
        replay_enter();
        return;
    }

    let automation = AUTOMATION.with_borrow(|a| a.clone());
    let automation = match automation {
        Some(a) => a,
        None => {
            replay_enter();
            return;
        }
    };

    let focused = match unsafe { automation.GetFocusedElement() } {
        Ok(el) => el,
        Err(_) => {
            replay_enter();
            return;
        }
    };

    let input = match read_focused_value(&focused) {
        Some(val) => val,
        None => {
            replay_enter();
            return;
        }
    };

    if input.is_empty() || !input.starts_with('/') {
        replay_enter();
        return;
    }

    let snap = current_snapshot();
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

    log_diagnostic(if resolved.distribution().is_none() {
        "event=route_wsl_root"
    } else {
        "event=route_distribution"
    });

    let unc_path = resolved.unc_display();

    if surface == SurfaceKind::Search {
        if !open_resolved_path(unc_path) {
            show_notification("Windows could not open the WSL location.", NIIF_ERROR);
        }
        send_virtual_key(VK_ESCAPE);
        return;
    }

    if surface == SurfaceKind::Explorer
        && resolved.distribution().is_none()
        && navigate_explorer_window(foreground, unc_path)
    {
        return;
    }

    if set_focused_value(&focused, unc_path) {
        replay_enter();
        return;
    }

    if !open_resolved_path(unc_path) {
        show_notification("Windows could not open the WSL location.", NIIF_ERROR);
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
    if key.vkCode != VK_RETURN as u32 || key.dwExtraInfo == REPLAY_MARKER {
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
    if PAUSED.load(Ordering::Relaxed) || classify_surface(foreground) == SurfaceKind::Unknown {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let broker_wnd = BROKER_WINDOW.load(Ordering::Relaxed) as HWND;
    if unsafe { PostMessageW(broker_wnd, PROCESS_ENTER, 0, foreground as LPARAM) } == 0 {
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
    let ok = AUTOMATION.with_borrow_mut(|guard| {
        if guard.is_none() {
            match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
                Ok(auto) => {
                    *guard = Some(auto);
                    true
                }
                Err(err) => {
                    log_diagnostic(&format!("event=debug_uia_failed code={}", err.code().0));
                    false
                }
            }
        } else {
            true
        }
    });
    if !ok {
        return false;
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
        log_diagnostic(&format!(
            "event=debug_hook_failed error={}",
            unsafe { GetLastError() }
        ));
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
    AUTOMATION.with_borrow_mut(|guard| *guard = None);
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
/// The health timer fires every five seconds forever, so without this the broker
/// would re-enumerate the Lxss subtree and make a `FilterSendMessage` kernel
/// round-trip on every tick. Mirrors `g_published_distributions`
/// (`src/broker/main.cpp:54`).
static PUBLISHED_DISTRIBUTIONS: Mutex<Option<Vec<String>>> = Mutex::new(None);

fn publish_filter_mappings(force: bool) {
    let distributions = if PAUSED.load(Ordering::Relaxed) {
        Vec::new()
    } else {
        let mut distros = list_registered_distributions();
        // Ordinal case-insensitive, matching the C++ `CompareStringOrdinal` sort.
        // The driver receives this array in order, so the comparison has to agree.
        distros.sort_by(|a, b| {
            let a_folded: Vec<char> = a.chars().flat_map(char::to_uppercase).collect();
            let b_folded: Vec<char> = b.chars().flat_map(char::to_uppercase).collect();
            a_folded.cmp(&b_folded)
        });
        distros
    };

    // Nothing changed and nobody asked for a resend, so skip the kernel round-trip.
    if !force
        && PUBLISHED_DISTRIBUTIONS
            .lock()
            .is_ok_and(|published| published.as_ref() == Some(&distributions))
    {
        return;
    }

    let mut port = FILTER_PORT.load(Ordering::Relaxed) as HANDLE;
    if port == INVALID_HANDLE_VALUE || port.is_null() {
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
            return;
        }
        port = connected_port;
        FILTER_PORT.store(port as isize, Ordering::Relaxed);
    }

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
            &msg as *const _ as *const _,
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

/// Distinct from the settings app's `"fwdslash settings"` so the two tray
/// icons are identifiable at a glance; both used to read identically.
fn tray_tip() -> &'static str {
    if PAUSED.load(Ordering::Relaxed) {
        "fwdslash broker (paused)"
    } else {
        "fwdslash broker"
    }
}

fn set_tray_icon(window: HWND, add: bool) {
    unsafe {
        let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
        icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = window;
        icon.uID = TRAY_ID as u32;
        icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        icon.uCallbackMessage = TRAY_MESSAGE;
        icon.hIcon = LoadIconW(GetModuleHandleW(std::ptr::null()), IDI_FSW_APP as *const u16);

        let tip = to_u16_vec(tray_tip());
        let tip_len = tip.len().min(icon.szTip.len() - 1);
        icon.szTip[..tip_len].copy_from_slice(&tip[..tip_len]);

        Shell_NotifyIconW(if add { NIM_ADD } else { NIM_DELETE }, &icon);
        if add {
            icon.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &icon);
        }
    }
}

/// Re-announces the tooltip (NIM_MODIFY) without touching the icon itself.
fn update_tray_tooltip(window: HWND) {
    unsafe {
        let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
        icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = window;
        icon.uID = TRAY_ID as u32;
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

fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
    let _ = persist_disabled(paused);
    if paused {
        remove_hook();
    } else {
        install_hook();
    }
    let window = BROKER_WINDOW.load(Ordering::Relaxed);
    if window != 0 {
        update_tray_tooltip(window as HWND);
    }
    publish_filter_mappings(true);
}

fn open_settings_section(section: &str) {
    let dir = match executable_directory() {
        Ok(d) => d,
        Err(_) => {
            show_notification("The settings application could not be located.", NIIF_ERROR);
            return;
        }
    };
    let exe = dir.join("fswsettings.exe");
    let arg = format!("fwdslash://settings/{}", section);

    unsafe {
        let wide_verb = to_u16_vec("open");
        let wide_file = to_u16_vec(&exe.to_string_lossy());
        let wide_arg = to_u16_vec(&arg);

        let mut exec: SHELLEXECUTEINFOW = std::mem::zeroed();
        exec.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
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

fn show_tray_menu(window: HWND) {
    unsafe {
        let mut cursor: POINT = std::mem::zeroed();
        GetCursorPos(&mut cursor);

        let menu = CreatePopupMenu();
        let integrations = CreatePopupMenu();

        let s_settings = to_u16_vec("Settings...");
        let s_windows = to_u16_vec("Windows surfaces");
        let s_cmd = to_u16_vec("Command Prompt");
        let s_win_ps = to_u16_vec("Windows PowerShell");
        let s_ps7 = to_u16_vec("PowerShell 7");
        let s_integrations = to_u16_vec("Integrations");
        let s_open_root = to_u16_vec("Open WSL root");
        let pause_label = if PAUSED.load(Ordering::Relaxed) {
            "Enable"
        } else {
            "Disable"
        };
        let s_pause = to_u16_vec(pause_label);
        let s_exit = to_u16_vec("Exit");

        AppendMenuW(menu, MF_STRING, MENU_SETTINGS as usize, s_settings.as_ptr());
        AppendMenuW(
            integrations,
            MF_STRING,
            MENU_WINDOWS as usize,
            s_windows.as_ptr(),
        );
        AppendMenuW(integrations, MF_STRING, MENU_CMD as usize, s_cmd.as_ptr());
        AppendMenuW(
            integrations,
            MF_STRING,
            MENU_WINDOWS_POWERSHELL as usize,
            s_win_ps.as_ptr(),
        );
        AppendMenuW(
            integrations,
            MF_STRING,
            MENU_POWERSHELL as usize,
            s_ps7.as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_POPUP,
            integrations as usize,
            s_integrations.as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            MENU_OPEN_ROOT as usize,
            s_open_root.as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_PAUSE as usize, s_pause.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_EXIT as usize, s_exit.as_ptr());

        SetForegroundWindow(window);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON,
            cursor.x,
            cursor.y,
            0,
            window,
            std::ptr::null(),
        );
        DestroyMenu(menu);
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
        FSW_WM_SET_PAUSED => {
            set_paused(wparam != 0);
            1
        }
        FSW_WM_SHOW_SETTINGS => {
            open_settings_section("general");
            1
        }
        PROCESS_ENTER => {
            process_enter_request(lparam as HWND);
            0
        }
        WM_TIMER => {
            if wparam == HEALTH_TIMER {
                publish_filter_mappings(false);
            }
            0
        }
        message if message == taskbar_created_message() => {
            set_tray_icon(window, true);
            0
        }
        // Session end: Windows destroys the window without WM_DESTROY running
        // our cleanup, so remove the icon now or it lingers as a ghost.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            set_tray_icon(window, false);
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as u32;
            match id {
                MENU_SETTINGS => open_settings_section("general"),
                MENU_WINDOWS => open_settings_section("windows"),
                MENU_CMD => open_settings_section("cmd"),
                MENU_WINDOWS_POWERSHELL => open_settings_section("windows-powershell"),
                MENU_POWERSHELL => open_settings_section("powershell"),
                MENU_OPEN_ROOT => {
                    open_resolved_path("\\\\wsl.localhost");
                }
                MENU_PAUSE => {
                    let current = PAUSED.load(Ordering::Relaxed);
                    set_paused(!current);
                }
                MENU_EXIT => unsafe {
                    DestroyWindow(window);
                },
                _ => {}
            }
            0
        }
        TRAY_MESSAGE => {
            let evt = (lparam & 0xFFFF) as u32;
            if evt == WM_RBUTTONUP || evt == WM_CONTEXTMENU {
                show_tray_menu(window);
            } else if evt == WM_LBUTTONDBLCLK {
                open_settings_section("general");
            }
            0
        }
        WM_CLOSE => unsafe {
            DestroyWindow(window);
            0
        },
        WM_DESTROY => unsafe {
            KillTimer(window, HEALTH_TIMER);
            set_tray_icon(window, false);
            remove_hook();
            disconnect_filter();
            BROKER_WINDOW.store(0, Ordering::Relaxed);
            PostQuitMessage(0);
            0
        },
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

        let class_name = to_u16_vec(FSW_BROKER_WINDOW_CLASS);
        let instance = GetModuleHandleW(std::ptr::null());

        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(window_proc);
        wc.hInstance = instance;
        wc.hIcon = LoadIconW(instance, IDI_FSW_APP as *const u16);
        wc.hIconSm = wc.hIcon;
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        wc.lpszClassName = class_name.as_ptr();

        if RegisterClassExW(&wc) == 0 {
            CoUninitialize();
            CloseHandle(mutex);
            return;
        }

        let title = to_u16_vec("Forward Slash Windows");
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
        set_tray_icon(broker_wnd, true);
        let hook_installed = PAUSED.load(Ordering::Relaxed) || install_hook();
        publish_filter_mappings(true);
        SetTimer(broker_wnd, HEALTH_TIMER, 5000, None);

        if !hook_installed {
            show_notification(
                "The shell keyboard hook could not be installed.",
                NIIF_ERROR,
            );
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        CoUninitialize();
        CloseHandle(mutex);
    }
}
