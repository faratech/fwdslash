//! Behavioural contract for the resolver.
//!
//! Every case from the C++ `tests/core_tests.cpp` is carried over, plus the
//! cases that suite never covered. Each row expands into its own named `#[test]`
//! so `cargo test bare_slash_pin_preserves_casing` works — the C++ harness was a
//! single binary with no filtering, which CLAUDE.md calls out as a gap.

use fsw_path::{
    BareSlashMode::{DefaultDistribution, DistributionList},
    Context, RenderBuf, ResolveError, Resolved, eq_ignore_case, is_valid_distribution_name,
    resolve, resolve_strict,
};

/// Mirrors the C++ `Registered` predicate, including the non-ASCII entry.
const REGISTERED: &[&str] = &["Ubuntu", "Dev Distro", "\u{65e5}\u{672c}\u{8a9e}"];

#[derive(Debug)]
enum Expect {
    /// The provider root that lists distributions.
    Root,
    /// `distribution`, `unc_display`, `linux_path`
    Path(&'static str, &'static str, &'static str),
    Fail(ResolveError),
}

fn check(input: &str, got: &Result<Resolved<'_>, ResolveError>, want: &Expect) {
    match (got, want) {
        (Ok(Resolved::WslRoot), Expect::Root) => {
            assert_eq!(got.as_ref().unwrap().unc_display(), r"\\wsl.localhost");
            assert_eq!(got.as_ref().unwrap().linux_path(), "/");
            assert!(got.as_ref().unwrap().distribution().is_none());
        }
        (Ok(Resolved::Distribution(path)), Expect::Path(distro, unc, linux)) => {
            assert_eq!(path.distribution(), *distro, "distribution for {input:?}");
            assert_eq!(path.unc_display(), *unc, "unc for {input:?}");
            assert_eq!(path.linux_path(), *linux, "linux for {input:?}");
        }
        (Err(error), Expect::Fail(want_error)) => {
            assert_eq!(error, want_error, "error for {input:?}");
        }
        _ => panic!("for {input:?}: got {got:?}, wanted {want:?}"),
    }
}

macro_rules! cases {
    ($($name:ident: $input:expr, $mode:expr, $pref:expr, $def:expr => $want:expr;)*) => {
        $(
            #[test]
            fn $name() {
                let mut buf = RenderBuf::new();
                let ctx = Context {
                    registry: REGISTERED,
                    mode: $mode,
                    preferred: $pref,
                    wsl_default: $def,
                };
                let got = resolve($input, &ctx, &mut buf);
                check($input, &got, &$want);
            }
        )*
    };
}

// ---------------------------------------------------------------------------
// Carried over from tests/core_tests.cpp
// ---------------------------------------------------------------------------

cases! {
    bare_slash_lists_distributions:
        "/", DistributionList, None, None => Expect::Root;

    distribution_root_gains_no_empty_component:
        "/Ubuntu/", DistributionList, None, None
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu", "/");

    dot_segments_normalize_and_trailing_slash_survives:
        "/ubuntu/home/me/../project/", DistributionList, None, None
        => Expect::Path("ubuntu", r"\\wsl.localhost\ubuntu\home\project\", "/home/project/");

    spaces_resolve:
        "/Dev Distro/home/user/My Project", DistributionList, None, None
        => Expect::Path(
            "Dev Distro",
            r"\\wsl.localhost\Dev Distro\home\user\My Project",
            "/home/user/My Project",
        );

    unicode_resolves:
        "/\u{65e5}\u{672c}\u{8a9e}/home/\u{30c6}\u{30b9}\u{30c8}", DistributionList, None, None
        => Expect::Path(
            "\u{65e5}\u{672c}\u{8a9e}",
            "\\\\wsl.localhost\\\u{65e5}\u{672c}\u{8a9e}\\home\\\u{30c6}\u{30b9}\u{30c8}",
            "/home/\u{30c6}\u{30b9}\u{30c8}",
        );

    drive_letter_is_not_a_slash_path:
        "C:/Ubuntu", DistributionList, None, None
        => Expect::Fail(ResolveError::NotASlashPath);

    double_leading_slash_rejected:
        "//Ubuntu", DistributionList, None, None
        => Expect::Fail(ResolveError::DoubleLeadingSlash);

    unregistered_distribution_rejected:
        "/Debian/home", DistributionList, None, None
        => Expect::Fail(ResolveError::UnregisteredDistribution);

    backslash_rejected:
        "/Ubuntu\\home", DistributionList, None, None
        => Expect::Fail(ResolveError::BackslashNotAllowed);

    traversal_above_distribution_root_rejected:
        "/Ubuntu/../Debian", DistributionList, None, None
        => Expect::Fail(ResolveError::TraversalAboveRoot);

    list_mode_keeps_the_bare_slash_root:
        "/", DistributionList, None, Some("Ubuntu") => Expect::Root;

    default_mode_resolves_the_bare_slash:
        "/", DefaultDistribution, None, Some("Ubuntu")
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu", "/");

    registered_pin_wins_over_the_wsl_default:
        "/", DefaultDistribution, Some("Dev Distro"), Some("Ubuntu")
        => Expect::Path("Dev Distro", r"\\wsl.localhost\Dev Distro", "/");

    bare_slash_pin_preserves_casing:
        "/", DefaultDistribution, Some("dev distro"), Some("Ubuntu")
        => Expect::Path("dev distro", r"\\wsl.localhost\dev distro", "/");

    unregistered_pin_falls_back_to_the_wsl_default:
        "/", DefaultDistribution, Some("Debian"), Some("Ubuntu")
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu", "/");

    no_usable_default_blocks_the_bare_slash:
        "/", DefaultDistribution, Some("Debian"), None
        => Expect::Fail(ResolveError::NoDefaultDistribution);

    unknown_wsl_default_blocks_the_bare_slash:
        "/", DefaultDistribution, None, None
        => Expect::Fail(ResolveError::NoDefaultDistribution);

    default_mode_leaves_explicit_distribution_paths_alone:
        "/Ubuntu/home", DefaultDistribution, Some("Ubuntu"), None
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu\home", "/home");

    non_distro_path_lands_in_the_default_distribution:
        "/tmp/build/log.txt", DefaultDistribution, None, Some("Ubuntu")
        => Expect::Path(
            "Ubuntu",
            r"\\wsl.localhost\Ubuntu\tmp\build\log.txt",
            "/tmp/build/log.txt",
        );

    non_distro_path_honours_the_pin:
        "/tmp", DefaultDistribution, Some("Dev Distro"), Some("Ubuntu")
        => Expect::Path("Dev Distro", r"\\wsl.localhost\Dev Distro\tmp", "/tmp");

    non_distro_path_keeps_its_trailing_separator:
        "/tmp/", DefaultDistribution, None, Some("Ubuntu")
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu\tmp\", "/tmp/");

    non_distro_path_normalizes_traversal:
        "/tmp/nested/../sibling", DefaultDistribution, None, Some("Ubuntu")
        => Expect::Path(
            "Ubuntu",
            r"\\wsl.localhost\Ubuntu\tmp\sibling",
            "/tmp/sibling",
        );

    non_distro_path_must_not_escape_the_distribution_root:
        "/../escape", DefaultDistribution, None, Some("Ubuntu")
        => Expect::Fail(ResolveError::TraversalAboveRoot);

    list_mode_still_rejects_an_unregistered_distribution:
        "/tmp", DistributionList, None, Some("Ubuntu")
        => Expect::Fail(ResolveError::UnregisteredDistribution);

    no_usable_default_blocks_a_non_distro_path:
        "/tmp", DefaultDistribution, Some("Debian"), None
        => Expect::Fail(ResolveError::NoDefaultDistribution);

    a_registered_distribution_wins_over_the_default:
        "/Dev Distro/home", DefaultDistribution, Some("Ubuntu"), Some("Ubuntu")
        => Expect::Path("Dev Distro", r"\\wsl.localhost\Dev Distro\home", "/home");
}

// ---------------------------------------------------------------------------
// Cases the C++ suite never covered
// ---------------------------------------------------------------------------

cases! {
    empty_input_is_not_a_slash_path:
        "", DistributionList, None, None => Expect::Fail(ResolveError::NotASlashPath);

    embedded_nul_rejected:
        "/Ubuntu/et\0c", DistributionList, None, None
        => Expect::Fail(ResolveError::EmbeddedNul);

    double_leading_slash_is_checked_before_nul:
        "//\0", DistributionList, None, None
        => Expect::Fail(ResolveError::DoubleLeadingSlash);

    backslash_is_checked_before_the_distribution_lookup:
        "/Debian\\home", DistributionList, None, None
        => Expect::Fail(ResolveError::BackslashNotAllowed);

    distribution_without_trailing_slash:
        "/Ubuntu", DistributionList, None, None
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu", "/");

    repeated_separators_collapse:
        "/Ubuntu//home///project", DistributionList, None, None
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu\home\project", "/home/project");

    current_directory_segments_vanish:
        "/Ubuntu/./home/./.", DistributionList, None, None
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu\home", "/home");

    traversal_back_to_the_root_is_rejected:
        "/Ubuntu/home/../..", DistributionList, None, None
        => Expect::Fail(ResolveError::TraversalAboveRoot);

    traversal_to_exactly_the_root_is_allowed:
        "/Ubuntu/home/..", DistributionList, None, None
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu", "/");

    three_dots_is_an_ordinary_component:
        "/Ubuntu/...", DistributionList, None, None
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu\...", "/...");

    reserved_characters_pass_through:
        "/Ubuntu/a:b", DistributionList, None, None
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu\a:b", "/a:b");

    an_empty_pin_falls_through_to_the_wsl_default:
        "/tmp", DefaultDistribution, Some(""), Some("Ubuntu")
        => Expect::Path("Ubuntu", r"\\wsl.localhost\Ubuntu\tmp", "/tmp");
}

#[test]
fn bare_slash_in_default_mode_reports_a_trailing_separator() {
    // The C++ rewrites to "/Ubuntu/", whose trailing slash is significant to
    // rule R6 even though R12 discards it for an empty component list.
    let mut buf = RenderBuf::new();
    let ctx = Context {
        registry: REGISTERED,
        mode: DefaultDistribution,
        preferred: None,
        wsl_default: Some("Ubuntu"),
    };
    let Ok(Resolved::Distribution(path)) = resolve("/", &ctx, &mut buf) else {
        panic!("bare slash should resolve in default mode");
    };
    assert!(path.had_trailing_separator());
    assert_eq!(path.unc_display(), r"\\wsl.localhost\Ubuntu");
}

#[test]
fn explicit_distribution_root_without_slash_reports_no_trailing_separator() {
    let mut buf = RenderBuf::new();
    let Ok(Resolved::Distribution(path)) = resolve_strict("/Ubuntu", &REGISTERED, &mut buf) else {
        panic!("should resolve");
    };
    assert!(!path.had_trailing_separator());
}

#[test]
fn win32_normalization_hazard_is_detected() {
    let mut buf = RenderBuf::new();
    for (input, hazard) in [
        ("/Ubuntu/secret ", true),
        ("/Ubuntu/foo.", true),
        ("/Ubuntu/foo.txt", false),
        ("/Ubuntu/plain", false),
    ] {
        let Ok(Resolved::Distribution(path)) = resolve_strict(input, &REGISTERED, &mut buf) else {
            panic!("{input} should resolve");
        };
        assert_eq!(
            path.has_win32_normalization_hazard(),
            hazard,
            "hazard for {input:?} (linux {:?})",
            path.linux_path()
        );
    }
}

#[test]
fn render_buffer_is_reusable_and_results_are_independent() {
    let mut buf = RenderBuf::new();
    for _ in 0..3 {
        let got = resolve_strict("/Ubuntu/home", &REGISTERED, &mut buf);
        assert_eq!(got.unwrap().unc_display(), r"\\wsl.localhost\Ubuntu\home");
    }
}

// ---------------------------------------------------------------------------
// The rewrite optimization, validated against the C++ algorithm
// ---------------------------------------------------------------------------

/// A faithful transcription of the C++ `ResolveSlashPathWithBareSlashMode`:
/// build `"/" + target + input` and re-parse. Kept only as the oracle.
fn reference_rewrite(
    input: &str,
    mode: fsw_path::BareSlashMode,
    preferred: Option<&str>,
    wsl_default: Option<&str>,
    buf: &mut RenderBuf,
) -> Result<(String, String, String), ResolveError> {
    let mut probe = RenderBuf::new();
    let direct = resolve_strict(input, &REGISTERED, &mut probe);
    if mode == DistributionList {
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

fn snapshot(resolved: Resolved<'_>) -> (String, String, String) {
    (
        resolved.distribution().unwrap_or_default().to_owned(),
        resolved.unc_display().to_owned(),
        resolved.linux_path().to_owned(),
    )
}

trait TestRegistry {
    fn is_registered_for_test(&self, name: &str) -> bool;
}
impl TestRegistry for &[&str] {
    fn is_registered_for_test(&self, name: &str) -> bool {
        self.iter().any(|candidate| eq_ignore_case(candidate, name))
    }
}

#[test]
fn rewrite_equivalence() {
    // The direct path passes the distribution out-of-band instead of
    // concatenating and re-parsing. Prove the two agree over a corpus rather
    // than resting on the argument in the source comment.
    let leading = [
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
    let pins = [
        None,
        Some("Ubuntu"),
        Some("Dev Distro"),
        Some("dev distro"),
        Some("Debian"),
        Some(""),
    ];
    let defaults = [None, Some("Ubuntu"), Some("Dev Distro"), Some("")];

    let mut checked = 0_u32;
    for input in leading {
        for pref in pins {
            for def in defaults {
                for mode in [DistributionList, DefaultDistribution] {
                    let mut direct_buf = RenderBuf::new();
                    let mut ref_buf = RenderBuf::new();
                    let ctx = Context {
                        registry: REGISTERED,
                        mode,
                        preferred: pref,
                        wsl_default: def,
                    };
                    let direct = resolve(input, &ctx, &mut direct_buf).map(snapshot);
                    let reference = reference_rewrite(input, mode, pref, def, &mut ref_buf);
                    assert_eq!(
                        direct, reference,
                        "divergence for input={input:?} mode={mode:?} pin={pref:?} default={def:?}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 500, "corpus shrank unexpectedly: {checked}");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[test]
fn case_insensitive_comparison() {
    assert!(eq_ignore_case("Ubuntu", "ubuntu"));
    assert!(eq_ignore_case("UBUNTU", "ubuntu"));
    assert!(eq_ignore_case("Dev Distro", "dev distro"));
    assert!(!eq_ignore_case("Ubuntu", "Debian"));
    assert!(!eq_ignore_case("Ubuntu", "Ubuntu2"));
    assert!(eq_ignore_case("", ""));
    // Non-ASCII, uncased script: unaffected by folding.
    assert!(eq_ignore_case(
        "\u{65e5}\u{672c}\u{8a9e}",
        "\u{65e5}\u{672c}\u{8a9e}"
    ));
    // Non-ASCII, cased.
    assert!(eq_ignore_case("\u{dc}nicode", "\u{fc}nicode"));
}

#[test]
fn distribution_name_validation() {
    assert!(is_valid_distribution_name("Ubuntu"));
    assert!(is_valid_distribution_name("Dev Distro"));
    assert!(is_valid_distribution_name("\u{65e5}\u{672c}\u{8a9e}"));
    assert!(is_valid_distribution_name("openSUSE-Tumbleweed"));
    assert!(!is_valid_distribution_name(""));
    assert!(!is_valid_distribution_name("."));
    assert!(!is_valid_distribution_name(".."));
    assert!(!is_valid_distribution_name("a/b"));
    assert!(!is_valid_distribution_name("a\\b"));
    assert!(!is_valid_distribution_name("a:b"));
    assert!(!is_valid_distribution_name("a\u{1}b"));
}

#[test]
fn error_names_match_the_cpp_wire_values() {
    // These strings are emitted as `reason=<name>` into the diagnostic log and
    // must not drift from the C++ ResolveErrorName.
    for (error, name) in [
        (ResolveError::NotASlashPath, "not_a_slash_path"),
        (ResolveError::DoubleLeadingSlash, "double_leading_slash"),
        (ResolveError::MissingDistribution, "missing_distribution"),
        (
            ResolveError::UnregisteredDistribution,
            "unregistered_distribution",
        ),
        (ResolveError::BackslashNotAllowed, "backslash_not_allowed"),
        (ResolveError::EmbeddedNul, "embedded_nul"),
        (ResolveError::TraversalAboveRoot, "traversal_above_root"),
        (
            ResolveError::NoDefaultDistribution,
            "no_default_distribution",
        ),
    ] {
        assert_eq!(error.name(), name);
        assert!(!error.message().is_empty());
    }
}

#[test]
fn case_folding_matches_the_win32_simple_uppercase_table() {
    // CompareStringOrdinal(.., TRUE) folds through the *simple* uppercase table,
    // which is 1:1 and never changes length. Rust's char::to_uppercase is the
    // *full* mapping and expands some characters, so the resolver takes only the
    // single-character mappings. These are the cases where that matters.

    // Turkish dotted/dotless I: both agree with Win32.
    assert!(
        !eq_ignore_case("\u{130}", "i"),
        "U+0130 has no simple lowercase to 'i'"
    );
    assert!(
        eq_ignore_case("\u{131}", "I"),
        "U+0131 simple-uppercases to 'I'"
    );

    // Full-mapping expansions must NOT collapse, or a distribution named
    // \u{df} would compare equal to one named SS.
    assert!(!eq_ignore_case("\u{df}", "SS"));
    assert!(!eq_ignore_case("stra\u{df}e", "STRASSE"));
    assert!(!eq_ignore_case("\u{fb01}", "FI"), "ligature fi vs FI");

    // Ordinary cased non-ASCII still folds.
    assert!(eq_ignore_case("\u{fc}nicode", "\u{dc}NICODE"));
    assert!(eq_ignore_case("\u{e9}t\u{e9}", "\u{c9}T\u{c9}"));
}
