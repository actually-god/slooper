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
pub mod config;
pub mod fx;
pub mod fxgui;
pub mod midi;
pub mod midipat;
pub mod ports;
pub mod project;
pub mod quantize;
pub mod samples808;
pub mod synth;
pub mod synth_api;
pub mod ui;
pub mod vstedit;
pub mod vsthost;

pub use midi::{MidiAction, MidiBind, MidiMsg};

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
