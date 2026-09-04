//! Corpus and oracles shared by the resolver test binaries.
//!
//! Kept in a `tests/common` module rather than a dev-dependency on purpose:
//! `cargo tree -p fsw-path` counts dev-dependencies too, and the documented
//! gate (`docs/dependencies.md`) requires that output to be exactly one line.
//! Everything here is std-only, as every test target in this crate must stay.

use fsw_path::{
    BareSlashMode, Context, RenderBuf, ResolveError, Resolved, eq_ignore_case, resolve_strict,
};

/// Mirrors the C++ `Registered` predicate, including the non-ASCII entry.
pub const REGISTERED: &[&str] = &["Ubuntu", "Dev Distro", "\u{65e5}\u{672c}\u{8a9e}"];

/// Inputs probed by `rewrite_equivalence` and by the perf/allocation tests.
pub const LEADING: &[&str] = &[
    "",
    "/",
    "/tmp",
    "/tmp/",
    "/Ubuntu",
    "/Ubuntu/",
    "/ubuntu",
    "/Debian",
    "/..",
    "/../..",
    "/.",
    "/./tmp",
    "/tmp/nested/../sibling",
    "/tmp//double",
    "/Dev Distro/x",
    "/a:b",
    "/deep/a/b/c/d/e",
    "/trailing/dot.",
    "/trailing/space ",
];

pub const PINS: &[Option<&str>] = &[
    None,
    Some("Ubuntu"),
    Some("Dev Distro"),
    Some("dev distro"),
    Some("Debian"),
    Some(""),
];

pub const DEFAULTS: &[Option<&str>] = &[None, Some("Ubuntu"), Some("Dev Distro"), Some("")];

pub fn snapshot(resolved: Resolved<'_>) -> (String, String, String) {
    (
        resolved.distribution().unwrap_or_default().to_owned(),
        resolved.unc_display().to_owned(),
        resolved.linux_path().to_owned(),
    )
}

pub trait TestRegistry {
    fn is_registered_for_test(&self, name: &str) -> bool;
}
impl TestRegistry for &[&str] {
    fn is_registered_for_test(&self, name: &str) -> bool {
        self.iter().any(|candidate| eq_ignore_case(candidate, name))
    }
}

/// A faithful transcription of the C++ `ResolveSlashPathWithBareSlashMode`:
/// build `"/" + target + input` and re-parse. Kept only as the oracle.
pub fn reference_rewrite(
    input: &str,
    mode: BareSlashMode,
    preferred: Option<&str>,
    wsl_default: Option<&str>,
    buf: &mut RenderBuf,
) -> Result<(String, String, String), ResolveError> {
    let mut probe = RenderBuf::new();
    let direct = resolve_strict(input, &REGISTERED, &mut probe);
    if mode == BareSlashMode::DistributionList {
        return direct.map(snapshot);
    }
    let passes_through = match &direct {
        Ok(resolved) => !resolved.is_wsl_root(),
        Err(error) => *error != ResolveError::UnregisteredDistribution,
    };
    if passes_through {
        return direct.map(snapshot);
    }

    let target = preferred
        .filter(|name| !name.is_empty() && REGISTERED.is_registered_for_test(name))
        .or_else(|| {
            wsl_default.filter(|name| !name.is_empty() && REGISTERED.is_registered_for_test(name))
        })
        .ok_or(ResolveError::NoDefaultDistribution)?;

    let mut rewritten = String::with_capacity(1 + target.len() + input.len());
    rewritten.push('/');
    rewritten.push_str(target);
    rewritten.push_str(input);
    resolve_strict(&rewritten, &REGISTERED, buf).map(snapshot)
}

/// Every (input × pin × default × mode) combination, for tests that want the
/// full corpus rather than a single case.
pub fn contexts() -> impl Iterator<Item = (&'static str, Context<'static, [ &'static str]>)> {
    LEADING.iter().flat_map(|input| {
        PINS.iter().flat_map(move |pref| {
            DEFAULTS.iter().flat_map(move |def| {
                [BareSlashMode::DistributionList, BareSlashMode::DefaultDistribution]
                    .into_iter()
                    .map(move |mode| {
                        (
                            *input,
                            Context {
                                registry: REGISTERED,
                                mode,
                                preferred: *pref,
                                wsl_default: *def,
                            },
                        )
                    })
            })
        })
    })
}
