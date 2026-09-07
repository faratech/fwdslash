#![allow(clippy::expect_used)]

use fsw_path::{
    BareSlashMode, Context, TargetError, TargetKind, TargetRejection, resolve_user_target,
};

fn ubuntu_context() -> Context<'static, [&'static str]> {
    static REGISTRY: [&str; 1] = ["Ubuntu"];
    let registry: &'static [&'static str] = &REGISTRY;
    Context {
        registry,
        mode: BareSlashMode::DistributionList,
        preferred: None,
        wsl_default: None,
    }
}

#[test]
fn mounted_drive_is_local_even_when_destination_does_not_exist() {
    let target = resolve_user_target(
        "/mnt/c/not-created-yet/release notes.txt",
        None::<&Context<'_, [&str]>>,
        None,
        None,
    )
    .expect("a local mount alias needs no WSL registry");

    assert_eq!(target.kind(), TargetKind::LocalDrive);
    assert_eq!(
        target.native_path(),
        r"C:\not-created-yet\release notes.txt"
    );
    assert_eq!(target.to_wsl_path("Ubuntu"), None);
}

#[test]
fn explicit_distribution_mount_preserves_its_actual_linux_context() {
    let context = ubuntu_context();
    let target = resolve_user_target("/Ubuntu/mnt/c/project/file.txt", Some(&context), None, None)
        .expect("explicit distribution target");

    assert_eq!(target.kind(), TargetKind::WslDistribution);
    assert_eq!(
        target.native_path(),
        r"\\wsl.localhost\Ubuntu\mnt\c\project\file.txt"
    );
    assert_eq!(
        target.to_wsl_path("Ubuntu").as_deref(),
        Some("/mnt/c/project/file.txt")
    );
    assert_eq!(target.to_wsl_path("Debian"), None);
}

#[test]
fn native_drive_and_unc_targets_remain_distinct() {
    let drive = resolve_user_target(
        r"C:\save-destinations\future file.txt",
        None::<&Context<'_, [&str]>>,
        None,
        None,
    )
    .expect("native drive");
    let unc = resolve_user_target(
        r"\\server\share\future file.txt",
        None::<&Context<'_, [&str]>>,
        None,
        None,
    )
    .expect("native UNC");

    assert_eq!(drive.kind(), TargetKind::LocalDrive);
    assert_eq!(unc.kind(), TargetKind::NativeUNC);
    assert_eq!(unc.native_path(), r"\\server\share\future file.txt");
}

#[test]
fn file_uri_round_trips_spaces_unicode_percent_and_hash() {
    let native = r"C:\save destinations\café 100%#.txt";
    let target = resolve_user_target(native, None::<&Context<'_, [&str]>>, None, None)
        .expect("native input");
    let uri = target.file_uri().expect("file URI");

    assert_eq!(
        uri,
        "file:///C:/save%20destinations/caf%C3%A9%20100%25%23.txt"
    );
    let reparsed = resolve_user_target(&uri, None::<&Context<'_, [&str]>>, None, None)
        .expect("file URI input");
    assert_eq!(reparsed.native_path(), native);
}

#[test]
fn malformed_percent_and_encoded_separator_are_rejected() {
    assert!(matches!(
        resolve_user_target(
            "file:///C:/bad%ZZ",
            None::<&Context<'_, [&str]>>,
            None,
            None,
        ),
        Err(TargetError::Rejected(
            TargetRejection::InvalidPercentEncoding
        ))
    ));
    assert!(matches!(
        resolve_user_target(
            "file:///C:%2Fescaped",
            None::<&Context<'_, [&str]>>,
            None,
            None,
        ),
        Err(TargetError::Rejected(TargetRejection::InvalidFileUri))
    ));
}

#[test]
fn devices_relatives_and_https_do_not_enter_navigation() {
    assert!(matches!(
        resolve_user_target(r"\\?\C:\secret", None::<&Context<'_, [&str]>>, None, None,),
        Err(TargetError::Rejected(TargetRejection::DevicePath))
    ));
    assert_eq!(
        resolve_user_target("new-folder", None::<&Context<'_, [&str]>>, None, None,),
        Err(TargetError::NeedsContext)
    );
    assert_eq!(
        resolve_user_target(
            "https://example.test/path",
            None::<&Context<'_, [&str]>>,
            None,
            None,
        ),
        Err(TargetError::NotHandled)
    );
}
