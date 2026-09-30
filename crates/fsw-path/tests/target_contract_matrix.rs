#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use fsw_path::{
    BareSlashMode, Context, ResolveError, TargetBase, TargetError, TargetKind, TargetRejection,
    resolve_user_target,
};

fn target(input: &str) -> fsw_path::UserTarget {
    resolve_user_target::<[&str]>(input, None, None, None)
        .unwrap_or_else(|error| panic!("{input:?}: {error:?}"))
}

#[test]
fn native_file_uri_round_trip_matrix() {
    for path in [
        "C:\\",
        r"D:\not-created-by-this-test\save target.txt",
        r"C:\literal%2F.txt",
        r"C:\literal%5c.txt",
        r"C:\literal%252f.txt",
        r"C:\100%#done.txt",
        r"C:\semi;and&plus+equals=bang!.txt",
        r"C:\brackets[1](2)\file.txt",
        "C:\\caf\u{e9}\\\u{65e5}\u{672c}\u{8a9e}\\\u{1f680}.txt",
        "C:\\decomposed-e\u{301}.txt",
        r"\\server\share",
        r"\\server\share\a b\100%#.txt",
        "\\\\server\\share\\caf\u{e9}.txt",
    ] {
        let original = target(path);
        let uri = original.file_uri().expect("native target has file URI");
        let decoded = target(&uri);
        assert_eq!(
            decoded.native_path(),
            original.native_path(),
            "{path:?} -> {uri}"
        );
        assert_eq!(decoded.kind(), original.kind(), "{uri}");
    }
}

#[test]
fn file_scheme_and_local_authority_are_case_insensitive() {
    for uri in [
        "file:///C:/some%20folder/file.txt",
        "FILE:///C:/some%20folder/file.txt",
        "File:///C:/some%20folder/file.txt",
        "file://localhost/C:/some%20folder/file.txt",
        "file://LOCALHOST/C:/some%20folder/file.txt",
    ] {
        let resolved = target(uri);
        assert_eq!(resolved.kind(), TargetKind::LocalDrive, "{uri}");
        assert_eq!(resolved.native_path(), r"C:\some folder\file.txt", "{uri}");
    }
}

#[test]
fn file_uri_never_reinterprets_query_fragment_or_userinfo_as_filename() {
    for uri in [
        "file:///C:/safe.txt?download=1",
        "file:///C:/safe.txt#fragment",
        "file://user@server/share/file.txt",
        "file://server:443/share/file.txt",
    ] {
        assert!(
            resolve_user_target::<[&str]>(uri, None, None, None).is_err(),
            "{uri}"
        );
    }
}

#[test]
fn malformed_and_control_percent_encodings_are_rejected() {
    for uri in [
        "file:///C:/bad%",
        "file:///C:/bad%1",
        "file:///C:/bad%GG",
        "file:///C:/bad%FF",
        "file:///C:/bad%C0%AF",
        "file:///C:/bad%ED%A0%80",
        "file:///C:/bad%00.txt",
        "file:///C:/bad%0A.txt",
        "file:///C:/bad%0D.txt",
        "file:///C:/a%2Fb.txt",
        "file:///C:/a%2fb.txt",
        "file:///C:/a%5Cb.txt",
        "file:///C:/a%5cb.txt",
    ] {
        assert!(
            resolve_user_target::<[&str]>(uri, None, None, None).is_err(),
            "{uri}"
        );
    }
}

#[test]
fn device_namespaces_never_enter_user_target_routing() {
    for path in [
        r"\\?\C:\secret",
        r"\\.\C:\secret",
        r"\\?\UNC\server\share\secret",
        r"\??\C:\secret",
        "//?/C:/secret",
        "//./C:/secret",
        "file://%3F/C:/secret",
        "C:\\bad\0name",
    ] {
        assert!(
            resolve_user_target::<[&str]>(path, None, None, None).is_err(),
            "{path:?}"
        );
    }
}

#[test]
fn ordinary_urls_and_shell_tokens_do_not_become_paths() {
    for input in [
        "https://example.com/mnt/c/path",
        "https://mnt/c/path",
        "http://mnt/c/path",
        "ftp://server/path",
        "mailto:user@example.com",
        "about:blank",
        "javascript:alert(1)",
        "data:text/plain,hello",
        "shell:Downloads",
        "--help",
        "C:relative.txt",
        "relative.txt",
    ] {
        assert!(
            resolve_user_target::<[&str]>(input, None, None, None).is_err(),
            "{input}"
        );
    }
}

#[test]
fn relative_targets_require_explicit_base_namespace() {
    let native = resolve_user_target::<[&str]>(
        "child.txt",
        None,
        None,
        Some(TargetBase::NativePath(r"C:\base")),
    )
    .unwrap();
    assert_eq!(native.kind(), TargetKind::LocalDrive);
    assert_eq!(native.native_path(), r"C:\base\child.txt");

    let distributions = ["Ubuntu", "Debian"];
    let context = Context {
        registry: &distributions[..],
        mode: BareSlashMode::DistributionList,
        preferred: None,
        wsl_default: None,
    };
    let linux = resolve_user_target(
        "child.txt",
        Some(&context),
        None,
        Some(TargetBase::WslDistribution {
            distribution: "Ubuntu",
            linux_path: "/home/user",
        }),
    )
    .unwrap();
    assert_eq!(linux.kind(), TargetKind::WslDistribution);
    assert_eq!(
        linux.to_wsl_path("Ubuntu").as_deref(),
        Some("/home/user/child.txt")
    );
    assert!(linux.to_wsl_path("Debian").is_none());
}

#[test]
fn explicit_wsl_base_ignores_default_distribution_and_custom_root() {
    let distributions = ["Ubuntu", "Debian"];
    for mode in [
        BareSlashMode::DistributionList,
        BareSlashMode::DefaultDistribution,
    ] {
        let context = Context {
            registry: &distributions[..],
            mode,
            preferred: Some("Debian"),
            wsl_default: Some("Debian"),
        };
        for root in [None, Some(r"C:\code"), Some(r"\\server\share")] {
            let resolved = resolve_user_target(
                "../documents/child.txt",
                Some(&context),
                root,
                Some(TargetBase::WslDistribution {
                    distribution: "Ubuntu",
                    linux_path: "/home/user",
                }),
            )
            .unwrap();
            assert_eq!(resolved.kind(), TargetKind::WslDistribution);
            assert_eq!(
                resolved.native_path(),
                r"\\wsl.localhost\Ubuntu\home\documents\child.txt"
            );
            assert_eq!(
                resolved.to_wsl_path("Ubuntu").as_deref(),
                Some("/home/documents/child.txt")
            );
            assert!(resolved.to_wsl_path("Debian").is_none());
        }
    }
}

#[test]
fn explicit_wsl_base_is_not_a_mounted_drive_alias() {
    let distributions = ["mnt"];
    let context = Context::list_mode(&distributions[..]);
    let resolved = resolve_user_target(
        "child.txt",
        Some(&context),
        None,
        Some(TargetBase::WslDistribution {
            distribution: "mnt",
            linux_path: "/c",
        }),
    )
    .unwrap();
    assert_eq!(resolved.kind(), TargetKind::WslDistribution);
    assert_eq!(resolved.native_path(), r"\\wsl.localhost\mnt\c\child.txt");
}

#[test]
fn explicit_wsl_base_still_validates_registration_and_traversal() {
    let distributions = ["Ubuntu"];
    let context = Context::list_mode(&distributions[..]);
    let resolve = |distribution, linux_path, input| {
        resolve_user_target(
            input,
            Some(&context),
            Some(r"C:\code"),
            Some(TargetBase::WslDistribution {
                distribution,
                linux_path,
            }),
        )
    };
    assert_eq!(
        resolve("Debian", "/home", "child.txt"),
        Err(TargetError::Rejected(TargetRejection::SlashPath(
            ResolveError::UnregisteredDistribution
        )))
    );
    assert_eq!(
        resolve("Ubuntu", "/", "../child.txt"),
        Err(TargetError::Rejected(TargetRejection::SlashPath(
            ResolveError::TraversalAboveRoot
        )))
    );
    for distribution in ["", "Ubuntu/other", "Ubuntu\\other"] {
        assert_eq!(
            resolve(distribution, "/home", "child.txt"),
            Err(TargetError::Rejected(TargetRejection::InvalidPath))
        );
    }
}
