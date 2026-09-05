//! The custom bare-slash root, as wired through `resolve_user_slash_path`.
//!
//! `fsw-path`'s own suite pins the pure join; this file pins the *funnel* —
//! which inputs fall through to the root, which stay WSL claims, and how a
//! stored root value changes (or does not change) each mode's answer.
//!
//! Snapshots are hand-built because the registry reads are `#[cfg(windows)]`:
//! the funnel logic under test here is the same on every platform.

use fsw_core::{Snapshot, resolve_user_slash_path};
use fsw_path::{BareSlashMode, RenderBuf, ResolveError};

fn snapshot(mode: BareSlashMode, pinned: Option<&str>, root: Option<&str>) -> Snapshot {
    Snapshot {
        distributions: vec!["Ubuntu".into(), "Dev Distro".into()],
        default_distribution: Some("Ubuntu".into()),
        bare_slash_mode: mode,
        bare_slash_pinned: pinned.map(str::to_string),
        bare_slash_root: root.map(str::to_string),
        disabled: false,
    }
}

fn resolve_str(input: &str, snap: &Snapshot) -> Result<String, ResolveError> {
    let mut buf = RenderBuf::new();
    resolve_user_slash_path(input, snap, &mut buf)
        .map(|resolved| resolved.unc_display().to_string())
}

#[test]
fn explicit_distribution_wins_over_the_root_in_both_modes() {
    for mode in [BareSlashMode::DistributionList, BareSlashMode::DefaultDistribution] {
        let snap = snapshot(mode, None, Some(r"C:\code"));
        assert_eq!(resolve_str("/Ubuntu/home", &snap).as_deref(), Ok(r"\\wsl.localhost\Ubuntu\home"));
    }
}

#[test]
fn bare_slash_targets_the_folder_in_list_mode() {
    let snap = snapshot(BareSlashMode::DistributionList, None, Some(r"C:\code"));
    assert_eq!(resolve_str("/", &snap).as_deref(), Ok(r"C:\code"));
}

#[test]
fn bare_slash_targets_the_folder_in_default_mode() {
    let snap = snapshot(BareSlashMode::DefaultDistribution, None, Some(r"C:\code"));
    assert_eq!(resolve_str("/", &snap).as_deref(), Ok(r"C:\code"));
    // The pin still applies to distro claims, but non-distro input goes to
    // the folder, not to the pinned distribution.
    assert_eq!(
        resolve_str("/tmp", &snap).as_deref(),
        Ok(r"C:\code\tmp")
    );
}

#[test]
fn unregistered_first_segment_targets_the_folder_in_list_mode() {
    let snap = snapshot(BareSlashMode::DistributionList, None, Some(r"C:\code"));
    // Today: Err(UnregisteredDistribution). With a root: the folder.
    assert_eq!(resolve_str("/tmp/build", &snap).as_deref(), Ok(r"C:\code\tmp\build"));
}

#[test]
fn no_default_distribution_falls_through_to_the_folder() {
    let mut snap = snapshot(BareSlashMode::DefaultDistribution, None, Some(r"C:\code"));
    snap.default_distribution = None;
    // Today: Err(NoDefaultDistribution). With a root: the folder.
    assert_eq!(resolve_str("/tmp", &snap).as_deref(), Ok(r"C:\code\tmp"));
    assert_eq!(resolve_str("/", &snap).as_deref(), Ok(r"C:\code"));
}

#[test]
fn input_shape_errors_are_not_intercepted() {
    let snap = snapshot(BareSlashMode::DistributionList, None, Some(r"C:\code"));
    assert_eq!(resolve_str("tmp", &snap), Err(ResolveError::NotASlashPath));
    assert_eq!(resolve_str("//tmp", &snap), Err(ResolveError::DoubleLeadingSlash));
    assert_eq!(resolve_str("/a\\b", &snap), Err(ResolveError::BackslashNotAllowed));
    // The funnel used to slice `input[1..]` before these checks: an empty
    // input was out of bounds and a multi-byte first character was not a char
    // boundary. Under `panic = "abort"` either aborted the process, so
    // `fwdslash resolve ''` killed the CLI instead of printing R1.
    for input in ["", "\u{fc}", "\u{fc}nix/x", "\u{65e5}\u{672c}"] {
        assert_eq!(
            resolve_str(input, &snap),
            Err(ResolveError::NotASlashPath),
            "for {input:?}"
        );
    }
}

#[test]
fn input_shape_errors_survive_without_a_configured_root() {
    // Same inputs, no root: the shape check is in the funnel, not the root.
    for mode in [BareSlashMode::DistributionList, BareSlashMode::DefaultDistribution] {
        let snap = snapshot(mode, None, None);
        for input in ["", "\u{fc}", "tmp"] {
            assert_eq!(
                resolve_str(input, &snap),
                Err(ResolveError::NotASlashPath),
                "for {input:?} in {mode:?}"
            );
        }
    }
}

#[test]
fn traversal_above_the_folder_root_is_rejected() {
    let snap = snapshot(BareSlashMode::DefaultDistribution, None, Some(r"C:\code"));
    assert_eq!(resolve_str("/..", &snap), Err(ResolveError::TraversalAboveRoot));
    // Traversal *inside* the root still works, clamped at the root.
    assert_eq!(resolve_str("/a/b/../../c", &snap).as_deref(), Ok(r"C:\code\c"));
}

#[test]
fn invalid_stored_root_is_ignored() {
    // A malformed value — however it got into the registry — must degrade to
    // today's behavior, not poison every resolve.
    let snap = snapshot(BareSlashMode::DistributionList, None, Some("relative\\junk"));
    assert_eq!(resolve_str("/", &snap).as_deref(), Ok(r"\\wsl.localhost"));
    let snap = snapshot(BareSlashMode::DistributionList, None, Some(""));
    assert_eq!(resolve_str("/", &snap).as_deref(), Ok(r"\\wsl.localhost"));
    let snap = snapshot(BareSlashMode::DistributionList, None, Some(r"\\?\C:\x"));
    assert_eq!(resolve_str("/", &snap).as_deref(), Ok(r"\\wsl.localhost"));
}

#[test]
fn without_a_root_resolution_is_unchanged() {
    for mode in [BareSlashMode::DistributionList, BareSlashMode::DefaultDistribution] {
        let snap = snapshot(mode, None, None);
        match mode {
            BareSlashMode::DistributionList => {
                assert_eq!(resolve_str("/", &snap).as_deref(), Ok(r"\\wsl.localhost"));
                assert_eq!(resolve_str("/tmp", &snap), Err(ResolveError::UnregisteredDistribution));
            }
            BareSlashMode::DefaultDistribution => {
                assert_eq!(resolve_str("/", &snap).as_deref(), Ok(r"\\wsl.localhost\Ubuntu"));
                assert_eq!(resolve_str("/tmp", &snap).as_deref(), Ok(r"\\wsl.localhost\Ubuntu\tmp"));
            }
        }
    }
}
