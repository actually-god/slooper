//! v42 build script: compiles the C++ exception-firewall shim
//! (`src/ffiguard/ffiguard.cpp`) into a static lib linked against the crate.
//! Skipped on Windows: native VST hosting there has no yabridge — and the
//! gnullvm cross-check gate stays free of a target C++ toolchain.

fn main() {
    println!("cargo:rerun-if-changed=src/ffiguard/ffiguard.cpp");
    println!("cargo:rerun-if-changed=build.rs");
    // W7: bake the path of the slooper executable THIS build produces so
    // unit tests can re-exec it as a probe child (the test-harness binary
    // cannot re-exec itself — libtest would reject `--slooper-probe`).
    // Build scripts get CARGO_TARGET_DIR; the fallback is cargo's default.
    // EXE suffix from the TARGET cfg, not the build script's host consts.
    let target = std::env::var("CARGO_TARGET_DIR")
        .or_else(|_| std::env::var("CARGO_MANIFEST_DIR").map(|m| format!("{m}/target")))
        .unwrap_or_else(|_| "target".to_string());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let sfx = if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        ".exe"
    } else {
        ""
    };
    println!(
        "cargo:rustc-env=SLOOPER_EXE_PATH={target}/{profile}/slooper{sfx}"
    );
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .file("src/ffiguard/ffiguard.cpp")
        .opt_level(2)
        .warnings(false)
        .compile("slooper_ffiguard");
}
