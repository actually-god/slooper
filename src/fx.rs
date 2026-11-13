//! slooper effect framework: runtime-compiled Rust effects per loop.
//!
//! Every `*.rs` file in the effects folder is a self-contained effect script
//! (contract: [`effect_api.rs`], embedded below and also written next to the
//! scripts as `effect_api.rs`, which the scripts `include!`). The registry
//! enumerates the folder, compiles each script with `rustc --crate-type
//! cdylib` into a `.build/` cache (recompiling only when the source is newer
//! than its artifact), loads the result with libloading and validates the
//! exported `slooper_effect_info` description.
//!
//! Instances ([`Instance`]) are created on the GUI thread, ref-counted with
//! Arc and handed to the audio engine through `Cmd::SetTrackEffects`. The
//! realtime thread only clones Arcs and calls the C-ABI `process` callback;
//! slider values ride on per-instance atomics bumped through a generation
//! counter, so live param edits never take a lock on the audio path.

use std::collections::hash_map::DefaultHasher;
use std::ffi::{CStr, OsStr};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

// The C-ABI contract. Compiled twice: once into slooper (this include)
// and once into every effect script (via its `include!("effect_api.rs")`),
// so both sides agree on the layout by construction.
mod api {
    #![allow(dead_code)]
    include!("effect_api.rs");
}
pub use api::{
    EffectGuiV1, EffectInfoV1, EffectInfoV2, EffectInfoV3, EffectInfoV4, EffectParamV1, GuiApiV1,
    GuiFrameV1, ParamMetaV1, SyncInfoV1, SLOOPER_EFFECT_ABI, SLOOPER_EFFECT_ABI_V2,
    SLOOPER_EFFECT_ABI_V3, SLOOPER_EFFECT_ABI_V4, SLOOPER_EFFECT_GUI_ABI, SLOOPER_MAX_PARAMS,
    SLOOPER_SYNC_ABI, SLOOPER_UNIT_DB, SLOOPER_UNIT_HZ, SLOOPER_UNIT_MS,
};

/// Param count cap mirrors the ABI (RT param snapshots are fixed-size).
pub const MAX_PARAMS: usize = SLOOPER_MAX_PARAMS;
/// Effects per loop chain (bounds the RT snapshot arrays).
pub const MAX_FX_PER_TRACK: usize = 8;

/// ABI version this build speaks.
pub const ABI_VERSION: u32 = SLOOPER_EFFECT_ABI;
/// v32 R5: sidechain-capable ABI generation this build also accepts.
pub const ABI_VERSION_V2: u32 = SLOOPER_EFFECT_ABI_V2;
/// Tier-3 R: stereo-pair ABI generation this build also accepts.
pub const ABI_VERSION_V3: u32 = SLOOPER_EFFECT_ABI_V3;
/// Tier-6 R: transport-sync + param-metadata ABI generation.
pub const ABI_VERSION_V4: u32 = SLOOPER_EFFECT_ABI_V4;
/// Child-GUI ABI version this build speaks.
pub const GUI_ABI_VERSION: u32 = SLOOPER_EFFECT_GUI_ABI;

impl SyncInfoV1 {
    /// Frozen pre-transport snapshot (the RT seed before the first block;
    /// never pushed — it exists so `Proc` can own a plain value).
    pub fn idle() -> Self {
        Self {
            abi: SLOOPER_SYNC_ABI,
            playing: 0,
            sample_rate: 0.0,
            bpm: 0.0,
            beats_per_bar: 4.0,
            pos_samples: 0,
            pos_beats: 0.0,
        }
    }
    /// One block push. `pos_samples` is the block/pass start on the
    /// timeline; beats derive from it exactly as documented in the ABI.
    pub fn block(
        pos_samples: i64,
        playing: bool,
        bpm: f64,
        beats_per_bar: f64,
        sample_rate: f64,
    ) -> Self {
        let pos_beats = if sample_rate > 0.0 && bpm > 0.0 && pos_samples >= 0 {
            pos_samples as f64 * bpm / (60.0 * sample_rate)
        } else {
            0.0
        };
        Self {
            abi: SLOOPER_SYNC_ABI,
            playing: playing as u32,
            sample_rate,
            bpm,
            beats_per_bar: beats_per_bar.max(1.0),
            pos_samples: pos_samples.max(0),
            pos_beats,
        }
    }
}

/// The generated shared-contract file every script includes.
pub const API_FILE: &str = "effect_api.rs";
/// The contract text (single source of truth: src/effect_api.rs).
pub const API_SRC: &str = include_str!("effect_api.rs");
/// The documented starter effect, offered by the "Add template" button.
pub const TEMPLATE_SRC: &str = include_str!("../effects/template.rs");
/// File name the template is written under.
pub const TEMPLATE_FILE: &str = "template.rs";
/// Bundled stock + example effects (parametric EQ with a response graph,
/// delay+reverb with an animated canvas, the v19 compressor and chorus).
/// Files already present are content-compared and updated in place (with a
/// `.bak`) on every release; files missing from an existing folder are added
/// — these names are ours by construction, so shipping a new stock effect
/// means every install gets it, not just fresh ones.
pub const EXAMPLES: &[(&str, &str)] = &[
    (
        "parametric_eq.rs",
        include_str!("../effects/parametric_eq.rs"),
    ),
    ("space_echo.rs", include_str!("../effects/space_echo.rs")),
    ("compressor.rs", include_str!("../effects/compressor.rs")),
    ("chorus.rs", include_str!("../effects/chorus.rs")),
    (
        "sidechain_comp.rs",
        include_str!("../effects/sidechain_comp.rs"),
    ),
    // Native suite tier 1 (docs/EFFECT-SUITE.md), added 2026-11-05.
    ("overdrive.rs", include_str!("../effects/overdrive.rs")),
    ("waveshaper.rs", include_str!("../effects/waveshaper.rs")),
    ("crush.rs", include_str!("../effects/crush.rs")),
    ("enhancer.rs", include_str!("../effects/enhancer.rs")),
    ("limiter.rs", include_str!("../effects/limiter.rs")),
    ("multiband.rs", include_str!("../effects/multiband.rs")),
    ("vfilter.rs", include_str!("../effects/vfilter.rs")),
    ("phaser.rs", include_str!("../effects/phaser.rs")),
    ("flanger.rs", include_str!("../effects/flanger.rs")),
    ("gater.rs", include_str!("../effects/gater.rs")),
    ("delay.rs", include_str!("../effects/delay.rs")),
    ("shifter.rs", include_str!("../effects/shifter.rs")),
    ("transient.rs", include_str!("../effects/transient.rs")),
    ("pitchshft.rs", include_str!("../effects/pitchshft.rs")),
    ("spread.rs", include_str!("../effects/spread.rs")),
    ("autopan.rs", include_str!("../effects/autopan.rs")),
    ("vocoder.rs", include_str!("../effects/vocoder.rs")),
    ("convolver.rs", include_str!("../effects/convolver.rs")),
];

/// Symbol every effect library must export (NUL-terminated: libloading 0.8
/// takes the raw name bytes).
const ENTRY: &[u8] = b"slooper_effect_info\0";
/// v32 R5: optional symbol exporting the sidechain-capable v2 vtable.
const ENTRY_V2: &[u8] = b"slooper_effect_info_v2\0";
/// Tier-3: optional symbol exporting the stereo-pair-capable v3 vtable.
const ENTRY_V3: &[u8] = b"slooper_effect_info_v3\0";
/// Tier-6: optional symbol exporting the sync/metadata-capable v4 vtable.
const ENTRY_V4: &[u8] = b"slooper_effect_info_v4\0";
/// Optional symbol exporting the custom child-GUI vtable. Absent = the
/// effect gets the auto slider window; the audio ABI does not care.
const GUI_ENTRY: &[u8] = b"slooper_effect_gui\0";

// ---------------------------------------------------------------------------
// Directory handling
// ---------------------------------------------------------------------------

/// The folder effects live in when nothing was configured: next to the
/// installed package. Exes under a `bin/` directory use the FHS-style
/// sibling (`../share/slooper/effects` → `/usr/share/...` or
/// `~/.local/share/...`); dev builds get `effects/` beside the binary
/// (repo checkout). Falls back to `$XDG_DATA_HOME/slooper/effects` when
/// the first choice cannot be created.
pub fn default_effects_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let under_bin = exe_dir.file_name() == Some(std::ffi::OsStr::new("bin"));
    let primary = if under_bin {
        exe_dir
            .parent()
            .map(|root| root.join("share").join("slooper").join("effects"))
            .unwrap_or_else(|| exe_dir.join("effects"))
    } else {
        exe_dir.join("effects")
    };
    if std::fs::create_dir_all(&primary).is_ok() {
        return primary;
    }
    default_effects_fallback_dir()
}

/// The `bin/`-sibling rule's fallback: `paths::data_dir()/effects`
/// (`$XDG_DATA_HOME/slooper/effects` or `~/.local/share/slooper/effects`
/// on Linux, `%LOCALAPPDATA%\slooper\effects` on Windows — WINDOWS-PORT
/// W2). Created best-effort, returned even when creation fails (a missing
/// folder reads as "no bundled effects yet", same as before).
pub fn default_effects_fallback_dir() -> PathBuf {
    let fallback = crate::paths::data_dir().join("effects");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// Resolve the effects folder and make it usable: create it when missing,
/// refresh the generated `effect_api.rs` when it differs (silently skipping
/// read-only folders that already ship a matching copy). `SLOOPER_EFFECTS_DIR`
/// overrides everything (used by tests and for quick experimentation).
pub fn prepare_effects_dir(cfg_dir: Option<&str>) -> PathBuf {
    let env_dir = std::env::var("SLOOPER_EFFECTS_DIR")
        .ok()
        .filter(|p| !p.trim().is_empty());
    let dir = match env_dir.as_deref().or(cfg_dir.map(str::trim)) {
        Some(d) if !d.is_empty() => {
            let p = PathBuf::from(d);
            let _ = std::fs::create_dir_all(&p);
            p
        }
        _ => default_effects_dir(),
    };
    seed_api(&dir);
    // The user-preset folder lives beside the scripts and is (re)created at
    // every launch — plugins find it ready-made the first time they save.
    let _ = ensure_user_presets_dir(&dir);
    // v0.13: factory presets are part of that folder now — write the ones
    // the user does not have (existing files are never touched).
    seed_builtin_presets(&dir);
    dir
}

// ---------------------------------------------------------------------------
// User presets
// ---------------------------------------------------------------------------
//
// Users save the current settings of any effect as a named preset. Files
// live in `<effects>/user presets/` and are named
// `{plugin stem}-{preset name}.json` — the stem prefix is the ownership
// marker: a plugin lists and reads ONLY files starting with its own
// `{stem}-`, so one folder serves every effect. Content is flat JSON:
// `{"plugin": stem, "name": preset, "params": {"label": value, …}}`.
//
// Since v0.13 the BUILT-IN presets live in this same folder too: they are
// ordinary JSON files seeded once at first launch
// (`seed_builtin_presets`), so one list, one loader and one delete path
// cover factory and user presets alike. Writers' guide: `docs/USAGE.md`
// (section "Presets").

/// Folder name (fixed by convention) inside the effects location.
pub const USER_PRESET_DIR: &str = "user presets";

pub fn user_presets_dir(fx_dir: &Path) -> PathBuf {
    fx_dir.join(USER_PRESET_DIR)
}

/// Create the user-preset folder if missing and return its path.
pub fn ensure_user_presets_dir(fx_dir: &Path) -> std::io::Result<PathBuf> {
    let d = user_presets_dir(fx_dir);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Filesystem-safe preset name: keep letters/digits/space/dot/dash/underscore
/// (UTF-8 aware so accented names survive), collapse runs of rejected chars
/// to a single dash, trim. Empty result becomes "preset".
pub fn sanitize_preset_name(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_alphanumeric() || ch == ' ' || ch == '.' || ch == '_' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = true;
        }
    }
    while out.ends_with(' ') || out.ends_with('-') || out.ends_with('.') {
        out.pop();
    }
    // No hidden files, no confusion with relative paths: a preset name never
    // starts with a dot or a separator.
    while out.starts_with(' ') || out.starts_with('-') || out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() {
        out.push_str("preset");
    }
    out
}

/// The preset file for (stem, name) — exact round-trip of `list_user_presets`.
pub fn user_preset_path(fx_dir: &Path, stem: &str, name: &str) -> PathBuf {
    user_presets_dir(fx_dir).join(format!("{stem}-{}.json", sanitize_preset_name(name)))
}

/// Preset names owned by `stem`, sorted. Only `{stem}-{name}.json` files.
pub fn list_user_presets(fx_dir: &Path, stem: &str) -> Vec<String> {
    let mut out = Vec::new();
    let prefix = format!("{stem}-");
    if let Ok(rd) = std::fs::read_dir(user_presets_dir(fx_dir)) {
        for e in rd.flatten() {
            let Some(n) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if let Some(rest) = n.strip_prefix(&prefix) {
                if let Some(name) = rest.strip_suffix(".json") {
                    if !name.is_empty() {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Built-in ("factory") presets shipped with the program — SINCE v0.13
/// THESE ARE SEEDED INTO THE USER PRESET FOLDER as ordinary preset files
/// (`seed_builtin_presets`), so they appear in the same list as the user's
/// own presets and load/save/delete exactly like them. This table is the
/// seed source only; treat the seeded JSON files as the live copies.
///
/// Shape: `(preset name, [(param label, value)])` — labels must match the
/// effect script's `EffectParamV1` labels. Unknown labels are skipped by
/// the loader, so an effect re-compiled with renamed params can never be
/// bricked by a stale preset. Values are clamped to each param's declared
/// range on load. The full format + rules for writing your own:
/// `docs/USAGE.md` (section "Presets").
///
/// Seeding rule (same ownership discipline as effect scripts): a preset
/// file is written ONLY if absent — user edits always survive, a deleted
/// factory preset stays deleted, and a hand-written file that merely
/// shares a name is never touched.
pub fn builtin_presets(stem: &str) -> Vec<(&'static str, Vec<(&'static str, f32)>)> {
    // EQ band order: type 0 off, 1 low shelf, 2 high shelf, 3 peak,
    // 4 low pass, 5 high pass, 6 notch, 7 band pass.
    fn eq(b: &[[f32; 4]], mix: f32, out: f32) -> Vec<(&'static str, f32)> {
        let mut v = Vec::with_capacity(26);
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        for (i, band) in b.iter().enumerate() {
            let n = (i + 1).to_string();
            for (suf, val) in [("type", 0usize), ("freq", 1), ("gain", 2), ("Q", 3)] {
                let s = leak(format!("B{n} {suf}"));
                v.push((s, band[val]));
            }
        }
        v.push((leak("mix".into()), mix));
        v.push((leak("out".into()), out));
        v
    }
    // space_echo params in PARAMS order; labels are the script's:
    // time, feedback, tone, wobble, rate, decay, damp, mix, out, echo, verb,
    // width (tier 5 appended width — indices are sticky per script stem).
    fn se(v: [f32; 12]) -> Vec<(&'static str, f32)> {
        const L: [&str; 12] = [
            "time", "feedback", "tone", "wobble", "rate", "decay", "damp", "mix", "out", "echo",
            "verb", "width",
        ];
        L.iter().zip(v).map(|(l, x)| (leak_str(l), x)).collect()
    }
    fn leak_str(s: &str) -> &'static str {
        Box::leak(s.to_string().into_boxed_str())
    }
    match stem {
        // ---------------------------------------------------------------- EQ
        // Every preset sets ALL 26 params (6 bands x {type,freq,gain,Q} +
        // mix + out), so loading one is deterministic from any starting
        // point. Band types: 0 off, 1 low shelf, 2 high shelf, 3 peak,
        // 4 low pass, 5 high pass, 6 notch, 7 band pass.
        "parametric_eq" => vec![
            (
                // Old-radio bandwidth: high-pass at 300 Hz and a low-pass
                // at 3.5 kHz fence in everything, two presence peaks
                // (1.5/2.5 kHz) add the "talking-in-a-can" honk.
                "Radio filter",
                eq(
                    &[
                        [5.0, 300.0, 0.0, 0.7],
                        [3.0, 1500.0, 5.0, 1.1],
                        [3.0, 2500.0, 4.0, 1.1],
                        [4.0, 3500.0, 0.0, 0.71],
                        [0.0, 6000.0, 0.0, 1.1],
                        [0.0, 11000.0, 0.0, 0.71],
                    ],
                    1.0,
                    1.0,
                ),
            ),
            (
                // Venue curve for playback: warm shelf under 100 Hz, a
                // cut at 250 Hz to remove box, lift on the detail bands
                // (4k presence, 8k air) and a gentle high shelf.
                "Concert hall",
                eq(
                    &[
                        [1.0, 90.0, 3.0, 0.71],
                        [3.0, 250.0, -2.0, 0.9],
                        [3.0, 1000.0, 0.0, 1.0],
                        [3.0, 4000.0, 2.0, 1.0],
                        [3.0, 8000.0, 1.5, 1.2],
                        [2.0, 12000.0, 2.0, 0.71],
                    ],
                    1.0,
                    1.0,
                ),
            ),
            (
                // Voice chain: high-pass the rumble, -3 dB mud cut at
                // 200 Hz, two presence lifts (2.5/5 kHz) for intelligence,
                // slight de-esser at 8 kHz, low-pass at 90 Hz.
                "Podcast voice",
                eq(
                    &[
                        [5.0, 90.0, 0.0, 0.7],
                        [3.0, 200.0, -3.0, 1.2],
                        [3.0, 2500.0, 3.0, 1.3],
                        [3.0, 5000.0, 2.0, 1.4],
                        [3.0, 8000.0, -1.0, 1.2],
                        [0.0, 11000.0, 0.0, 0.71],
                    ],
                    1.0,
                    1.0,
                ),
            ),
            (
                // +6 dB low shelf at 120 Hz with a small upper-bass bump;
                // the mids stay honest, a light 10 kHz shelf keeps it from
                // going muddy.
                "Bass boost",
                eq(
                    &[
                        [1.0, 120.0, 6.0, 0.71],
                        [3.0, 250.0, 2.0, 1.0],
                        [0.0, 800.0, 0.0, 1.1],
                        [0.0, 2500.0, 0.0, 1.1],
                        [0.0, 6000.0, 0.0, 1.1],
                        [2.0, 10000.0, 1.0, 0.71],
                    ],
                    1.0,
                    1.0,
                ),
            ),
            (
                // Cracked cassette: low-passed at 150 Hz and high-passed
                // at 4 kHz (the shell), a +4 dB midrange honk at 1 kHz.
                "Lo-fi tape",
                eq(
                    &[
                        [5.0, 150.0, 0.0, 0.7],
                        [0.0, 250.0, 0.0, 1.1],
                        [3.0, 1000.0, 4.0, 1.0],
                        [4.0, 4000.0, 0.0, 0.71],
                        [0.0, 6000.0, 0.0, 1.1],
                        [0.0, 11000.0, 0.0, 0.71],
                    ],
                    1.0,
                    1.0,
                ),
            ),
            (
                // Air band only: +1.5 dB at 4 kHz, +4 dB at 9 kHz, +3 dB
                // shelf from 12 kHz; everything below untouched.
                "Treble sparkle",
                eq(
                    &[
                        [0.0, 80.0, 0.0, 0.71],
                        [0.0, 250.0, 0.0, 1.1],
                        [0.0, 800.0, 0.0, 1.1],
                        [3.0, 4000.0, 1.5, 1.0],
                        [3.0, 9000.0, 4.0, 1.0],
                        [2.0, 12000.0, 3.0, 0.71],
                    ],
                    1.0,
                    1.0,
                ),
            ),
        ],
        // ---------------------------------------------------- echo + reverb
        // Room presets run echo LOW and verb FULL so the FDN tail (not the
        // slap) carries the space; time/feedback shape the early slap,
        // decay/damp set tail length/colour, mix is the wet level.
        // (The effect's own canvas also draws these five as a preset row —
        // the script copy lives in effects/space_echo.rs `PRESETS`.)
        "space_echo" => vec![
            // The classic dub-style echo: 380 ms slap, moderate feedback,
            // no wobble; verb barely open. This is the old default sound.
            (
                "Echo",
                se([
                    0.38, 0.62, 3600.0, 0.0, 0.6, 0.84, 2800.0, 0.4, 1.0, 1.0, 0.5, 0.6,
                ]),
            ),
            // Tiny bright room: 70 ms first reflection, tight decay —
            // more "close walls" than an effect.
            (
                "Practice Hall",
                se([
                    0.07, 0.3, 8000.0, 0.05, 0.5, 0.72, 7500.0, 0.5, 0.95, 0.3, 1.0, 1.0,
                ]),
            ),
            // Medium hall, a touch of slap before the tail.
            (
                "Auditorium",
                se([
                    0.14, 0.38, 5200.0, 0.1, 0.35, 0.84, 4500.0, 0.55, 0.95, 0.25, 1.0, 1.0,
                ]),
            ),
            // The warm classic: 200 ms slap, longish damped tail.
            (
                "Opera House",
                se([
                    0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 1.0, 1.0,
                ]),
            ),
            // Vast and dark: slow slap, 0.95 feedback, heavy air damping —
            // the tail outlives everything.
            (
                "Cathedral",
                se([
                    0.32, 0.5, 2200.0, 0.25, 0.14, 0.95, 1500.0, 0.7, 0.9, 0.18, 1.0, 1.0,
                ]),
            ),
        ],
        _ => Vec::new(),
    }
}

/// Every stem that ships factory presets.
pub const BUILTIN_PRESET_STEMS: [&str; 2] = ["parametric_eq", "space_echo"];

/// Marker file (inside the user-preset folder) recording that the factory
/// presets have already been seeded once. Its presence means "this folder
/// belongs to the user now": deletions stay deleted and edits stay edited
/// across launches and upgrades. Delete it (or the folder) to restore the
/// starter set — that is the documented recovery, see `docs/PLUGINS.md`.
pub const FACTORY_SEED_MARKER: &str = ".factory-seeded";

/// Merge the factory presets into the USER preset folder (v0.13 — presets
/// are not a separate system any more, they are ordinary preset files).
///
/// The whole set is seeded ONCE, into a folder without the marker; from
/// then on the folder is user territory: files are never rewritten, a
/// deleted factory preset stays deleted, and foreign files are untouched.
/// Returns how many files were created (0 whenever the marker is present).
pub fn seed_builtin_presets(fx_dir: &Path) -> usize {
    let dir = match ensure_user_presets_dir(fx_dir) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let marker = dir.join(FACTORY_SEED_MARKER);
    if marker.exists() {
        return 0;
    }
    let mut written = 0usize;
    for stem in BUILTIN_PRESET_STEMS {
        for (name, pairs) in builtin_presets(stem) {
            let path = user_preset_path(fx_dir, stem, name);
            if path.exists() {
                continue; // a hand-made file under that name is the user's
            }
            let params: Vec<(String, f32)> =
                pairs.into_iter().map(|(l, v)| (l.to_string(), v)).collect();
            if let Ok(json) = preset_to_json(stem, name, &params) {
                if std::fs::write(dir.join(path.file_name().unwrap()), json).is_ok() {
                    written += 1;
                }
            }
        }
    }
    let _ = std::fs::write(
        &marker,
        "factory presets seeded once (slooper v0.13+). This file marks the\n\
         preset folder as user-owned: nothing rewrites or re-adds files any\n\
         more. Delete it (or the folder) and restart to restore the starter\n\
         set. Format of the preset files: see docs/PLUGINS.md (Presets).\n",
    );
    written
}

/// Delete a preset. Missing file counts as success (idempotent).
pub fn delete_user_preset(fx_dir: &Path, stem: &str, name: &str) -> Result<(), String> {
    let p = user_preset_path(fx_dir, stem, name);
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not delete preset: {e}")),
    }
}

/// Minimal flat JSON writer for a preset. Labels come from effect metadata;
/// still escape the mandatory characters, and reject non-finite values —
/// JSON has no NaN and a poisoned file would load garbage later.
pub fn preset_to_json(stem: &str, name: &str, params: &[(String, f32)]) -> Result<String, String> {
    let mut s = format!(
        "{{\n  \"plugin\": \"{}\",\n  \"name\": \"{}\",\n  \"params\": {{\n",
        json_escape(stem),
        json_escape(name)
    );
    for (i, (label, v)) in params.iter().enumerate() {
        if !v.is_finite() {
            return Err(format!("parameter '{label}' is not a number"));
        }
        s.push_str(&format!(
            "    \"{}\": {}{}",
            json_escape(label),
            f64::from(*v),
            if i + 1 < params.len() { "," } else { "" }
        ));
        s.push('\n');
    }
    s.push_str("  }\n}\n");
    Ok(s)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Pull every `"label": number` pair out of a preset file. Deliberately
/// lenient (string values like "plugin"/"name" simply don't parse as
/// numbers and are skipped): a hand-tweaked preset should still load its
/// numbers. Garbage that yields no pairs at all is still an error.
pub fn preset_from_json(text: &str) -> Result<Vec<(String, f64)>, String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        // Read one string literal starting at i (this position is a key
        // candidate; string VALUES fall through the colon test below).
        let mut j = i + 1;
        let mut key = String::new();
        let mut closed = false;
        while j < b.len() {
            if b[j] == b'\\' && j + 1 < b.len() {
                key.push(text[j + 1..].chars().next().unwrap_or(' '));
                j += 2;
            } else if b[j] == b'"' {
                closed = true;
                break;
            } else {
                let ch = text[j..].chars().next().unwrap();
                key.push(ch);
                j += ch.len_utf8();
            }
        }
        if !closed {
            break;
        }
        i = j + 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b':' {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && matches!(b[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
            i += 1;
        }
        if let Ok(v) = text[start..i].parse::<f64>() {
            out.push((key, v));
        }
    }
    if out.is_empty() {
        return Err("no numeric parameters found in preset file".into());
    }
    Ok(out)
}

/// Write `effect_api.rs` next to the scripts when absent or stale.
fn seed_api(dir: &Path) {
    let p = dir.join(API_FILE);
    match std::fs::read_to_string(&p) {
        Ok(cur) if cur == API_SRC => {}
        _ => {
            let _ = std::fs::write(&p, API_SRC);
        }
    }
    // The template is written once for a brand-new folder.
    let fresh = !has_scripts(dir);
    let t = dir.join(TEMPLATE_FILE);
    if fresh && !t.exists() {
        let _ = std::fs::write(&t, TEMPLATE_SRC);
    }
    // Bundled examples are CONTENT-COMPARED on every app release, not just
    // seeded once: the previous write-only-if-absent rule meant plugin fixes
    // shipped in later releases never reached an existing install — users ran
    // the day-one scripts forever ("EQ still has all bugs" while the repo
    // copy was fixed). A file about to change is copied to `<name>.rs.bak`
    // (single generation) first, so nothing is lost; a user customization
    // that must survive updates belongs under its own file name. Files that
    // are missing are ADDED even to an existing folder: these names are the
    // stock set, owned by the app by construction, so a new stock effect
    // reaches every install the same way bundled synths always did.
    for (name, src) in EXAMPLES {
        let p = dir.join(name);
        match std::fs::read_to_string(&p) {
            Ok(cur) if cur == *src => {}
            Ok(cur) => {
                let _ = std::fs::write(p.with_extension("rs.bak"), cur.as_bytes());
                let _ = std::fs::write(&p, src);
            }
            Err(_) => {
                let _ = std::fs::write(&p, src);
            }
        }
    }
}

/// Any effect script in the folder (ignoring the API file and drafts)?
fn has_scripts(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("rs") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem != API_FILE.strip_suffix(".rs").unwrap_or(API_FILE)
            && !stem.starts_with('.')
            && !stem.starts_with('_')
        {
            return true;
        }
    }
    false
}

/// True when `rustc` can be launched (effects cannot compile without it).
pub fn rustc_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        Command::new("rustc")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// What to tell the user when `rustc` cannot be launched (W3). Linux
/// keeps the classic rustup advice; on Windows plain rustup is NOT
/// enough — the default `x86_64-pc-windows-msvc` toolchain needs the
/// Microsoft C linker/headers, so the hint names MSVC Build Tools.
pub fn rustc_install_hint() -> &'static str {
    if cfg!(windows) {
        "install Rust via rustup AND the MSVC Build Tools (Visual Studio \
         Build Tools > \"Desktop development with C++\"; rustc needs the MSVC \
         linker to build cdylibs)"
    } else {
        "install Rust (rustup)"
    }
}

// ---------------------------------------------------------------------------
// Descriptors & instances
// ---------------------------------------------------------------------------

/// Safe copy of one slider description.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamDesc {
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    /// Tier-6 ABI v4: `SLOOPER_UNIT_*` display unit (0 for v1..v3 scripts
    /// and for v4 params with no declared unit).
    pub unit: u32,
    /// Tier-6: > 0 → the auto window snaps to `min + k*step` (ignored
    /// when `labels` is non-empty, which implies its own grid).
    pub step: f32,
    /// Tier-6: enum labels for equally spaced values min..max (empty =
    /// plain slider). The auto window shows the current label instead of
    /// a number.
    pub labels: Vec<String>,
}

/// Safe copy of the effect description shown in the GUI.
#[derive(Clone, Debug)]
pub struct EffectDesc {
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub params: Vec<ParamDesc>,
    /// v32 R5: loaded through the ABI-v2 entry — the host feeds it the
    /// track's sidechain bus (mixer shows an "(SC)" tag on these).
    pub sidechain: bool,
    /// Tier-3: loaded through the ABI-v3 entry — the host feeds it the
    /// chain's stereo pair and carries its output pair to the track pan
    /// (mixer shows an "(ST)" tag on these).
    pub stereo: bool,
}

/// The three live callbacks, copied out of the library (plain fn pointers —
/// Send + Sync by construction).
#[derive(Clone, Copy)]
struct EffectFns {
    create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    destroy: extern "C" fn(state: *mut core::ffi::c_void),
    process: extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32,
    /// ABI v2 sidechain entry (None for v1 scripts — they never see `sc`).
    process_sc: Option<
        extern "C" fn(
            state: *mut core::ffi::c_void,
            input: f32,
            params: *const f32,
            sc: f32,
        ) -> f32,
    >,
    /// Tier-3 ABI v3 stereo-pair entry (None for v1/v2 scripts — they are
    /// fed the mid sample through `process_sc`/`process` instead).
    process_st: Option<
        extern "C" fn(
            state: *mut core::ffi::c_void,
            l: f32,
            r: f32,
            params: *const f32,
            sc: f32,
            out_r: *mut f32,
        ) -> f32,
    >,
    /// Tier-6 ABI v4 block-rate transport push (None for v1..v3 scripts).
    set_sync: Option<extern "C" fn(state: *mut core::ffi::c_void, sync: *const SyncInfoV1)>,
    /// Tier-6 display metadata table (NULL for v1..v3 scripts). Borrows
    /// the script's read-only image, which the leaked LibHandle keeps
    /// mapped for the process lifetime.
    param_meta: *const ParamMetaV1,
}

/// Safe typed view of one param's display metadata (tier 6). Produced by
/// [`Instance::param_meta`]; `labels` borrows the script's read-only image.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamMetaView<'a> {
    /// `SLOOPER_UNIT_*` (0 = none / plain float).
    pub unit: u32,
    /// Non-zero: slider snaps to multiples of `step` (in raw units).
    pub step: f32,
    /// Non-empty: the param is an enumerated choice list.
    pub labels: Vec<&'a str>,
}

/// Owning wrapper for a loaded library (raw handle inside ⇒ manual impls;
/// only ever touched while building instances or dropping them).
struct LibHandle(std::mem::ManuallyDrop<libloading::Library>);
// Intentional leak: dropping libloading::Library calls dlclose, and closing
// a plugin that embeds its own Rust std runtime frees that runtime's TLS
// entries and lazy pool bookkeeping while the process's own teardown
// destructors are still running — dangling references land as
// `corrupted size vs. prev_size` heap aborts at shutdown (reproduced in the
// headless EQ test). Effects are few and small; keep them mapped for good.
unsafe impl Send for LibHandle {}
unsafe impl Sync for LibHandle {}

/// One live effect instance: created per (loop, chain slot), destroyed when
/// the last Arc drops. Slider values are atomics so the GUI can ride them
/// while the audio thread reads generation-stamped snapshots of them.
pub struct Instance {
    fns: EffectFns,
    state: *mut core::ffi::c_void,
    sample_rate: f64,
    n_params: usize,
    params: Box<[AtomicU32; MAX_PARAMS]>,
    param_gen: AtomicU64,
    _lib: Arc<LibHandle>,
}
// `state` is only dereferenced by the audio thread (or the render thread on
// a private instance); params are atomics; the library Arc keeps the code
// mapped for as long as any thread can call into it.
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Instance {
    /// Create via the effect's `create`. `values` seeds the sliders
    /// (missing entries fall back to the defaults in `desc`).
    fn new(
        fns: EffectFns,
        lib: Arc<LibHandle>,
        desc: &EffectDesc,
        sample_rate: f64,
        values: &[f32],
    ) -> Option<Arc<Self>> {
        let state = (fns.create)(sample_rate);
        if state.is_null() {
            return None;
        }
        let params = core::array::from_fn(|k| {
            let d = desc.params.get(k);
            let v = values
                .get(k)
                .copied()
                .or(d.map(|p| p.default))
                .unwrap_or(0.0);
            // Clamp to the DECLARED range, never at 0: bipolar params
            // (EQ band gain -24..+24 dB) must survive loading a saved
            // negative value — the same trap `set_param` already fixed.
            let v = match d {
                Some(p) => v.clamp(p.min, p.max),
                None => v,
            };
            AtomicU32::new(v.to_bits())
        });
        Some(Arc::new(Self {
            fns,
            state,
            sample_rate,
            n_params: desc.params.len().min(MAX_PARAMS),
            params: Box::new(params),
            // Start at 1 so an RT snapshot that last saw 0 always picks the
            // seeded values up on its first block.
            param_gen: AtomicU64::new(1),
            _lib: lib,
        }))
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Raw pointer to the effect's per-instance state (what `create`
    /// returned). Only ever handed to a live child GUI's `draw` while the
    /// editor holds an `Arc<Instance>` clone — the instance (and its
    /// library) cannot be destroyed underneath the GUI.
    pub fn state_ptr(&self) -> *mut core::ffi::c_void {
        self.state
    }

    pub fn n_params(&self) -> usize {
        self.n_params
    }

    pub fn param(&self, k: usize) -> f32 {
        self.params
            .get(k)
            .map(|p| f32::from_bits(p.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    /// Ride one slider (GUI thread; audio picks it up next block).
    /// Values keep their sign — many params are bipolar (EQ band gain runs
    /// -24..+24 dB) — only non-finite writes are ignored. Range clamping is
    /// the caller's job (fxgui clamps by the declared param bounds).
    pub fn set_param(&self, k: usize, v: f32) {
        if let Some(p) = self.params.get(k) {
            if !v.is_finite() {
                return;
            }
            p.store(v.to_bits(), Ordering::Relaxed);
            self.param_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn param_gen(&self) -> u64 {
        self.param_gen.load(Ordering::Relaxed)
    }

    /// Copy the slider values into `out` (used by the audio thread on
    /// generation change and by the render path).
    pub fn read_params(&self, out: &mut [f32; MAX_PARAMS]) {
        for (o, p) in out.iter_mut().zip(self.params.iter()) {
            *o = f32::from_bits(p.load(Ordering::Relaxed));
        }
    }

    /// Process one sample. ONLY from the audio thread (or with a private
    /// instance during offline render); `params` must hold current values.
    ///
    /// # Safety
    /// The caller must be the single thread that owns this instance's state
    /// (the RT thread for live chains, the render thread for private
    /// clones) and `params` must point at `MAX_PARAMS` live slider values.
    #[inline]
    pub unsafe fn call_sample(&self, input: f32, params: &[f32; MAX_PARAMS]) -> f32 {
        (self.fns.process)(self.state, input, params.as_ptr())
    }

    /// v32 R5: `call_sample` with this track's sidechain sample. v1
    /// scripts (no v2 entry) keep receiving `process` exactly as before.
    ///
    /// # Safety
    /// FFI into a loaded scripted-effect library: `self.state`/`self.fns`
    /// must come from a successfully opened effect whose owner lock is
    /// held by the caller, and `params` is the caller's frozen param set.
    #[inline]
    pub unsafe fn call_sample_sc(&self, input: f32, params: &[f32; MAX_PARAMS], sc: f32) -> f32 {
        match self.fns.process_sc {
            Some(f) => f(self.state, input, params.as_ptr(), sc),
            None => (self.fns.process)(self.state, input, params.as_ptr()),
        }
    }

    /// Tier-3: true when this script exports the v3 stereo entry (the
    /// audio path may feed it a real L/R pair; every other slot is fed
    /// the mid sample and replaces both sides with its output).
    #[inline]
    pub fn is_stereo(&self) -> bool {
        self.fns.process_st.is_some()
    }

    /// Tier-6: true when this script wants block-rate transport pushes
    /// (ABI v4 with a `set_sync` export).
    #[inline]
    pub fn wants_sync(&self) -> bool {
        self.fns.set_sync.is_some()
    }

    /// Tier-6: push the block-start transport snapshot. Call ONCE per
    /// audio block per instance, BEFORE any process call of that block,
    /// from the thread that owns the instance (RT for live chains, the
    /// render thread for private offline copies). No-op for v1..v3
    /// scripts. The sync pointer is read synchronously — a stack value
    /// at the call site is fine.
    #[inline]
    pub fn push_sync(&self, sync: &SyncInfoV1) {
        if let Some(f) = self.fns.set_sync {
            f(self.state, sync as *const SyncInfoV1);
        }
    }

    /// Display meta for param `k` (tier 6): unit enum, log flag, snap
    /// step, and enum labels. An empty view (unit 0, no labels) means
    /// the script declared no meta — plain linear rendering. Label
    /// strings borrow script-owned statics and live exactly as long as
    /// this instance.
    pub fn param_meta(&self, k: usize) -> ParamMetaView<'_> {
        let mut v = ParamMetaView {
            unit: 0,
            step: 0.0,
            labels: Vec::new(),
        };
        if self.fns.param_meta.is_null() || k >= self.n_params {
            return v;
        }
        let m = unsafe { &*self.fns.param_meta.add(k) };
        v.unit = m.unit;
        v.step = m.step;
        if !m.labels.is_null() && m.n_labels > 0 {
            v.labels = (0..m.n_labels as usize)
                .map(|i| unsafe {
                    let p = *m.labels.add(i);
                    if p.is_null() {
                        ""
                    } else {
                        // SAFETY: label literals live in the script's
                        // read-only image for the instance's whole life.
                        std::ffi::CStr::from_ptr(p).to_str().unwrap_or("")
                    }
                })
                .collect();
        }
        v
    }

    /// Display string for param `k` honouring meta: enums -> the chosen
    /// label; declared units -> engineering value; plain -> 3 decimals.
    /// Declared dB ranges are read as absolute dB (scripts map to gain
    /// internally), Hz/ms ranges as physical values.
    pub fn fmt_param(&self, k: usize, norm: f32, lo: f32, hi: f32) -> String {
        let m = self.param_meta(k);
        if !m.labels.is_empty() {
            let t = ((norm - lo) / (hi - lo).max(1e-9)).clamp(0.0, 1.0);
            let i = (t * m.labels.len() as f32) as usize;
            return m.labels[i.min(m.labels.len() - 1)].to_string();
        }
        let v = lo + norm * (hi - lo);
        match m.unit {
            SLOOPER_UNIT_HZ => {
                if v >= 1000.0 {
                    format!("{:.2} kHz", v / 1000.0)
                } else {
                    format!("{:.1} Hz", v)
                }
            }
            SLOOPER_UNIT_DB => format!("{:.1} dB", v),
            SLOOPER_UNIT_MS => format!("{:.1} ms", v),
            _ => format!("{:.3}", v),
        }
    }

    /// `call_sample_sc` for the stereo chain bus. v3 scripts receive the
    /// real pair; v1/v2 scripts are fed the mid exactly once (mono slot
    /// semantics) and their output stands for both sides.
    ///
    /// # Safety
    /// As `call_sample_sc`, plus: this thread owns the instance state.
    #[inline]
    pub unsafe fn call_sample_st(
        &self,
        l: f32,
        r: f32,
        params: &[f32; MAX_PARAMS],
        sc: f32,
    ) -> (f32, f32) {
        match self.fns.process_st {
            Some(f) => {
                let mut or = 0.0f32;
                let ol = f(self.state, l, r, params.as_ptr(), sc, &mut or);
                (ol, or)
            }
            None => {
                let y = match self.fns.process_sc {
                    Some(f) => f(self.state, (l + r) * 0.5, params.as_ptr(), sc),
                    None => (self.fns.process)(self.state, (l + r) * 0.5, params.as_ptr()),
                };
                (y, y)
            }
        }
    }

    /// A brand-new instance of the same effect at the same rate, seeded with
    /// this instance's current slider values. For offline renders, which
    /// must never share mutable state with the live audio thread.
    pub fn spawn_fresh(&self, desc: &EffectDesc) -> Option<Arc<Self>> {
        Self::new(
            self.fns,
            self._lib.clone(),
            desc,
            self.sample_rate,
            &self.snapshot_params(),
        )
    }

    /// Process one sample with this instance's own current slider values.
    /// Call only from a single thread that owns the instance (offline
    /// renders); the live audio path uses the generation-stamped snapshot.
    pub fn process_private(&self, input: f32) -> f32 {
        let mut p = [0.0f32; MAX_PARAMS];
        self.read_params(&mut p);
        unsafe { self.call_sample(input, &p) }
    }

    /// `process_private` on the (typically mid, mid) pair — used by the
    /// offline chain pass for v3 scripts, whose REAL processing lives in
    /// `process_st` (the v1 `process` of a v3 script is only a fallback).
    pub fn process_private_st(&self, l: f32, r: f32) -> (f32, f32) {
        let mut p = [0.0f32; MAX_PARAMS];
        self.read_params(&mut p);
        unsafe { self.call_sample_st(l, r, &p, 0.0) }
    }

    /// v48: `process_private_st` with this render pass's sidechain sample.
    /// Offline chain passes that carry a routed send use this so v2+ scripts
    /// duck in the export exactly like on the live bus; `sc = 0.0` is the
    /// documented "no send routed" rule (identical to `process_private_st`).
    pub fn process_private_st_sc(&self, l: f32, r: f32, sc: f32) -> (f32, f32) {
        let mut p = [0.0f32; MAX_PARAMS];
        self.read_params(&mut p);
        unsafe { self.call_sample_st(l, r, &p, sc) }
    }

    fn snapshot_params(&self) -> Vec<f32> {
        (0..MAX_PARAMS).map(|k| self.param(k)).collect()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        (self.fns.destroy)(self.state);
    }
}

// ---------------------------------------------------------------------------
// Custom child GUI handles
// ---------------------------------------------------------------------------

/// A validated child-GUI vtable from a loaded effect, plus the library ref
/// that keeps its code mapped. Cheap to clone; the editor window owns one.
#[derive(Clone)]
pub struct EffectGuiRef {
    v: &'static EffectGuiV1,
    _lib: Arc<LibHandle>,
}
// Same reasoning as LibHandle: the Arc keeps the dlopen'd library mapped
// while any clone exists, and the vtable static is read-only.
unsafe impl Send for EffectGuiRef {}
unsafe impl Sync for EffectGuiRef {}

impl EffectGuiRef {
    /// Preferred canvas size (0/NaN hints fall back to a sane default).
    pub fn size_hint(&self) -> (f32, f32) {
        let w = self.v.want_width;
        let h = self.v.want_height;
        let w = if w.is_finite() && w >= 120.0 {
            w
        } else {
            480.0
        };
        let h = if h.is_finite() && h >= 90.0 { h } else { 320.0 };
        (w, h)
    }

    /// Open a per-window GUI state. Null = effect declined (auto window).
    pub fn create(&self, sample_rate: f64) -> *mut core::ffi::c_void {
        (self.v.create)(sample_rate)
    }

    /// Draw one frame. `state` is the audio instance state (valid while
    /// the editor's instance Arc lives); `api`/`frame` are per-call data.
    ///
    /// # Safety
    /// `gs` must come from `create` on this same handle (not yet destroyed)
    /// and `state` must belong to a live [`Instance`]; `frame`/`api` must
    /// stay valid for the duration of the call.
    pub unsafe fn draw(
        &self,
        gs: *mut core::ffi::c_void,
        state: *mut core::ffi::c_void,
        frame: &GuiFrameV1,
        api: &GuiApiV1,
    ) {
        (self.v.draw)(gs, state, frame, api)
    }

    /// Close the window's GUI state. The instance must still be alive.
    pub fn destroy(&self, gs: *mut core::ffi::c_void) {
        (self.v.destroy)(gs)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// One script found in the effects folder. Exactly one of `fns`/`error` is
/// meaningful: valid effects carry descriptors + callbacks; everything else
/// is listed in the GUI with its rejection reason.
pub struct EffectEntry {
    /// File stem (display id + persistence key).
    pub stem: String,
    pub desc: Option<EffectDesc>,
    fns: Option<EffectFns>,
    /// Custom child GUI exported by this effect (None = auto slider window).
    gui: Option<&'static EffectGuiV1>,
    pub error: Option<String>,
    /// Keep the library mapped while instances may exist.
    _lib: Option<Arc<LibHandle>>,
}

/// The effects folder plus everything loaded from it. Lives on the GUI
/// thread; Send/Sync is asserted for the engine handoff of its Arcs (the
/// registry itself is not shared across threads in practice, but the
/// wrapper types must be Send for the Arcs it produces).
pub struct Registry {
    dir: PathBuf,
    items: Vec<EffectEntry>,
}
unsafe impl Send for Registry {}
unsafe impl Sync for Registry {}

impl Registry {
    pub fn open(dir: PathBuf) -> Self {
        Registry {
            dir,
            items: Vec::new(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// All scripts, valid and invalid, in folder order (alphabetical stem).
    pub fn items(&self) -> &[EffectEntry] {
        &self.items
    }

    /// Valid effects in alphabetical stem order.
    pub fn stems(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|e| e.fns.is_some())
            .map(|e| e.stem.clone())
            .collect()
    }

    /// Loadable entry by stem.
    fn find(&self, stem: &str) -> Option<&EffectEntry> {
        self.items
            .iter()
            .find(|e| e.stem == stem && e.fns.is_some())
    }

    pub fn desc(&self, stem: &str) -> Option<&EffectDesc> {
        self.find(stem).and_then(|e| e.desc.as_ref())
    }

    /// True when the folder holds a script with this stem (any state).
    pub fn has_file(&self, stem: &str) -> bool {
        self.items.iter().any(|e| e.stem == stem)
    }

    pub fn has_template(&self) -> bool {
        self.dir.join(TEMPLATE_FILE).is_file()
    }

    /// Write the starter template into the folder (never overwriting).
    /// Returns the file stem created.
    pub fn add_template(&mut self) -> Result<String, String> {
        if self.has_template() {
            return Err("template.rs already exists in the effects folder".into());
        }
        std::fs::write(self.dir.join(TEMPLATE_FILE), TEMPLATE_SRC)
            .map_err(|e| format!("could not write template: {e}"))?;
        self.rescan();
        Ok(TEMPLATE_FILE.trim_end_matches(".rs").to_string())
    }

    /// Re-point the registry at another folder (effects window "Apply").
    pub fn set_dir(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.items.clear();
    }

    /// Compile stale scripts, (re)load and validate everything. Compile
    /// errors are per-file: a broken script never hides the healthy ones.
    /// Runs on the GUI thread — rustc takes a moment per *changed* file.
    pub fn rescan(&mut self) {
        self.items.clear();
        seed_api(&self.dir);
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for ent in rd.flatten() {
                let path = ent.path();
                if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let stem = match path.file_stem() {
                    Some(s) => s.to_string_lossy().into_owned(),
                    None => continue,
                };
                // Skip the generated contract and dot/underscore scratch files.
                if stem
                    == Path::new(API_FILE)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(API_FILE)
                    || stem.starts_with('_')
                    || stem.starts_with('.')
                {
                    continue;
                }
                files.push((stem, path));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        for (stem, path) in files {
            let item = self.compile_and_load(&stem, &path);
            self.items.push(item);
        }
    }

    fn compile_and_load(&self, stem: &str, src: &Path) -> EffectEntry {
        let mk = |error: String| EffectEntry {
            stem: stem.to_string(),
            desc: None,
            fns: None,
            gui: None,
            error: Some(error),
            _lib: None,
        };
        if !rustc_available() {
            return mk(format!(
                "rustc not found — {} to compile effects",
                rustc_install_hint()
            ));
        }
        let so = match self.stamped_artifact(stem, src) {
            Ok(so) => so,
            Err(e) => return mk(e),
        };
        if !so.is_file() {
            if let Err(e) = compile_effect(src, &so) {
                return mk(e);
            }
            self.prune_stale(stem, &so);
        }
        let lib = match unsafe { libloading::Library::new(&so) } {
            Ok(l) => Arc::new(LibHandle(std::mem::ManuallyDrop::new(l))),
            Err(e) => return mk(format!("could not load {}: {e}", so.display())),
        };
        match unsafe { validate(&lib.0) } {
            Ok((desc, fns, gui)) => EffectEntry {
                stem: stem.to_string(),
                desc: Some(desc),
                fns: Some(fns),
                gui,
                error: None,
                _lib: Some(lib),
            },
            Err(e) => mk(e),
        }
    }

    /// `.build/<stem>-<stamp>.<dylib>` (`so` on Linux, `dll` on Windows);
    /// the stamp folds source mtime+size so an edited script builds to a
    /// fresh path (fresh dlopen, no in-place overwrites of mapped files).
    fn stamped_artifact(&self, stem: &str, src: &Path) -> Result<PathBuf, String> {
        let meta = std::fs::metadata(src).map_err(|e| format!("stat failed: {e}"))?;
        let mut h = DefaultHasher::new();
        stem.hash(&mut h);
        meta.len().hash(&mut h);
        meta.modified()
            .map(|t| format!("{t:?}"))
            .unwrap_or_default()
            .hash(&mut h);
        let stamp = h.finish();
        Ok(self
            .dir
            .join(".build")
            .join(format!("{stem}-{stamp:016x}.{}", crate::paths::dylib_ext())))
    }

    /// Delete older artifacts of the same stem (best-effort; deleting a
    /// still-mapped file is safe on Linux; on Windows the remove simply
    /// fails while the dll is still loaded and the stale file lingers
    /// until it unmaps — harmless, nothing looks it up by name).
    fn prune_stale(&self, stem: &str, keep: &Path) {
        let Some(dir) = keep.parent() else { return };
        if let Ok(rd) = std::fs::read_dir(dir) {
            let prefix = format!("{stem}-");
            for ent in rd.flatten() {
                let p = ent.path();
                let name = p
                    .file_name()
                    .unwrap_or_else(|| OsStr::new(""))
                    .to_string_lossy()
                    .into_owned();
                if name.starts_with(&prefix) && p != keep {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }

    /// Create a live instance of a valid effect (null-create ⇒ None).
    pub fn create_instance(
        &self,
        stem: &str,
        sample_rate: f64,
        values: &[f32],
    ) -> Option<Arc<Instance>> {
        let e = self.find(stem)?;
        let fns = e.fns?;
        let desc = e.desc.as_ref()?;
        let lib = e._lib.as_ref()?.clone();
        Instance::new(fns, lib, desc, sample_rate, values)
    }

    /// The effect's custom child GUI, if it exported a valid one. The
    /// handle keeps the library mapped independently of the registry.
    pub fn gui(&self, stem: &str) -> Option<EffectGuiRef> {
        let e = self.find(stem)?;
        let v = e.gui?;
        Some(EffectGuiRef {
            v,
            _lib: e._lib.as_ref()?.clone(),
        })
    }
}

/// rustc invocation shared by the loader and its tests. Output paths are
/// unique so parallel compiles never collide.
fn compile_effect(src: &Path, out: &Path) -> Result<(), String> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create build dir: {e}"))?;
    }
    let crate_name = sanitize_crate_name(
        out.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("slooper_effect"),
    );
    let tmp = crate::paths::build_sidecar(out, "tmp");
    let output = Command::new("rustc")
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "cdylib",
            "-C",
            "opt-level=2",
        ])
        .arg("--crate-name")
        .arg(&crate_name)
        .arg("-o")
        .arg(&tmp)
        .arg(src)
        .output()
        .map_err(|e| format!("could not run rustc: {e} — {}", rustc_install_hint()))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let err = trim_head(err.trim(), 1800);
        std::fs::write(crate::paths::build_sidecar(out, "failed"), &err).ok();
        return Err(format!("compile failed:\n{err}"));
    }
    // W14 (WINDOWS-PORT, synth.rs twin): a concurrent instance may have
    // built the same artifact; on Windows renaming onto a dll it has mapped
    // fails. If the artifact exists now, it is exactly what we wanted.
    if let Err(e) = std::fs::rename(&tmp, out) {
        if !out.is_file() {
            return Err(format!("could not store {}: {e}", out.display()));
        }
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

/// rustc crate names are `[a-z0-9_]`, cannot be empty or a keyword prefix
/// issue for our purposes — fold anything else to `_` and prefix so a stem
/// like `test` or `2s` still builds.
fn sanitize_crate_name(stem: &str) -> String {
    let mut s = String::from("fx_");
    for c in stem.chars() {
        s.push(if c.is_ascii_alphanumeric() || c == '_' {
            c.to_ascii_lowercase()
        } else {
            '_'
        });
    }
    s
}

/// Read + validate the exported description from a loaded library. Also
/// picks up the optional custom child-GUI vtable (an effect that exports a
/// broken one is rejected outright — silent auto-fallback would hide the
/// very bug the author needs to see in the effects window).
unsafe fn validate(
    lib: &libloading::Library,
) -> Result<(EffectDesc, EffectFns, Option<&'static EffectGuiV1>), String> {
    // v32 R5: v2 side symbol first (sidechain-capable scripts). A script
    // exporting both symbols lands here; v1-only scripts are unchanged.
    // The v2 struct's prefix is bit-identical to v1 (validated in the ABI
    // file), so the common path below reads it as a plain v1 description.
    // Tier-3: v3 side symbol takes priority over v2 — same prefix rule.
    let mut process_sc = None;
    let mut process_st = None;
    // Tier-6: v4-only captures (defaults = "not a v4 script").
    let mut set_sync = None;
    let mut stereo_decl: Option<bool> = None;
    let mut param_meta: *const ParamMetaV1 = std::ptr::null();
    let d: &EffectInfoV1 = match lib.get(ENTRY_V4) {
        Ok(s) => {
            let s: libloading::Symbol<extern "C" fn() -> *const EffectInfoV4> = s;
            let p = s();
            if p.is_null() {
                return Err("slooper_effect_info_v4 returned null".into());
            }
            let v4 = &*p;
            if v4.abi != ABI_VERSION_V4 {
                return Err(format!(
                    "effect ABI v4 version {} (this slooper speaks v4 = {ABI_VERSION_V4})",
                    v4.abi
                ));
            }
            process_sc = Some(v4.process_sc);
            process_st = Some(v4.process_st);
            set_sync = v4.set_sync;
            stereo_decl = Some(v4.stereo != 0);
            param_meta = v4.param_meta;
            std::mem::transmute::<*const EffectInfoV4, *const EffectInfoV1>(v4)
                .as_ref()
                .unwrap()
        }
        Err(_) => match lib.get(ENTRY_V3) {
            Ok(s) => {
                let s: libloading::Symbol<extern "C" fn() -> *const EffectInfoV3> = s;
                let p = s();
                if p.is_null() {
                    return Err("slooper_effect_info_v3 returned null".into());
                }
                let v3 = &*p;
                if v3.abi != ABI_VERSION_V3 {
                    return Err(format!(
                        "effect ABI v3 version {} (this slooper speaks v3 = {ABI_VERSION_V3})",
                        v3.abi
                    ));
                }
                process_sc = Some(v3.process_sc);
                process_st = Some(v3.process_st);
                std::mem::transmute::<*const EffectInfoV3, *const EffectInfoV1>(v3)
                    .as_ref()
                    .unwrap()
            }
            Err(_) => match lib.get(ENTRY_V2) {
                Ok(s) => {
                    let s: libloading::Symbol<extern "C" fn() -> *const EffectInfoV2> = s;
                    let p = s();
                    if p.is_null() {
                        return Err("slooper_effect_info_v2 returned null".into());
                    }
                    let v2 = &*p;
                    if v2.abi != ABI_VERSION_V2 {
                        return Err(format!(
                            "effect ABI v2 version {} (this slooper speaks v2 = {ABI_VERSION_V2})",
                            v2.abi
                        ));
                    }
                    process_sc = Some(v2.process_sc);
                    std::mem::transmute::<*const EffectInfoV2, *const EffectInfoV1>(v2)
                        .as_ref()
                        .unwrap()
                }
                Err(_) => {
                    let sym: libloading::Symbol<extern "C" fn() -> *const EffectInfoV1> =
                        lib.get(ENTRY).map_err(|_| {
                            "missing entry point slooper_effect_info — see template.rs".to_string()
                        })?;
                    let info = sym();
                    if info.is_null() {
                        return Err("slooper_effect_info returned null".into());
                    }
                    let d = &*info;
                    if d.abi != ABI_VERSION {
                        return Err(format!(
                            "effect ABI version {} (this slooper speaks {ABI_VERSION})",
                            d.abi
                        ));
                    }
                    d
                }
            },
        },
    };
    let name = read_cstr(d.name).ok_or("effect has no name — DESC.name must be text")?;
    let author = d.author.cast_non_null().and_then(read_cstr);
    let description = d.description.cast_non_null().and_then(read_cstr);
    if d.n_params as usize > MAX_PARAMS {
        return Err(format!(
            "effect declares {} params (max {MAX_PARAMS})",
            d.n_params
        ));
    }
    if d.n_params > 0 && d.params.is_null() {
        return Err("n_params > 0 but the params array is null".into());
    }
    let mut params = Vec::with_capacity(d.n_params as usize);
    for k in 0..d.n_params as usize {
        let p = &*d.params.add(k);
        let label = read_cstr(p.label).ok_or_else(|| format!("param {k} has no label"))?;
        let (min, max) = if p.min <= p.max {
            (p.min, p.max)
        } else {
            (p.max, p.min)
        };
        params.push(ParamDesc {
            label,
            min,
            max,
            default: p.default.clamp(min, max),
            unit: 0,
            step: 0.0,
            labels: Vec::new(),
        });
    }
    // Tier-6 ABI v4: per-param display metadata. Only v4 scripts get here
    // with a non-null table; every entry rides the already-validated
    // (label/min/max/default) param at the same index.
    if !param_meta.is_null() {
        if d.n_params == 0 {
            return Err("param_meta table without any params".into());
        }
        if d.n_params as usize > params.len() {
            return Err("param_meta table longer than the param list".into());
        }
        for (k, pd) in params.iter_mut().enumerate().take(d.n_params as usize) {
            let m = unsafe { &*param_meta.add(k) };
            pd.unit = m.unit;
            if m.step.is_finite() && m.step > 0.0 {
                pd.step = m.step;
            }
            if m.n_labels > 0 {
                if m.n_labels > 32 {
                    return Err(format!(
                        "param {k} declares {} enum labels (max 32)",
                        m.n_labels
                    ));
                }
                if m.labels.is_null() {
                    return Err(format!("param {k}: n_labels > 0 but labels is null"));
                }
                if m.n_labels > 1
                    && pd.max.partial_cmp(&pd.min) != Some(core::cmp::Ordering::Greater)
                {
                    return Err(format!("enum param {k} needs max > min"));
                }
                let mut ls = Vec::with_capacity(m.n_labels as usize);
                for i in 0..m.n_labels as usize {
                    let lp = unsafe { *m.labels.add(i) };
                    let s = read_cstr(lp)
                        .ok_or_else(|| format!("param {k} label {i} is null or not UTF-8"))?;
                    ls.push(s);
                }
                pd.labels = ls;
            }
        }
    }
    let gui = match lib.get(GUI_ENTRY) {
        Ok(gsym) => {
            let gsym: libloading::Symbol<extern "C" fn() -> *const EffectGuiV1> = gsym;
            let g = gsym();
            if g.is_null() {
                return Err("slooper_effect_gui returned null".into());
            }
            let g = &*g;
            if g.abi != GUI_ABI_VERSION {
                return Err(format!(
                    "child GUI ABI version {} (this slooper speaks {GUI_ABI_VERSION})",
                    g.abi
                ));
            }
            // create/draw/destroy are non-nullable fn types in the shared
            // ABI file — a script cannot put null in them and compile, so
            // (like EffectInfoV1) there is nothing to null-check.
            Some(g)
        }
        // No GUI symbol: the auto slider window handles this effect.
        Err(_) => None,
    };
    Ok((
        EffectDesc {
            name,
            author,
            description,
            params,
            // A v3 script MUST export the v2 prefix, so `process_sc`
            // presence only proves sidechain INTENT on the exact v2
            // generation. v3 scripts that want the key signal read the
            // `sc` argument of process_st itself.
            sidechain: process_sc.is_some() && process_st.is_none(),
            // Tier-6: a v4 script DECLARES its stereo-ness (its required
            // process_st may be the documented mono fallback); v1..v3
            // scripts keep the structural rule (process_st == real pair).
            stereo: stereo_decl.unwrap_or(process_st.is_some()),
        },
        EffectFns {
            create: d.create,
            destroy: d.destroy,
            process: d.process,
            process_sc,
            process_st,
            set_sync,
            param_meta,
        },
        gui,
    ))
}

/// `*const T` non-null helper (kept tiny; `Option::from` is for refs).
trait CastNonNull {
    fn cast_non_null(self) -> Option<*const core::ffi::c_char>;
}
impl CastNonNull for *const core::ffi::c_char {
    fn cast_non_null(self) -> Option<*const core::ffi::c_char> {
        (!self.is_null()).then_some(self)
    }
}

fn read_cstr(p: *const core::ffi::c_char) -> Option<String> {
    (!p.is_null())
        .then(|| {
            unsafe { CStr::from_ptr(p) }
                .to_str()
                .ok()
                .map(|s| s.to_string())
        })
        .flatten()
}

fn trim_head(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n…", &s[..cut])
    }
}

// ---------------------------------------------------------------------------
// GUI-facing chain model (plain data; ordering lives here, audio is fed
// Arc<Instance> snapshots from the UI layer)
// ---------------------------------------------------------------------------

/// One row of the effects window: a script, whether this loop runs it, and
/// the current slider values (empty = not yet seeded; the UI fills defaults
/// from the descriptor).
#[derive(Clone, Debug)]
pub struct UiEntry {
    pub stem: String,
    pub enabled: bool,
    pub params: Vec<f32>,
}

/// Merge the folder contents into a loop's window list:
/// - enabled effects first, keeping their previous signal order (and dropping
///   ones whose script disappeared or failed to compile),
/// - then the disabled ones, alphabetically.
///
/// `saved` is the persisted chain for this loop (enabled order + values);
/// pass an empty slice on rescan (the live list already carries state).
pub fn merge_entries(
    existing: &[UiEntry],
    stems: &[String],
    saved: &[(String, Vec<f32>)],
) -> Vec<UiEntry> {
    let mut out: Vec<UiEntry> = Vec::new();
    // Enabled block, in chain order: previous GUI list first (rescan), else
    // the saved config order (startup).
    let order: Vec<String> = if existing.iter().any(|e| e.enabled) {
        existing
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.stem.clone())
            .collect()
    } else {
        saved.iter().map(|(s, _)| s.clone()).collect()
    };
    for stem in order {
        if !stems.iter().any(|s| s == &stem) {
            continue; // script gone or now broken: silently dropped
        }
        let params = existing
            .iter()
            .find(|e| e.stem == stem)
            .map(|e| e.params.clone())
            .filter(|p| !p.is_empty())
            .or_else(|| {
                saved
                    .iter()
                    .find(|(s, _)| *s == stem)
                    .map(|(_, p)| p.clone())
            })
            .unwrap_or_default();
        out.push(UiEntry {
            stem,
            enabled: true,
            params,
        });
    }
    // Disabled block: everything valid that is not enabled, alphabetical
    // (`stems` arrives alphabetical).
    for s in stems {
        if !out.iter().any(|e| e.stem == *s) {
            let keep = existing
                .iter()
                .find(|e| &e.stem == s)
                .map(|e| e.params.clone())
                .unwrap_or_default();
            out.push(UiEntry {
                stem: s.clone(),
                enabled: false,
                params: keep,
            });
        }
    }
    out
}

/// GUI order after a toggle: enable → snap to the end of the enabled block,
/// disable → drop into the alphabetical disabled block.
pub fn resort_entries(entries: &mut Vec<UiEntry>, stems: &[String]) {
    let enabled: Vec<UiEntry> = entries.iter().filter(|e| e.enabled).cloned().collect();
    let mut v = merge_entries(&enabled, stems, &[]);
    // merge_entries starts from `enabled` (all enabled) and appends nothing;
    // the disabled half must come from `stems` fresh:
    for s in stems {
        if !v.iter().any(|e| e.stem == *s) {
            v.push(UiEntry {
                stem: s.clone(),
                enabled: false,
                params: Vec::new(),
            });
        }
    }
    *entries = v;
}

/// Swap `k` with the neighbouring enabled entry (chain reordering in the
/// window). Returns true when the list changed.
pub fn move_enabled(entries: &mut [UiEntry], k: usize, up: bool) -> bool {
    let mut idx: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.enabled)
        .map(|(i, _)| i)
        .collect();
    let pos = match idx.iter().position(|&i| i == k) {
        Some(p) => p,
        None => return false,
    };
    let new_pos = if up {
        if pos == 0 {
            return false;
        }
        pos - 1
    } else {
        if pos + 1 >= idx.len() {
            return false;
        }
        pos + 1
    };
    idx.swap(pos, new_pos);
    entries.swap(idx[pos], idx[new_pos]);
    true
}

/// Values a chain slot should use: stored ones, padded with the descriptor
/// defaults.
pub fn seed_values(saved: &[f32], desc: &EffectDesc) -> Vec<f32> {
    desc.params
        .iter()
        .enumerate()
        .map(|(k, p)| {
            saved
                .get(k)
                .copied()
                .unwrap_or(p.default)
                .clamp(p.min, p.max)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("slooper-fx-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn bundled_scripts_update_in_place_with_backup() {
        // A stale copy of a bundled script is refreshed on seed (app fixes
        // must reach existing installs), and the outgoing contents survive
        // as `<name>.rs.bak`.
        let dir = temp_dir("seedupdate");
        std::fs::write(dir.join(API_FILE), API_SRC).unwrap();
        let (name, src) = EXAMPLES[0];
        let p = dir.join(name);
        std::fs::write(&p, "// old stale copy\n").unwrap();
        seed_api(&dir);
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            *src,
            "{name} not refreshed"
        );
        assert_eq!(
            std::fs::read_to_string(p.with_extension("rs.bak")).unwrap(),
            "// old stale copy\n"
        );
        // A current copy is left untouched (and no spurious backup).
        let bak = p.with_extension("rs.bak");
        std::fs::remove_file(&bak).unwrap();
        seed_api(&dir);
        assert!(!bak.exists(), "unchanged script must not be re-backed-up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_files_stay_in_sync() {
        // The repo copies (what PKGBUILD installs) must equal the embedded
        // sources the app writes/creates.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            std::fs::read_to_string(root.join("effects/effect_api.rs")).unwrap(),
            API_SRC
        );
        assert_eq!(
            std::fs::read_to_string(root.join("effects/template.rs")).unwrap(),
            TEMPLATE_SRC
        );
        for (name, src) in EXAMPLES {
            assert_eq!(
                std::fs::read_to_string(root.join("effects").join(name)).unwrap(),
                *src,
                "{name} out of sync"
            );
        }
    }

    #[test]
    fn bundled_examples_load_with_custom_guis() {
        // A fresh effects folder seeds template + examples; every one must
        // compile through the real loader, and the two examples must carry
        // a valid child GUI while the plain template must not.
        if !rustc_available() {
            return; // CI without rustc: nothing to verify
        }
        let dir = temp_dir("examples");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let want = ["template", "parametric_eq", "space_echo"];
        for name in want {
            let it = reg
                .items()
                .iter()
                .find(|e| e.stem == name)
                .unwrap_or_else(|| {
                    panic!(
                        "missing {name}: {:?}",
                        reg.items()
                            .iter()
                            .map(|e| (&e.stem, e.error.clone()))
                            .collect::<Vec<_>>()
                    )
                });
            assert!(
                it.error.is_none(),
                "{name} failed to load: {}",
                it.error.clone().unwrap_or_default()
            );
        }
        assert_eq!(reg.desc("parametric_eq").unwrap().params.len(), 26);
        // v32: the native EQ must reach +30 dB — through the REAL parsed
        // descriptor, not a source-grep. The ceiling moved from +6; both
        // the slider range and the graph axis moved with it.
        let gains: Vec<&crate::fx::ParamDesc> = reg
            .desc("parametric_eq")
            .unwrap()
            .params
            .iter()
            .filter(|p| p.label.contains("gain"))
            .collect();
        assert_eq!(gains.len(), 6, "six band gain params");
        for g in &gains {
            assert!(
                (g.max - 30.0).abs() < 1e-3 && (g.min + 120.0).abs() < 1e-3,
                "{} range {}..{}, want -120..30",
                g.label,
                g.min,
                g.max
            );
        }
        assert_eq!(reg.desc("space_echo").unwrap().params.len(), 12);
        // v32 R5: the stock sidechain compressor is the ABI-v2 reference.
        // It must load (both entry symbols present) and be recognized as
        // sidechain-capable; every v1 stock effect must not be.
        {
            let sc = reg
                .desc("sidechain_comp")
                .unwrap_or_else(|| panic!("sidechain_comp missing/failed"));
            assert!(sc.sidechain, "must load through the v2 entry");
            assert_eq!(sc.params.len(), 6);
            assert!(!reg.desc("compressor").unwrap().sidechain, "v1 stays v1");
            assert!(!reg.desc("template").unwrap().sidechain);
        }
        assert!(
            reg.gui("parametric_eq").is_some(),
            "EQ must expose a child GUI"
        );
        assert!(
            reg.gui("space_echo").is_some(),
            "space echo must expose a child GUI"
        );
        assert!(
            reg.gui("template").is_none(),
            "template keeps the auto slider window"
        );
        // Native-suite tier: the waveshaper carries the draggable-curve GUI,
        // and the vocoder ships the full v1/v2/v3 export quartet (tier-5b
        // stereo). v3 flag semantics (tier-3b): `sidechain` is EXACT-v2
        // only — a v3 script reads the key through the `sc` argument of
        // process_st — so vocoder now flags stereo, never sidechain.
        assert!(
            reg.gui("waveshaper").is_some(),
            "waveshaper must expose its curve editor GUI"
        );
        let voc = reg.desc("vocoder").unwrap();
        assert!(voc.stereo, "vocoder must load through the v3 entry");
        assert!(
            !voc.sidechain,
            "vocoder sidechain flag is exact-v2-only (it keys off process_st's sc arg)"
        );
        assert!(!reg.desc("overdrive").unwrap().sidechain, "overdrive is v1");
        assert!(
            reg.gui("overdrive").is_none(),
            "auto slider window covers overdrive"
        );
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bipolar_params_survive_loading() {
        // Band gain on the EQ is bipolar (-24..+24 dB). Both the initial
        // seed (loading a saved chain) and live slider moves must keep
        // negative values — a `clamp(0, ..)` here made the EQ "do nothing"
        // for cuts, the original user complaint.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("bipolar");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let inst = reg
            .create_instance("parametric_eq", 48000.0, &[1.0, 1000.0, -12.0, 1.0])
            .expect("eq instance");
        assert_eq!(inst.param(2), -12.0, "saved -12 dB gain must load");
        inst.set_param(2, -6.5);
        assert_eq!(inst.param(2), -6.5, "live set must keep the sign");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_echo_holds_a_wet_tail() {
        // After a single impulse, a wet echo must keep producing decaying
        // output for at least one delay time — proof the effect state and
        // feedback run at the instance level (what the serial chain feeds
        // through to whatever slot comes next).
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("echotail");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        // time .25 s, fb .5, tone 8 k, wobble 0, rate 0, decay .5, damp 8 k, wet 1, out 1
        let inst = reg
            .create_instance(
                "space_echo",
                48000.0,
                &[0.25, 0.5, 8000.0, 0.0, 0.05, 0.5, 8000.0, 1.0, 1.0],
            )
            .expect("echo instance");
        let _ = inst.process_private(1.0);
        // Energy strictly after the impulse: the tail. Windows are wide
        // enough to bracket wherever the first tap lands (the delay line
        // reserves headroom beyond `time`).
        let win: Vec<f32> = (0..8)
            .map(|_| (0..4800).map(|_| inst.process_private(0.0).abs()).sum())
            .collect();
        let total: f32 = win.iter().sum();
        assert!(total > 1e-3, "wet echo produced no tail (sum {total})");
        let peak = win.iter().cloned().fold(0.0f32, f32::max);
        assert!(win[7] < peak, "tail must decay: {win:?}");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_echo_tail_modes_are_dense_not_metallic() {
        // Objective guard against the metallic "ping" on transients: after
        // an impulse, the LATE reverb tail must be a dense wash, not a
        // handful of knife-edge resonances. Method: impulse -> settle 0.4 s
        // -> Hann-windowed Goertzel of the next 0.4 s at 97 log-spaced
        // frequencies, count resonances standing out of the average floor.
        // Four bunched combs produced a handful of huge isolated peaks; the
        // eight-comb Freeverb bank must produce many more, much flatter
        // ones.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("echomodes");
        let probe = |dir: &Path, stem: &str| -> (usize, f64, f64, f64) {
            let mut reg = Registry::open(dir.to_path_buf());
            reg.rescan();
            // cathedral-ish: long decay, moderate damp, echo level 0 so the
            // FDN alone is measured.
            let inst = reg
                .create_instance(
                    stem,
                    48000.0,
                    &[
                        0.25, 0.5, 8000.0, 0.0, 0.0, 0.95, 2800.0, 1.0, 1.0, 0.0, 1.0,
                    ],
                )
                .expect("echo instance");
            let _ = inst.process_private(1.0);
            for _ in 0..24000 {
                let _ = inst.process_private(0.0);
            }
            // Goertzel magnitudes of one window at 161 log-spaced freqs.
            // 2 s windows give fine (0.5 Hz) resolution so INDIVIDUAL comb
            // modes resolve — that is exactly where sparse banks look
            // metallic and dense ones look like a wash.
            let window_mags = |x: &[f64]| -> Vec<f64> {
                let n = x.len();
                let mut mags = Vec::with_capacity(161);
                for k in 0..161 {
                    let f = 150.0f64 * (14000.0f64 / 150.0).powf(k as f64 / 160.0);
                    let w = core::f64::consts::TAU * f / 48000.0;
                    let cw = w.cos();
                    let (mut s1, mut s2) = (0.0f64, 0.0f64);
                    for (i, &v) in x.iter().enumerate() {
                        let h = 0.5
                            - 0.5 * (core::f64::consts::TAU * i as f64 / (n as f64 - 1.0)).cos();
                        let s0 = v * h + 2.0 * cw * s1 - s2;
                        s2 = s1;
                        s1 = s0;
                    }
                    mags.push((s1 * s1 + s2 * s2 - 2.0 * cw * s1 * s2).max(0.0).sqrt());
                }
                mags
            };
            const N: usize = 96000;
            let mut early = Vec::with_capacity(N);
            for _ in 0..N {
                early.push(inst.process_private(0.0) as f64);
            }
            let mags = window_mags(&early);
            // Late slice, ~1 s further in: where air absorption should have
            // eaten the treble (the thing that made the old tail "tinny").
            for _ in 0..48000 {
                let _ = inst.process_private(0.0);
            }
            let mut late = Vec::with_capacity(N);
            for _ in 0..N {
                late.push(inst.process_private(0.0) as f64);
            }
            let lmags = window_mags(&late);
            let band_mean = |m: &[f64], lo: usize, hi: usize| -> f64 {
                let s: f64 = m[lo..hi].iter().sum();
                s / (hi - lo) as f64
            };
            // hi (≈5.5–14 k) vs mid (≈1–4 k): natural tails darken.
            let hm_e = band_mean(&mags, 127, 161) / band_mean(&mags, 67, 116).max(1e-30);
            let hm_l = band_mean(&lmags, 127, 161) / band_mean(&lmags, 67, 116).max(1e-30);
            let mean = mags.iter().sum::<f64>() / mags.len() as f64;
            let mut peaks = 0usize;
            // Tilt-removed concentration: each bin over a ±4-bin moving
            // average of its neighbours. A dark high-frequency rolloff (what
            // a *good* tail does) cancels in the ratio; a lone ringing mode
            // does not. Raw neighbour ratios punish the shelf edge instead
            // (1000x drops across 290 Hz at 9 kHz on a deep tilt).
            let smooth: Vec<f64> = (0..mags.len())
                .map(|i| {
                    let lo = i.saturating_sub(4);
                    let hi = (i + 5).min(mags.len());
                    mags[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
                })
                .collect();
            let mut conc = 1.0f64;
            for i in 1..mags.len() - 1 {
                if mags[i] > mags[i - 1] && mags[i] >= mags[i + 1] && mags[i] > mean * 2.0 {
                    peaks += 1;
                }
                conc = conc.max(mags[i] / smooth[i].max(1e-30));
            }
            drop(reg);
            (peaks, conc, hm_e, hm_l)
        };
        let (peaks, conc, hm_e, hm_l) = probe(&dir, "space_echo");
        println!(
            "MODEPROBE current: peaks={peaks} concentration={conc:.1} hi/mid {hm_e:.3}→{hm_l:.3}"
        );
        // Calibration (tilt-removed concentration): current bank ~7.5,
        // deliberately bunched combs through the same damping stack ~8.7,
        // the original unmodulated metallic bank (no damping, no diffusion)
        // far higher; density (peaks) and the hi/mid darkening below are
        // the primary guards — the tail must be a wash AND must lose
        // treble as it decays ("tinny" was the complaint).
        assert!(
            peaks >= 5,
            "late tail empty? only {peaks} resonances visible"
        );
        assert!(
            conc < 12.0,
            "late tail metallic: one mode {conc:.1}x over its local envelope"
        );
        assert!(
            hm_l < hm_e * 0.65,
            "tail stays tinny: hi/mid {hm_e:.3} → {hm_l:.3} barely darkens"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diag_whine_probe() {
        // TEMPORARY diagnostic — high-pitched whine report. Removed before ship.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("diagwhine");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let mut rng = 0x2545F491u64;
        let mut rnd = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            ((rng as f64) / (u64::MAX as f64)) * 2.0 - 1.0
        };
        let window_mags = |x: &[f64]| -> Vec<(f64, f64)> {
            let n = x.len();
            let mut out = Vec::new();
            for k in 0..161 {
                let f = 150.0f64 * (16000.0f64 / 150.0).powf(k as f64 / 160.0);
                let w = core::f64::consts::TAU * f / 48000.0;
                let cw = w.cos();
                let (mut s1, mut s2) = (0.0f64, 0.0f64);
                for (i, &v) in x.iter().enumerate() {
                    let h =
                        0.5 - 0.5 * (core::f64::consts::TAU * i as f64 / (n as f64 - 1.0)).cos();
                    let s0 = v * h + 2.0 * cw * s1 - s2;
                    s2 = s1;
                    s1 = s0;
                }
                out.push((f, (s1 * s1 + s2 * s2 - 2.0 * cw * s1 * s2).max(0.0).sqrt()));
            }
            out
        };
        // (a) sustain test: noise burst, then silence; RMS every 100 ms.
        // A self-oscillating tone shows as an RMS PLATEAU instead of decay.
        let sets: [(&str, [f32; 11]); 4] = [
            (
                "default-Echo",
                [
                    0.38, 0.62, 3600.0, 0.0, 0.6, 0.84, 2800.0, 0.4, 1.0, 1.0, 1.0,
                ],
            ),
            (
                "cathedral",
                [
                    0.32, 0.5, 2200.0, 0.25, 0.14, 0.95, 1500.0, 0.7, 0.9, 0.18, 1.0,
                ],
            ),
            (
                "verb-only-hot",
                [
                    0.25, 0.5, 8000.0, 0.0, 0.0, 0.95, 16000.0, 1.0, 1.0, 0.0, 1.5,
                ],
            ),
            (
                "bright-damp-max",
                [
                    0.25, 0.5, 8000.0, 0.0, 0.0, 0.95, 16000.0, 1.0, 1.0, 1.0, 1.5,
                ],
            ),
        ];
        for (name, params) in sets {
            let inst = reg
                .create_instance("space_echo", 48000.0, &params)
                .expect("inst");
            for _ in 0..4800 {
                let _ = inst.process_private(rnd() as f32 * 0.3);
            }
            let mut rms = Vec::new();
            for _ in 0..40 {
                let e: f64 = (0..4800)
                    .map(|_| {
                        let y = inst.process_private(0.0) as f64;
                        y * y
                    })
                    .sum();
                rms.push((e / 4800.0).sqrt());
            }
            let head = rms[0].max(1e-12);
            let ratios: Vec<String> = rms
                .iter()
                .step_by(4)
                .map(|r| format!("{:.2}", r / head))
                .collect();
            println!("SUSTAIN {name}: ratios(0.4s steps) {ratios:?}");
            // late-window spectrum 1.6–2.6 s in: dominant peaks over local floor
            for _ in 0..28800 {
                let _ = inst.process_private(0.0);
            }
            let late: Vec<f64> = (0..48000)
                .map(|_| inst.process_private(0.0) as f64)
                .collect();
            let mags = window_mags(&late);
            let mm: Vec<f64> = mags.iter().map(|m| m.1).collect();
            let smooth: Vec<f64> = (0..mm.len())
                .map(|i| {
                    let lo = i.saturating_sub(3);
                    let hi = (i + 4).min(mm.len());
                    mm[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
                })
                .collect();
            let mut peaks: Vec<(f64, f64)> = (1..mm.len() - 1)
                .filter(|&i| mm[i] > mm[i - 1] && mm[i] >= mm[i + 1])
                .map(|i| (mags[i].0, mm[i] / smooth[i].max(1e-30)))
                .collect();
            peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let top: Vec<String> = peaks
                .iter()
                .take(6)
                .map(|(f, c)| format!("{f:.0}Hz x{c:.1}"))
                .collect();
            let late_peak = late.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            println!("LATE {name}: peak={late_peak:.2e} top-peaks {top:?}");
            drop(inst);
        }
        // (b) tone sensitivity: echo tail hi/lo energy vs tone param.
        for tone in [200.0f32, 3600.0, 16000.0] {
            let inst = reg
                .create_instance(
                    "space_echo",
                    48000.0,
                    &[0.25, 0.9, tone, 0.0, 0.05, 0.5, 8000.0, 1.0, 1.0, 1.0, 0.0],
                )
                .expect("inst");
            for _ in 0..2400 {
                let _ = inst.process_private(rnd() as f32 * 0.5);
            }
            let tail: Vec<f64> = (0..48000)
                .map(|_| inst.process_private(0.0) as f64)
                .collect();
            let mags = window_mags(&tail);
            let mean = |lo: usize, hi: usize| -> f64 {
                mags[lo..hi].iter().map(|m| m.1).sum::<f64>() / (hi - lo) as f64
            };
            let hilo = mean(120, 161) / mean(20, 70).max(1e-30);
            let peak = tail.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            println!("TONE {tone:.0}: tail peak={peak:.3} hi/lo={hilo:.4}");
            drop(inst);
        }
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diag_sine_adds_nothing() {
        // TEMPORARY diagnostic - sine in, anything else out? A whine is energy
        // the plugin invents away from the input. Removed before ship.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("diagsine");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let goertzel = |x: &[f64], f: f64| -> f64 {
            let w = core::f64::consts::TAU * f / 48000.0;
            let cw = w.cos();
            let (mut s1, mut s2) = (0.0f64, 0.0f64);
            for &v in x {
                let s0 = v + 2.0 * cw * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            (s1 * s1 + s2 * s2 - 2.0 * cw * s1 * s2).max(0.0).sqrt()
        };
        let sets: [(&str, [f32; 11]); 4] = [
            (
                "user-opera",
                [
                    0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 1.0,
                ],
            ),
            (
                "opera-no-wob",
                [
                    0.2, 0.42, 3800.0, 0.0, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 1.0,
                ],
            ),
            (
                "opera-no-verb",
                [
                    0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 0.0,
                ],
            ),
            (
                "opera-no-echo",
                [
                    0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.0, 1.0,
                ],
            ),
        ];
        for (name, params) in sets {
            let inst = reg
                .create_instance("space_echo", 48000.0, &params)
                .expect("inst");
            for i in 0..144000u64 {
                let x = (core::f64::consts::TAU * 1000.0 * i as f64 / 48000.0).sin() * 0.4;
                let _ = inst.process_private(x as f32);
            }
            let win: Vec<f64> = (0..48000u64)
                .map(|i| {
                    let x = (core::f64::consts::TAU * 1000.0 * i as f64 / 48000.0).sin() * 0.4;
                    inst.process_private(x as f32) as f64
                })
                .collect();
            let norm = 2.0 / win.len() as f64; // single-bin amplitude from Goertzel
            let fund = goertzel(&win, 1000.0) * norm;
            let mut sum_out = 0.0f64;
            let mut hi_out = 0.0f64;
            let mut top = (0.0f64, 0.0f64);
            for k in 0..120 {
                let f = 60.0f64 * (16000.0f64 / 60.0).powf(k as f64 / 119.0);
                if (f - 1000.0).abs() < 30.0 {
                    continue; // skip the fundamental bin neighbourhood
                }
                let m = goertzel(&win, f) * norm;
                sum_out += m * m;
                if f > 4000.0 {
                    hi_out += m * m;
                }
                if m > top.1 {
                    top = (f, m);
                }
            }
            println!(
                "SINE {name}: fundAmp={fund:.4} out-of-band rms={:.4} hi-band rms={:.4} top={:.0}Hz amp={:.5} ratio-to-fund={:.4}",
                sum_out.sqrt(),
                hi_out.sqrt(),
                top.0,
                top.1,
                top.1 / fund.max(1e-12)
            );
            drop(inst);
        }
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_echo_verb_and_echo_levels_are_independent() {
        // The two level params pick between the two effect halves:
        // with VERB at 0 nothing exists between the impulse and the first
        // echo tap (delay 250 ms = 12000 samples); with ECHO at 0 the FDN
        // still fills that window from the dry feed — a room with no slap.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("echoverb");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let mk = |echo: f32, verb: f32| {
            reg.create_instance(
                "space_echo",
                48000.0,
                &[
                    0.25, 0.5, 8000.0, 0.0, 0.05, 0.9, 8000.0, 1.0, 1.0, echo, verb,
                ],
            )
            .expect("echo instance")
        };
        let window_sum = |inst: &std::sync::Arc<Instance>| -> f32 {
            let _ = inst.process_private(1.0);
            // Samples 2800..11900: after the FDN fills (the diffuser chain's
            // group delay puts the first verb sample around 2700) and before
            // the first echo tap (0.25 s = 12000 samples).
            (0..11900)
                .skip(2800)
                .map(|_| inst.process_private(0.0).abs())
                .sum()
        };
        let echo_only = window_sum(&mk(1.0, 0.0));
        let verb_only = window_sum(&mk(0.0, 1.0));
        assert!(
            echo_only < 1e-6,
            "echo-only must be silent before the first tap (sum {echo_only})"
        );
        assert!(
            verb_only > 1e-3,
            "verb-only must fill the pre-echo window (sum {verb_only})"
        );
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eq_peaking_bands_are_stable_and_correct() {
        // The peaking (bell) denominator must be (1 + α/A, −2cos, 1 − α/A).
        // A sign swap there (1 − α/A … 1 + α/A) parks the poles OUTSIDE the
        // unit circle, so a peaking band self-oscillates to ±inf for ANY
        // nonzero alpha — even at 0 dB gain. With four peaking bands at the
        // EQ's defaults, enabling it pegged the output meter at inf.
        // Also covers the shelf-slope α: the shelf-SLOPE form goes
        // imaginary at high gain + high Q; clamped to 0 it parked shelf
        // poles exactly ON the unit circle (perpetual ringing).
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("eqstab");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let flat_off = [0.0f32, 1000.0, 0.0, 1.0];
        let mut vals = Vec::new();
        vals.extend_from_slice(&flat_off); // band 1: off
        for _ in 1..6 {
            vals.extend_from_slice(&flat_off);
        }
        vals.extend_from_slice(&[1.0, 1.0]); // mix, out
                                             // 1) defaults-equivalent cascade + impulse: finite, near pass-through.
        let inst = reg
            .create_instance("parametric_eq", 48000.0, &vals)
            .expect("eq");
        // impulse + settle, then a sine; the pass-through must keep the
        // impulse energy at ~1 head, dead-silent tail, and unity sine gain.
        let head: f32 = (0..240)
            .map(|_| inst.process_private(1.0).abs())
            .fold(0.0, f32::max);
        let tail: f32 = (0..4560)
            .map(|_| inst.process_private(0.0).abs())
            .fold(0.0, f32::max);
        assert!(
            head > 0.5 && tail < 0.05,
            "flat EQ impulse head {head} tail {tail}"
        );
        let out: Vec<f32> = (0..48000)
            .map(|i| {
                let x = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin() * 0.3;
                inst.process_private(x + if i % 7 == 0 { 0.05 } else { -0.05 })
            })
            .collect();
        assert!(out.iter().all(|y| y.is_finite()), "non-finite EQ output");
        let peak = out.iter().fold(0.0f32, |a, y| a.max(y.abs()));
        assert!(
            (0.25..0.5).contains(&peak),
            "flat EQ must pass ~unity, peak {peak}"
        );
        // 2) a +6 dB peaking band at 1 kHz must lift a 1 kHz sine by ~2×.
        let mut vals = vals.clone();
        vals[0] = 3.0; // peaking
        vals[2] = 6.0; // +6 dB
        let inst = reg
            .create_instance("parametric_eq", 48000.0, &vals)
            .unwrap();
        let mut sum = 0.0f64;
        let mut n = 0.0f64;
        for i in 0..48000u64 {
            let x = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin();
            let y = inst.process_private(x) as f64;
            assert!(y.is_finite(), "peaking band produced {y}");
            if i > 9600 {
                sum += y * y;
                n += 1.0;
            }
        }
        let rms = (sum / n).sqrt() as f32;
        let want = 0.707 * 10.0f32.powf(6.0 / 20.0); // sine RMS × gain
        assert!(
            (rms / want - 1.0).abs() < 0.04,
            "peaking +6 dB: rms {rms}, expected ~{want}"
        );
        // 3) high-Q/high-gain shelves: impulse must DIE AWAY, not ring forever.
        let mut vals = vals.clone();
        vals[0] = 1.0; // low shelf
        vals[2] = 24.0;
        vals[3] = 8.0; // Q 8 — the shelf-S formula went imaginary here
        vals[20] = 2.0; // high shelf
        vals[22] = 24.0;
        vals[23] = 8.0;
        let inst = reg
            .create_instance("parametric_eq", 48000.0, &vals)
            .unwrap();
        // A resonant shelf peaks ~tens of ms after the impulse (band build-up
        // + coefficient glide); the OLD clamped-alpha shelf kept ringing
        // forever instead. Head = first 100 ms, tail = the next 2.5 s.
        let head: f32 = (0..4800)
            .map(|_| inst.process_private(1.0).abs())
            .fold(0.0, f32::max);
        let tail: f32 = (0..120000)
            .map(|_| inst.process_private(0.0).abs())
            .fold(0.0, f32::max);
        assert!(head > 1.0, "resonant shelf produced no response: {head}");
        assert!(
            tail < head * 0.01,
            "shelf ringing forever: tail {tail} vs head {head}"
        );
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_keeps_crate_names_legal() {
        assert_eq!(sanitize_crate_name("my-effect 2"), "fx_my_effect_2");
        assert_eq!(sanitize_crate_name("api"), "fx_api");
        assert!(sanitize_crate_name("")
            .chars()
            .all(|c| c == '_' || c.is_alphanumeric()));
    }

    #[test]
    fn merge_and_resort_rules() {
        let stems: Vec<String> = ["clean", "fuzz", "trem"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Startup: saved order wins for the enabled block.
        let saved = vec![
            ("trem".to_string(), vec![0.5]),
            ("fuzz".to_string(), vec![]),
        ];
        let mut v = merge_entries(&[], &stems, &saved);
        assert_eq!(
            v.iter()
                .map(|e| (e.stem.as_str(), e.enabled))
                .collect::<Vec<_>>(),
            vec![("trem", true), ("fuzz", true), ("clean", false)]
        );
        // Rescan keeps the live order and drops dead scripts.
        v.push(UiEntry {
            stem: "gone".into(),
            enabled: true,
            params: vec![],
        });
        let mut v = merge_entries(&v, &stems, &[]);
        assert_eq!(
            v.iter()
                .map(|e| (e.stem.as_str(), e.enabled))
                .collect::<Vec<_>>(),
            vec![("trem", true), ("fuzz", true), ("clean", false)]
        );
        // Toggle a disabled one on: snaps to the end of the enabled block.
        v.iter_mut().find(|e| e.stem == "clean").unwrap().enabled = true;
        resort_entries(&mut v, &stems);
        assert_eq!(
            v.iter()
                .map(|e| (e.stem.as_str(), e.enabled))
                .collect::<Vec<_>>(),
            vec![("trem", true), ("fuzz", true), ("clean", true)]
        );
        // Disable the head of the chain: it drops to the disabled block,
        // the rest keeps its order.
        v[0].enabled = false;
        resort_entries(&mut v, &stems);
        assert_eq!(
            v.iter()
                .map(|e| (e.stem.as_str(), e.enabled))
                .collect::<Vec<_>>(),
            vec![("fuzz", true), ("clean", true), ("trem", false)]
        );
    }

    #[test]
    fn move_enabled_walks_the_chain_order() {
        let mut v: Vec<UiEntry> = ["a", "b", "c", "d"]
            .iter()
            .map(|s| UiEntry {
                stem: s.to_string(),
                enabled: *s != "c",
                params: vec![],
            })
            .collect();
        // v order: a(on) b(on) c(off) d(on) — enabled chain is a,b,d.
        assert!(move_enabled(&mut v, 3, true), "d moves up past b");
        assert_eq!(
            v.iter()
                .map(|e| (e.stem.as_str(), e.enabled))
                .collect::<Vec<_>>(),
            vec![("a", true), ("d", true), ("c", false), ("b", true)]
        );
        assert!(!move_enabled(&mut v, 0, true), "head cannot go up");
        assert!(
            !move_enabled(&mut v, 2, true),
            "disabled entries never move"
        );
    }

    #[test]
    fn seed_values_pads_with_defaults() {
        let d = EffectDesc {
            name: "x".into(),
            author: None,
            description: None,
            params: vec![
                ParamDesc {
                    label: "a".into(),
                    min: 0.0,
                    max: 1.0,
                    default: 0.25,
                    unit: 0,
                    step: 0.0,
                    labels: vec![],
                },
                ParamDesc {
                    label: "b".into(),
                    min: 0.0,
                    max: 2.0,
                    default: 1.0,
                    unit: 0,
                    step: 0.0,
                    labels: vec![],
                },
            ],
            sidechain: false,
            stereo: false,
        };
        assert_eq!(seed_values(&[0.9, 1.5, 9.0], &d), vec![0.9, 1.5]);
        assert_eq!(seed_values(&[], &d), vec![0.25, 1.0]);
    }

    #[test]
    fn template_compiles_and_runs() {
        // The whole pipeline the GUI relies on, on a real effect: write the
        // embedded API contract + template to a temp folder, compile, load,
        // validate, create, process, destroy.
        if !rustc_available() {
            eprintln!("skipping: rustc not on PATH");
            return;
        }
        let dir = temp_dir("template");
        std::fs::write(dir.join(API_FILE), API_SRC).unwrap();
        std::fs::write(dir.join(TEMPLATE_FILE), TEMPLATE_SRC).unwrap();
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        // Expected rows DERIVE from the bundle — a hard-coded list once
        // silently demanded an edit for every new stock effect.
        let mut want: Vec<String> = EXAMPLES
            .iter()
            .map(|(n, _)| n.trim_end_matches(".rs").to_string())
            .collect();
        want.push("template".to_string());
        want.sort();
        assert_eq!(
            reg.stems(),
            want,
            "errors: {:?}",
            reg.items().iter().map(|i| &i.error).collect::<Vec<_>>()
        );
        let desc = reg.desc("template").unwrap();
        assert_eq!(desc.name, "Template Tremolo");
        assert_eq!(desc.params.len(), 4);
        let inst = reg
            .create_instance("template", 48000.0, &[0.0, 4.0, 1.0, 1.0])
            .expect("create");
        let mut p = [0.0f32; MAX_PARAMS];
        inst.read_params(&mut p);
        assert_eq!(p[0], 0.0);
        // depth 0 => exact pass-through.
        unsafe {
            assert_eq!(inst.call_sample(0.5, &p), 0.5);
            assert_eq!(inst.call_sample(-0.25, &p), -0.25);
        }
        // Ride the depth slider: the generation bumps, a snapshot picks it
        // up, and a deep fast wobble actually wobbles.
        inst.set_param(0, 1.0);
        inst.set_param(1, 1000.0);
        inst.read_params(&mut p);
        let g = inst.param_gen();
        unsafe {
            let a = inst.call_sample(1.0, &p);
            let b = inst.call_sample(1.0, &p);
            assert!((a - b).abs() > 1e-6, "LFO must advance between samples");
            assert!((0.0..=1.0001).contains(&a) && (0.0..=1.0001).contains(&b));
        }
        assert!(g > 1);
        // Offline-render clone is independent but carries the values.
        let fresh = inst.spawn_fresh(desc).expect("spawn_fresh");
        fresh.read_params(&mut p);
        assert_eq!(p[0], 1.0);
        drop(fresh);
        drop(inst);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gui_symbol_optional_and_validated() {
        // Optional child GUI: a script exporting it loads with a handle;
        // one exporting a WRONG GUI ABI is rejected; scripts without the
        // symbol (template) load with gui() == None.
        if !rustc_available() {
            eprintln!("skipping: rustc not on PATH");
            return;
        }
        let minimal_audio = r#"
include!("effect_api.rs");
extern "C" fn create(_sr: f64) -> *mut core::ffi::c_void { core::ptr::null_mut() }
extern "C" fn destroy(_s: *mut core::ffi::c_void) {}
extern "C" fn process(_s: *mut core::ffi::c_void, i: f32, _p: *const f32) -> f32 { i }
pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI, name: c"t".as_ptr(), author: core::ptr::null(),
    description: core::ptr::null(), n_params: 0, params: core::ptr::null(),
    create, destroy, process,
};
#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 { &DESC }
"#;
        let gui_fns = r#"
extern "C" fn gcreate(_sr: f64) -> *mut core::ffi::c_void { core::ptr::null_mut() }
extern "C" fn gdraw(_g: *mut core::ffi::c_void, _s: *mut core::ffi::c_void,
    _f: *const GuiFrameV1, _a: *const GuiApiV1) {}
extern "C" fn gdestroy(_g: *mut core::ffi::c_void) {}
pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: ABI_CONST, want_width: 100.0, want_height: 100.0,
    create: gcreate, draw: gdraw, destroy: gdestroy,
};
#[no_mangle]
pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 { &GUI }
"#;
        let dir = temp_dir("gui");
        std::fs::write(dir.join(API_FILE), API_SRC).unwrap();
        std::fs::write(dir.join("plain.rs"), minimal_audio).unwrap();
        std::fs::write(
            dir.join("guied.rs"),
            format!("const ABI_CONST: u32 = SLOOPER_EFFECT_GUI_ABI;\n{minimal_audio}{gui_fns}"),
        )
        .unwrap();
        std::fs::write(
            dir.join("badgui.rs"),
            format!("const ABI_CONST: u32 = 99;\n{minimal_audio}{gui_fns}"),
        )
        .unwrap();
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let stems = reg.stems();
        assert!(
            stems.contains(&"plain".to_string()) && stems.contains(&"guied".to_string()),
            "{:?}",
            reg.items()
                .iter()
                .map(|i| (&i.stem, &i.error))
                .collect::<Vec<_>>()
        );
        assert!(
            reg.gui("plain").is_none(),
            "no GUI symbol must mean auto window"
        );
        assert!(
            reg.gui("guied").is_some(),
            "valid GUI symbol must produce a handle"
        );
        let err = reg
            .items()
            .iter()
            .find(|i| i.stem == "badgui")
            .and_then(|i| i.error.clone())
            .unwrap();
        assert!(err.contains("GUI ABI"), "{err}");
        // Size hint falls back when the effect leaves it at zero.
        drop(reg);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn v19_seed_adds_missing_stock_effects_to_existing_folder() {
        // A user folder that predates the compressor/chorus drop must gain
        // them on next launch: stock names are app-owned, so seeding no
        // longer stops at "folder already has stuff".
        let dir = temp_dir("v19_seed");
        std::fs::write(dir.join("my_thing.rs"), "// user file\nfn main(){}\n").unwrap();
        seed_api(&dir);
        for name in ["compressor.rs", "chorus.rs", API_FILE] {
            assert!(
                dir.join(name).exists(),
                "{name} must be seeded into a pre-existing folder"
            );
        }
        assert!(
            dir.join("my_thing.rs").is_file(),
            "user files are untouched"
        );
        assert!(
            !dir.join("template.rs").exists(),
            "template stays fresh-folder-only"
        );
        // Second seed is a no-op: no .bak churn when content already matches.
        seed_api(&dir);
        assert!(!dir.join("compressor.rs.bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_scripts_are_reported_not_fatal() {
        let dir = temp_dir("broken");
        std::fs::write(dir.join("ok.rs"), TEMPLATE_SRC).unwrap();
        std::fs::write(dir.join("broken.rs"), "fn main() { this is not rust }").unwrap();
        std::fs::write(dir.join("noinfo.rs"), "// compiles, but exports nothing\n").unwrap();
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        // A missing compiler is an environment issue, not a code bug.
        if !rustc_available() {
            assert!(reg
                .items()
                .iter()
                .all(|i| i.error.as_deref().unwrap_or("").contains("rustc")));
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        let mut want: Vec<String> = EXAMPLES
            .iter()
            .map(|(n, _)| n.trim_end_matches(".rs").to_string())
            .collect();
        want.push("ok".to_string());
        want.sort();
        assert_eq!(reg.stems(), want);
        let errs: HashMap<_, _> = reg
            .items()
            .iter()
            .map(|i| (i.stem.clone(), i.error.clone().unwrap_or_default()))
            .collect();
        assert!(
            errs["broken"].contains("compile failed"),
            "{}",
            errs["broken"]
        );
        assert!(
            errs["noinfo"].contains("slooper_effect_info"),
            "{}",
            errs["noinfo"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// v13 R2 AC2.3: DC-carrying transients must not leave a wash. Drum
    /// impacts, finger snaps and fast note changes carry DC/sub-20 Hz
    /// energy; every comb in the FDN runs feedback up to 0.95 (≈20 dB gain
    /// at DC) and both wet feeds copy the dry signal, so before the wet-path
    /// DC blockers a click train left ~-43 dBFS of DC-heavy residue THREE
    /// SECONDS after the sound ended — the audible "static" users reported
    /// after abrupt input changes. Cathedral params, DC-biased half-sine
    /// clicks: the tail must return to silence.
    #[test]
    fn v13_space_echo_dc_transients_leave_no_wash() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("v13dcwash");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        // Cathedral: echo low, verb full, decay 0.95 (worst DC gain).
        let cath = [
            0.32f32, 0.5, 2200.0, 0.25, 0.14, 0.95, 1500.0, 0.7, 0.9, 0.18, 1.0,
        ];
        let inst = reg
            .create_instance("space_echo", 48000.0, &cath)
            .expect("echo instance");
        let mut out: Vec<f32> = Vec::with_capacity(8 * 48000);
        for s in 0..8 * 48000 {
            let mut x = 0.0f32;
            if s < 2 * 48000 {
                let off = (s % 12000) as f32; // one impact every 250 ms
                if off < 288.0 {
                    x = (-off / 38.0).exp()
                        * (2.0 * std::f32::consts::PI * 120.0 * off / 48000.0).sin();
                }
            }
            out.push(inst.process_private(x));
        }
        let win = |a: usize, b: usize| -> (f32, f32) {
            let t = &out[a * 48000..b * 48000];
            let n = t.len() as f32;
            let mean = t.iter().sum::<f32>() / n;
            let rms = (t.iter().map(|v| v * v).sum::<f32>() / n).sqrt();
            (mean, rms)
        };
        // The room must still be ALIVE just after the last click.
        let (_, alive) = win(2, 3);
        assert!(alive > 1e-4, "wet tail vanished too (rms {alive})");
        // 4 s after the sound ended: no DC, no wash. (Pre-fix this window
        // measured mean +0.0064 / rms 0.0070 — 30x over both bounds.)
        let (mean, rms) = win(4, 6);
        assert!(mean.abs() < 2e-4, "tail DC after transients: {mean}");
        assert!(rms < 5e-4, "tail wash after transients: rms {rms}");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v14 user report: after v0.13 killed the DC wash, bright presets still
    /// answered every drum impact / snap with HIGH-PITCHED glitches around
    /// the transient — the room kept receiving the raw edge of the input
    /// (dry + echo copies at full tone brightness), the echo line re-injected
    /// the stored click into the comb bank at each round-trip, and the wet
    /// tap's DC blocker re-differentiated its edge on every repeat. Fixes:
    /// the room is now fed from the DAMPED loop state through an air-roll
    /// cap, the DC blocker lives inside the loop (plain-copy tap), and the
    /// comb bank carries air absorption.
    ///
    /// Pin: three deterministic white-noise snaps (LCG — bit-exact across
    /// runs) through the brightest shipped preset; per-snap HF (first-
    /// difference) energy in the 10–60 ms and 150–300 ms post-snap windows.
    /// Pre-fix the same windows measured −67.9 dBFS and −34.4 dBFS (worst
    /// snap) — ~5–10 dB of high-pitched residue per impact. Thresholds sit
    /// between: passes at −77.7/−40.4, fails the old build by ≥4.6 dB.
    #[test]
    fn v14_space_echo_snap_glitch_quiet_bright_room() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("v14snapglitch");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        // Brightest shipped flavor: tone 8000, damp 1100, decay 0.9, verb 0.65.
        let bright = [
            0.38f32, 0.62, 8000.0, 0.35, 0.55, 0.9, 1100.0, 1.9, 0.65, 0.3, 1.0,
        ];
        let inst = reg
            .create_instance("space_echo", 48000.0, &bright)
            .expect("bright room instance");
        let sr = 48000usize;
        let hits: [usize; 3] = [600usize, 1800, 3000].map(|ms| ms * sr / 1000);
        let mut out: Vec<f32> = Vec::with_capacity(55 * sr / 10);
        let mut s = 12345u64;
        for i in 0..55 * sr / 10 {
            let mut x = 0.0f32;
            for &h in &hits {
                if i >= h {
                    let off = i - h;
                    if off < sr / 1667 {
                        // 0.6 ms white-noise snap (LCG, deterministic — same
                        // recurrence as the offline probe that set the bounds)
                        s = s.wrapping_mul(1103515245).wrapping_add(12345) & 0x7FFF_FFFF;
                        x += 0.5 * ((s as f32 / 0x7FFF_FFFF as f32) * 2.0 - 1.0);
                    }
                    if off < sr / 20 {
                        x += 0.6
                            * (-(off as f32) / (0.006 * sr as f32)).exp()
                            * (2.0 * std::f32::consts::PI * 52.0 * off as f32 / sr as f32).sin();
                    }
                }
            }
            out.push(inst.process_private(x));
        }
        let hf_db = |a: usize, b: usize| -> f64 {
            let t = &out[a..b];
            let e: f64 = t[1..]
                .iter()
                .zip(&t[..t.len() - 1])
                .map(|(u, v)| (*u as f64 - *v as f64).powi(2))
                .sum::<f64>()
                / (t.len() - 1) as f64;
            10.0 * e.max(1e-18).log10()
        };
        for &h in &hits[1..] {
            // Skip the first snap: the room starts cold there.
            let early = hf_db(h + sr / 100, h + sr * 6 / 100);
            let plateau = hf_db(h + sr * 15 / 100, h + sr * 3 / 10);
            assert!(
                early < -73.0,
                "post-snap fizz 10-60 ms too bright: {early:.1} dBFS"
            );
            assert!(
                plateau < -38.5,
                "snap plateau 150-300 ms too bright: {plateau:.1} dBFS"
            );
        }
        // The room must still be ALIVE: broadband tail after the last snap.
        let tail = &out[7 * sr / 2..9 * sr / 2];
        let rms = (tail.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / tail.len() as f64)
            .sqrt();
        assert!(rms > 5e-6, "bright room tail vanished: rms {rms:.2e}");
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v14 companion pin for the FAST bright echo (0.15 s, feedback 0.90,
    /// tone 8 kHz) — the dub-style setting where every round-trip re-reads
    /// the stored snap edge and, pre-fix, re-injected it into the comb bank
    /// at full brightness: post-snap HF settled into a bright tick flutter
    /// at the echo period (machine-gun "brrt" after each impact). Fed from
    /// the damped loop state now, so the repeats enter the room as dark as
    /// they audibly decay. Pre-fix windows measured −37.0 / −55.1 dBFS;
    /// post-fix −48.2 / −65.7 — thresholds split the difference with ≥3 dB
    /// margin on both sides.
    #[test]
    fn v14_space_echo_fast_echo_no_tick_flutter() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("v14tickflutter");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let fast = [
            0.15f32, 0.9, 8000.0, 0.5, 0.6, 0.85, 900.0, 1.5, 0.6, 0.35, 1.0,
        ];
        let inst = reg
            .create_instance("space_echo", 48000.0, &fast)
            .expect("fast echo instance");
        let sr = 48000usize;
        let hits: [usize; 2] = [600usize, 2200].map(|ms| ms * sr / 1000);
        let mut out: Vec<f32> = Vec::with_capacity(5 * sr);
        let mut s = 12345u64;
        for i in 0..5 * sr {
            let mut x = 0.0f32;
            for &h in &hits {
                if i >= h {
                    let off = i - h;
                    if off < sr / 1667 {
                        s = s.wrapping_mul(1103515245).wrapping_add(12345) & 0x7FFF_FFFF;
                        x += 0.5 * ((s as f32 / 0x7FFF_FFFF as f32) * 2.0 - 1.0);
                    }
                    if off < sr / 20 {
                        x += 0.6
                            * (-(off as f32) / (0.006 * sr as f32)).exp()
                            * (2.0 * std::f32::consts::PI * 52.0 * off as f32 / sr as f32).sin();
                    }
                }
            }
            out.push(inst.process_private(x));
        }
        let hf_db = |a: usize, b: usize| -> f64 {
            let t = &out[a..b];
            let e: f64 = t[1..]
                .iter()
                .zip(&t[..t.len() - 1])
                .map(|(u, v)| (*u as f64 - *v as f64).powi(2))
                .sum::<f64>()
                / (t.len() - 1) as f64;
            10.0 * e.max(1e-18).log10()
        };
        let h0 = hits[1];
        let w1 = hf_db(h0 + 3 * sr / 10, h0 + 6 * sr / 10);
        let w2 = hf_db(h0 + 6 * sr / 10, h0 + 16 * sr / 10);
        assert!(
            w1 < -44.0,
            "post-snap flutter window 1 too bright: {w1:.1} dBFS"
        );
        assert!(
            w2 < -62.0,
            "post-snap flutter window 2 too bright: {w2:.1} dBFS"
        );
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_presets_folder_prefix_and_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sp-userpresets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = ensure_user_presets_dir(&dir).unwrap();
        assert!(d.is_dir());
        assert_eq!(
            d.file_name().unwrap(),
            std::ffi::OsStr::new(USER_PRESET_DIR)
        );
        // Ownership by prefix: only `{stem}-*.json` files belong to a plugin.
        std::fs::write(d.join("space_echo-Warm Room.json"), "{}").unwrap();
        std::fs::write(d.join("space_echo-Cathedral Big.json"), "{}").unwrap();
        std::fs::write(d.join("parametric_eq-Warm Room.json"), "{}").unwrap();
        std::fs::write(d.join("space_echo-notes.txt"), "").unwrap();
        std::fs::write(d.join("unowned.json"), "").unwrap();
        std::fs::write(d.join("space_echo-X.json"), "{}").unwrap();
        assert_eq!(
            list_user_presets(&dir, "space_echo"),
            vec!["Cathedral Big", "Warm Room", "X"]
        );
        assert_eq!(list_user_presets(&dir, "parametric_eq"), vec!["Warm Room"]);
        assert_eq!(list_user_presets(&dir, "nope"), Vec::<String>::new());

        // JSON round trip: numbers out and back, plugin/name never leak as params.
        let params = vec![
            ("time".to_string(), 0.32f32),
            ("damp".to_string(), 1500.0),
            ("tiny".to_string(), -2.5e-3),
        ];
        let j = preset_to_json("space_echo", "Warm Room", &params).unwrap();
        let path = user_preset_path(&dir, "space_echo", "Warm Room");
        std::fs::write(&path, &j).unwrap();
        let back = preset_from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.len(), 3, "{j}");
        assert!((back[0].1 - 0.32).abs() < 1e-6);
        assert!((back[1].1 - 1500.0).abs() < 1e-3);
        assert!((back[2].1 + 0.0025).abs() < 1e-9);
        assert!(!back.iter().any(|(k, _)| k == "plugin" || k == "name"));

        // Non-finite values must not be written at all.
        assert!(preset_to_json("x", "y", &[("bad".to_string(), f32::NAN)]).is_err());

        // Name sanitizing: no traversal, no hidden files, unicode-safe.
        assert_eq!(sanitize_preset_name("../../evil"), "evil");
        assert_eq!(sanitize_preset_name("  My/Room 2  "), "My-Room 2");
        assert_eq!(sanitize_preset_name("///"), "preset");
        assert_eq!(sanitize_preset_name("Warm Room ★"), "Warm Room");

        // Delete, and idempotent delete of a gone file.
        delete_user_preset(&dir, "space_echo", "Warm Room").unwrap();
        assert_eq!(
            list_user_presets(&dir, "space_echo"),
            vec!["Cathedral Big", "X"]
        );
        delete_user_preset(&dir, "space_echo", "Warm Room").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===================== v0.10 acceptance pins =====================

    /// AC5.1: default EQ presets ship with the program (no user files) and
    /// are well-formed: named, 26 labeled values, finite, in range.
    #[test]
    fn v10_eq_ships_default_presets() {
        let defs = builtin_presets("parametric_eq");
        let names: Vec<&str> = defs.iter().map(|(n, _)| *n).collect();
        assert!(
            names.contains(&"Radio filter"),
            "radio filter preset missing: {names:?}"
        );
        assert!(
            names.contains(&"Concert hall"),
            "concert hall preset missing: {names:?}"
        );
        assert!(defs.len() >= 5, "a decent starter set: {names:?}");
        for (name, pairs) in &defs {
            assert!(!pairs.is_empty(), "{name}: empty param list");
            let mut labels: Vec<&str> = pairs.iter().map(|(l, _)| *l).collect();
            labels.sort_unstable();
            labels.dedup();
            assert_eq!(labels.len(), pairs.len(), "{name}: labels unique");
            for (label, v) in pairs {
                assert!(v.is_finite(), "{name}/{label} not finite");
                if label.ends_with("type") {
                    assert!((0.0..=7.0).contains(v), "{name}/{label} bad type {v}");
                }
            }
        }
        // v0.13: the echo+reverb rooms shipped with the program too (they
        // used to live only in the script's canvas; now the whole family is
        // seeded into the user preset folder).
        let se = builtin_presets("space_echo");
        assert_eq!(se.len(), 5, "five shipped rooms: {se:?}");
        assert_eq!(
            se[0].1.len(),
            12,
            "every room sets all twelve params (tier 5 appended width)"
        );
    }

    /// AC5.2: a default preset lands on a REAL compiled EQ instance —
    /// applying "Radio filter" must visibly crush the low end it promises.
    #[test]
    fn v10_default_preset_lands_on_the_chain() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("v10-eqpreset");
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let desc = reg.desc("parametric_eq").expect("desc").clone();

        let measure = |apply: bool, freq: f32| -> f32 {
            let inst = reg
                .create_instance("parametric_eq", 48_000.0, &[])
                .expect("eq");
            if apply {
                let (_, pairs) = builtin_presets("parametric_eq")
                    .into_iter()
                    .find(|(n, _)| *n == "Radio filter")
                    .unwrap();
                // Exactly the fxgui label lookup (clamped, unknown skipped).
                for (label, v) in &pairs {
                    if let Some((k, p)) = desc
                        .params
                        .iter()
                        .enumerate()
                        .find(|(_, p)| p.label == *label)
                    {
                        inst.set_param(k, v.clamp(p.min, p.max));
                    }
                }
            }
            // Let coefficient glide settle, then measure.
            let mut rms = 0.0f64;
            let n = 48_000usize / 2;
            for i in 0..n {
                let t = i as f32 / 48_000.0;
                let s = (t * freq * std::f32::consts::TAU).sin() * 0.5;
                let y = inst.process_private(s);
                if i > n / 2 {
                    rms += (y as f64) * (y as f64);
                }
            }
            (rms / ((n / 2) as f64)).sqrt() as f32
        };

        let flat_lo = measure(false, 100.0);
        let radio_lo = measure(true, 100.0);
        let radio_hi = measure(true, 1200.0);
        assert!(flat_lo > 0.2, "flat EQ passes 100 Hz (got {flat_lo})");
        assert!(
            radio_lo < flat_lo / 4.0,
            "radio filter must gut 100 Hz: flat {flat_lo} vs {radio_lo}"
        );
        assert!(
            radio_hi > radio_lo * 4.0,
            "…while keeping presence band (got {radio_hi})"
        );
        drop(reg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===================== v0.13 acceptance pins =====================

    /// AC1.1/AC1.2/AC1.5: factory presets are ordinary files in the USER
    /// preset folder after seeding — same list, same loader, same delete —
    /// and the seeding obeys the ownership rule: seeded ONCE, then the
    /// folder is user territory (edits and deletions survive everything).
    #[test]
    fn v13_fx_factory_presets_merge_into_user_folder() {
        let dir = temp_dir("v13seedpresets");
        // AC1.1: prepare_effects_dir (the launch path) seeds factory AND
        // user presets into one folder, and one list shows every name.
        std::env::set_var("SLOOPER_EFFECTS_DIR", &dir);
        let dir = prepare_effects_dir(None);
        let names = list_user_presets(&dir, "parametric_eq");
        for want in [
            "Radio filter",
            "Concert hall",
            "Podcast voice",
            "Bass boost",
            "Lo-fi tape",
            "Treble sparkle",
        ] {
            assert!(
                names.iter().any(|n| n == want),
                "factory '{want}' not in the preset list: {names:?}"
            );
        }
        assert_eq!(
            list_user_presets(&dir, "space_echo").len(),
            5,
            "five rooms listed"
        );
        // AC1.2: a seeded file is real, parseable, and loads through the
        // ordinary user-preset loader path.
        let p = user_preset_path(&dir, "parametric_eq", "Podcast voice");
        let parsed = preset_from_json(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(parsed.len(), 26, "one value per EQ param");
        assert!(parsed
            .iter()
            .any(|(l, v)| l == "B3 gain" && (*v - 3.0).abs() < 1e-6));
        // A user preset saved under a NEW name joins the same list.
        std::fs::write(
            user_preset_path(&dir, "parametric_eq", "My thing"),
            "{\"params\":{\"mix\":0.5}}",
        )
        .unwrap();
        let after = list_user_presets(&dir, "parametric_eq");
        assert!(
            after.contains(&"My thing".to_string()) && after.contains(&"Bass boost".to_string())
        );

        // AC1.5 ownership: after the one-time seed NOTHING is rewritten and
        // NOTHING is re-added — user edits and deletions are permanent…
        std::fs::write(&p, "{\"params\":{\"mix\":0.25}}").unwrap();
        delete_user_preset(&dir, "parametric_eq", "Bass boost").unwrap();
        assert_eq!(
            seed_builtin_presets(&dir),
            0,
            "seeded folder is never re-touched"
        );
        assert_eq!(prepare_effects_dir(None), dir, "launch path is idempotent");
        std::env::remove_var("SLOOPER_EFFECTS_DIR");
        assert!(
            std::fs::read_to_string(&p).unwrap().contains("0.25"),
            "user edit was rewritten"
        );
        assert!(!list_user_presets(&dir, "parametric_eq").contains(&"Bass boost".to_string()));
        // …while a brand-new folder (no marker) does get the starter set,
        // and an existing hand-written file under a factory name survives.
        let dir2 = temp_dir("v13seedpresets2");
        std::fs::create_dir_all(user_presets_dir(&dir2)).unwrap();
        std::fs::write(
            user_preset_path(&dir2, "parametric_eq", "Bass boost"),
            "{\"params\":{\"mix\":0.7}}",
        )
        .unwrap();
        assert_eq!(
            seed_builtin_presets(&dir2),
            10,
            "11 factory presets minus the hand-written one"
        );
        assert_eq!(
            std::fs::read_to_string(user_preset_path(&dir2, "parametric_eq", "Bass boost"))
                .unwrap(),
            "{\"params\":{\"mix\":0.7}}",
            "a hand-made file under a factory name must never be overwritten"
        );
        // A foreign file in the folder is never touched either.
        std::fs::write(user_presets_dir(&dir).join("other-thing.json"), "x").unwrap();
        seed_builtin_presets(&dir);
        assert_eq!(
            std::fs::read_to_string(user_presets_dir(&dir).join("other-thing.json")).unwrap(),
            "x"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn bundled_effects_produce_sound() {
        // Headless "did it actually change audio" gate for the native-suite
        // tier (docs/EFFECT-SUITE.md): every bundled effect runs a fixed
        // fixture (tone + percussive bursts) through the REAL loader as a
        // private instance at default params. Output must stay finite and
        // in range, and must not be silent — the compile+load tests prove
        // validity; this proves audible intent.
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("render");
        // The convolver scans a folder at create — keep it in the sandbox.
        std::env::set_var("SLOOPER_IMPULSES_DIR", dir.join("impulses"));
        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let sr = 44100.0f64;
        let n = 22050usize;
        let fixture: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let env = if i < n / 4 { 0.9 } else { 0.3 };
                env * (core::f32::consts::TAU * 220.0 * t).sin()
                    + if i % 7350 < 300 {
                        0.5 * (core::f32::consts::TAU * 4400.0 * t).sin()
                    } else {
                        0.0
                    }
            })
            .collect();
        for (name, _) in EXAMPLES {
            let stem = name.trim_end_matches(".rs");
            let inst = reg
                .create_instance(stem, sr, &[])
                .unwrap_or_else(|| panic!("{stem}: no instance"));
            let mut peak = 0.0f32;
            let mut energy = 0.0f64;
            for &x in &fixture {
                let y = inst.process_private(x);
                assert!(y.is_finite(), "{stem}: produced {y}");
                peak = peak.max(y.abs());
                energy += (y as f64) * (y as f64);
            }
            let rms = (energy / n as f64).sqrt();
            assert!(peak <= 4.0, "{stem}: peak {peak} out of range");
            assert!(
                rms > 1e-5,
                "{stem}: silent output at default params (rms {rms})"
            );
            drop(inst);
        }
        // Sidechain path: the v2 entry must run with a live key signal.
        for stem in ["sidechain_comp", "vocoder"] {
            let inst = reg
                .create_instance(stem, sr, &[])
                .unwrap_or_else(|| panic!("{stem}: no instance"));
            for (i, &x) in fixture.iter().take(4410).enumerate() {
                let sc = if i < 2200 { x } else { 0.0 };
                let y = unsafe { inst.call_sample_sc(x, &[0.0; MAX_PARAMS], sc) };
                assert!(y.is_finite(), "{stem}: sc path produced {y}");
            }
            drop(inst);
        }
        // Tier-3 STEREO path: every bundled v3 script must (a) run the
        // pair path without NaN, (b) stay non-silent, and (c) actually
        // DECORRELATE a one-sided pulse — ol != or is the whole point.
        let mut st_checked = 0;
        for (name, _) in EXAMPLES {
            let stem = name.trim_end_matches(".rs");
            let Some(d) = reg.desc(stem) else { continue };
            if !d.stereo {
                continue;
            }
            st_checked += 1;
            let inst = reg
                .create_instance(stem, sr, &[])
                .unwrap_or_else(|| panic!("{stem}: no instance"));
            // Seed the DEFAULT slider values (a zeroed param array would
            // legitimately mute any effect whose gain defaults > 0).
            let d = reg.desc(stem).unwrap();
            let mut dp = [0.0f32; MAX_PARAMS];
            for (k, pd) in d.params.iter().enumerate() {
                dp[k] = pd.default;
            }
            let (mut peak, mut depro, mut energy) = (0.0f32, 0.0f64, 0.0f64);
            for (i, &x) in fixture.iter().enumerate() {
                // Left-only feed, with occasional right content too so
                // internal side-branches (delay lines) see both rails.
                let (l, r) = if i % 7000 < 3500 {
                    (x, 0.0)
                } else {
                    (x * 0.5, x)
                };
                // Tier-5b: feed a LIVE key — a v3 script that wants the
                // sidechain (vocoder) reads the `sc` arg of process_st,
                // and sc=0 legitimately mutes it (silent-modulator design),
                // which is NOT the decorrelation gate's business.
                let (ol, or) = unsafe { inst.call_sample_st(l, r, &dp, x) };
                assert!(ol.is_finite() && or.is_finite(), "{stem}: st path NaN");
                peak = peak.max(ol.abs()).max(or.abs());
                let d = (ol - or) as f64;
                depro += d * d;
                energy += (ol as f64).powi(2) + (or as f64).powi(2);
            }
            let rms = (energy / (2 * n) as f64).sqrt();
            assert!(peak <= 4.0, "{stem}: st peak {peak} out of range");
            assert!(rms > 1e-5, "{stem}: st path silent (rms {rms})");
            let decor = (depro / n as f64).sqrt();
            assert!(
                decor > 1e-3,
                "{stem}: st path never decorrelated L/R ({decor})"
            );
            drop(inst);
        }
        assert!(
            st_checked >= 11,
            "expected the v3 spread/autopan/chorus/delay/space_echo/phaser/flanger/shifter/gater/vocoder/pitchshft to load as stereo, got {st_checked}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tier-4 regression (docs/EFFECT-SUITE.md rework log): tier-1 delay and
    /// flanger `read_lin` indexed the ring at ABSOLUTE slots, and the
    /// chorus feedback chased its own filter state — all three compiled,
    /// loaded and passed the non-silence gate while producing NO usable
    /// delay/feedback (an impulse came out as dry + a faint smear; RMS
    /// gates are blind to a starved feedback loop). This pin feeds an
    /// impulse / noise burst and demands real echo structure: delay
    /// places discrete echoes with the right decay ratio at its default
    /// 320 ms, the chorus feedback sustains the tail long after the
    /// input is gone, the flanger comb rings, and the ping-pong right
    /// ear echo arrives a fraction of a repeat LATE.
    #[test]
    fn tier4_feedback_loops_ring() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("t4fbk");
        let mut reg = Registry::open(dir.clone());
        for (name, src) in EXAMPLES {
            std::fs::write(dir.join(name), *src).unwrap();
        }
        reg.rescan();
        let sr = 48000.0f64;
        let n = (sr * 1.25) as usize;

        // ---- delay: the default 320 ms slider = real decaying echoes ----
        let inst = reg.create_instance("delay", sr, &[]).expect("delay inst");
        let mut y = Vec::with_capacity(n);
        for i in 0..n {
            y.push(inst.process_private(if i == 100 { 1.0 } else { 0.0 }));
        }
        let win_peak = |a: f64, b: f64, y: &[f32]| -> f32 {
            y[(100.0 + a) as usize..(100.0 + b) as usize]
                .iter()
                .fold(0.0f32, |m, v| m.max(v.abs()))
        };
        let dry = win_peak(0.0, 20.0, &y);
        let t = 0.32 * sr; // default time = 320 ms
        let e1 = win_peak(t - 500.0, t + 500.0, &y);
        let e2 = win_peak(2.0 * t - 500.0, 2.0 * t + 500.0, &y);
        assert!(dry > 0.5, "delay dry click missing ({dry})");
        assert!(
            e1 > dry * 0.15 && e1 < dry * 0.6,
            "delay echo1 not at the 320 ms default (peak {e1}, dry {dry})"
        );
        let ratio = e2 / e1.max(1e-6);
        assert!(
            ratio > 0.25 && ratio < 0.85,
            "delay echo2/echo1 ratio {ratio} off the 0.55-feedback decay"
        );
        drop(inst);

        // ---- delay ping-pong (v3): the R echo lags pp * 320 ms --------
        let inst = reg
            .create_instance("delay", sr, &[])
            .expect("delay st inst");
        let mut dp = [0.0f32; MAX_PARAMS];
        for (k, pd) in reg.desc("delay").unwrap().params.iter().enumerate() {
            dp[k] = pd.default;
        }
        let (mut yl, mut yr) = (Vec::new(), Vec::new());
        for i in 0..n {
            let (l, r) =
                unsafe { inst.call_sample_st(if i == 100 { 1.0 } else { 0.0 }, 0.0, &dp, 0.0) };
            yl.push(l);
            yr.push(r);
        }
        drop(inst);
        let r_lag = win_peak(1.7 * t - 600.0, 1.7 * t + 600.0, &yr);
        let r_ontime = win_peak(t + 600.0, 1.7 * t - 700.0, &yr);
        assert!(r_lag > 0.02, "ping-pong R echo never arrived ({r_lag})");
        assert!(
            r_lag > r_ontime * 2.5,
            "R echo is NOT late: lag-window {r_lag} vs on-time-window {r_ontime}"
        );

        // ---- chorus feedback is ALIVE (tier-1: dead code, always 0) ----
        let burst_rms_after = |fb: f32| -> f64 {
            let p = [0.8, 3.5, 12.0, fb, 5500.0, 1.0, 1.0, 1.0, 1.0];
            let inst = reg.create_instance("chorus", sr, &p).expect("chorus inst");
            let mut e = 0.0f64;
            let m = (sr * 1.0) as usize;
            for i in 0..m {
                let x = if i < (sr * 0.2) as usize {
                    let h = (i as u32).wrapping_mul(2654435761);
                    ((h as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.6
                } else {
                    0.0
                };
                let y = inst.process_private(x);
                if i > (sr * 0.55) as usize {
                    e += (y as f64) * (y as f64);
                }
            }
            drop(inst);
            (e / (m - (sr * 0.55) as usize) as f64).sqrt()
        };
        let lo = burst_rms_after(0.0);
        let hi = burst_rms_after(0.85);
        assert!(
            hi > 5e-7 && hi > lo * 50.0,
            "chorus feedback does not sustain a tail (fb0 {lo}, fb0.85 {hi})"
        );

        // ---- flanger comb rings with feedback --------------------------
        let burst_rms_fl = |fb: f32| -> f64 {
            let p = [0.22, 0.7, 1.8, fb, 3.0, 0.5, 1.0];
            let inst = reg
                .create_instance("flanger", sr, &p)
                .expect("flanger inst");
            let mut e = 0.0f64;
            let m = (sr * 1.0) as usize;
            for i in 0..m {
                let x = if i < (sr * 0.4) as usize {
                    let h = (i as u32).wrapping_mul(2654435761);
                    ((h as f32 / u32::MAX as f32) * 2.0 - 1.0) * 0.6
                } else {
                    0.0
                };
                let y = inst.process_private(x);
                // Ring window: 20..60 ms after the burst ends — a 1.8 ms
                // comb at 0.9 is down 20 dB/20 ms, so the LATE window a
                // long-delay script would use would see only silence.
                if i > (sr * 0.42) as usize && i < (sr * 0.46) as usize {
                    e += (y as f64) * (y as f64);
                }
            }
            drop(inst);
            (e / ((sr * 0.46) as usize - (sr * 0.42) as usize) as f64).sqrt()
        };
        let flo = burst_rms_fl(0.0);
        let fhi = burst_rms_fl(0.9);
        assert!(
            fhi > 1e-6 && fhi > flo * 20.0,
            "flanger feedback comb does not ring (fb0 {flo}, fb0.9 {fhi})"
        );
        // Sabotage-proof absolute: with the tier-1 absolute read the tap
        // never contained the impulse (impulse lands at slot 100, the tap
        // reads slot ~86) — the wet was EXACTLY zero. Demand real energy.
        {
            let p = [0.22f32, 0.7, 1.8, 0.9, 3.0, 0.5, 1.0];
            let inst = reg.create_instance("flanger", sr, &p).expect("flanger imp");
            let mut e = 0.0f64;
            for i in 0..(sr * 0.2) as usize {
                let y = inst.process_private(if i == 1000 { 1.0 } else { 0.0 });
                if i > (sr * 0.03) as usize {
                    e += (y as f64) * (y as f64);
                }
            }
            drop(inst);
            let late = (e / ((sr * 0.2) as usize - (sr * 0.03) as usize) as f64).sqrt();
            assert!(
                late > 1e-5,
                "flanger impulse leaves no comb ringing 30 ms late ({late})"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Native Suite tier 5 pins (docs/EFFECT-SUITE.md): space_echo went ABI-v3
    /// STEREO (algorithmic-room task). Impulse-STRUCTURE shape per the
    /// tier-4 rule — energy gates are blind to dead loops, and here they
    /// would ALSO be blind to a fake "stereo" that is just the same mono
    /// tail twice. Asserts (mono feed l == r so every difference is the
    /// effect's own decorrelation):
    /// 1. discrete echoes at the default 380 ms with plausible decay —
    ///    the per-side echo loops actually recirculate on the ST path;
    /// 2. the late tail DECORRELATES (L-R energy vs tail energy) while the
    ///    same run at width = 0 is BIT-IDENTICAL left/right — the width
    ///    knob owns a real cross-mix (and this double assertion IS the
    ///    negative control: one run must pass, the other must be exactly 0);
    /// 3. the v1 fallback (`process`) equals the ST path's LEFT channel at
    ///    default width — mono hosts keep the historical circuit exactly.
    #[test]
    fn tier5_space_echo_stereo_tail() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("t5rev");
        let mut reg = Registry::open(dir.clone());
        for (name, src) in EXAMPLES {
            std::fs::write(dir.join(name), *src).unwrap();
        }
        reg.rescan();
        let d = reg.desc("space_echo").expect("space_echo desc").clone();
        assert!(d.stereo, "space_echo must load as an ABI-v3 stereo script");
        let sr = 48000.0f64;
        let n = (sr * 2.5) as usize;
        let imp = |i: usize| if i == 100 { 1.0f32 } else { 0.0 };

        // Room-leaning settings for the STRUCTURE pins.
        let mut dp = [0.0f32; MAX_PARAMS];
        for (k, pd) in d.params.iter().enumerate() {
            dp[k] = pd.default;
        }
        dp[5] = 0.95; // decay (max — the tail must outlive the echoes)
        dp[7] = 0.9; // mix

        // Pin 1: ECHO ONLY (verb closed) — discrete repeats with no room
        // smear standing in for the loop.
        dp[7] = 0.6; // 60 % wet: 0.4 dry click + roomy echo
        dp[9] = 0.6; // echo
        dp[10] = 0.0; // verb off
        let (yl, yr) = {
            let inst = reg
                .create_instance("space_echo", sr, &dp)
                .expect("echo inst");
            let mut p = dp;
            p[11] = 1.0;
            let (mut yl, mut yr) = (Vec::with_capacity(n), Vec::with_capacity(n));
            for i in 0..n {
                let (l, r) = unsafe { inst.call_sample_st(imp(i), imp(i), &p, 0.0) };
                yl.push(l);
                yr.push(r);
            }
            drop(inst);
            (yl, yr)
        };

        // ---- 1. discrete echoes at the default 380 ms ------------------
        let win_peak = |a: f64, b: f64, y: &[f32]| -> f32 {
            y[(100.0 + a) as usize..(100.0 + b) as usize]
                .iter()
                .fold(0.0f32, |m, v| m.max(v.abs()))
        };
        let dry = win_peak(0.0, 20.0, &yl);
        let t = 0.38 * sr; // default time slider
        let e1 = win_peak(t - 500.0, t + 500.0, &yl);
        let e2 = win_peak(2.0 * t - 500.0, 2.0 * t + 500.0, &yl);
        eprintln!("T5 dry {dry} e1 {e1} e2 {e2} ratio {}", e2 / e1.max(1e-9));
        assert!(dry > 0.3, "space_echo dry click missing ({dry})");
        assert!(
            e1 > dry * 0.10 && e1 < dry * 0.9,
            "space_echo echo1 not at the 380 ms default (peak {e1}, dry {dry})"
        );
        let ratio = e2 / e1.max(1e-6);
        assert!(
            ratio > 0.15 && ratio < 0.95,
            "space_echo echo2/echo1 ratio {ratio} off the 0.62-feedback decay"
        );
        // A CENTER impulse must exit bit-centered: the two echo paths
        // share tuning, tap `d` and LFO phase, so identical sides run the
        // identical circuit (measured max diff = 0 bit over the full run).
        // Catches any future accidental L/R asymmetry in the echo path.
        let asymmetric = yl
            .iter()
            .zip(&yr)
            .filter(|(a, b)| a.to_bits() != b.to_bits())
            .count();
        assert_eq!(
            asymmetric, 0,
            "echo-only CENTER input did not stay bit-centered (asymmetric samples: {asymmetric})"
        );

        // Pin 2: REVERB ONLY (echo closed) — the correlated echo component
        // would sink the decorrelation ratio (measured 0.17 with echo at
        // 0.6: the shared echo is 100 % correlated BY DESIGN and belongs
        // to the middle, not the sides; only the tail decorrelates).
        dp[7] = 0.9; // wet-dominated for the tail measurement
        dp[9] = 0.0; // echo off
        dp[10] = 1.5; // verb full

        let run_st = |width: f32| -> (Vec<f32>, Vec<f32>) {
            let inst = reg.create_instance("space_echo", sr, &dp).expect("st inst");
            let mut p = dp;
            p[11] = width;
            let (mut yl, mut yr) = (Vec::with_capacity(n), Vec::with_capacity(n));
            for i in 0..n {
                let (l, r) = unsafe { inst.call_sample_st(imp(i), imp(i), &p, 0.0) };
                yl.push(l);
                yr.push(r);
            }
            drop(inst);
            (yl, yr)
        };
        let (yl, yr) = run_st(1.0);

        // ---- 2. late tail decorrelates (width 1) ------------------------
        let tail = |yl: &[f32], yr: &[f32]| -> (f64, f64, f64) {
            let (a, b) = ((sr * 0.5) as usize + 100, (sr * 0.9) as usize + 100);
            let (mut el, mut er, mut ed) = (0.0f64, 0.0f64, 0.0f64);
            for i in a..b {
                el += (yl[i] as f64) * (yl[i] as f64);
                er += (yr[i] as f64) * (yr[i] as f64);
                let g = (yl[i] - yr[i]) as f64;
                ed += g * g;
            }
            let m = (b - a) as f64;
            ((el / m).sqrt(), (er / m).sqrt(), (ed / m).sqrt())
        };
        let (tl, tr, td) = tail(&yl, &yr);
        let mean_tail = (tl + tr) * 0.5;
        eprintln!(
            "T5 tail w1: L {tl} R {tr} D {td} ratio {}",
            td / mean_tail.max(1e-12)
        );
        assert!(
            mean_tail > 1e-4,
            "space_echo room tail died (rms {mean_tail})"
        );
        assert!(
            td > mean_tail * 0.35,
            "stereo room tail never decorrelated (dif {td} vs tail {mean_tail})"
        );
        // Hard-panned input keeps its side (echo re-opened for this one:
        // a source hard-panned left must ECHO left, not split).
        let inst = reg
            .create_instance("space_echo", sr, &dp)
            .expect("pan inst");
        let mut p1 = dp;
        p1[9] = 0.6; // echo on
        p1[10] = 0.0; // verb off
        p1[11] = 1.0;
        let (mut pl, mut pr) = (0.0f64, 0.0f64);
        // Window: everything between the dry click and just past echo 1 —
        // the echo must arrive LEFT, with (at width 1) little in R.
        let (w0, w1) = (900usize, (t + 600.0) as usize);
        for i in 0..w1 {
            let (l, r) =
                unsafe { inst.call_sample_st(if i == 100 { 1.0 } else { 0.0 }, 0.0, &p1, 0.0) };
            if i >= w0 {
                pl += (l as f64) * (l as f64);
                pr += (r as f64) * (r as f64);
            }
        }
        drop(inst);
        eprintln!("T5 pan L-energy {pl} R-energy {pr}");
        assert!(
            pr < pl * 0.5,
            "L-only feed did not stay left in the echo (L {pl} R {pr})"
        );

        // ---- 2b. width = 0 => BIT-IDENTICAL tail (exact mono) -----------
        let (wl, wr) = run_st(0.0);
        let (tl0, tr0, td0) = tail(&wl, &wr);
        let exact = wl
            .iter()
            .zip(wr.iter())
            .all(|(a, b)| a.to_bits() == b.to_bits());
        eprintln!("T5 tail w0: L {tl0} R {tr0} D {td0} bitexact {exact}");
        assert!(
            td0 == 0.0 && exact,
            "width 0 is not an exact mono tail (dif {td0})"
        );

        // ---- 3. v1 fallback == ST path LEFT at default width ------------
        // Self-contained pair of fresh runs (same frozen params) so the
        // comparison can never drift from another section's settings.
        {
            let p = dp; // echo off, verb full, width default 1.0
            let st = reg.create_instance("space_echo", sr, &p).expect("st3");
            let mono = reg.create_instance("space_echo", sr, &p).expect("v13");
            let mut exact3 = true;
            for i in 0..n {
                let l = unsafe { st.call_sample_st(imp(i), imp(i), &p, 0.0) }.0;
                let y = mono.process_private(imp(i));
                if y.to_bits() != l.to_bits() {
                    exact3 = false;
                    break;
                }
            }
            drop(st);
            drop(mono);
            assert!(
                exact3,
                "v1 mono path drifted from the ST path's left channel"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tier 5 persistence guard: param indices are STICKY per script stem
    /// (saved chain rows reference them by index), so the stereo upgrade
    /// had to APPEND `width` — this pin freezes the historical label list
    /// and defaults in order and fails loudly if anyone reorders, renames
    /// or re-tunes 0..10, or flips the v3 flag semantics (stereo yes,
    /// sidechain no: `desc.sidechain` is exact-v2-only, tier-3b rule).
    #[test]
    fn tier5_space_echo_params_appended_only() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("t5params");
        let mut reg = Registry::open(dir.clone());
        for (name, src) in EXAMPLES {
            std::fs::write(dir.join(name), *src).unwrap();
        }
        reg.rescan();
        let d = reg.desc("space_echo").expect("space_echo desc");
        let want: [(&str, f32); 12] = [
            ("time", 0.38),
            ("feedback", 0.62),
            ("tone", 3600.0),
            ("wobble", 0.0),
            ("rate", 0.6),
            ("decay", 0.84),
            ("damp", 2800.0),
            ("mix", 0.4),
            ("out", 1.0),
            ("echo", 1.0),
            ("verb", 1.0),
            ("width", 1.0),
        ];
        assert_eq!(d.params.len(), 12);
        for (k, (lab, def)) in want.into_iter().enumerate() {
            assert_eq!(d.params[k].label, lab, "param {k} label changed");
            assert!(
                (d.params[k].default - def).abs() < 1e-6,
                "param {k} default changed"
            );
        }
        assert!(d.stereo, "tier-5 space_echo is an ABI-v3 script");
        assert!(
            !d.sidechain,
            "v3 flag semantics: sidechain is exact-v2-only"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Native Suite tier 5 candidate #2 (docs/EFFECT-SUITE.md): phaser and flanger
    /// upgraded to ABI-v3 stereo (dual-path vintage sweep / shared comb
    /// with opposite-sweep sides). STRUCTURE pins per the tier-4 rule —
    /// an energy gate cannot tell a real stereo script from one mono
    /// signal played twice:
    ///  * spread 0 ⇒ BIT-identical L/R for identical inputs,
    ///  * default spread ⇒ decorrelated sweep (probe-measured D/RMS:
    ///    phaser 1.27, flanger 0.67 — thresholds well below),
    ///  * flanger hard-panned L: dry keeps its side (R exactly 0 in the
    ///    dry window) while the shared comb feeds BOTH sides,
    ///  * v1 fallback ≡ ST-path LEFT bit-for-bit — the tier-4 mono pins
    ///    and every pre-tier-5 chain keep measuring the same circuit.
    ///
    /// Also freezes the param tables (0..6 sticky, 7 = appended `spread`).
    #[test]
    fn tier5_phaser_flanger_stereo() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("t5pf");
        let mut reg = Registry::open(dir.clone());
        for (name, src) in EXAMPLES {
            std::fs::write(dir.join(name), *src).unwrap();
        }
        reg.rescan();

        // Deterministic LCG noise (never std::rand — pins must replay).
        let n: usize = 14400;
        let mut noise = Vec::with_capacity(n);
        let mut seed: u32 = 0x5eed_1234;
        for _ in 0..n {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            noise.push(((seed >> 8) as f32) * (2.0 / 16777216.0) - 1.0);
        }
        let sr = 48000.0f64;

        for (stem, min_decor) in [("phaser", 0.5f64), ("flanger", 0.3f64)] {
            let d = reg.desc(stem).unwrap_or_else(|| panic!("{stem} missing"));
            assert!(d.stereo, "{stem}: must load as an ABI-v3 stereo script");
            assert!(!d.sidechain, "{stem}: sidechain flag is exact-v2-only");
            assert_eq!(
                d.params.len(),
                8,
                "{stem}: seven sticky params + appended spread"
            );
            assert_eq!(d.params[7].label, "spread", "param 7 appended as 'spread'");
            assert!(
                (d.params[7].default - 0.5).abs() < 1e-6,
                "spread default 0.5 — wide out of the box"
            );

            let mut dp = [0.0f32; MAX_PARAMS];
            for (k, pd) in reg.desc(stem).unwrap().params.iter().enumerate() {
                dp[k] = pd.default;
            }
            let run = |spread: f32| -> (Vec<f32>, Vec<f32>) {
                let inst = reg.create_instance(stem, sr, &dp).expect("inst");
                let mut p = dp;
                p[7] = spread;
                let (mut yl, mut yr) = (Vec::with_capacity(n), Vec::with_capacity(n));
                for &sx in noise.iter().take(n) {
                    let (l, r) = unsafe { inst.call_sample_st(sx, sx, &p, 0.0) };
                    yl.push(l);
                    yr.push(r);
                }
                drop(inst);
                (yl, yr)
            };

            // 1. spread 0 => bit-exact mono for identical inputs.
            let (zl, zr) = run(0.0);
            let bad = zl
                .iter()
                .zip(&zr)
                .filter(|(a, b)| a.to_bits() != b.to_bits())
                .count();
            assert_eq!(
                bad, 0,
                "{stem}: spread 0 is not an exact-mono output ({bad} differing samples)"
            );

            // 2. default spread => the sweep splits the sides.
            let (yl, yr) = run(0.5);
            let (mut el, mut er, mut ed) = (0.0f64, 0.0f64, 0.0f64);
            for i in 2000..12000 {
                el += (yl[i] as f64).powi(2);
                er += (yr[i] as f64).powi(2);
                ed += (yl[i] as f64 - yr[i] as f64).powi(2);
            }
            let m = 10000.0;
            let decor = (ed / m).sqrt() / (((el / m).sqrt() + (er / m).sqrt()) * 0.5).max(1e-12);
            eprintln!("T5 {stem} decor {decor}");
            assert!(
                decor > min_decor,
                "{stem}: sweep never split the sides (D/RMS {decor})"
            );

            // 3. v1 fallback == ST path LEFT, bit-for-bit.
            let st = reg.create_instance(stem, sr, &dp).expect("st5");
            let mono = reg.create_instance(stem, sr, &dp).expect("v15");
            let mut drift = 0usize;
            for &sx in noise.iter().take(n) {
                let l = unsafe { st.call_sample_st(sx, sx, &dp, 0.0) }.0;
                let y = unsafe { mono.call_sample(sx, &dp) };
                if y.to_bits() != l.to_bits() {
                    drift += 1;
                }
            }
            drop(st);
            drop(mono);
            assert_eq!(
                drift, 0,
                "{stem}: v1 mono path drifted from the ST left channel"
            );
        }

        // 4. Flanger image: hard-panned L — the DRY click stays exactly
        // left, and the shared comb (mono by topology) feeds both sides
        // about equally. Both halves matter: wet-only-left would fake
        // "image preservation" while killing the width.
        {
            let mut dp = [0.0f32; MAX_PARAMS];
            for (k, pd) in reg.desc("flanger").unwrap().params.iter().enumerate() {
                dp[k] = pd.default;
            }
            let inst = reg.create_instance("flanger", sr, &dp).expect("fl inst");
            let (mut er95, mut el3, mut er3) = (0.0f64, 0.0f64, 0.0f64);
            for i in 0..20000 {
                let x = if i == 100 { 1.0f32 } else { 0.0 };
                let (l, r) = unsafe { inst.call_sample_st(x, 0.0, &dp, 0.0) };
                if (95..101).contains(&i) {
                    er95 += (r as f64).powi(2);
                }
                if (300..1500).contains(&i) {
                    el3 += (l as f64).powi(2);
                    er3 += (r as f64).powi(2);
                }
            }
            drop(inst);
            assert_eq!(er95, 0.0, "L-only impulse leaked into the RIGHT dry path");
            assert!(el3 > 1e-5, "comb did not ring in L ({el3})");
            assert!(
                er3 > 1e-5 && er3 < el3 * 4.0 && el3 < er3 * 4.0,
                "shared comb unbalanced between sides (L {el3} R {er3})"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Native Suite tier 5b (docs/EFFECT-SUITE.md): shifter / gater / pitchshft /
    /// vocoder upgraded to ABI-v3 stereo. STRUCTURE pins per the tier-4
    /// rule (energy gates cannot tell a real stereo script from one mono
    /// signal played twice). Measured probe values (2026-11-07): decor
    /// shifter 1.41, gater 0.93 (antiphase 2.00), vocoder 0.43,
    /// pitchshft 1.43; vocoder downmix error 1.1e-8 against a 0.25 peak.
    ///  * spread 0 ⇒ BIT-identical L/R for identical inputs (all four),
    ///  * default spread ⇒ decorrelation floors (halves of measured),
    ///  * true-rail scripts (shifter/gater/pitchshft): a hard-panned L
    ///    impulse leaves the R rail EXACTLY zero — no cross-feed at all,
    ///  * vocoder: the antisymmetric band interleave keeps the mono
    ///    downmix equal to the v2 mono circuit at ANY spread,
    ///  * v1 fallback ≡ ST-path LEFT bit-for-bit (vocoder: v1 stays the
    ///    documented passthrough).
    #[test]
    fn tier5b_stereo_mods() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("t5b");
        let mut reg = Registry::open(dir.clone());
        for (name, src) in EXAMPLES {
            std::fs::write(dir.join(name), *src).unwrap();
        }
        reg.rescan();

        let n: usize = 14400;
        let mut noise = Vec::with_capacity(n);
        let mut seed: u32 = 0x5eed_1234;
        for _ in 0..n {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            noise.push(((seed >> 8) as f32) * (2.0 / 16777216.0) - 1.0);
        }
        let sr = 48000.0f64;
        let defp = |stem: &str| -> [f32; MAX_PARAMS] {
            let mut dp = [0.0f32; MAX_PARAMS];
            for (k, pd) in reg.desc(stem).unwrap().params.iter().enumerate() {
                dp[k] = pd.default;
            }
            dp
        };

        // ---- shared shape: bit-exact 0, decor floor, hard-pan image,
        // fallback identity — for the three true-rail scripts.
        // (stem, spread idx, decor floor, extra param tweak)
        for (stem, sidx, min_decor, tweak) in [
            ("shifter", 4usize, 0.6f64, 0.0f32),
            ("gater", 7, 0.5, 0.0),
            ("pitchshft", 10, 0.6, 5.0),
        ] {
            let d = reg.desc(stem).unwrap_or_else(|| panic!("{stem} missing"));
            assert!(d.stereo, "{stem}: must load as an ABI-v3 stereo script");
            assert!(!d.sidechain, "{stem}: sidechain flag is exact-v2-only");
            assert!(
                d.params[sidx].label.starts_with("spread"),
                "{stem}: param {sidx} is spread"
            );
            let mut dp = defp(stem);
            if tweak != 0.0 {
                dp[1] = tweak; // pitchshft semitones: an idle shifter never decorrelates
            }
            let run = |spread: f32| -> (Vec<f32>, Vec<f32>) {
                let inst = reg.create_instance(stem, sr, &dp).expect("inst");
                let mut p = dp;
                p[sidx] = spread;
                let (mut yl, mut yr) = (Vec::with_capacity(n), Vec::with_capacity(n));
                for &sx in noise.iter().take(n) {
                    let (l, r) = unsafe { inst.call_sample_st(sx, sx, &p, 0.0) };
                    yl.push(l);
                    yr.push(r);
                }
                drop(inst);
                (yl, yr)
            };

            // 1. spread 0 => bit-exact mono for identical inputs.
            let (zl, zr) = run(0.0);
            let bad = zl
                .iter()
                .zip(&zr)
                .filter(|(a, b)| a.to_bits() != b.to_bits())
                .count();
            assert_eq!(
                bad, 0,
                "{stem}: spread 0 is not an exact-mono output ({bad} differing samples)"
            );

            // 2. default spread => the sides split.
            let (yl, yr) = run(0.5);
            let (mut el, mut er, mut ed) = (0.0f64, 0.0f64, 0.0f64);
            for i in 2000..12000 {
                el += (yl[i] as f64).powi(2);
                er += (yr[i] as f64).powi(2);
                ed += (yl[i] as f64 - yr[i] as f64).powi(2);
            }
            let m = 10000.0;
            let decor = (ed / m).sqrt() / (((el / m).sqrt() + (er / m).sqrt()) * 0.5).max(1e-12);
            eprintln!("T5b {stem} decor {decor}");
            assert!(
                decor > min_decor,
                "{stem}: the spread never split the sides (D/RMS {decor})"
            );

            // 3. hard-panned L impulse: the R rail stays EXACTLY zero —
            // these scripts are per-side circuits, any R energy at all
            // would mean a cross-feed nobody asked for. WINDOW 12000:
            // pitchshft's mod delay line is 6000 samples deep, so a
            // shorter window cannot reach the wet taps and a wet
            // cross-feed sabotage would sail through (tier-5b sabotage
            // matrix caught exactly this).
            let inst = reg.create_instance(stem, sr, &dp).expect("inst5b");
            let mut p = dp;
            p[sidx] = 0.5;
            let mut ermax = 0.0f32;
            for i in 0..12000usize {
                let x = if i == 100 { 1.0f32 } else { 0.0 };
                let (_, r) = unsafe { inst.call_sample_st(x, 0.0, &p, 0.0) };
                ermax = ermax.max(r.abs());
            }
            drop(inst);
            assert_eq!(
                ermax, 0.0,
                "{stem}: hard-panned L leaked {ermax} into the R rail"
            );

            // 4. v1 fallback == ST path LEFT, bit-for-bit.
            let st = reg.create_instance(stem, sr, &dp).expect("st5b");
            let mono = reg.create_instance(stem, sr, &dp).expect("v15b");
            let mut drift = 0usize;
            for &sx in noise.iter().take(n) {
                let l = unsafe { st.call_sample_st(sx, sx, &dp, 0.0) }.0;
                let y = unsafe { mono.call_sample(sx, &dp) };
                if y.to_bits() != l.to_bits() {
                    drift += 1;
                }
            }
            drop(st);
            drop(mono);
            assert_eq!(
                drift, 0,
                "{stem}: v1 mono path drifted from the ST left channel"
            );
        }

        // ---- vocoder: shared envelopes + antisymmetric band interleave.
        {
            let stem = "vocoder";
            let d = reg.desc(stem).unwrap();
            assert!(d.stereo, "vocoder must load through the v3 entry");
            assert!(
                !d.sidechain,
                "vocoder flags stereo, never sidechain (tier-3b semantics)"
            );
            let dp = defp(stem);
            assert_eq!(dp[8], 0.5, "vocoder spread default 0.5");
            // Carrier AND key ride the same noise stream (envs must open).
            let run = |spread: f32| -> (Vec<f32>, Vec<f32>) {
                let inst = reg.create_instance(stem, sr, &dp).expect("voc inst");
                let mut p = dp;
                p[8] = spread;
                let (mut yl, mut yr) = (Vec::with_capacity(n), Vec::with_capacity(n));
                for &sx in noise.iter().take(n) {
                    let (l, r) = unsafe { inst.call_sample_st(sx, sx, &p, sx) };
                    yl.push(l);
                    yr.push(r);
                }
                drop(inst);
                (yl, yr)
            };
            let (zl, zr) = run(0.0);
            let bad = zl
                .iter()
                .zip(&zr)
                .filter(|(a, b)| a.to_bits() != b.to_bits())
                .count();
            assert_eq!(
                bad, 0,
                "vocoder: spread 0 is not an exact-mono output ({bad} differing)"
            );

            let (yl, yr) = run(0.5);
            let (mut el, mut er, mut ed) = (0.0f64, 0.0f64, 0.0f64);
            for i in 2000..12000 {
                el += (yl[i] as f64).powi(2);
                er += (yr[i] as f64).powi(2);
                ed += (yl[i] as f64 - yr[i] as f64).powi(2);
            }
            let m = 10000.0;
            let decor = (ed / m).sqrt() / (((el / m).sqrt() + (er / m).sqrt()) * 0.5).max(1e-12);
            eprintln!("T5b vocoder decor {decor}");
            assert!(
                decor > 0.25,
                "vocoder: band interleave never split the sides (D/RMS {decor})"
            );

            // Downmix invariance: (L+R)/2 == the v2 mono circuit (a fresh
            // process_sc instance) — the side component must be exactly
            // antisymmetric, so the mono sum is untouchable by `spread`.
            let mono = reg.create_instance(stem, sr, &dp).expect("voc mono");
            let mut worst = 0.0f64;
            let mut peak = 0.0f64;
            for i in 0..n {
                let m0 = unsafe { mono.call_sample_sc(noise[i], &dp, noise[i]) };
                let dm = (yl[i] as f64 + yr[i] as f64) * 0.5;
                worst = worst.max((dm - m0 as f64).abs());
                peak = peak.max(m0.abs() as f64);
            }
            drop(mono);
            assert!(peak > 0.05, "vocoder sc path silent ({peak})");
            assert!(
                worst < peak * 1e-4,
                "vocoder: spread changed the mono downmix ({worst:.2e} vs peak {peak:.3})"
            );

            // v1 semantics: the vocoder stays a documented passthrough.
            let v1 = reg.create_instance(stem, sr, &dp).expect("voc v1");
            let mut drift = 0usize;
            for &sx in noise.iter().take(n) {
                if unsafe { v1.call_sample(sx, &dp) }.to_bits() != sx.to_bits() {
                    drift += 1;
                }
            }
            drop(v1);
            assert_eq!(drift, 0, "vocoder: v1 entry stopped passing through");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tier-5b persistence guard (mirrors tier5_space_echo_params_appended_only):
    /// chain rows index params by SLOT, so the stereo upgrade had to APPEND
    /// `spread` in all four scripts. This freezes the historical label +
    /// default tables in order and fails if anyone reorders, renames or
    /// re-tunes the sticky 0..N-1 params, or flips the v3 flag semantics.
    #[test]
    fn tier5b_params_appended_only() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("t5bparams");
        let mut reg = Registry::open(dir.clone());
        for (name, src) in EXAMPLES {
            std::fs::write(dir.join(name), *src).unwrap();
        }
        reg.rescan();
        macro_rules! freeze {
            ($stem:expr, [$($lab:expr => $def:expr),+], $spread_lab:expr) => {{
                let d = reg.desc($stem).unwrap_or_else(|| panic!("{} desc", $stem));
                let want: &[(&str, f32)] = &[$(($lab, $def)),+];
                assert_eq!(d.params.len(), want.len() + 1, "{}: exactly one appended param", $stem);
                for (k, (lab, def)) in want.iter().enumerate() {
                    assert_eq!(&d.params[k].label, lab, "{}: sticky param {k} label changed", $stem);
                    assert!((d.params[k].default - def).abs() < 1e-6, "{}: sticky param {k} default changed", $stem);
                }
                let last = d.params.len() - 1;
                assert!(d.params[last].label.starts_with("spread"), "{}: appended param must be 'spread…'", $stem);
                assert!((d.params[last].default - 0.5).abs() < 1e-6, "{}: spread default 0.5 — wide out of the box", $stem);
                assert!(d.stereo, "{}: tier-5b script must be ABI v3", $stem);
                assert!(!d.sidechain, "{}: sidechain flag is exact-v2-only", $stem);
            }};
        }
        freeze!("shifter", [
            "shift Hz" => 240.0, "feedback" => 0.0, "mix" => 1.0, "out" => 1.0
        ], "spread");
        // gater went tier-6 after tier-5b: the shape label's "(0 sine/…)"
        // hint moved into META (an enum picker renders it), and `host` was
        // appended after spread. The freeze below pins that every RANGE and
        // DEFAULT of params 0..7 survived untouched — only that one label's
        // text changed, plus the new trailing param.
        {
            let d = reg.desc("gater").expect("gater desc");
            let want: &[(&str, f32)] = &[
                ("rate", 2.0),
                ("shape", 2.0),
                ("width/decay", 0.5),
                ("smooth ms", 4.0),
                ("phase", 0.0),
                ("mix", 1.0),
                ("out", 1.0),
                ("spread (R lane offset x180deg)", 0.5),
                ("host", 0.0),
            ];
            assert_eq!(
                d.params.len(),
                want.len(),
                "gater: 9 params after host append"
            );
            for (k, (lab, def)) in want.iter().enumerate() {
                assert_eq!(&d.params[k].label, lab, "gater: param {k} label changed");
                assert!(
                    (d.params[k].default - def).abs() < 1e-6,
                    "gater: param {k} default changed"
                );
            }
            assert!(d.stereo, "gater: still an ABI-v4 stereo script");
            assert!(!d.sidechain, "gater: sidechain flag is exact-v2-only");
        }
        freeze!("vocoder", [
            "attack ms" => 5.0, "release ms" => 60.0, "peakiness" => 1.3,
            "freq low" => 220.0, "freq high" => 7500.0, "brightness" => 0.0,
            "mix" => 1.0, "out" => 1.1
        ], "spread");
        freeze!("pitchshft", [
            "mode" => 0.0, "semitones" => 0.0, "fine cents" => 0.0,
            "strength" => 0.7, "slew ms" => 15.0, "tone" => 0.8, "wet" => 1.0,
            "gain" => 1.0, "ref Hz" => 440.0, "gate dB" => -55.0
        ], "spread");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Native Suite tier 6 CAPABILITY PIN (docs/EFFECT-SUITE.md: param metadata +
    /// host sync): v4 param meta surfaces host-side as enum pickers and
    /// engineering units, and `host = song` locks a script's wheel to the
    /// transport push — sample-exact against BPM, jumping on seeks, held
    /// while paused, and self-advancing between pushes (block-size
    /// agnostic), with free mode left bit-identical for old sessions.
    #[test]
    fn tier6_param_metadata_and_host_sync() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("t6sync");
        let mut reg = Registry::open(dir.clone());
        for (name, src) in EXAMPLES {
            std::fs::write(dir.join(name), *src).unwrap();
        }
        reg.rescan();

        // ---- meta surfaces host-side ------------------------------------
        let g = reg
            .create_instance("gater", 48000.0, &[])
            .expect("gater inst");
        assert_eq!(
            g.param_meta(1).labels,
            vec!["sine", "tri", "pulse", "pluck", "rnd"]
        );
        assert_eq!(g.param_meta(8).labels, vec!["free", "song"]);
        assert_eq!(g.param_meta(0).unit, SLOOPER_UNIT_HZ);
        assert_eq!(g.param_meta(3).unit, SLOOPER_UNIT_MS);
        let plain = g.param_meta(5); // mix: declared plain
        assert_eq!((plain.unit, plain.labels.len(), plain.step), (0, 0, 0.0));
        let d = reg.desc("gater").unwrap();
        let (lo, hi) = (d.params[0].min, d.params[0].max);
        let hz = lo + 0.5 * (hi - lo);
        let want = if hz >= 1000.0 {
            format!("{:.2} kHz", hz / 1000.0)
        } else {
            format!("{:.1} Hz", hz)
        };
        assert_eq!(g.fmt_param(0, 0.5, lo, hi), want, "Hz unit formats");
        assert_eq!(g.fmt_param(8, 0.0, 0.0, 1.0), "free");
        assert_eq!(g.fmt_param(8, 1.0, 0.0, 1.0), "song");
        drop(g);
        let o = reg
            .create_instance("overdrive", 48000.0, &[])
            .expect("drive inst");
        assert_eq!(
            o.param_meta(1).labels,
            vec!["soft sat", "hard clip", "fold", "silky"]
        );
        drop(o);
        let od = reg.desc("overdrive").unwrap();
        assert!(!od.stereo, "v4 declared-mono flag must stick");
        assert!(
            !od.sidechain,
            "graceful process_sc must not read as sidechain"
        );
        assert!(
            reg.desc("autopan").unwrap().stereo,
            "autopan declares true stereo"
        );

        // ---- host = song locks the wheel ----------------------------------
        let sr = 48000.0f64;
        let bpm = 120.0f64;
        let inst = reg.create_instance("gater", sr, &[]).expect("sync inst");
        let mut dp = [0.0f32; MAX_PARAMS];
        for (k, pd) in reg.desc("gater").unwrap().params.iter().enumerate() {
            dp[k] = pd.default;
        }
        dp[0] = 0.5; // song mode: 0.5 cycles per beat = one 1 s period @120
        dp[1] = 2.0; // pulse
        dp[2] = 0.5; // duty
        dp[3] = 0.1; // nearly hard edges
        dp[8] = 1.0; // HOST = SONG
        let push = |inst: &Instance, from_beats: f64, playing: bool| {
            let pos = (from_beats * sr * 60.0 / bpm) as i64;
            inst.push_sync(&SyncInfoV1::block(pos, playing, bpm, 4.0, sr));
        };
        let feed = |inst: &Instance, n: usize, p: &[f32; MAX_PARAMS]| -> Vec<f32> {
            (0..n)
                .map(|_| unsafe { inst.call_sample_st(1.0, 1.0, p, 0.0).0 })
                .collect()
        };
        let peak = |y: &[f32]| -> f32 { y.iter().fold(0.0f32, |m, v| m.max(*v)) };

        // One full period in a single push: open for the first half-beat-
        // second, closed for the second (self-advance inside the block).
        push(&inst, 0.0, true);
        let y = feed(&inst, 48000, &dp);
        assert!(
            peak(&y[500..5000]) > 0.9,
            "song gate never opened ({})",
            peak(&y[500..5000])
        );
        assert!(
            peak(&y[30000..40000]) < 0.05,
            "song gate never closed ({})",
            peak(&y[30000..40000])
        );

        // Seek: push a pos inside the closed half → closed immediately.
        push(&inst, 1.0, true); // beat 1 = phase 0.5 = closed edge
        let y = feed(&inst, 2000, &dp);
        assert!(
            peak(&y) < 0.05,
            "seek to beat 1 did not jump the phase ({})",
            peak(&y)
        );

        // Paused: phase held at beat 0.25 (open) — output stays open.
        push(&inst, 0.25, false);
        let y = feed(&inst, 2000, &dp);
        assert!(
            peak(&y[1000..]) > 0.9,
            "paused push did not hold the open phase"
        );

        // Block-size agnostic: 1000-sample pushes along the same timeline
        // reproduce the open half / closed half split.
        push(&inst, 0.0, true);
        let mut split = Vec::with_capacity(48000);
        for b in 0..48 {
            split.extend(feed(&inst, 1000, &dp));
            push(&inst, (b + 1) as f64 * (1000.0 * bpm / (60.0 * sr)), true);
        }
        assert!(
            peak(&split[500..20000]) > 0.9,
            "split run lost the open half"
        );
        assert!(
            peak(&split[30000..47000]) < 0.05,
            "split run never closed ({})",
            peak(&split[30000..47000])
        );

        // Free mode default: the very same instance ignores sync pushes.
        dp[8] = 0.0;
        push(&inst, 0.0, true);
        let free = feed(&inst, 48000, &dp);
        assert!(peak(&free) > 0.9, "free mode must still gate normally");
        drop(inst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Native Suite tier 2 CAPABILITY PIN (docs/EFFECT-SUITE.md: folder file
    /// scan): the convolver must load `.wav` impulses from the impulses
    /// folder and the `ir` slider must select them. A delta IR has to
    /// reproduce its input as a one-hop-delayed single blip with NO tail
    /// — that only happens if WAV parse, normalization, FFT tables and
    /// partitioned convolution are all real. Index 0 (built-in hall)
    /// must instead produce a long decaying tail.
    #[test]
    fn convolver_folder_ir_delta() {
        if !rustc_available() {
            return;
        }
        let dir = temp_dir("convir");
        let imp = dir.join("impulses");
        std::fs::create_dir_all(&imp).unwrap();
        std::env::set_var("SLOOPER_IMPULSES_DIR", &imp);
        // Minimal float32 mono WAV writer: one 1.0 delta.
        fn wav_f32(path: &std::path::Path, sr: u32, x: &[f32]) {
            let data: Vec<u8> = x.iter().flat_map(|v| v.to_le_bytes()).collect();
            let mut b: Vec<u8> = Vec::new();
            b.extend_from_slice(b"RIFF");
            // NB: keep this an u32 — `36 + data.len()` is usize and its
            // to_le_bytes() is EIGHT bytes (silent 4-byte header shift).
            b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
            b.extend_from_slice(b"WAVEfmt ");
            b.extend_from_slice(&16u32.to_le_bytes());
            b.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
            b.extend_from_slice(&1u16.to_le_bytes()); // mono
            b.extend_from_slice(&sr.to_le_bytes());
            b.extend_from_slice(&(sr * 4).to_le_bytes());
            b.extend_from_slice(&4u16.to_le_bytes());
            b.extend_from_slice(&32u16.to_le_bytes());
            b.extend_from_slice(b"data");
            b.extend_from_slice(&(data.len() as u32).to_le_bytes());
            b.extend_from_slice(&data);
            std::fs::write(path, b).unwrap();
        }
        wav_f32(&imp.join("a-delta.wav"), 48_000, &[1.0]);

        let mut reg = Registry::open(dir.clone());
        reg.rescan();
        let d = reg.desc("convolver").expect("convolver desc");
        assert_eq!(d.params.len(), 4);
        let inst = reg
            .create_instance("convolver", 48_000.0, &[])
            .expect("convolver instance");

        let run = |ir: f32| -> Vec<f32> {
            let mut p = [0.0f32; MAX_PARAMS];
            p[0] = ir;
            p[1] = 1.0; // full wet
            p[2] = 1.0;
            p[3] = 20_000.0; // damping open
            (0..24_000)
                .map(|i| {
                    let x = if i == 8000 { 1.0 } else { 0.0 };
                    unsafe { inst.call_sample(x, &p) }
                })
                .collect()
        };

        let y = run(1.0); // folder delta IR
        let pre = y[..8_000].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(pre < 1e-6, "convolver leaked before input (pre {pre})");
        let win: &[f32] = &y[8_000 + 1_000..8_000 + 2_100];
        let (pi, pv) =
            win.iter().enumerate().fold(
                (0, 0.0f32),
                |a, (i, v)| if v.abs() > a.1 { (i, *v) } else { a },
            );
        assert!(pv > 0.3, "delta IR produced no blip (peak {pv})");
        let after = y[8_000 + 1_000 + pi + 40..]
            .iter()
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(after < 1e-4, "delta IR must have no tail (tail {after})");

        let y = run(0.0); // built-in hall: long tail required
        let tail_e: f64 = y[8_000 + 4_000..]
            .iter()
            .map(|v| (*v as f64) * (*v as f64))
            .sum();
        assert!(
            tail_e > 1e-4,
            "built-in hall has no reverb tail (energy {tail_e})"
        );
        drop(inst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- W3 (WINDOWS-PORT): script artifacts follow the target suffix --------

    #[test]
    fn w3_stamped_artifact_target_ext() {
        let dir = temp_dir("w3-stamp");
        let reg = Registry::open(dir.clone());
        let src = dir.join("kick.rs");
        std::fs::write(&src, "// w3\n").unwrap();
        let p = reg.stamped_artifact("kick", &src).unwrap();
        assert_eq!(p.parent().unwrap().file_name().unwrap(), ".build");
        let name = p.file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.starts_with("kick-"), "{name}");
        assert!(
            name.ends_with(&format!(".{}", crate::paths::dylib_ext())),
            "{name}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn w3_compile_effect_sidecars_follow_target_ext() {
        if !rustc_available() {
            return; // no compiler on this machine: nothing to exercise
        }
        let dir = temp_dir("w3-compile");
        let out = dir.join(format!("pin.{}", crate::paths::dylib_ext()));
        // Broken source: the poison marker must carry the target suffix.
        let bad = dir.join("bad.rs");
        std::fs::write(&bad, "fn broken( {").unwrap();
        let e = compile_effect(&bad, &out).unwrap_err();
        assert!(e.contains("compile failed"), "{e}");
        assert!(crate::paths::build_sidecar(&out, "failed").is_file());
        assert!(!out.is_file());
        // Good source: artifact itself carries the target suffix and the
        // staging sidecar is renamed away.
        let good = dir.join("good.rs");
        std::fs::write(&good, "pub fn _noop() {}\n").unwrap();
        compile_effect(&good, &out).unwrap();
        assert!(out.is_file(), "{} missing", out.display());
        assert!(!crate::paths::build_sidecar(&out, "tmp").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn w3_rustc_hint_is_rustup_on_unix() {
        let h = rustc_install_hint();
        assert!(h.contains("rustup") && !h.contains("MSVC"), "{h}");
    }

    #[cfg(windows)]
    #[test]
    fn w3_rustc_hint_names_build_tools_on_windows() {
        let h = rustc_install_hint();
        assert!(
            h.contains("MSVC Build Tools") && h.contains("rustup"),
            "{h}"
        );
    }
}
