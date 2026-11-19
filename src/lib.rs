//! slooper — a minimalist clip-based audio DAW for Linux (JACK / PipeWire).
//!
//! Library surface:
//! - [`audio`]: JACK/PipeWire-JACK backend (manager thread + realtime
//!   handler, transport, clip mixing, take recording).
//! - [`arrange`]: arrangement model — tracks → lanes → clips, placement,
//!   snapping, child-lane rules.
//! - [`assets`]: asset-rack folder store (scan, take saves, resolution).
//! - [`project`]: the `.slooper` save format and projects folder.
//! - [`midi`]: MIDI message parsing, trigger bindings, shortcut migration.
//! - [`quantize`]: metronome grid math, content quantize, silence trim.
//! - [`config`]: persistent settings (rack folders, projects dir, mixer,
//!   shortcuts, MIDI bindings).
//! - [`fx`]: runtime-compiled per-track effects (the effect framework).
//! - [`ports`]: enabled-device port filtering for the input/output pickers.
//! - [`ui`]: the egui front-end (used by the binary).

pub mod arrange;
pub mod assets;
pub mod audio;
pub mod audio_io;
pub mod automation;
pub mod backend;
pub mod claphost;
pub mod config;
pub mod crash;
pub mod ffiguard;
pub mod fx;
pub mod fxgui;
pub(crate) mod kit;
pub mod meter;
pub mod midi;
pub mod midiin;
pub mod midipat;
pub mod paths;
pub mod ports;
pub mod presetbar;
pub mod project;
pub mod quantize;
pub mod sample_edit;
pub mod samples808;
pub mod synth;
pub mod synth_api;
pub mod ui;
pub mod vstedit;
pub mod vsthost;

pub use midi::{MidiAction, MidiBind, MidiMsg};

/// Single source of truth for the internal series stamp (vNN). The public
/// release version is Cargo.toml's version — read through
/// `env!("CARGO_PKG_VERSION")` (header, host reports, crash reports).
/// This constant is the ONLY place the internal series number lives:
/// bump one line here per pass; the settings-panel footer reads it.
/// Never rendered in the header (pins v91_* hold that seam).
pub const INTERNAL_VERSION: &str = "v95";

/// Arm the vendor-teardown hush (v73/S8, shared body): SIGTRAP becomes
/// `_exit(0)`. Built for the production Unix path (below) and, as before,
/// for every test build on both OSes (`any(unix, test)` keeps Windows
/// release binaries free of it).
#[cfg(any(unix, test))]
fn arm_trap_handler() {
    const SIGTRAP: i32 = 5; // glibc/x86-64
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
        fn _exit(code: i32) -> !;
    }
    extern "C" fn exit_on_trap(_sig: i32) {
        unsafe { _exit(0) }
    }
    unsafe {
        signal(SIGTRAP, exit_on_trap as *const () as usize);
    }
}

/// Vendored plugins can trap at PROCESS EXIT: sfizz 1.2.3 leaks one
/// `Layer` per synth and its LeakDetector `__builtin_trap()`s from a
/// static destructor (sfizz-ui LeakDetector.h:60) — long after the
/// editor session ran perfectly, and long after our own last statement.
/// The user saw this as "slooper dies with Trace/breakpoint trap on
/// quit" whenever sfizz was resident (the S8 report class). Called by
/// `main` once the event loop has RETURNED: every slooper-side end
/// task (config saves on change, project autosave) is already done, so
/// the only thing left standing between us and a clean exit is vendor
/// static destruction. Mid-session SIGTRAPs stay fully fatal — the
/// handler is armed only after `eframe::run_native` returns. Built for
/// Windows TEST builds too (the suite-side helper below shares it; only
/// the Unix release binary leaves it out).
#[cfg(any(unix, test))]
pub fn hush_vendor_exit_trap() {
    static SET: std::sync::Once = std::sync::Once::new();
    SET.call_once(arm_trap_handler);
}

/// Test helper: the same hush, armed from every test-side plugin open
/// (the suite's own process exit would otherwise SIGTRAP after
/// "test result: ok", an outcome a static-destruction order lottery).
/// Mid-run traps after results printed still surface: without a
/// `test result:` line the chunk protocol FAILS the run.
#[cfg(test)]
pub fn test_hush_vendor_exit_trap() {
    hush_vendor_exit_trap();
}

/// Test helper: the REAL account home, immune to the `HOME` swaps that
/// sandboxed plugin tests perform mid-suite. Resolved from the passwd
/// database (uid-based), never from the environment.
#[cfg(test)]
pub fn test_real_home() -> String {
    use std::sync::OnceLock;
    static HOME: OnceLock<String> = OnceLock::new();
    HOME.get_or_init(|| {
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok());
        if let Some(u) = uid {
            if let Ok(pw) = std::fs::read_to_string("/etc/passwd") {
                for line in pw.lines() {
                    let f: Vec<&str> = line.split(':').collect();
                    if f.len() >= 6 && f[2].parse::<u32>() == Ok(u) {
                        return f[5].to_string();
                    }
                }
            }
        }
        std::env::var("HOME").unwrap_or_default()
    })
    .clone()
}
