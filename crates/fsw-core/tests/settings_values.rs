//! `SettingsValues` — the one-handle read of the settings key — and the
//! adapter-version helpers.
//!
//! The registry itself is `#[cfg(windows)]`, so what is pinned here is the
//! contract that holds on every platform: the defaults an absent key yields,
//! and that the single-value getters and `Snapshot::current` agree with the
//! batched read.

use fsw_core::{
    SettingsValues, Snapshot, adapter_outdated, adapter_version, get_bare_slash_mode,
    get_bare_slash_override, get_bare_slash_root, is_disabled,
};
use fsw_path::BareSlashMode;

/// A marker key no install ever writes.
const ABSENT_MARKER: &str = r"Software\ForwardSlashWindows\NoSuchAdapter";

#[test]
fn defaults_are_nothing_paused_and_nothing_pinned() {
    let defaults = SettingsValues::default();
    assert!(!defaults.disabled);
    assert_eq!(defaults.bare_slash_mode, BareSlashMode::DistributionList);
    assert_eq!(defaults.bare_slash_pinned, None);
    assert_eq!(defaults.bare_slash_root, None);
}

#[cfg(not(windows))]
#[test]
fn read_yields_the_defaults_without_a_registry() {
    assert_eq!(SettingsValues::read(), SettingsValues::default());
}

#[test]
fn the_single_value_getters_agree_with_the_batched_read() {
    // The getters now delegate to `SettingsValues::read`; this is the pin that
    // says a future divergence is a bug, not a refactor.
    let values = SettingsValues::read();
    assert_eq!(values.disabled, is_disabled());
    assert_eq!(values.bare_slash_mode, get_bare_slash_mode());
    assert_eq!(
        values.bare_slash_pinned.clone().unwrap_or_default(),
        get_bare_slash_override()
    );
    assert_eq!(values.bare_slash_root, get_bare_slash_root());
    // A pin is either absent or non-empty; an empty stored value is no pin.
    assert_ne!(values.bare_slash_pinned.as_deref(), Some(""));
}

#[test]
fn snapshot_carries_the_same_settings_it_read() {
    let snapshot = Snapshot::current();
    let values = SettingsValues::read();
    assert_eq!(snapshot.disabled, values.disabled);
    assert_eq!(snapshot.bare_slash_mode, values.bare_slash_mode);
    assert_eq!(snapshot.bare_slash_pinned, values.bare_slash_pinned);
    assert_eq!(snapshot.bare_slash_root, values.bare_slash_root);
    // The Lxss half is read through one handle now; the default must still be
    // one of the registered names (or nothing at all).
    if let Some(default) = snapshot.default_distribution.as_deref() {
        assert!(
            snapshot
                .distributions
                .iter()
                .any(|name| fsw_path::eq_ignore_case(name, default)),
            "the default distribution must be registered"
        );
    }
}

#[test]
fn an_uninstalled_adapter_has_no_version_and_is_never_outdated() {
    assert_eq!(adapter_version(ABSENT_MARKER), None);
    assert!(!adapter_outdated(ABSENT_MARKER, "0.0.3"));
}
