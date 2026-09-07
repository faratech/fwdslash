//! Filesystem navigation shared by the broker and CLI. No action invokes a
//! shell verb or a file association: files are selected in their parent.

use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationAction {
    OpenDirectory(String),
    SelectFile(String),
}

impl NavigationAction {
    /// Classifies an absolute filesystem path according to its resolved target.
    ///
    /// Directory links and junctions stay navigable directories. Links resolving
    /// to files are selected in their parent, so no file association is invoked.
    pub fn for_path(path: &str) -> io::Result<Self> {
        if path.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "An absolute filesystem path is required.",
            ));
        }
        // These provider roots enumerate WSL distributions but do not have
        // ordinary filesystem metadata, and they are never executable targets.
        // The check precedes the absolute-path gate: a server-only UNC like
        // `\\wsl.localhost` carries no share component, so Win32 path parsing
        // does not consider it absolute and would reject it here first.
        if path.eq_ignore_ascii_case(r"\\wsl.localhost") || path.eq_ignore_ascii_case(r"\\wsl$") {
            return Ok(Self::OpenDirectory(path.to_owned()));
        }
        if !std::path::Path::new(path).is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "An absolute filesystem path is required.",
            ));
        }
        // `symlink_metadata` would identify every directory symlink as a file,
        // preventing normal Windows traversal through directory links/junctions.
        // This operation follows the link solely to choose a non-executing shell
        // navigation action; `navigate_if` still uses only shell namespace APIs.
        let metadata = std::fs::metadata(path)?;
        Ok(if metadata.is_dir() {
            Self::OpenDirectory(path.to_owned())
        } else {
            Self::SelectFile(path.to_owned())
        })
    }

    /// Navigates only while the caller's authorization remains current.
    /// The predicate is called after potentially slow PIDL resolution and
    /// immediately before asking Explorer to open the directory/select a file.
    #[cfg(windows)]
    pub fn navigate_if(&self, authorized: impl Fn() -> bool) -> io::Result<()> {
        native::navigate(self, authorized)
    }

    #[cfg(not(windows))]
    pub fn navigate_if(&self, _authorized: impl Fn() -> bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Explorer navigation requires Windows.",
        ))
    }
}

#[cfg(windows)]
mod native {
    use super::{NavigationAction, io};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows_sys::Win32::UI::Shell::{
        Common::ITEMIDLIST, ILFindLastID, SHOpenFolderAndSelectItems, SHParseDisplayName,
    };

    struct Apartment(bool);
    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    struct Pidl(*mut ITEMIDLIST);
    impl Drop for Pidl {
        fn drop(&mut self) {
            unsafe { CoTaskMemFree(self.0.cast()) };
        }
    }

    fn parse(path: &str) -> io::Result<Pidl> {
        // Mirror `for_path`: a server-only provider root is not "absolute" to
        // Win32 path parsing, but the shell namespace parses it fine.
        let is_provider_root =
            path.eq_ignore_ascii_case(r"\\wsl.localhost") || path.eq_ignore_ascii_case(r"\\wsl$");
        if path.contains('\0') || (!is_provider_root && !std::path::Path::new(path).is_absolute()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "An absolute filesystem path is required.",
            ));
        }
        let wide: Vec<u16> = std::ffi::OsStr::new(path)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut pidl = Pidl(std::ptr::null_mut());
        let result = unsafe {
            SHParseDisplayName(
                wide.as_ptr(),
                std::ptr::null_mut(),
                &raw mut pidl.0,
                0,
                std::ptr::null_mut(),
            )
        };
        if result < 0 || pidl.0.is_null() {
            Err(io::Error::other(format!(
                "Windows could not resolve the navigation target ({result:#010x})."
            )))
        } else {
            Ok(pidl)
        }
    }

    pub(super) fn navigate(
        action: &NavigationAction,
        authorized: impl Fn() -> bool,
    ) -> io::Result<()> {
        let result =
            unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED.cast_unsigned()) };
        // An already initialized MTA may use the shell API too; do not balance
        // another owner's COM initialization in that case.
        if result < 0 && result.cast_unsigned() != 0x8001_0106 {
            return Err(io::Error::other("Windows could not initialize navigation."));
        }
        let _apartment = Apartment(result >= 0);
        let (folder, item) = match action {
            NavigationAction::OpenDirectory(path) => (parse(path)?, None),
            NavigationAction::SelectFile(path) => {
                let parent = std::path::Path::new(path)
                    .parent()
                    .and_then(|path| path.to_str())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "The file has no parent directory.",
                        )
                    })?;
                (parse(parent)?, Some(parse(path)?))
            }
        };
        let child = item.as_ref().map_or(std::ptr::null(), |item| unsafe {
            ILFindLastID(item.0).cast_const()
        });
        if item.is_some() && child.is_null() {
            return Err(io::Error::other("Windows could not select the file."));
        }
        if !authorized() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Navigation was cancelled because the foreground changed.",
            ));
        }
        let result = unsafe {
            SHOpenFolderAndSelectItems(
                folder.0,
                u32::from(item.is_some()),
                if item.is_some() {
                    &raw const child
                } else {
                    std::ptr::null()
                },
                0,
            )
        };
        if result < 0 {
            Err(io::Error::other(format!(
                "Windows navigation failed ({result:#010x})."
            )))
        } else {
            Ok(())
        }
    }
}
