//! Conservative browser admission for the broker's Enter transaction.
//!
//! Top-level classes are candidates only. The worker additionally verifies
//! App Paths provenance and an observed UI Automation profile before reading
//! or replacing text. Unknown profiles stay native by design.

use std::ffi::OsStr;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationElement, UIA_ComboBoxControlTypeId, UIA_DocumentControlTypeId,
    UIA_ToolBarControlTypeId,
};
use windows_sys::Win32::Foundation::{CloseHandle, HWND};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Input::Ime::ImmGetDefaultIMEWnd;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;

/// The sole Chromium UI profile recorded so far. This is not a claim that all
/// Chromium-family versions expose the same tree.
pub const OBSERVED_CHROMIUM_PROFILE: &str = "edge-152-arm64-omnibox-v1";
/// The Brave profile observed in the isolated 152.1.94.121 compatibility run.
pub const OBSERVED_BRAVE_PROFILE: &str = "brave-152.1.94.121-omnibox-v1";
/// The Firefox profile observed on the installed ARM64 Firefox 155 build.
/// It is deliberately locale/version specific rather than a Gecko-wide claim.
pub const OBSERVED_GECKO_PROFILE: &str = "firefox-155-arm64-urlbar-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserFamily {
    Chromium,
    Gecko,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusEligibility {
    Eligible {
        family: BrowserFamily,
        profile: &'static str,
    },
    /// Gecko process ownership passed, but no observed UIA profile exists.
    UnverifiedGecko,
    Rejected,
}

const MAX_CONTROL_VIEW_ANCESTRY: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
struct StructuralNode {
    process_id: u32,
    class_name: String,
    automation_id: String,
    name: String,
    control_type: i32,
    native_window: isize,
}

fn equals(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[allow(dead_code)] // Current TrustedSurface admission owns the class predicate.
pub fn is_candidate_window_class(class: &str) -> bool {
    equals(class, "Chrome_WidgetWin_0")
        || equals(class, "Chrome_WidgetWin_1")
        || equals(class, "MozillaWindowClass")
}

pub fn family_for_executable(executable: &str) -> Option<BrowserFamily> {
    match executable.to_ascii_lowercase().as_str() {
        "msedge.exe" | "chrome.exe" | "brave.exe" | "vivaldi.exe" | "opera.exe"
        | "chromium.exe" => Some(BrowserFamily::Chromium),
        "firefox.exe" | "librewolf.exe" | "waterfox.exe" => Some(BrowserFamily::Gecko),
        _ => None,
    }
}

#[allow(dead_code)] // Retained as the class-admission companion for callers outside this module.
pub fn is_browser_executable_name(executable: &str) -> bool {
    family_for_executable(executable).is_some()
}

fn to_wide(value: &str) -> Vec<u16> {
    let mut value: Vec<u16> = OsStr::new(value).encode_wide().collect();
    value.push(0);
    value
}

fn from_wide(value: &[u16]) -> String {
    let length = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    std::ffi::OsString::from_wide(value.get(..length).unwrap_or(value))
        .to_string_lossy()
        .into_owned()
}

fn process_image_path(process_id: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return None;
        }
        // `MAX_PATH`-sized for the common case; a longer image path (the
        // \\?\ namespace) simply fails the query and the surface is treated as
        // unattested, which is the safe outcome.
        let mut image = [0u16; 1024];
        let mut length = u32::try_from(image.len()).ok()?;
        let ok = QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &raw mut length);
        CloseHandle(process);
        if ok == 0 {
            return None;
        }
        image.get(..usize::try_from(length).ok()?).map(from_wide)
    }
}

fn app_path_registration(executable: &str) -> Option<String> {
    let subkey = to_wide(&format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{executable}"
    ));
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(hive, subkey.as_ptr(), 0, KEY_READ, &raw mut key) != 0 {
                continue;
            }
            let mut bytes = 0u32;
            let status = RegQueryValueExW(
                key,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut bytes,
            );
            if status != 0 || bytes == 0 || !bytes.is_multiple_of(2) {
                RegCloseKey(key);
                continue;
            }
            let mut value = vec![0u16; (bytes / 2) as usize];
            let status = RegQueryValueExW(
                key,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                value.as_mut_ptr().cast(),
                &raw mut bytes,
            );
            RegCloseKey(key);
            if status == 0 {
                return Some(from_wide(&value));
            }
        }
    }
    None
}

fn normalized_path(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .replace('/', "\\")
        .to_ascii_lowercase()
}

/// App Paths is the authority: a same-named binary elsewhere is rejected.
pub fn registered_path_matches(process_path: &str, registration: &str) -> bool {
    normalized_path(process_path) == normalized_path(registration)
}

fn registered_browser_family(process_id: u32) -> Option<BrowserFamily> {
    let path = process_image_path(process_id)?;
    let executable = Path::new(&path).file_name()?.to_str()?;
    let family = family_for_executable(executable)?;
    let registration = app_path_registration(executable)?;
    registered_path_matches(&path, &registration).then_some(family)
}

fn is_observed_chromium_omnibox(element: &IUIAutomationElement) -> bool {
    let Ok(class) = (unsafe { element.CurrentClassName() }) else {
        return false;
    };
    // The class plus the App Paths provenance check in
    // `registered_browser_family` are the identity claim. The accessible name
    // is deliberately not matched: Chromium localizes it, and an English name
    // requirement would silently exclude every localized installation. A
    // web-content edit control never carries the Chromium chrome class.
    equals(&class.to_string(), "OmniboxViewViews")
}

fn is_web_content_class(class: &str) -> bool {
    let class = class.to_ascii_lowercase();
    class == "document"
        || class.contains("renderwidget")
        || class.contains("webview")
        || class == "internet explorer_server"
}

/// Pure structural proof used by the UIA walker and its decoy tests.  A
/// matching accessible name, ID, and process is never sufficient: every node
/// must be browser chrome, and the chain must end at the captured foreground
/// HWND before the bounded walk expires.
fn control_view_chain_is_browser_chrome(
    nodes: &[StructuralNode],
    foreground_process: u32,
    foreground: isize,
) -> bool {
    if nodes.is_empty() || nodes.len() > MAX_CONTROL_VIEW_ANCESTRY {
        return false;
    }
    for node in nodes {
        // IDs and names are intentionally non-authoritative. Reading them
        // into the structural record makes that contract explicit in tests.
        let _ = (&node.automation_id, &node.name);
        if node.process_id != foreground_process
            || node.control_type == UIA_DocumentControlTypeId.0
            || is_web_content_class(&node.class_name)
        {
            return false;
        }
        if node.native_window == foreground {
            return true;
        }
    }
    false
}

fn has_safe_control_view_ancestry(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    foreground_process: u32,
    foreground: HWND,
) -> bool {
    let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
        return false;
    };
    let mut current = focused.clone();
    let mut nodes = Vec::with_capacity(MAX_CONTROL_VIEW_ANCESTRY);
    for _ in 0..MAX_CONTROL_VIEW_ANCESTRY {
        let Ok(process_id) = (unsafe { current.CurrentProcessId() }) else {
            return false;
        };
        let Ok(class_name) = (unsafe { current.CurrentClassName() }) else {
            return false;
        };
        let Ok(automation_id) = (unsafe { current.CurrentAutomationId() }) else {
            return false;
        };
        let Ok(name) = (unsafe { current.CurrentName() }) else {
            return false;
        };
        let Ok(control_type) = (unsafe { current.CurrentControlType() }) else {
            return false;
        };
        let Ok(native_window) = (unsafe { current.CurrentNativeWindowHandle() }) else {
            return false;
        };
        let Ok(process_id) = u32::try_from(process_id) else {
            return false;
        };
        nodes.push(StructuralNode {
            process_id,
            class_name: class_name.to_string(),
            automation_id: automation_id.to_string(),
            name: name.to_string(),
            control_type: control_type.0,
            native_window: native_window.0 as isize,
        });
        if nodes
            .last()
            .is_some_and(|node| node.native_window == foreground as isize)
        {
            return control_view_chain_is_browser_chrome(
                &nodes,
                foreground_process,
                foreground as isize,
            );
        }
        let Ok(parent) = (unsafe { walker.GetParentElement(&current) }) else {
            return false;
        };
        current = parent;
    }
    false
}

fn has_observed_gecko_urlbar_ancestry(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    foreground_process: u32,
    foreground: HWND,
) -> bool {
    let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
        return false;
    };
    let mut current = focused.clone();
    let mut nodes = Vec::with_capacity(5);
    for _ in 0..5 {
        let Ok(process_id) = (unsafe { current.CurrentProcessId() }) else {
            return false;
        };
        let Ok(class_name) = (unsafe { current.CurrentClassName() }) else {
            return false;
        };
        let Ok(automation_id) = (unsafe { current.CurrentAutomationId() }) else {
            return false;
        };
        let Ok(name) = (unsafe { current.CurrentName() }) else {
            return false;
        };
        let Ok(control_type) = (unsafe { current.CurrentControlType() }) else {
            return false;
        };
        let Ok(native_window) = (unsafe { current.CurrentNativeWindowHandle() }) else {
            return false;
        };
        let Ok(process_id) = u32::try_from(process_id) else {
            return false;
        };
        nodes.push(StructuralNode {
            process_id,
            class_name: class_name.to_string(),
            automation_id: automation_id.to_string(),
            name: name.to_string(),
            control_type: control_type.0,
            native_window: native_window.0 as isize,
        });
        if nodes
            .last()
            .is_some_and(|node| node.native_window == foreground as isize)
        {
            break;
        }
        let Ok(parent) = (unsafe { walker.GetParentElement(&current) }) else {
            return false;
        };
        current = parent;
    }
    let matched = observed_gecko_urlbar_chain(&nodes, foreground_process, foreground as isize);
    if !matched {
        let chain = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                format!(
                    "[{index}] class='{}' automation_id='{}' name='{}' control_type={} pid={} hwnd={:#x}",
                    node.class_name,
                    node.automation_id,
                    node.name,
                    node.control_type,
                    node.process_id,
                    node.native_window
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        admit_log(&format!(
            "admit=gecko_chain_mismatch foreground_hwnd={:#x} chain: {chain}",
            foreground as usize
        ));
    }
    matched
}

fn has_observed_brave_omnibox_ancestry(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    foreground_process: u32,
    foreground: HWND,
) -> bool {
    let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
        return false;
    };
    let mut current = focused.clone();
    let mut nodes = Vec::with_capacity(9);
    for _ in 0..9 {
        let Ok(process_id) = (unsafe { current.CurrentProcessId() }) else {
            return false;
        };
        let Ok(class_name) = (unsafe { current.CurrentClassName() }) else {
            return false;
        };
        let Ok(automation_id) = (unsafe { current.CurrentAutomationId() }) else {
            return false;
        };
        let Ok(name) = (unsafe { current.CurrentName() }) else {
            return false;
        };
        let Ok(control_type) = (unsafe { current.CurrentControlType() }) else {
            return false;
        };
        let Ok(native_window) = (unsafe { current.CurrentNativeWindowHandle() }) else {
            return false;
        };
        let Ok(process_id) = u32::try_from(process_id) else {
            return false;
        };
        nodes.push(StructuralNode {
            process_id,
            class_name: class_name.to_string(),
            automation_id: automation_id.to_string(),
            name: name.to_string(),
            control_type: control_type.0,
            native_window: native_window.0 as isize,
        });
        if nodes
            .last()
            .is_some_and(|node| node.native_window == foreground as isize)
        {
            break;
        }
        let Ok(parent) = (unsafe { walker.GetParentElement(&current) }) else {
            return false;
        };
        current = parent;
    }
    let matched = observed_brave_omnibox_chain(&nodes, foreground_process, foreground as isize);
    if !matched {
        // Brave is the deepest nine-node pin and the most likely to break on
        // a point release; without this dump every such break was invisible.
        let chain = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                format!(
                    "[{index}] class='{}' automation_id='{}' name='{}' control_type={} pid={} hwnd={:#x}",
                    node.class_name,
                    node.automation_id,
                    node.name,
                    node.control_type,
                    node.process_id,
                    node.native_window
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        admit_log(&format!(
            "admit=brave_chain_mismatch foreground_hwnd={:#x} chain: {chain}",
            foreground as usize
        ));
    }
    matched
}

fn observed_brave_omnibox_chain(
    nodes: &[StructuralNode],
    foreground_process: u32,
    foreground: isize,
) -> bool {
    let [
        first,
        second,
        third,
        fourth,
        fifth,
        sixth,
        seventh,
        eighth,
        ninth,
    ] = nodes
    else {
        return false;
    };
    control_view_chain_is_browser_chrome(nodes, foreground_process, foreground)
        && equals(&first.class_name, "BraveOmniboxViewViews")
        && equals(&first.automation_id, "view_1012")
        && equals(&first.name, "Address and search bar")
        && equals(&second.class_name, "BraveLocationBarView")
        && equals(&third.class_name, "BraveToolbarView")
        && equals(&third.automation_id, "view_1000")
        && equals(&fourth.class_name, "TopContainerView")
        && equals(&fifth.class_name, "BraveBrowserView")
        && equals(&sixth.class_name, "BraveBrowserFrameViewWin")
        && equals(&seventh.class_name, "NonClientView")
        && equals(&eighth.class_name, "BraveBrowserRootView")
        && equals(&ninth.class_name, "Chrome_WidgetWin_1")
        && ninth.native_window == foreground
}

fn observed_gecko_urlbar_chain(
    nodes: &[StructuralNode],
    foreground_process: u32,
    foreground: isize,
) -> bool {
    let [first, second, third, fourth, fifth] = nodes else {
        return false;
    };
    control_view_chain_is_browser_chrome(nodes, foreground_process, foreground)
        && equals(&first.class_name, "urlbar-input textbox-input")
        && equals(&first.automation_id, "urlbar-input")
        // The accessible name is Firefox-templated on the user's default
        // search engine and localized ("Search with Google or enter
        // address"), so it can never be an admission criterion. Structure
        // and the App Paths provenance check carry the identity claim.
        && !first.name.is_empty()
        && first.control_type == UIA_ComboBoxControlTypeId.0
        && equals(&second.class_name, "urlbar-input-container")
        && equals(&third.class_name, "urlbar")
        && equals(&third.automation_id, "urlbar")
        && equals(&fourth.class_name, "browser-toolbar chromeclass-location")
        && equals(&fourth.automation_id, "nav-bar")
        && fourth.control_type == UIA_ToolBarControlTypeId.0
        && equals(&fifth.class_name, "MozillaWindowClass")
        && fifth.native_window == foreground
}

/// Called only after the main worker has checked writable `ValuePattern` and
/// password state. It reads UI metadata, never a field value.
pub fn focused_field_eligibility(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
    foreground: HWND,
) -> FocusEligibility {
    let mut foreground_process = 0u32;
    unsafe { GetWindowThreadProcessId(foreground, &raw mut foreground_process) };
    let Ok(focused_process) = (unsafe { focused.CurrentProcessId() }) else {
        admit_log("admit=rejected reason=focused_process_unreadable");
        return FocusEligibility::Rejected;
    };
    let Ok(focused_process) = u32::try_from(focused_process) else {
        admit_log("admit=rejected reason=focused_process_unrepresentable");
        return FocusEligibility::Rejected;
    };
    if foreground_process == 0 || focused_process != foreground_process {
        admit_log(&format!(
            "admit=rejected reason=process_mismatch foreground_pid={foreground_process} focused_pid={focused_process}"
        ));
        return FocusEligibility::Rejected;
    }
    if !has_safe_control_view_ancestry(automation, focused, foreground_process, foreground) {
        admit_log(&format!(
            "admit=rejected reason=ancestry foreground_pid={foreground_process} foreground_hwnd={:#x}",
            foreground as usize
        ));
        return FocusEligibility::Rejected;
    }
    match registered_browser_family(foreground_process) {
        Some(BrowserFamily::Chromium) if is_observed_chromium_omnibox(focused) => {
            admit_log("admit=eligible family=chromium profile=omnibox");
            FocusEligibility::Eligible {
                family: BrowserFamily::Chromium,
                profile: OBSERVED_CHROMIUM_PROFILE,
            }
        }
        Some(BrowserFamily::Chromium)
            if has_observed_brave_omnibox_ancestry(
                automation,
                focused,
                foreground_process,
                foreground,
            ) =>
        {
            admit_log("admit=eligible family=brave profile=omnibox");
            FocusEligibility::Eligible {
                family: BrowserFamily::Chromium,
                profile: OBSERVED_BRAVE_PROFILE,
            }
        }
        Some(BrowserFamily::Gecko)
            if has_observed_gecko_urlbar_ancestry(
                automation,
                focused,
                foreground_process,
                foreground,
            ) =>
        {
            admit_log("admit=eligible family=gecko profile=urlbar");
            FocusEligibility::Eligible {
                family: BrowserFamily::Gecko,
                profile: OBSERVED_GECKO_PROFILE,
            }
        }
        Some(BrowserFamily::Gecko) => {
            admit_log(&format!(
                "admit=unverified_gecko foreground_pid={foreground_process} foreground_hwnd={:#x}",
                foreground as usize
            ));
            FocusEligibility::UnverifiedGecko
        }
        _ => {
            admit_log("admit=rejected reason=not_a_registered_browser");
            FocusEligibility::Rejected
        }
    }
}

/// Admission diagnostics share the broker's `FSW_DIAGNOSTIC_LOG` convention.
/// Kept local so the pure admission module stays independent of the tray.
fn admit_log(message: &str) {
    let Ok(path) = std::env::var("FSW_DIAGNOSTIC_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{message}");
    }
}

/// The pure transaction policy is deliberately fail-closed. Selection is not
/// simulated or inspected: `ValuePattern` replaces the complete approved
/// omnibox value atomically, so no selection shortcut can race this policy.
pub const fn transaction_is_allowed(modifier_held: bool, ime_is_open: bool) -> bool {
    !modifier_held && !ime_is_open
}

/// One `ValuePattern` write and one marked Enter are used; selection shortcuts
/// are never synthesized. Modifier and active-IME states stay native.
///
/// The IME probe deliberately avoids `ImmGetContext(foreground)`: IME contexts
/// are thread-affine, and the frame window belongs to the browser's process,
/// so the call always returned NULL there — reading as "closed" for every
/// third-party browser and letting a write land on an open composition. The
/// default IME window for the foreground thread is the cross-process proxy:
/// it is visible exactly while the IME is composing on that window.
pub fn plain_browser_enter_is_safe(foreground: HWND) -> bool {
    unsafe {
        let modifier_held = [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
            .iter()
            .any(|key| (GetAsyncKeyState(i32::from(*key)) & i16::MIN) != 0);
        let ime_window = ImmGetDefaultIMEWnd(foreground);
        let ime_open = !ime_window.is_null() && IsWindowVisible(ime_window) != 0;
        transaction_is_allowed(modifier_held, ime_open)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BrowserFamily, FocusEligibility, OBSERVED_BRAVE_PROFILE, OBSERVED_CHROMIUM_PROFILE,
        OBSERVED_GECKO_PROFILE, StructuralNode, control_view_chain_is_browser_chrome,
        family_for_executable, is_candidate_window_class, observed_brave_omnibox_chain,
        observed_gecko_urlbar_chain, registered_path_matches, transaction_is_allowed,
    };
    use windows::Win32::UI::Accessibility::{
        UIA_ComboBoxControlTypeId, UIA_DocumentControlTypeId, UIA_ToolBarControlTypeId,
    };

    #[test]
    fn candidates_exclude_web_content_documents_and_generic_edits() {
        assert!(is_candidate_window_class("Chrome_WidgetWin_1"));
        assert!(is_candidate_window_class("MozillaWindowClass"));
        assert!(!is_candidate_window_class("Chrome_RenderWidgetHostHWND"));
        assert!(!is_candidate_window_class("Document"));
        assert!(!is_candidate_window_class("Edit"));
    }

    #[test]
    fn family_names_are_candidates_not_provenance() {
        assert_eq!(
            family_for_executable("msedge.exe"),
            Some(BrowserFamily::Chromium)
        );
        assert_eq!(
            family_for_executable("LibreWolf.exe"),
            Some(BrowserFamily::Gecko)
        );
        assert_eq!(family_for_executable("browser.exe"), None);
    }

    #[test]
    fn a_registered_full_path_is_required() {
        assert!(registered_path_matches(
            r"C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
            r"C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe"
        ));
        assert!(!registered_path_matches(
            r"C:\\Temp\\msedge.exe",
            r"C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe"
        ));
    }

    #[test]
    fn gecko_remains_explicitly_unverified() {
        assert_ne!(
            FocusEligibility::UnverifiedGecko,
            FocusEligibility::Eligible {
                family: BrowserFamily::Chromium,
                profile: OBSERVED_CHROMIUM_PROFILE
            }
        );
    }

    #[test]
    fn firefox_155_profile_requires_the_observed_chrome_chain() {
        let nodes = [
            StructuralNode {
                process_id: 44,
                class_name: "urlbar-input textbox-input".to_owned(),
                automation_id: "urlbar-input".to_owned(),
                name: "Search with Google or enter address".to_owned(),
                control_type: UIA_ComboBoxControlTypeId.0,
                native_window: 0,
            },
            StructuralNode {
                process_id: 44,
                class_name: "urlbar-input-container".to_owned(),
                automation_id: String::new(),
                name: String::new(),
                control_type: 50026,
                native_window: 0,
            },
            StructuralNode {
                process_id: 44,
                class_name: "urlbar".to_owned(),
                automation_id: "urlbar".to_owned(),
                name: String::new(),
                control_type: 50026,
                native_window: 0,
            },
            StructuralNode {
                process_id: 44,
                class_name: "browser-toolbar chromeclass-location".to_owned(),
                automation_id: "nav-bar".to_owned(),
                name: String::new(),
                control_type: UIA_ToolBarControlTypeId.0,
                native_window: 0,
            },
            StructuralNode {
                process_id: 44,
                class_name: "MozillaWindowClass".to_owned(),
                automation_id: String::new(),
                name: String::new(),
                control_type: 50032,
                native_window: 99,
            },
        ];
        assert!(observed_gecko_urlbar_chain(&nodes, 44, 99));
        assert_ne!(OBSERVED_GECKO_PROFILE, OBSERVED_CHROMIUM_PROFILE);
    }

    #[test]
    fn firefox_decoy_with_a_document_or_wrong_ancestor_is_rejected() {
        let nodes = [
            StructuralNode {
                process_id: 44,
                class_name: "urlbar-input textbox-input".to_owned(),
                automation_id: "urlbar-input".to_owned(),
                name: "Search with Google or enter address".to_owned(),
                control_type: UIA_DocumentControlTypeId.0,
                native_window: 0,
            },
            StructuralNode {
                process_id: 44,
                class_name: "MozillaWindowClass".to_owned(),
                automation_id: String::new(),
                name: String::new(),
                control_type: 50032,
                native_window: 99,
            },
        ];
        assert!(!observed_gecko_urlbar_chain(&nodes, 44, 99));
    }

    #[test]
    fn brave_profile_requires_the_observed_nine_node_chrome_chain() {
        let classes = [
            "BraveOmniboxViewViews",
            "BraveLocationBarView",
            "BraveToolbarView",
            "TopContainerView",
            "BraveBrowserView",
            "BraveBrowserFrameViewWin",
            "NonClientView",
            "BraveBrowserRootView",
            "Chrome_WidgetWin_1",
        ];
        let nodes: Vec<StructuralNode> = classes
            .iter()
            .enumerate()
            .map(|(index, class_name)| StructuralNode {
                process_id: 44,
                class_name: (*class_name).to_owned(),
                automation_id: match index {
                    0 => "view_1012".to_owned(),
                    2 => "view_1000".to_owned(),
                    _ => String::new(),
                },
                name: if index == 0 {
                    "Address and search bar".to_owned()
                } else {
                    String::new()
                },
                control_type: 50004,
                native_window: if index == 8 { 99 } else { 0 },
            })
            .collect();
        assert!(observed_brave_omnibox_chain(&nodes, 44, 99));
        assert_ne!(OBSERVED_BRAVE_PROFILE, OBSERVED_CHROMIUM_PROFILE);
    }

    #[test]
    fn browser_transaction_never_dispatches_modified_or_ime_enter() {
        assert!(transaction_is_allowed(false, false));
        assert!(!transaction_is_allowed(true, false));
        assert!(!transaction_is_allowed(false, true));
        assert!(!transaction_is_allowed(true, true));
    }

    #[test]
    fn document_decoy_with_matching_process_and_name_cannot_prove_browser_chrome() {
        let nodes = [
            StructuralNode {
                process_id: 44,
                class_name: "Edit".to_owned(),
                automation_id: "urlbar-input".to_owned(),
                name: "Address and search bar".to_owned(),
                control_type: UIA_DocumentControlTypeId.0,
                native_window: 0,
            },
            StructuralNode {
                process_id: 44,
                class_name: "Chrome_WidgetWin_1".to_owned(),
                automation_id: String::new(),
                name: String::new(),
                control_type: 50033,
                native_window: 99,
            },
        ];
        assert!(!control_view_chain_is_browser_chrome(&nodes, 44, 99));
    }

    #[test]
    fn foreign_process_or_missing_foreground_window_fails_closed() {
        let foreign = [StructuralNode {
            process_id: 45,
            class_name: "OmniboxViewViews".to_owned(),
            automation_id: "urlbar-input".to_owned(),
            name: "Address and search bar".to_owned(),
            control_type: 50004,
            native_window: 99,
        }];
        assert!(!control_view_chain_is_browser_chrome(&foreign, 44, 99));

        let no_window = [StructuralNode {
            process_id: 44,
            class_name: "OmniboxViewViews".to_owned(),
            automation_id: "urlbar-input".to_owned(),
            name: "Address and search bar".to_owned(),
            control_type: 50004,
            native_window: 0,
        }];
        assert!(!control_view_chain_is_browser_chrome(&no_window, 44, 99));
    }
}
