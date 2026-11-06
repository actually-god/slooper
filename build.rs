//! v42 build script: compiles the C++ exception-firewall shim
//! (`src/ffiguard/ffiguard.cpp`) into a static lib linked against the crate.
//! Skipped on Windows: native VST hosting there has no yabridge — and the
//! gnullvm cross-check gate stays free of a target C++ toolchain.

fn main() {
    println!("cargo:rerun-if-changed=src/ffiguard/ffiguard.cpp");
    println!("cargo:rerun-if-changed=build.rs");
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
