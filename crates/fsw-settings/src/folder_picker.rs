//! Native folder picker (`IFileOpenDialog` + `FOS_PICKFOLDERS`) — the modern
//! "open a file in Word"-style dialog, restricted to filesystem folders.
//!
//! `windows-sys` ships the GUIDs and the raw COM allocator but no interface
//! vtables, so the two vtables we touch are hand-declared. The slot order is
//! the SDK's own `ShObjIdl_core.h` declaration order (IFileDialog methods
//! verified slot-by-slot against it); unused slots are typed `*mut c_void`
//! and never called. Everything runs on the UI thread, which reactor has
//! already CoInitializeEx'd as STA.

use windows_sys::core::{GUID, PCWSTR, PWSTR};

const CLSID_FILE_OPEN_DIALOG: GUID = GUID {
    data1: 0xdc1c_5a9c,
    data2: 0xe88a,
    data3: 0x4dde,
    data4: [0xa5, 0xa1, 0x60, 0xf8, 0x2a, 0x20, 0xae, 0xf7],
};
const IID_IFILE_OPEN_DIALOG: GUID = GUID {
    data1: 0xd57c_7288,
    data2: 0xd4ad,
    data3: 0x4768,
    data4: [0xbe, 0x02, 0x9d, 0x96, 0x95, 0x32, 0xd9, 0x60],
};
const FOS_PICKFOLDERS: u32 = 0x20;
const FOS_FORCEFILESYSTEM: u32 = 0x40;
const SIGDN_FILESYSPATH: u32 = 0x8005_8000;
const S_OK: i32 = 0;

#[allow(non_snake_case)]
#[repr(C)]
struct IFileDialogVtbl {
    QueryInterface: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> i32,
    AddRef: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    Release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    Show: unsafe extern "system" fn(*mut core::ffi::c_void, HWND) -> i32,
    SetFileTypes: *mut core::ffi::c_void,
    SetFileTypeIndex: *mut core::ffi::c_void,
    GetFileTypeIndex: *mut core::ffi::c_void,
    Advise: *mut core::ffi::c_void,
    Unadvise: *mut core::ffi::c_void,
    SetOptions: unsafe extern "system" fn(*mut core::ffi::c_void, u32) -> i32,
    GetOptions: unsafe extern "system" fn(*mut core::ffi::c_void, *mut u32) -> i32,
    SetDefaultFolder: *mut core::ffi::c_void,
    SetFolder: *mut core::ffi::c_void,
    GetFolder: *mut core::ffi::c_void,
    GetCurrentSelection: *mut core::ffi::c_void,
    SetFileName: *mut core::ffi::c_void,
    GetFileName: *mut core::ffi::c_void,
    SetTitle: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR) -> i32,
    SetOkButtonLabel: *mut core::ffi::c_void,
    SetFileNameLabel: *mut core::ffi::c_void,
    GetResult: unsafe extern "system" fn(*mut core::ffi::c_void, *mut *mut core::ffi::c_void) -> i32,
    AddPlace: *mut core::ffi::c_void,
    SetDefaultExtension: *mut core::ffi::c_void,
    Close: *mut core::ffi::c_void,
    SetClientGuid: *mut core::ffi::c_void,
    ClearClientData: *mut core::ffi::c_void,
    SetFilter: *mut core::ffi::c_void,
}

#[allow(non_snake_case)]
#[repr(C)]
struct IShellItemVtbl {
    QueryInterface: unsafe extern "system" fn(*mut core::ffi::c_void, *const GUID, *mut *mut core::ffi::c_void) -> i32,
    AddRef: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    Release: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    BindToHandler: *mut core::ffi::c_void,
    GetParent: *mut core::ffi::c_void,
    GetDisplayName: unsafe extern "system" fn(*mut core::ffi::c_void, u32, *mut PWSTR) -> i32,
    GetAttributes: *mut core::ffi::c_void,
    Compare: *mut core::ffi::c_void,
}

type HWND = *mut core::ffi::c_void;

/// Opens the modal picker owned by `parent` (the settings window). Returns the
/// chosen filesystem folder, or `None` on cancel or any COM failure — a
/// broken picker must never fabricate a root.
pub fn pick_folder(parent: HWND) -> Option<String> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

        let mut dialog: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_FILE_OPEN_DIALOG,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IFILE_OPEN_DIALOG,
            &mut dialog,
        );
        if hr != S_OK || dialog.is_null() {
            return None;
        }
        let result = run_dialog(dialog, parent);
        // The vtable lives behind the same pointer; Release is slot 2.
        let vtbl = (*(dialog as *mut *mut IFileDialogVtbl)).cast::<IFileDialogVtbl>();
        ((*vtbl).Release)(dialog);
        result
    }
    #[cfg(not(windows))]
    {
        let _ = parent;
        None
    }
}

#[cfg(windows)]
unsafe fn run_dialog(dialog: *mut core::ffi::c_void, parent: HWND) -> Option<String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;

    let vtbl = unsafe { (*(dialog as *mut *mut IFileDialogVtbl)).as_ref() }?;
    if unsafe { (vtbl.SetOptions)(dialog, FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM) } != S_OK {
        return None;
    }
    let title = crate::to_wide("Choose the folder that / opens");
    if unsafe { (vtbl.SetTitle)(dialog, title.as_ptr()) } != S_OK {
        return None;
    }
    if unsafe { (vtbl.Show)(dialog, parent) } != S_OK {
        return None; // cancelled or failed
    }

    let mut item: *mut core::ffi::c_void = std::ptr::null_mut();
    if unsafe { (vtbl.GetResult)(dialog, &mut item) } != S_OK || item.is_null() {
        return None;
    }
    let path = unsafe { display_path(item) };
    unsafe {
        let item_vtbl = (*(item as *mut *mut IShellItemVtbl)).cast::<IShellItemVtbl>();
        ((*item_vtbl).Release)(item);
        if !path.0.is_null() {
            CoTaskMemFree(path.0.cast());
        }
    }
    path.1
}

/// `(raw PWSTR, string)` so the caller can free after conversion.
#[cfg(windows)]
unsafe fn display_path(item: *mut core::ffi::c_void) -> (PWSTR, Option<String>) {
    let Some(vtbl) = (unsafe { (*(item as *mut *mut IShellItemVtbl)).as_ref() }) else {
        return (std::ptr::null_mut(), None);
    };
    let mut wide: PWSTR = std::ptr::null_mut();
    if unsafe { (vtbl.GetDisplayName)(item, SIGDN_FILESYSPATH, &mut wide) } != S_OK
        || wide.is_null()
    {
        return (wide, None);
    }
    // Wide string → String, bounded by the NUL.
    let mut len = 0usize;
    unsafe {
        while *wide.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(wide, len);
        (wide, Some(String::from_utf16_lossy(slice)))
    }
}
