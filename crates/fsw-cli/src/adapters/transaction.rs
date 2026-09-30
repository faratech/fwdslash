//! Cross-process adapter ownership and compare-before-write profile edits.

use super::AdapterError;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_DISPOSITION_INFO, FILE_SHARE_READ, FileDispositionInfo,
    GetFileInformationByHandle, SetFileInformationByHandle,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// This is deliberately separate from the parent's adapter *sweep* mutex:
/// children own this mutex while mutating, whereas parents coordinate sweeps.
pub struct AdapterTransaction(HANDLE);

impl AdapterTransaction {
    pub fn acquire() -> Result<Self, AdapterError> {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;
        let user_root = super::local_app_data()?.to_string_lossy().to_lowercase();
        let mut suffix = String::with_capacity(64);
        for byte in Sha256::digest(user_root.as_bytes()) {
            let _ = write!(suffix, "{byte:02x}");
        }
        Self::named(
            &format!("Global\\ForwardSlashWindows.AdapterTransaction-{suffix}"),
            90_000,
        )
    }

    fn named(name: &str, timeout_ms: u32) -> Result<Self, AdapterError> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        match unsafe { WaitForSingleObject(handle, timeout_ms) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            _ => {
                unsafe { CloseHandle(handle) };
                Err(AdapterError::new(
                    "Another shell-integration transaction is busy. Try again after it finishes.",
                ))
            }
        }
    }
}

impl Drop for AdapterTransaction {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

#[derive(Clone)]
pub struct ProfileSnapshot {
    pub bytes: Option<Vec<u8>>,
    identity: Option<(u32, u64)>,
}

impl ProfileSnapshot {
    pub fn read(path: &Path) -> Result<Self, AdapterError> {
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    bytes: None,
                    identity: None,
                });
            }
            Err(error) => return Err(error.into()),
        };
        let identity = Some(file_identity(&file)?);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Self {
            bytes: Some(bytes),
            identity,
        })
    }

    /// Hold a handle that denies external writes and deletes while checking
    /// and editing. Recovery snapshots remain durable before callers enter
    /// here, so a failed write must retain them rather than guess at rollback.
    pub fn replace(&self, path: &Path, replacement: Option<&[u8]>) -> Result<Self, AdapterError> {
        self.replace_with(path, replacement, write_contents)
    }

    fn replace_with(
        &self,
        path: &Path,
        replacement: Option<&[u8]>,
        mut write: impl FnMut(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
    ) -> Result<Self, AdapterError> {
        if self.bytes.is_none() && replacement.is_none() {
            if path.try_exists()? {
                return Err(profile_conflict());
            }
            return Ok(self.clone());
        }
        if self.bytes.is_none()
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        // DELETE access lets deletion be marked on this same protected handle,
        // without a close/reopen gap in which another editor could replace it.
        options
            .read(true)
            .write(true)
            .access_mode(0xc001_0000)
            .share_mode(FILE_SHARE_READ);
        if self.bytes.is_none() {
            options.create_new(true);
        }
        let mut file = options.open(path)?;
        let identity = file_identity(&file)?;
        let mut current = Vec::new();
        file.read_to_end(&mut current)?;
        if self.identity.is_some_and(|expected| expected != identity)
            || self
                .bytes
                .as_deref()
                .is_some_and(|expected| expected != current)
        {
            return Err(profile_conflict());
        }
        if let Some(bytes) = replacement {
            if let Err(error) = write(&mut file, bytes) {
                // Outside editors are still excluded: repair a failed write
                // using the bytes read from this handle before releasing it.
                let restore = if self.bytes.is_some() {
                    write(&mut file, &current).map_err(AdapterError::from)
                } else {
                    mark_deleted(&file)
                };
                let original = AdapterError::from(error);
                return match restore {
                    Ok(()) => Err(original),
                    Err(restore) => Err(AdapterError::new(&format!(
                        "{original} The previous profile could not be restored ({restore}); recovery files were retained."
                    ))),
                };
            }
            Ok(Self {
                bytes: Some(bytes.to_vec()),
                identity: Some(identity),
            })
        } else {
            mark_deleted(&file)?;
            Ok(Self {
                bytes: None,
                identity: None,
            })
        }
    }
}

fn write_contents(file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()
}

fn mark_deleted(file: &std::fs::File) -> Result<(), AdapterError> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let size = u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFO>())
        .map_err(|_| AdapterError::new("invalid disposition structure size"))?;
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn profile_conflict() -> AdapterError {
    AdapterError::new(
        "The PowerShell profile changed during the transaction. Your edits and the recovery files were preserved; reconcile the profile and retry.",
    )
}

fn file_identity(file: &std::fs::File) -> Result<(u32, u64), AdapterError> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn transaction_mutex_excludes_other_threads_and_allows_nested_cleanup() {
        let name = format!(
            "Local\\fsw-adapter-test-{}",
            super::super::new_transaction_id()
        );
        let first = AdapterTransaction::named(&name, 100).expect("first owner");
        let nested = AdapterTransaction::named(&name, 100).expect("recursive owner");
        let other_name = name.clone();
        assert!(
            std::thread::spawn(move || AdapterTransaction::named(&other_name, 20).is_err())
                .join()
                .expect("worker")
        );
        drop(nested);
        drop(first);
        assert!(AdapterTransaction::named(&name, 100).is_ok());
    }

    #[test]
    fn profile_compare_before_write_preserves_intervening_edits_and_file_identity() {
        let directory = std::env::temp_dir().join(format!(
            "fsw-profile-test-{}",
            super::super::new_transaction_id()
        ));
        std::fs::create_dir(&directory).expect("fixture");
        let path = directory.join("profile.ps1");
        std::fs::write(&path, b"original").expect("original");
        let snapshot = ProfileSnapshot::read(&path).expect("snapshot");
        std::fs::write(&path, b"external edit").expect("external edit");
        assert!(snapshot.replace(&path, Some(b"installed")).is_err());
        assert_eq!(std::fs::read(&path).expect("preserved"), b"external edit");
        let current = ProfileSnapshot::read(&path).expect("current");
        let installed = current.replace(&path, Some(b"installed")).expect("commit");
        std::fs::write(&path, b"later edit").expect("later edit");
        assert!(installed.replace(&path, Some(b"original")).is_err());
        assert_eq!(std::fs::read(&path).expect("preserved"), b"later edit");
        std::fs::remove_dir_all(&directory).expect("fixture cleanup");
    }

    #[test]
    fn failed_partial_write_and_truncate_restore_bytes_before_releasing_the_handle() {
        for truncate_first in [false, true] {
            let directory = std::env::temp_dir().join(format!(
                "fsw-profile-failure-{}",
                super::super::new_transaction_id()
            ));
            std::fs::create_dir(&directory).expect("fixture");
            let path = directory.join("profile.ps1");
            std::fs::write(&path, b"original complete profile").expect("original");
            let snapshot = ProfileSnapshot::read(&path).expect("snapshot");
            let mut failed = false;
            let result = snapshot.replace_with(&path, Some(b"replacement"), |file, bytes| {
                if !failed {
                    failed = true;
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(b"part")?;
                    if truncate_first {
                        file.set_len(4)?;
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::StorageFull,
                        "injected write failure",
                    ));
                }
                write_contents(file, bytes)
            });
            assert!(result.is_err());
            assert_eq!(
                std::fs::read(&path).expect("restored"),
                b"original complete profile"
            );
            std::fs::remove_dir_all(directory).expect("cleanup");
        }
    }
}
