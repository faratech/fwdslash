fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    compile_resources();
}

/// The `FSW_VER_COMMAS` / `FSW_VER_STR` defines `app.rc` reads, derived from
/// `CARGO_PKG_VERSION` -- i.e. from `workspace.package.version` in the root
/// `Cargo.toml`. This is what stops the numeric `FILEVERSION` field and the
/// string block from drifting apart, which is exactly how 0.0.2 shipped
/// binaries that identified themselves as 0.0.1.0.
///
/// A three-part crate version becomes a four-part resource version by
/// appending the `0` Windows expects as the build field.
fn version_defines() -> [String; 2] {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let mut fields: Vec<String> = version.split('.').map(str::to_owned).collect();
    fields.resize(4, "0".to_owned());
    fields.truncate(4);
    [
        format!("FSW_VER_COMMAS={}", fields.join(",")),
        // Quoted here, so the macro expands to an RC string literal. This is
        // the form embed-resource documents for string defines.
        format!("FSW_VER_STR=\"{version}\""),
    ]
}

/// Compiles `app.rc` for a Windows target.
///
/// A rejected `app.rc` is fatal on a Windows host, because that is where the
/// shippable binaries are built and a silently skipped resource means an exe
/// with no icon and no version -- exactly the class of defect this file exists
/// to prevent. Cross-compiling from Linux (the CI type-check, the WSL dev
/// loop) only warns: the resource toolchain there may be absent or incomplete,
/// and nothing shippable comes out of that host anyway. `manifest_optional`
/// already treats "no resource compiler at all" as success on both.
fn compile_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if let Err(error) = embed_resource::compile("app.rc", version_defines()).manifest_optional() {
        eprintln!("app.rc could not be compiled: {error}");
        if cfg!(windows) {
            std::process::exit(1);
        }
    }
}
