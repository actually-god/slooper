//! v164 (card t_0c73850c): arbitrary N-EDO tuning core.
//!
//! ONE primitive maps a physical MIDI key to a sounding pitch:
//!
//! ```text
//! tune_key(n, root, k) = (k − root) · (12.0 / N) + (root − 69.0)
//! ```
//!
//! the *fractional 12-EDO key position relative to A4*, so every consumer
//! keeps working in the units it already knows:
//!
//! * pitch(k) = 440 · 2^(tune_key/12) ≡ root_freq · 2^(k/N) — the card's
//!   model, with root_freq = 440·2^((root−69)/12);
//! * scripted synths fetch it through the append-only
//!   [`crate::synth_api::SynthHostV1::tune_key`] host callback and just add
//!   69.0 to get a "virtual note number" (`note as f64` today);
//! * hosted plugins play the NEAREST 12-EDO key and receive the residual
//!   `r(k) = tune_key − round(tune_key)` (|r| < 0.5 st, always representable
//!   in a roll-space pitch bend — see `audio::stage_edo`);
//! * GUI labeling (edo-2) derives every key name from the same function.
//!
//! # N = 12 is bit-identical by construction
//!
//! `12.0 / 12.0 == 1.0` exactly and every intermediate value is an exact
//! integer in f64, so `tune_key(12, root, k) == k as f64 − 69.0` bit-for-bit
//! for EVERY root (pin `v164_tune_n12_bitexact`). The residual is exactly
//! 0.0, so the plugin path injects no bends and script frequencies feed the
//! same f64 the old literal did.
//!
//! # Live state
//!
//! The current (N, root) pair is a process-wide packed atomic: the RT
//! callback, the script host trampoline and the UI all read/write it
//! lock-free (`set` clamps N into [1, 128]; 1 = octave drone, 128 ≈ 9.4 ¢).
//! Persistence lives in Config/Project (`edo_n`/`edo_root`); whoever loads
//! state calls [`set`] — the atomics are the single runtime truth.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;

/// Inclusive clamp bounds for the divisions-per-octave setting.
pub const EDO_MIN: u32 = 1;
pub const EDO_MAX: u32 = 128;
/// Standard 12-EDO — the shipped default, bit-identical to pre-v164.
pub const EDO_DEFAULT_N: u32 = 12;
/// Root note = MIDI C4 (the "root C" default of the card).
pub const EDO_DEFAULT_ROOT: u8 = 60;

/// `(n << 8) | root` — one relaxed atomic read answers any tuning question,
/// RT-safe on any thread.
#[inline]
const fn pack(n: u32, root: u8) -> u32 {
    (n << 8) | root as u32
}

static TUNE: AtomicU32 = AtomicU32::new(pack(EDO_DEFAULT_N, EDO_DEFAULT_ROOT));

/// Publish the live tuning (clamped into [EDO_MIN, EDO_MAX]).
#[inline]
pub fn set(n: u32, root: u8) {
    TUNE.store(pack(n.clamp(EDO_MIN, EDO_MAX), root), Ordering::Relaxed);
}

/// Read the live tuning `(N, root)`.
#[inline]
pub fn get() -> (u32, u8) {
    let p = TUNE.load(Ordering::Relaxed);
    (p >> 8, (p & 0xFF) as u8)
}

/// Pure primitive (see the module doc). NOT clamped: `n` must already be
/// within [EDO_MIN, EDO_MAX] (callers read it back through [`get`], and
/// pins pass it directly). v168 (pbc-2) widened the row to any integer:
/// a drawn pitch curve bends far beyond the 0..=127 keyboard, and the
/// continuous-pitch labelers below name those rows too (`k` as u8 keeps
/// working — every existing caller compiles unchanged).
#[inline]
pub fn key<K: Into<f64>>(n: u32, root: u8, k: K) -> f64 {
    (k.into() - root as f64) * (12.0 / n as f64) + (root as f64 - 69.0)
}

/// Primitive at the LIVE tuning — the script host trampoline and every
/// key-driven consumer call this.
#[inline]
pub fn tune_key(k: u8) -> f64 {
    let (n, r) = get();
    key(n, r, k)
}

/// Cent width of one EDO step.
#[inline]
pub fn cents_per_step(n: u32) -> f64 {
    1200.0 / n as f64
}

/// Plugin-path pair for physical key `k`: the 12-EDO key to actually play
/// and the residual in SEMITONES (`roll-space` pitch-bend currency) that
/// lands the note on the exact EDO pitch. `nearest + 69.0` is clamped into
/// the MIDI key range (an extreme root like 127 can push it out); the
/// residual is reported unmodified so callers can still see the intent.
#[inline]
pub fn residual(n: u32, root: u8, k: u8) -> (u8, f32) {
    let tk = key(n, root, k);
    let nearest = tk.round();
    let note = (nearest + 69.0).clamp(0.0, 127.0) as u8;
    (note, (tk - nearest) as f32)
}

// ---------------------------------------------------------------------------
// edo-2 (card t_cdca60c1): note LABELING — the ONLY note-name generator in
// the repo. Every labeled surface (piano-roll key rows, sampler-map strip,
// MIDI/status echoes, Settings) calls these functions; a per-surface name
// table or inline formatter is a contract violation pinned by
// `v166_single_home_source_scan`.
// ---------------------------------------------------------------------------

/// Standard name of a 12-EDO MIDI row (60 = "C4"). This function owns the
/// repo's single name table. i32 rows beyond the keyboard stay fully named
/// (the octave math extends: `std_name(128) == "C10"`), so EDO anchors may
/// name rows the 128-key board only reaches through its own index. Plain
/// letters + '#' only: the default fonts carry no accident glyphs beyond
/// the proven-safe set, and names ride widgets.
#[inline]
pub fn std_name(midi: i32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[midi.rem_euclid(12) as usize],
        midi.div_euclid(12) - 1
    )
}

/// The display label of one physical key at a tuning, plus whether it sits
/// on a standard 12-EDO row.
pub struct NoteLabel {
    /// Anchor: the standard name (`"C#4"`); intermediate: the nearest
    /// standard name plus an explicit-signed residual in cents
    /// (`"C4+50c"`). Always the DEFAULT naming — status/MIDI echoes read
    /// this even when a font pack decorates the painted rows.
    pub text: String,
    /// True when the step's pitch is an exact integer number of semitones
    /// from A4 — i.e. it coincides with the 12-EDO row the name says.
    pub anchor: bool,
    /// v172 (edo-3, card t_7ca820d3): the font-pack decoration for this
    /// step, `Some` only when a pack is bound to this EDO and the step is
    /// an intermediate. Painted surfaces compose it; text-only consumers
    /// keep using [`Self::text`] (the pack NEVER rewrites the default).
    pub pack: Option<PackPart>,
}

/// v172 (edo-3, card t_7ca820d3): how a bound notation font pack decorates
/// one intermediate label. Anchors NEVER get a pack part — the standard
/// names are the pack-free floor of the notation (the manifest keys glyph
/// tables by residual cents, and an anchor's cents is 0 by law).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackPart {
    /// The pack's manifest has a glyph for this step's residual cents:
    /// paint `base` (the standard nearest-row name, e.g. `"C4"`) in the
    /// default family, then `glyph` in family `family` (e.g. a half-sharp
    /// sign). The `+50c` text suffix is REPLACED by the glyph.
    Glyph {
        base: String,
        glyph: String,
        family: String,
    },
    /// The bound pack ships NO manifest glyph table: paint the whole
    /// default text `text` in family `family` (fonts that re-draw the
    /// digits and signs of the plain `C4+50c` spelling).
    Whole { text: String, family: String },
}

/// Anchor law: key `k` lands on an exact 12-EDO row iff its EDO offset from
/// `root` is a whole number of semitones: `(k − root) · 12 / n` integral.
/// Equivalent to `tune_key` being integral — the equivalence is pinned
/// exhaustively (`v166_label_anchor_law`). NOTE: the anchor period is
/// `n / gcd(n, 12)` steps (e.g. 19-EDO anchors every 19 steps; 24-EDO every
/// 2), NOT every n — the naive `1200·s % n` test is wrong (n=16 would call
/// every step an anchor).
#[inline]
pub fn is_anchor(n: u32, root: u8, k: u8) -> bool {
    ((k as i64 - root as i64) * 12) % n as i64 == 0
}

/// The label of physical key `k` at `(n, root)` — the single entry point
/// for readable note names (edo-2).
///
/// * Anchor: `std_name(tune_key + 69)` — the standard name of the exact
///   12-EDO row the step sits on, so octaves align: a 24-EDO octave above
///   the root reads `"C5"` exactly where 12-EDO's C5 sits. For n = 12 every
///   key is an anchor and the text is byte-identical to the historical
///   `note_name` (`v166_12edo_all_anchors_identical`).
/// * Intermediate: nearest standard row + signed cents. An exact
///   half-semitone tie takes the LOWER row, so 50-cent steps always render
///   as `<lower>+50c` (the card's `C4+50c`), never a root/sign-dependent
///   mix of `+50c` and `-50c`.
pub fn label(n: u32, root: u8, k: u8) -> NoteLabel {
    let tk = key(n, root, k);
    if is_anchor(n, root, k) {
        return NoteLabel {
            text: std_name(tk.round() as i32 + 69),
            anchor: true,
            pack: None,
        };
    }
    nearest_row_label(n, tk)
}

/// The label for an ARBITRARY sounding pitch in semitones-relative-to-A4:
/// the nearest standard 12-EDO row plus its explicit-signed residual —
/// exact half-semitone ties go to the LOWER row (`tk.floor()`), so 50-cent
/// steps always render `<lower>+50c`, never a root/sign-dependent mix.
/// A pitch that lands (after cent-rounding) on a standard row reads as
/// the plain anchor name. Shared by [`label`]'s intermediate path and the
/// v168 continuous-pitch labelers — ONE tie law in the repo.
fn nearest_row_label(n: u32, tk: f64) -> NoteLabel {
    // f64 equality is safe here: a true tie is a binary-exact .5, and no
    // true non-tie is within 1e-12 of one for n ≤ 128 (and mouse-derived
    // curve cents are never expected to fabricate an exact .5).
    let nearest = if tk - tk.floor() == 0.5 {
        tk.floor()
    } else {
        tk.round()
    };
    let cents = ((tk - nearest) * 100.0).round() as i64;
    if cents == 0 {
        return NoteLabel {
            text: std_name(nearest as i32 + 69),
            anchor: true,
            pack: None,
        };
    }
    let name = std_name(nearest as i32 + 69);
    let text = format!("{name}{cents:+}c");
    let pack = pack_part_for(n, cents, &name, &text);
    NoteLabel { text, anchor: false, pack }
}

/// [`label`] at the live tuning (the packed atomic).
#[inline]
pub fn label_live(k: u8) -> NoteLabel {
    let (n, r) = get();
    label(n, r, k)
}

/// [`label_live`]'s text — the echo/status convenience wrapper.
#[inline]
pub fn label_text_live(k: u8) -> String {
    label_live(k).text
}

// ---------------------------------------------------------------------------
// v168 (card t_73fea549, pbc-2-gui): CONTINUOUS-pitch labeling — the drawn
// pitch curve bends a row by an arbitrary cents offset far past the
// keyboard; every live echo (drag/hover/release) names THAT pitch through
// these wrappers. Same single home, same tie law as [`label`].
// ---------------------------------------------------------------------------

/// The label of the CONTINUOUS pitch "row `k` (an i32 — bends may leave
/// the 0..=127 keyboard) bent by `cents_off`" at `(n, root)`. The pitch is
/// the sounding position `key(n, root, k) + cents_off/100` semitones from
/// A4, then [`nearest_row_label`] — so at N = 12 `label_pitch(12, 60, 60,
/// 300) == "F4"`, at 24-EDO row 61 reads `"C4+50c"`, and a grid point bent
/// by 0 cents is byte-identical to [`label`] (`v168_label_pitch_grid_
/// matches_label`).
#[inline]
pub fn label_pitch(n: u32, root: u8, k: i32, cents_off: f32) -> NoteLabel {
    nearest_row_label(n, key(n, root, k) + f64::from(cents_off) / 100.0)
}

/// [`label_pitch`] at the live tuning (the packed atomic).
#[inline]
pub fn label_pitch_live(k: i32, cents_off: f32) -> NoteLabel {
    let (n, r) = get();
    label_pitch(n, r, k, cents_off)
}

/// [`label_pitch_live`]'s text — the echo/status convenience wrapper.
#[inline]
pub fn label_pitch_text_live(k: i32, cents_off: f32) -> String {
    label_pitch_live(k, cents_off).text
}

// ---------------------------------------------------------------------------
// edo-3 (v172, card t_7ca820d3): NOTATION FONT PACKS — user fonts bound per
// EDO number. A pack is a folder (or a loose font file) under
// `<data_dir>/edo-fonts/` (paths::data_dir — the DATA_DIR parity home, so
// `%LOCALAPPDATA%\slooper\edo-fonts` on Windows): a .ttf/.otf/.ttc font plus
// an OPTIONAL `pack.toml` manifest (tiny key=value subset — the repo carries
// no TOML crate; see [`parse_pack_manifest`]).
//
// Law, in order:
// * A pack's font bytes are validated with the EXACT parser egui builds its
//   atlas with (`skrifa::FontRef::from_index`, epaint 0.36) — egui PANICS on
//   undecodable FontData, so anything the scanner accepts structurally
//   cannot panic the atlas; anything it rejects degrades to a status echo.
// * The registry here is the runtime truth (the packed TUNE atomic's
//   pattern): consumers never re-read the disk. Config owns persistence.
// * Callers MUST install the egui families (ctx.set_fonts) BEFORE publishing
//   a pack here — an unbound family FontId is an egui-internal panic
//   (`FontFamily::... is not bound to any fonts`). ui.rs enforces the order
//   at both publish points (first-frame hook, settings rescan).
// * Only intermediates get decorated; the default `NoteLabel::text` is
//   NEVER rewritten, so echoes and every non-bound EDO keep v166 names.
// ---------------------------------------------------------------------------

/// A discovered, font-validated notation pack (see the module's edo-3 law).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackInfo {
    /// Display/binding name: the pack folder name, or a loose font file's
    /// stem. Unique inside a scan.
    pub name: String,
    /// The egui `FontFamily::Name` this pack installs under
    /// ([`pack_family_name`] of `name`).
    pub family: String,
    /// The manifest's `edo = N` suggestion (UI hint only — bindings are
    /// ALWAYS the user's explicit choice in Settings).
    pub edo_hint: Option<u32>,
    /// Residual cents -> glyph string (may be multiple chars). Empty =
    /// whole-family mode ([`PackPart::Whole`]).
    pub glyphs: BTreeMap<i64, String>,
    /// The validated font bytes (installed into egui at publish time).
    pub font_bytes: Vec<u8>,
}

/// Result of a pack scan: usable packs plus one line per skipped
/// candidate (the status echo; a bad file is NEVER fatal).
#[derive(Clone, Debug, Default)]
pub struct PackScan {
    pub packs: Vec<PackInfo>,
    pub errors: Vec<String>,
}

/// Registry state (replaced whole on every scan — the disk listing IS the
/// truth; a failed read is skipped with an echo).
static PACKS: RwLock<BTreeMap<String, PackInfo>> = RwLock::new(BTreeMap::new());
/// EDO number -> pack name. Empty key = unbound (v166 default labels).
static PACK_BINDINGS: RwLock<BTreeMap<u32, String>> = RwLock::new(BTreeMap::new());

/// Poison never panics a paint: the lock bodies are plain data, so the
/// inner value of a poisoned lock is always still valid to use.
fn rd<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|p| p.into_inner())
}

/// Publish the discovered packs (callers MUST install the egui families
/// first — see the edo-3 law block above).
pub fn set_packs(packs: Vec<PackInfo>) {
    let mut w = PACKS.write().unwrap_or_else(|p| p.into_inner());
    *w = packs.into_iter().map(|p| (p.name.clone(), p)).collect();
}

/// The discovered packs, sorted by name.
pub fn packs() -> Vec<PackInfo> {
    rd(&PACKS).values().cloned().collect()
}

/// Publish the EDO -> pack-name bindings (Config is the persistent copy).
pub fn set_pack_bindings(map: BTreeMap<u32, String>) {
    let mut w = PACK_BINDINGS.write().unwrap_or_else(|p| p.into_inner());
    *w = map;
}

/// The live bindings (UI list, settings echo, pins).
pub fn pack_bindings() -> BTreeMap<u32, String> {
    rd(&PACK_BINDINGS).clone()
}

/// The egui font family a pack name installs under: `slooper-edo-` plus the
/// name lowercased with non-alphanumerics collapsed to single `-`.
/// Sanitized names colliding across different pack names is a scan error
/// (only the first pack with a family survives).
pub fn pack_family_name(name: &str) -> String {
    let mut s = String::from("slooper-edo-");
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            s.push('-');
            prev_dash = true;
        }
    }
    s.trim_end_matches('-').to_owned()
}

/// The pack decoration for ONE intermediate step (called only from the
/// label builder — the single home). `None` (=> default text) when nothing
/// is bound to `n`, the bound name is not (or no longer) on disk, or the
/// pack's glyph table has no entry for these exact cents.
fn pack_part_for(n: u32, cents: i64, base: &str, text: &str) -> Option<PackPart> {
    let name = rd(&PACK_BINDINGS).get(&n).cloned()?;
    let p = rd(&PACKS).get(&name).cloned()?;
    if p.glyphs.is_empty() {
        return Some(PackPart::Whole {
            text: text.to_owned(),
            family: p.family,
        });
    }
    let glyph = p.glyphs.get(&cents)?.clone();
    Some(PackPart::Glyph {
        base: base.to_owned(),
        glyph,
        family: p.family,
    })
}

/// Folder name under [`crate::paths::data_dir`] holding the notation packs.
pub const PACKS_DIR: &str = "edo-fonts";
/// Manifest file name inside a pack folder (optional).
pub const PACK_MANIFEST: &str = "pack.toml";

/// `<data_dir>/edo-fonts` — chained single-component join (the mixed-
/// separator Windows law: every app-state-compared path is scan-form).
pub fn packs_dir() -> PathBuf {
    crate::paths::data_dir().join(PACKS_DIR)
}

/// Scan the user pack folder; missing/unreadable folder = EMPTY scan, zero
/// errors (the default state, not a failure).
pub fn scan_packs() -> PackScan {
    scan_packs_at(&packs_dir())
}

/// The scanner body over an explicit root (pins point this at a sandbox
/// without touching env). A pack = `<root>/<name>/` with a font file +
/// optional [`PACK_MANIFEST`], or a loose `<root>/<stem>.<ext>` font file
/// (stem = name, whole-family mode). Hidden entries are ignored.
pub fn scan_packs_at(root: &Path) -> PackScan {
    let mut scan = PackScan::default();
    let mut map: BTreeMap<String, PackInfo> = BTreeMap::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return scan,
    };
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut loose: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            dirs.push((name, p));
        } else if is_font_file(&p) {
            loose.push(p);
        }
    }
    dirs.sort();
    for (name, dir) in &dirs {
        let mut notes: Vec<String> = Vec::new();
        let mut hint = None;
        let mut glyphs = BTreeMap::new();
        let mp = dir.join(PACK_MANIFEST);
        if mp.is_file() {
            match std::fs::read_to_string(&mp) {
                Ok(t) => parse_pack_manifest(&t, &mut hint, &mut glyphs, &mut notes),
                Err(e) => notes.push(format!("manifest unreadable: {e}")),
            }
        }
        // The font is the alphabetically-first font file in the folder.
        let mut font: Option<PathBuf> = None;
        if let Ok(rd2) = std::fs::read_dir(dir) {
            let mut cands: Vec<PathBuf> = rd2
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && is_font_file(p))
                .collect();
            cands.sort();
            font = cands.into_iter().next();
        }
        let Some(fp) = font else {
            scan.errors.push(format!("pack {name}: no .ttf/.otf/.ttc font file"));
            scan.errors.extend(notes.iter().map(|n| format!("pack {name}: {n}")));
            continue;
        };
        match std::fs::read(&fp)
            .map_err(|e| format!("pack {name}: font unreadable: {e}"))
            .and_then(|b| validate_font_bytes(&b).map_err(|m| format!("pack {name}: {m}")))
        {
            Ok(()) => {}
            Err(msg) => {
                scan.errors.push(msg);
                scan.errors.extend(notes.iter().map(|n| format!("pack {name}: {n}")));
                continue;
            }
        }
        let bytes = std::fs::read(&fp).unwrap_or_default();
        let fam = pack_family_name(name);
        if map.contains_key(name) {
            scan.errors.push(format!("pack {name}: name skipped (duplicate)"));
        } else if map.values().any(|p| p.family == fam) {
            scan.errors.push(format!("pack {name}: family {fam} taken by another pack"));
        } else {
            map.insert(
                name.clone(),
                PackInfo {
                    name: name.clone(),
                    family: fam,
                    edo_hint: hint,
                    glyphs,
                    font_bytes: bytes,
                },
            );
        }
        scan.errors.extend(notes.iter().map(|n| format!("pack {name}: {n}")));
    }
    loose.sort();
    for p in loose {
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match std::fs::read(&p)
            .map_err(|e| format!("pack {stem}: font unreadable: {e}"))
            .and_then(|b| validate_font_bytes(&b).map_err(|m| format!("pack {stem}: {m}")))
        {
            Ok(()) => {}
            Err(msg) => {
                scan.errors.push(msg);
                continue;
            }
        }
        let bytes = std::fs::read(&p).unwrap_or_default();
        let fam = pack_family_name(&stem);
        if map.contains_key(&stem) {
            scan.errors.push(format!("pack {stem}: name skipped (duplicate)"));
        } else if map.values().any(|p| p.family == fam) {
            scan.errors.push(format!("pack {stem}: family {fam} taken by another pack"));
        } else {
            map.insert(
                stem.clone(),
                PackInfo {
                    name: stem,
                    family: fam,
                    edo_hint: None,
                    glyphs: BTreeMap::new(),
                    font_bytes: bytes,
                },
            );
        }
    }
    scan.packs = map.into_values().collect();
    scan
}

fn is_font_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc"))
}

/// The load gate: parse the font with the SAME skrifa entry point epaint
/// uses for every FontData (`FontRef::from_index`, panic at fonts.rs:990 on
/// failure). Accepting here is a proof egui can install the bytes.
fn validate_font_bytes(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 32 {
        return Err("file too small to be a font".to_owned());
    }
    skrifa::FontRef::from_index(bytes, 0)
        .map(|_| ())
        .map_err(|e| format!("font file rejected by the font engine ({e})"))
}

/// `pack.toml` parser: a line-based key = value subset (NO TOML crate in the
/// repo, and nothing here needs more):
/// * `edo = 24` — the target-EDO suggestion (one u32).
/// * `glyph = 50 𝄪` — residual cents, then the glyph string to its end
///   (one pair of surrounding double quotes is stripped). Negative cents
///   and multi-char glyphs are fine; keys may repeat.
/// * `#` comments and blank lines are skipped; anything else is a NOTE
///   (echoed, never fatal) and the line is skipped.
fn parse_pack_manifest(
    text: &str,
    hint: &mut Option<u32>,
    glyphs: &mut BTreeMap<i64, String>,
    notes: &mut Vec<String>,
) {
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (key, val) = match t.split_once('=') {
            Some(kv) => kv,
            None => {
                notes.push(format!("line {}: expected key = value", i + 1));
                continue;
            }
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "edo" => match val.trim().parse::<u32>() {
                Ok(n) => *hint = Some(n),
                Err(_) => notes.push(format!("line {}: edo must be a number", i + 1)),
            },
            "glyph" => {
                let v = val.trim();
                let sp = v.find(char::is_whitespace);
                let (cs, g) = match sp {
                    Some(i2) => (&v[..i2], v[i2..].trim()),
                    None => {
                        notes.push(format!("line {}: glyph needs '<cents> <chars>'", i + 1));
                        continue;
                    }
                };
                let g = g
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or(g);
                match cs.parse::<i64>() {
                    Ok(c) if !g.is_empty() => {
                        glyphs.insert(c, g.to_owned());
                    }
                    Ok(_) => notes.push(format!("line {}: glyph string is empty", i + 1)),
                    Err(_) => notes.push(format!("line {}: glyph cents is not a number", i + 1)),
                }
            }
            other => notes.push(format!("line {}: unknown key {other:?}", i + 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- v166 (card t_cdca60c1): the label builder ----

    /// Independent reference naming for the N=12 sabotage polarity
    /// (whitespace-split literal stays apart from the single-home
    /// source-scan needle on purpose).
    fn ref_name(m: i32) -> String {
        let names: Vec<&str> = "C C# D D# E F F# G G# A A# B".split_whitespace().collect();
        format!("{}{}", names[m.rem_euclid(12) as usize], m.div_euclid(12) - 1)
    }

    #[test]
    fn v166_label_anchor_law() {
        // The integer-law and the float-primitive agree for EVERY
        // (n, root, k) triple across four representative roots. The
        // float side compares to within 1e-9: for n ∤ 12·2^k (e.g. 11)
        // the f64 product carries rounding noise around the exact value,
        // while is_anchor is exact integer arithmetic — THE DEFINITION.
        for n in 1..=128u32 {
            for &root in &[0u8, 60, 69, 127] {
                for k in 0..=127u8 {
                    let tk = key(n, root, k);
                    let integral = (tk - tk.round()).abs() < 1e-9;
                    assert_eq!(
                        is_anchor(n, root, k),
                        integral,
                        "anchor law n={n} root={root} k={k}"
                    );
                }
            }
        }
        // The period is n/gcd(n,12) steps, not n (19-EDO: every 19).
        assert!(!is_anchor(19, 60, 61));
        assert!(is_anchor(19, 60, 79));
    }

    #[test]
    fn v166_label_24edo_anchors_octaves() {
        // 24-EDO anchors carry the standard name of the 12-EDO row they
        // coincide with — one octave (24 steps) reads C4..C5, octaves
        // aligned with 12-EDO by construction.
        let expect = [
            "C4", "C#4", "D4", "D#4", "E4", "F4", "F#4", "G4", "G#4", "A4", "A#4", "B4", "C5",
        ];
        for (i, want) in expect.iter().enumerate() {
            let k = (60 + 2 * i) as u8;
            let l = label(24, 60, k);
            assert!(l.anchor, "24-EDO key {k} (step {}) must anchor", 2 * i);
            assert_eq!(l.text, *want, "24-EDO anchor name/octave k={k}");
        }
        assert_eq!(label(24, 60, 36).text, "C3", "octave below aligns too");
        // Bottom of the keyboard: 60 steps below C4 = 30 st = F#1.
        assert_eq!(label(24, 60, 0).text, "F#1");
    }

    #[test]
    fn v166_label_intermediates_relative_cents() {
        let l = label(24, 60, 61);
        assert!(!l.anchor);
        assert_eq!(l.text, "C4+50c", "the card example, byte-exact");
        // 1.5 st above C4 → tie → LOWER row (C#4) +50c: the tie law makes
        // every 50c step read as (lower)+50c, independent of root/sign.
        assert_eq!(label(24, 60, 63).text, "C#4+50c");
        assert_eq!(label(24, 60, 59).text, "B3+50c", "below A4: same law");
        assert_eq!(label(24, 60, 73).text, "F#4+50c");
        // Non-exact residuals round to the nearest cent, sign explicit.
        assert_eq!(label(19, 60, 61).text, "C#4-37c");
        assert_eq!(label(19, 60, 62).text, "C#4+26c");
    }

    #[test]
    fn v166_root_reanchors_naming() {
        // The root choice moves the anchor grid: the same physical key is
        // an anchor under root A but an intermediate under root C.
        let l = label(24, 69, 69);
        assert!(l.anchor);
        assert_eq!(l.text, "A4");
        assert_eq!(label(24, 69, 70).text, "A4+50c", "step above root");
        assert_eq!(label(24, 69, 68).text, "G#4+50c", "step below root");
        // Key 69 at root C: 9 steps = 4.5 st → between E4 and F4 → lower
        // row E4 (+50c) — NOT "A4".
        assert_eq!(label(24, 60, 69).text, "E4+50c");
    }

    #[test]
    fn v166_12edo_all_anchors_identical() {
        // Hard law: at N=12 every key is an anchor and the text equals the
        // historical naming formula for every root. (Reference table lives
        // HERE, in the test — a forked/altered table in tune.rs turns this
        // RED: sabotage polarity.)
        for &root in &[0u8, 60, 69, 127] {
            for k in 0..=127u8 {
                let l = label(12, root, k);
                assert!(l.anchor, "N=12 k={k} must anchor");
                assert_eq!(l.text, ref_name(k as i32), "N=12 k={k} root={root}");
            }
        }
        // std_name matches the historical note_name pins and extends
        // (C9 = 120, so the keyboard's top key names G9 and beyond it
        // the octave math simply keeps counting: 132 = C10).
        assert_eq!(std_name(60), "C4");
        assert_eq!(std_name(0), "C-1");
        assert_eq!(std_name(127), "G9");
        assert_eq!(std_name(-1), "B-2");
        assert_eq!(std_name(128), "G#9");
        assert_eq!(std_name(132), "C10");
    }

    #[test]
    fn v166_label_no_zero_cents() {
        // An intermediate is never an anchor in disguise: no "±0c" text,
        // and anchor texts carry no cents suffix at all.
        for n in 1..=128u32 {
            for &root in &[0u8, 41, 60, 69, 127] {
                for k in 0..=127u8 {
                    let l = label(n, root, k);
                    if l.anchor {
                        assert!(
                            !l.text.contains('c'),
                            "anchor text carries cents: {}",
                            l.text
                        );
                    } else {
                        assert!(
                            !l.text.ends_with("+0c") && !l.text.ends_with("-0c"),
                            "zero-cent intermediate n={n} root={root} k={k}: {}",
                            l.text
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn v166_label_live_wrappers() {
        let back = get();
        set(12, EDO_DEFAULT_ROOT);
        assert_eq!(label_text_live(61), "C#4");
        set(24, 60);
        assert_eq!(label_text_live(61), "C4+50c");
        set(back.0, back.1);
    }

    fn old_formula(k: u8) -> f64 {
        // The exact literal expression every pre-v164 consumer used.
        k as f64 - 69.0
    }

    #[test]
    fn v164_tune_step_cents() {
        // R1: one EDO step = 1200/N cents, for every class of N: exact
        // binary (1, 12, 24), irrational-ish (19, 31, 53) and the max.
        for n in [1u32, 2, 12, 19, 24, 31, 53, 128] {
            let want = 12.0 / n as f64; // st per step
            for root in [60u8, 69] {
                for k in [0u8, 36, 60, 61, 100] {
                    if k + 1 > 127 {
                        continue;
                    }
                    let d = key(n, root, k + 1) - key(n, root, k);
                    assert!(
                        (d - want).abs() <= 1e-9 * want.max(1.0),
                        "step {n}/{root} at {k}: {d} vs {want}"
                    );
                }
            }
            assert!((cents_per_step(n) - 1200.0 / n as f64).abs() < 1e-12);
        }
    }

    #[test]
    fn v164_tune_octave_closes() {
        // R1/R2: an octave closes at exactly N keys — two physical octaves
        // of keys (2N) span two sounded octaves.
        for n in [1u32, 2, 5, 12, 19, 24, 31, 53, 64] {
            for root in [0u8, 60, 69, 127] {
                for k in [0u8, 12, 60] {
                    let kn = match k.checked_add(n as u8) {
                        Some(x) if x <= 127 => x,
                        _ => continue,
                    };
                    let d = key(n, root, kn) - key(n, root, k);
                    assert!(
                        (d - 12.0).abs() < 1e-9,
                        "octave {n}/{root}: {k}->{kn} = {d}"
                    );
                }
            }
        }
    }

    #[test]
    fn v164_tune_root_a() {
        // R3: the root key sits on its own 12-EDO anchor — k = root ⇒
        // tune_key = root − 69 ⇒ 440 Hz when root = A4 = 69, for ANY N.
        for n in [1u32, 12, 19, 24, 53, 128] {
            assert_eq!(key(n, 69, 69), 0.0, "A4 root reads 0.0 for N={n}");
            // The physical root always sounds its 12-EDO pitch.
            for root in [0u8, 36, 60, 69, 127] {
                assert_eq!(key(n, root, root), root as f64 - 69.0);
            }
        }
        // Root C4 default: key 60 = 2^(−9/12) rel. A4 (C4), i.e. the OLD
        // formula at key 60 — the EDO ladder is anchored, not shifted.
        assert_eq!(key(24, 60, 60), -9.0);
    }

    #[test]
    fn v164_tune_n12_bitexact() {
        // R4 (HARD): N=12 is the old world BIT-FOR-BIT for every root and
        // every one of the 128 keys (root is mathematically inert at N=12).
        for root in [0u8, 1, 36, 60, 61, 69, 120, 127] {
            for k in 0..=127u8 {
                assert_eq!(
                    key(12, root, k).to_bits(),
                    old_formula(k).to_bits(),
                    "N=12 root {root} key {k} drifted"
                );
            }
        }
        // And the residual is exactly zero ⇒ the plugin staging pass
        // injects NOTHING at N=12 (byte-identical streams, pin in audio).
        for root in [0u8, 60, 69, 127] {
            for k in 0..=127u8 {
                let (note, r) = residual(12, root, k);
                assert_eq!(note, k, "nearest key drifted at {k}");
                assert_eq!(r.to_bits(), 0.0f32.to_bits(), "residual at {k}");
            }
        }
    }

    #[test]
    fn v164_tune_residual_24() {
        // R6 law at N=24, root C4: even keys stay ON 12-EDO (residual 0),
        // odd keys sit between two keys and land on the NEARER one with a
        // ±50 ¢ residual. Tie law is f64::round = AWAY FROM ZERO in
        // position units, so a −x.5 tie takes the key BELOW (residual
        // +0.5) while a +x.5 tie takes the key ABOVE (residual −0.5) —
        // documented behavior, not an accident.
        let cases: &[(u8, u8, f32)] = &[
            (60, 60, 0.0),
            (61, 60, 0.5),
            (62, 61, 0.0),
            (63, 61, 0.5),
            (64, 62, 0.0),
            (59, 59, 0.5),
            (58, 59, 0.0),
            (79, 70, -0.5),
        ];
        for &(k, note, r) in cases {
            let (n, got) = residual(24, 60, k);
            assert_eq!(n, note, "nearest key for {k}");
            assert_eq!(got, r, "residual for {k}");
            // The pair lands on the exact EDO pitch (st units).
            let landed = (n as f64 - 69.0) + got as f64;
            assert!((landed - key(24, 60, k)).abs() < 1e-6, "land {k}");
        }
    }

    #[test]
    fn v164_tune_atomic_publish() {
        // R8: set/get round-trips with clamping; the packed layout must
        // keep N=128 (does not fit the naive u8 mental model).
        let back = get();
        set(24, 61);
        assert_eq!(get(), (24, 61));
        set(0, 60); // clamp LOW
        assert_eq!(get(), (1, 60));
        set(999, 60); // clamp HIGH
        assert_eq!(get(), (EDO_MAX, 60));
        set(128, 127);
        assert_eq!(get(), (128, 127));
        assert!((tune_key(127) - key(128, 127, 127)).abs() < 1e-12);
        set(back.0, back.1);
        assert_eq!(get(), back);
    }

    // ---- v168 (card t_73fea549, pbc-2-gui): continuous-pitch labels ----

    #[test]
    fn v168_label_pitch_n12_octaves() {
        // The drawn-curve currency at 12-EDO: ±2 octaves land on exact
        // rows, a 35¢ offset names the nearest row + explicit residual,
        // and the −50¢ tie takes the LOWER row (same law as `label`).
        assert_eq!(label_pitch(12, 60, 60, 1200.0).text, "C5");
        assert!(label_pitch(12, 60, 60, 1200.0).anchor);
        assert_eq!(label_pitch(12, 60, 60, 2400.0).text, "C6");
        assert_eq!(label_pitch(12, 60, 60, -2400.0).text, "C2");
        assert_eq!(label_pitch(12, 60, 60, 300.0).text, "D#4", "the name table spells 63 D#");
        assert_eq!(label_pitch(12, 60, 60, 1235.0).text, "C5+35c");
        assert!(!label_pitch(12, 60, 60, 1235.0).anchor);
        assert_eq!(label_pitch(12, 60, 60, -50.0).text, "B3+50c", "tie → LOWER row");
        assert_eq!(label_pitch(12, 60, 60, -49.9).text, "C4-50c", "non-tie takes its side");
    }

    #[test]
    fn v168_label_pitch_grid_matches_label() {
        // A grid point bent by 0 cents labels BYTE-IDENTICALLY to
        // `label()` — text AND anchor flag — for every key at several
        // tunings: the two builders share the one tie/nearest law.
        for &n in &[12u32, 16, 19, 24, 31, 53] {
            for k in 0u8..=127 {
                let a = label(n, 60, k);
                let b = label_pitch(n, 60, i32::from(k), 0.0);
                assert_eq!(b.text, a.text, "n{n} k{k}");
                assert_eq!(b.anchor, a.anchor, "n{n} k{k} anchor");
            }
        }
    }

    #[test]
    fn v168_label_pitch_i32_rows() {
        // Curves bend far beyond the keyboard: the primitive takes i32
        // rows and the names extend (octave math has no 0..=127 edge).
        assert_eq!(key(12, 60, -5i32), -74.0);
        assert_eq!(label_pitch(12, 60, -5, 0.0).text, "G-2");
        assert_eq!(label_pitch(12, 60, 132, 0.0).text, "C10");
        // The tuning is baked into the row pitch: 24-EDO row 61 IS C4+50c,
        // bent +650¢ it sounds exactly G4.
        assert_eq!(label_pitch(24, 60, 61, 0.0).text, "C4+50c");
        assert_eq!(label_pitch(24, 60, 61, 650.0).text, "G4");
        // live wrappers read the packed atomic.
        let back = get();
        set(24, 60);
        assert_eq!(label_pitch_text_live(61, 0.0), "C4+50c");
        assert!(label_pitch_live(61, 650.0).anchor);
        set(back.0, back.1);
    }

    // ---- v172 (card t_7ca820d3, edo-3): notation font packs ----

    /// Snapshot/reset guard for the pack registry statics: the suite runs
    /// ONE process, so leaked bindings would poison the v166 label law.
    struct PackGuard {
        packs: Vec<PackInfo>,
        bindings: BTreeMap<u32, String>,
    }
    impl PackGuard {
        fn new() -> Self {
            let g = Self {
                packs: packs(),
                bindings: pack_bindings(),
            };
            set_packs(vec![]);
            set_pack_bindings(BTreeMap::new());
            g
        }
    }
    impl Drop for PackGuard {
        fn drop(&mut self) {
            set_packs(std::mem::take(&mut self.packs));
            set_pack_bindings(std::mem::take(&mut self.bindings));
        }
    }

    fn v172_pack(name: &str, glyphs: &[(i64, &str)], hint: Option<u32>) -> PackInfo {
        PackInfo {
            name: name.to_owned(),
            family: pack_family_name(name),
            edo_hint: hint,
            glyphs: glyphs.iter().map(|(c, g)| (*c, (*g).to_owned())).collect(),
            // Registry-only pins: bytes never touch egui here.
            font_bytes: vec![7; 64],
        }
    }

    /// Real, egui-parseable font bytes (the default Hack face): scan pins
    /// must ship files the scanner's skrifa gate ACCEPTS, on any OS.
    fn v172_real_font() -> Vec<u8> {
        let fd = egui::FontDefinitions::default();
        (&*fd.font_data["Hack"].font).to_vec()
    }

    #[test]
    fn v172_pack_label_glyph_lookup() {
        let _g = PackGuard::new();
        set_packs(vec![v172_pack("half", &[(50, "\u{03A6}")], Some(24))]);
        set_pack_bindings([
            (24, "half".to_owned()),
            (19, "half".to_owned()),
        ]
        .into_iter()
        .collect());
        let l = label(24, 60, 61);
        assert_eq!(l.text, "C4+50c", "default text is NEVER rewritten");
        assert_eq!(
            l.pack,
            Some(PackPart::Glyph {
                base: "C4".to_owned(),
                glyph: "\u{03A6}".to_owned(),
                family: "slooper-edo-half".to_owned(),
            })
        );
        // Anchors NEVER get decorated, bound pack or not.
        assert!(label(24, 60, 60).pack.is_none());
        assert!(label(24, 60, 62).pack.is_none());
        // A glyph table keys by the EXACT label cents: 19-EDO steps sit at
        // -37/+26 (v166 pins), which the table does not carry -> fallback.
        assert_eq!(label(19, 60, 61).text, "C#4-37c");
        assert!(label(19, 60, 61).pack.is_none(), "no glyph for -37");
        // Unbinding restores the default paint decision.
        set_pack_bindings(BTreeMap::new());
        assert!(label(24, 60, 61).pack.is_none());
    }

    #[test]
    fn v172_pack_label_whole_family_no_manifest() {
        let _g = PackGuard::new();
        set_packs(vec![v172_pack("redraw", &[], None)]);
        set_pack_bindings([(24, "redraw".to_owned())].into_iter().collect());
        assert_eq!(
            label(24, 60, 61).pack,
            Some(PackPart::Whole {
                text: "C4+50c".to_owned(),
                family: "slooper-edo-redraw".to_owned(),
            })
        );
        assert!(label(24, 60, 60).pack.is_none(), "anchors stay free");
    }

    #[test]
    fn v172_pack_label_stale_binding_falls_back() {
        // Bound name not in the registry (pack removed from disk / boot
        // config before rescan): labels degrade to the defaults, no panic.
        let _g = PackGuard::new();
        set_packs(vec![]);
        set_pack_bindings([(24, "ghost".to_owned())].into_iter().collect());
        assert!(label(24, 60, 61).pack.is_none());
        assert_eq!(label(24, 60, 61).text, "C4+50c");
    }

    #[test]
    fn v172_pack_label_n12_never_decorated() {
        // 12-EDO is all anchors: binding even a glyph-for-everything pack
        // decorates NOTHING (the standard keyboard world is pack-free).
        let _g = PackGuard::new();
        let all: Vec<(i64, String)> = (0..100)
            .flat_map(|c| [(c, "x".to_string()), (-c, "y".to_string())])
            .collect();
        let mut glyphs = BTreeMap::new();
        for (c, g) in all {
            glyphs.insert(c, g);
        }
        set_packs(vec![PackInfo {
            name: "all".into(),
            family: pack_family_name("all"),
            edo_hint: None,
            glyphs,
            font_bytes: vec![7; 64],
        }]);
        set_pack_bindings([(12, "all".to_owned())].into_iter().collect());
        for k in 0..=127u8 {
            assert!(label(12, 60, k).pack.is_none(), "N=12 k={k}");
        }
    }

    #[test]
    fn v172_pack_label_pitch_shares_the_law() {
        // The v168 continuous-pitch builder shares ONE tie/nearest/pack law:
        // a grid point bends identically to `label`, and a curve point bent
        // onto the +50 tie takes the LOWER row — whose cents the glyph keys.
        let _g = PackGuard::new();
        set_packs(vec![v172_pack("half", &[(50, "\u{03A6}")], None)]);
        set_pack_bindings([(24, "half".to_owned())].into_iter().collect());
        assert_eq!(label_pitch(24, 60, 61, 0.0).pack, label(24, 60, 61).pack);
        let bent = label_pitch(24, 60, 60, -50.0);
        assert_eq!(bent.text, "B3+50c", "tie goes to the LOWER row (v166 law)");
        assert_eq!(
            bent.pack,
            Some(PackPart::Glyph {
                base: "B3".to_owned(),
                glyph: "\u{03A6}".to_owned(),
                family: "slooper-edo-half".to_owned(),
            })
        );
    }

    #[test]
    fn v172_pack_family_name_sanitize() {
        assert_eq!(pack_family_name("Half-Tones X"), "slooper-edo-half-tones-x");
        assert_eq!(pack_family_name("A  B"), "slooper-edo-a-b");
        assert_eq!(pack_family_name("!!!"), "slooper-edo");
        // Non-ASCII names survive as their ASCII skeleton (families are
        // debuggability aids; the DISPLAY name stays the exact folder name).
        assert_eq!(pack_family_name("K\u{f6}ln"), "slooper-edo-k-ln");
    }

    #[test]
    fn v172_pack_manifest_parser() {
        let src = "# a comment\n\
                   edo = 24\n\
                   glyph = 50 \u{03A6}\n\
                   glyph =-50 '\u{03A8}'\n\
                   glyph = 50 \"\u{03A6}2\"\n\
                   broken line\n\
                   nope = 3\n\
                   glyph = 26\n\
                   glyph = 60 \"\"\n";
        let mut hint = None;
        let mut glyphs = BTreeMap::new();
        let mut notes = Vec::new();
        parse_pack_manifest(src, &mut hint, &mut glyphs, &mut notes);
        assert_eq!(hint, Some(24));
        assert_eq!(glyphs.get(&50).map(String::as_str), Some("\u{03A6}2"), "repeat overwrites");
        assert_eq!(glyphs.get(&-50).map(String::as_str), Some("'\u{03A8}'"));
        assert_eq!(glyphs.len(), 2);
        // Every malformed line is a NOTE (echoed), never a crash: 4 here
        // (broken line, unknown key, missing glyph chars, empty string).
        assert_eq!(notes.len(), 4, "notes: {notes:?}");
    }

    #[test]
    fn v172_pack_scan_tree_and_malformed() {
        // The DATA_DIR-shaped sandbox (chained joins, OS-neutral): a good
        // pack dir (real font + manifest), a loose font file, a folder
        // without a font, a folder with a REJECTED font, and two names that
        // sanitize to the same family. The frame never panics and every
        // skip is one echoed line.
        let root = std::env::temp_dir().join(format!("v172-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let hack = v172_real_font();
        let mk = |dir: &str, font: Option<&str>, bytes: &[u8], manifest: Option<&str>| {
            let d = root.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            if let Some(f) = font {
                std::fs::write(d.join(f), bytes).unwrap();
            } else {
                std::fs::write(d.join("README.md"), b"no font here").unwrap();
            }
            if let Some(m) = manifest {
                std::fs::write(d.join("pack.toml"), m.as_bytes()).unwrap();
            }
        };
        mk("good", Some("font.ttf"), &hack, Some("edo = 24\nglyph = 50 \u{03A6}\n"));
        mk("nofont", None, &[], None);
        mk("bad", Some("broken.ttf"), &[0xABu8; 4096], None);
        mk("sp ace", Some("f.ttf"), &hack, None);
        mk("sp-ace", Some("f.ttf"), &hack, None);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("loose.ttf"), &hack).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".hidden").join("x.ttf"), &hack).unwrap();
        let scan = scan_packs_at(&root);
        let names: Vec<&str> = scan.packs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["good", "loose", "sp ace"], "sorted usable packs: {names:?}, errors: {:?}", scan.errors);
        let good = scan.packs.iter().find(|p| p.name == "good").unwrap();
        assert_eq!(good.family, "slooper-edo-good");
        assert_eq!(good.edo_hint, Some(24));
        assert_eq!(good.glyphs.get(&50).map(String::as_str), Some("\u{03A6}"));
        assert_eq!(good.font_bytes, hack, "bytes ride the registry");
        let loose = scan.packs.iter().find(|p| p.name == "loose").unwrap();
        assert!(loose.glyphs.is_empty() && loose.edo_hint.is_none(), "loose = whole-family mode");
        // Three echo classes: missing font, engine-rejected font, and the
        // family collision — all non-fatal, each names its pack.
        assert!(scan.errors.iter().any(|e| e.contains("nofont") && e.contains("no .ttf")), "{:?}", scan.errors);
        assert!(scan.errors.iter().any(|e| e.contains("bad") && e.contains("rejected")), "{:?}", scan.errors);
        assert!(scan.errors.iter().any(|e| e.contains("sp-ace") && e.contains("family")), "{:?}", scan.errors);
        assert_eq!(scan.errors.len(), 3, "{:?}", scan.errors);
        // Idempotent (the disk listing is the truth, order-stable).
        let again = scan_packs_at(&root);
        assert_eq!(again.packs, scan.packs);
        // Missing root = empty quiet scan (the default state).
        let none = scan_packs_at(&root.join("no-such-dir"));
        assert!(none.packs.is_empty() && none.errors.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
