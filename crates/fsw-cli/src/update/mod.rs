//! Self-update support for the CLI.
//!
//! This module currently holds only the vendored Microsoft Store bindings the
//! update routes are built on; the `fwdslash update check|install|status` verbs
//! land on top of them. The bindings are compiled from here so that a stale or
//! non-compiling regeneration fails the ordinary `cargo check`, not a later
//! change.

// Generated file: rustfmt is never to touch it, because the committed bytes are
// compared against a fresh generation in CI (`tools/regen_install_control.py
// --check`) and any reformatting would look like drift.
#[rustfmt::skip]
pub mod install_control;
