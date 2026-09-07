//! Native verification for a downloaded GitHub MSIX bundle.
//!
//! `PackageManager` verifies a package signature, but it does not bind an
//! arbitrary valid package to this product. Keep a deny-write/delete handle
//! open from this policy check through deployment so the package it installs is
//! the same byte sequence inspected here.

use fsw_core::update::{UpdateVerificationError, expected_bundle_version};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use windows::Win32::Storage::Packaging::Appx::{
    APPX_PACKAGE_ARCHITECTURE_ARM64, APPX_PACKAGE_ARCHITECTURE_X64, AppxBundleFactory,
    IAppxBundleFactory, IAppxManifestPackageId,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, STGM_READ, STGM_SHARE_DENY_WRITE,
};
use windows::Win32::UI::Shell::{SHCreateStreamOnFileEx, UrlCreateFromPathW};
use windows::core::{PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    WinVerifyTrust,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, OPEN_EXISTING,
};

/// A bundle locked against replacement until the caller finishes deployment.
pub struct VerifiedBundle {
    // Denying delete sharing on both controlled directories prevents a rename
    // of an ancestor from making the verified path resolve to another file.
    _directories: Vec<File>,
    _file: File,
    _digest_file: File,
    path: PathBuf,
}

impl VerifiedBundle {
    pub fn uri_text(&self) -> Result<String, UpdateVerificationError> {
        let path = wide(&self.path);
        let mut url = vec![0_u16; 32_768];
        let mut length =
            u32::try_from(url.len()).map_err(|_| UpdateVerificationError::IoFailure)?;
        unsafe {
            UrlCreateFromPathW(
                PCWSTR(path.as_ptr()),
                PWSTR(url.as_mut_ptr()),
                &raw mut length,
                0,
            )
        }
        .map_err(|_| UpdateVerificationError::IoFailure)?;
        let length = usize::try_from(length).map_err(|_| UpdateVerificationError::IoFailure)?;
        let url = url
            .get(..length)
            .ok_or(UpdateVerificationError::IoFailure)?;
        String::from_utf16(url).map_err(|_| UpdateVerificationError::IoFailure)
    }
}

pub fn verify_staged_bundle(bundle: &Path) -> Result<VerifiedBundle, UpdateVerificationError> {
    let tag =
        fsw_core::update::cached_update_tag().ok_or(UpdateVerificationError::VersionMismatch)?;
    let expected =
        fsw_core::update::pending_bundle_path().ok_or(UpdateVerificationError::IoFailure)?;
    if bundle != expected {
        return Err(UpdateVerificationError::AssetNameMismatch);
    }
    let directories = lock_private_staging_path(&expected)?;
    let file = lock_read_only(&expected)?;
    let digest_path = expected.with_extension("msixbundle.sha256");
    let digest_file = lock_read_only(&digest_path)?;
    verify_digest(&file, &digest_file)?;
    verify_authenticode(&file, &expected)?;
    verify_bundle_manifest(&expected, &tag)?;
    // Both handles remain open until the caller completes package deployment.
    Ok(VerifiedBundle {
        _directories: directories,
        _file: file,
        _digest_file: digest_file,
        path: expected,
    })
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn lock_private_staging_path(path: &Path) -> Result<Vec<File>, UpdateVerificationError> {
    let update =
        fsw_core::update::update_directory_path().ok_or(UpdateVerificationError::IoFailure)?;
    if path.parent() != Some(update.as_path()) {
        return Err(UpdateVerificationError::AssetNameMismatch);
    }
    if !update.is_absolute() {
        return Err(UpdateVerificationError::IoFailure);
    }

    // Lock root first, then every descendant. Once an ancestor is open without
    // delete sharing, its pathname cannot be redirected while the next
    // component is opened. Inspect attributes through the resulting handle so
    // a junction swapped in during open is rejected rather than followed.
    let mut ancestors: Vec<_> = update.ancestors().collect();
    ancestors.reverse();
    ancestors.into_iter().map(lock_directory).collect()
}

fn lock_directory(path: &Path) -> Result<File, UpdateVerificationError> {
    let wide = wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(UpdateVerificationError::IoFailure);
    }
    let directory = unsafe { File::from_raw_handle(handle.cast()) };
    verify_not_reparse_point(
        directory
            .metadata()
            .map_err(|_| UpdateVerificationError::IoFailure)?
            .file_attributes(),
    )?;
    Ok(directory)
}

fn lock_read_only(path: &Path) -> Result<File, UpdateVerificationError> {
    let wide = wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(UpdateVerificationError::IoFailure);
    }
    let file = unsafe { File::from_raw_handle(handle.cast()) };
    verify_not_reparse_point(
        file.metadata()
            .map_err(|_| UpdateVerificationError::IoFailure)?
            .file_attributes(),
    )?;
    Ok(file)
}

fn verify_not_reparse_point(attributes: u32) -> Result<(), UpdateVerificationError> {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Err(UpdateVerificationError::IoFailure)
    } else {
        Ok(())
    }
}

fn verify_digest(bundle: &File, sidecar: &File) -> Result<(), UpdateVerificationError> {
    let mut expected = String::new();
    sidecar
        .try_clone()
        .map_err(|_| UpdateVerificationError::IoFailure)?
        .read_to_string(&mut expected)
        .map_err(|_| UpdateVerificationError::IoFailure)?;
    let expected = expected.trim();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UpdateVerificationError::DigestMismatch);
    }
    let mut hash = Sha256::new();
    let mut reader = bundle
        .try_clone()
        .map_err(|_| UpdateVerificationError::IoFailure)?;
    let mut bytes = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = reader
            .read(&mut bytes)
            .map_err(|_| UpdateVerificationError::IoFailure)?;
        if count == 0 {
            break;
        }
        hash.update(
            bytes
                .get(..count)
                .ok_or(UpdateVerificationError::IoFailure)?,
        );
    }
    // digest 0.11: the output array no longer implements LowerHex, so hex is
    // spelled out byte by byte.
    let actual = hash
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut actual, byte| {
            use std::fmt::Write as _;
            let _ = write!(actual, "{byte:02x}");
            actual
        });
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(UpdateVerificationError::DigestMismatch)
    }
}

fn verify_authenticode(file: &File, path: &Path) -> Result<(), UpdateVerificationError> {
    let path = wide(path);
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: u32::try_from(std::mem::size_of::<WINTRUST_FILE_INFO>())
            .map_err(|_| UpdateVerificationError::IoFailure)?,
        pcwszFilePath: path.as_ptr(),
        hFile: file.as_raw_handle(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data: WINTRUST_DATA = unsafe { std::mem::zeroed() };
    data.cbStruct = u32::try_from(std::mem::size_of::<WINTRUST_DATA>())
        .map_err(|_| UpdateVerificationError::IoFailure)?;
    data.dwUIChoice = WTD_UI_NONE;
    data.fdwRevocationChecks = WTD_REVOKE_NONE;
    data.dwUnionChoice = WTD_CHOICE_FILE;
    data.Anonymous = WINTRUST_DATA_0 {
        pFile: &raw mut info,
    };
    data.dwStateAction = WTD_STATEACTION_VERIFY;
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let result = unsafe {
        WinVerifyTrust(
            std::ptr::null_mut(),
            &raw mut action,
            (&raw mut data).cast(),
        )
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    let _ = unsafe {
        WinVerifyTrust(
            std::ptr::null_mut(),
            &raw mut action,
            (&raw mut data).cast(),
        )
    };
    verify_authenticode_status(result)
}

fn verify_authenticode_status(status: i32) -> Result<(), UpdateVerificationError> {
    if status == 0 {
        Ok(())
    } else {
        Err(UpdateVerificationError::SignatureFailure)
    }
}

fn package_string(value: windows::core::PWSTR) -> Result<String, UpdateVerificationError> {
    let text =
        unsafe { value.to_string() }.map_err(|_| UpdateVerificationError::IdentityMismatch)?;
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    Ok(text)
}

#[derive(Clone, Copy)]
struct PackageIdentity<'a> {
    name: &'a str,
    publisher: &'a str,
    version: &'a str,
    architecture: u32,
}

fn verify_package_identity(
    identity: PackageIdentity<'_>,
    expected_version: &str,
) -> Result<u32, UpdateVerificationError> {
    if identity.name != fsw_core::STORE_IDENTITY_NAME {
        return Err(UpdateVerificationError::IdentityMismatch);
    }
    if identity.publisher != fsw_core::GITHUB_PUBLISHER {
        return Err(UpdateVerificationError::PublisherMismatch);
    }
    if identity.version != expected_version {
        return Err(UpdateVerificationError::VersionMismatch);
    }
    Ok(identity.architecture)
}

fn verify_package_id(
    id: &IAppxManifestPackageId,
    version: &str,
) -> Result<u32, UpdateVerificationError> {
    let name = package_string(
        unsafe { id.GetName() }.map_err(|_| UpdateVerificationError::IdentityMismatch)?,
    )?;
    let publisher = package_string(
        unsafe { id.GetPublisher() }.map_err(|_| UpdateVerificationError::PublisherMismatch)?,
    )?;
    let found = unsafe { id.GetVersion() }.map_err(|_| UpdateVerificationError::VersionMismatch)?;
    let found = format!(
        "{}.{}.{}.{}",
        (found >> 48) & 0xffff,
        (found >> 32) & 0xffff,
        (found >> 16) & 0xffff,
        found & 0xffff,
    );
    let architecture = unsafe { id.GetArchitecture() }
        .map_err(|_| UpdateVerificationError::ArchitectureMismatch)?
        .0
        .cast_unsigned();
    verify_package_identity(
        PackageIdentity {
            name: &name,
            publisher: &publisher,
            version: &found,
            architecture,
        },
        version,
    )
}

fn verify_bundle_architectures<I>(architectures: I) -> Result<(), UpdateVerificationError>
where
    I: IntoIterator<Item = u32>,
{
    let mut seen_x64 = false;
    let mut seen_arm64 = false;
    for architecture in architectures {
        match architecture {
            value if value == APPX_PACKAGE_ARCHITECTURE_X64.0 as u32 => seen_x64 = true,
            value if value == APPX_PACKAGE_ARCHITECTURE_ARM64.0 as u32 => seen_arm64 = true,
            _ => return Err(UpdateVerificationError::ArchitectureMismatch),
        }
    }
    if seen_x64 && seen_arm64 {
        Ok(())
    } else {
        Err(UpdateVerificationError::ArchitectureMismatch)
    }
}

fn verify_bundle_manifest(path: &Path, tag: &str) -> Result<(), UpdateVerificationError> {
    let version = expected_bundle_version(tag).ok_or(UpdateVerificationError::VersionMismatch)?;
    let wide = wide(path);
    let stream = unsafe {
        SHCreateStreamOnFileEx(
            PCWSTR(wide.as_ptr()),
            (STGM_READ | STGM_SHARE_DENY_WRITE).0,
            FILE_ATTRIBUTE_NORMAL,
            false,
            None,
        )
    }
    .map_err(|_| UpdateVerificationError::IoFailure)?;
    let factory: IAppxBundleFactory =
        unsafe { CoCreateInstance(&AppxBundleFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| UpdateVerificationError::IoFailure)?;
    let reader = unsafe { factory.CreateBundleReader(&stream) }
        .map_err(|_| UpdateVerificationError::IdentityMismatch)?;
    let manifest =
        unsafe { reader.GetManifest() }.map_err(|_| UpdateVerificationError::IdentityMismatch)?;
    let _ = verify_package_id(
        &unsafe { manifest.GetPackageId() }
            .map_err(|_| UpdateVerificationError::IdentityMismatch)?,
        &version,
    )?;
    let packages = unsafe { manifest.GetPackageInfoItems() }
        .map_err(|_| UpdateVerificationError::IdentityMismatch)?;
    let mut architectures = Vec::new();
    while unsafe { packages.GetHasCurrent() }
        .map_err(|_| UpdateVerificationError::IdentityMismatch)?
        .as_bool()
    {
        let package = unsafe { packages.GetCurrent() }
            .map_err(|_| UpdateVerificationError::IdentityMismatch)?;
        architectures.push(verify_package_id(
            &unsafe { package.GetPackageId() }
                .map_err(|_| UpdateVerificationError::IdentityMismatch)?,
            &version,
        )?);
        let _ = unsafe { packages.MoveNext() }
            .map_err(|_| UpdateVerificationError::IdentityMismatch)?;
    }
    verify_bundle_architectures(architectures)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};

    static NEXT_TEST_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn new() -> Self {
            let number = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fsw-verification-test-{}-{number}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create isolated verifier test directory");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn identity<'a>(
        name: &'a str,
        publisher: &'a str,
        version: &'a str,
        architecture: u32,
    ) -> PackageIdentity<'a> {
        PackageIdentity {
            name,
            publisher,
            version,
            architecture,
        }
    }

    fn valid_identity() -> PackageIdentity<'static> {
        identity(
            fsw_core::STORE_IDENTITY_NAME,
            fsw_core::GITHUB_PUBLISHER,
            "0.0.7.0",
            APPX_PACKAGE_ARCHITECTURE_X64.0 as u32,
        )
    }

    #[test]
    fn an_authenticode_failure_is_never_reclassified() {
        assert_eq!(
            verify_authenticode_status(-1),
            Err(UpdateVerificationError::SignatureFailure)
        );
        assert_eq!(verify_authenticode_status(0), Ok(()));
    }

    #[test]
    fn package_identity_failures_are_precise() {
        let expected_version = "0.0.7.0";
        let mut invalid = valid_identity();
        invalid.name = "unrelated.product";
        assert_eq!(
            verify_package_identity(invalid, expected_version),
            Err(UpdateVerificationError::IdentityMismatch)
        );

        let mut invalid = valid_identity();
        invalid.publisher = "CN=Unrelated Publisher";
        assert_eq!(
            verify_package_identity(invalid, expected_version),
            Err(UpdateVerificationError::PublisherMismatch)
        );

        let mut invalid = valid_identity();
        invalid.version = "0.0.8.0";
        assert_eq!(
            verify_package_identity(invalid, expected_version),
            Err(UpdateVerificationError::VersionMismatch)
        );
    }

    #[test]
    fn a_bundle_requires_exactly_supported_architectures() {
        assert_eq!(
            verify_bundle_architectures([APPX_PACKAGE_ARCHITECTURE_X64.0 as u32]),
            Err(UpdateVerificationError::ArchitectureMismatch)
        );
        assert_eq!(
            verify_bundle_architectures([
                APPX_PACKAGE_ARCHITECTURE_X64.0 as u32,
                APPX_PACKAGE_ARCHITECTURE_ARM64.0 as u32,
                999,
            ]),
            Err(UpdateVerificationError::ArchitectureMismatch)
        );
        assert_eq!(
            verify_bundle_architectures([
                APPX_PACKAGE_ARCHITECTURE_X64.0 as u32,
                APPX_PACKAGE_ARCHITECTURE_ARM64.0 as u32,
            ]),
            Ok(())
        );
    }

    #[test]
    fn reparse_point_attributes_are_rejected_for_ancestors_and_bundle_files() {
        assert_eq!(
            verify_not_reparse_point(FILE_ATTRIBUTE_REPARSE_POINT),
            Err(UpdateVerificationError::IoFailure),
            "an ancestor junction must fail closed"
        );
        assert_eq!(
            verify_not_reparse_point(FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL),
            Err(UpdateVerificationError::IoFailure),
            "a bundle-file link must fail closed"
        );
        assert_eq!(verify_not_reparse_point(FILE_ATTRIBUTE_NORMAL), Ok(()));
    }

    #[test]
    fn actual_reparse_ancestor_is_rejected_without_symlink_privilege() {
        let directory = TemporaryDirectory::new();
        let target_directory = directory.join("target-directory");
        let ancestor_link = directory.join("ancestor-link");
        fs::create_dir(&target_directory).expect("create directory target");
        let system_root = std::env::var_os("SystemRoot").expect("Windows supplies SystemRoot");
        let command = PathBuf::from(system_root).join("System32\\cmd.exe");
        let output = std::process::Command::new(command)
            .args([
                "/d",
                "/c",
                "mklink",
                "/J",
                ancestor_link.to_string_lossy().as_ref(),
                target_directory.to_string_lossy().as_ref(),
            ])
            .output()
            .expect("create a junction without symbolic-link privilege");
        assert!(output.status.success(), "create test junction");
        assert_eq!(
            lock_directory(&ancestor_link).map(|_| ()),
            Err(UpdateVerificationError::IoFailure),
            "the opened staging ancestor, not just its pathname, must be non-reparse"
        );
    }

    #[test]
    fn locked_bundle_cannot_be_replaced_until_verification_finishes() {
        let directory = TemporaryDirectory::new();
        let bundle = directory.join("fwdslash.msixbundle");
        let replacement = directory.join("replacement.msixbundle");
        fs::write(&bundle, b"verified bundle").expect("write bundle");
        fs::write(&replacement, b"replacement bundle").expect("write replacement");

        let lock = lock_read_only(&bundle).expect("lock bundle against replacement");
        let bundle_wide = wide(&bundle);
        let replacement_wide = wide(&replacement);
        let replaced = unsafe {
            MoveFileExW(
                replacement_wide.as_ptr(),
                bundle_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING,
            )
        };
        assert_eq!(replaced, 0, "locked bundle must not be replaceable");
        let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        assert!(
            error == ERROR_SHARING_VIOLATION || error == ERROR_ACCESS_DENIED,
            "replacement must fail because the verifier retained its handle, got {error}"
        );
        drop(lock);

        let replaced = unsafe {
            MoveFileExW(
                replacement_wide.as_ptr(),
                bundle_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING,
            )
        };
        assert_ne!(
            replaced, 0,
            "replacement may resume only after handle release"
        );
        assert_eq!(
            fs::read(&bundle).expect("read replacement"),
            b"replacement bundle"
        );
    }
}
