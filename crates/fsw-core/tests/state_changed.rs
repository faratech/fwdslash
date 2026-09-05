//! The cross-process change notification (issue #55).
//!
//! `FSW_STATE_CHANGED_MESSAGE` is a wire contract in the same sense the broker
//! window class is: three binaries, shipped and updated separately (a Store
//! settings app can be running beside a staged unpackaged `fwdslash.exe` two
//! versions older), agree only because they register the *same string*. A
//! rename is therefore not a refactor — it silently splits every listener from
//! every writer, with no error anywhere — so the literal is pinned here.

use fsw_core::{FSW_STATE_CHANGED_MESSAGE, broadcast_state_changed, state_changed_message};

#[test]
fn state_changed_message_name_is_stable() {
    assert_eq!(
        FSW_STATE_CHANGED_MESSAGE, "ForwardSlashWindows.StateChanged",
        "the registered message name is a cross-version contract; \
         changing it must be a deliberate, coordinated change"
    );
}

/// The name shares the product's `ForwardSlashWindows.` prefix with the broker
/// window class, and carries no user data: it is a global atom, readable by
/// anything on the desktop (PRIVACY.md).
#[test]
fn state_changed_message_name_is_product_scoped() {
    assert!(FSW_STATE_CHANGED_MESSAGE.starts_with("ForwardSlashWindows."));
    assert!(!FSW_STATE_CHANGED_MESSAGE.contains(char::is_whitespace));
}

/// Registration is cached, so every call after the first is a load — the
/// broker asks for this id on the thread that owns the keyboard hook.
#[test]
fn state_changed_message_id_is_stable_within_a_session() {
    assert_eq!(state_changed_message(), state_changed_message());
}

/// Off Windows the whole notification compiles to nothing: no id, and a
/// broadcast that does not post. `fsw-core` type-checks on Linux, and these
/// two are the fallbacks that let it.
#[cfg(not(windows))]
#[test]
fn broadcast_is_inert_off_windows() {
    assert_eq!(state_changed_message(), 0);
    broadcast_state_changed();
}

/// On Windows the broadcast must be safe to call from anywhere, including a
/// process with no window of its own — which is what every CLI verb is.
#[cfg(windows)]
#[test]
fn broadcast_from_a_windowless_process_is_safe() {
    assert_ne!(state_changed_message(), 0);
    broadcast_state_changed();
}
