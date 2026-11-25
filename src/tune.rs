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

use std::sync::atomic::{AtomicU32, Ordering};

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
/// pins pass it directly).
#[inline]
pub fn key(n: u32, root: u8, k: u8) -> f64 {
    (k as f64 - root as f64) * (12.0 / n as f64) + (root as f64 - 69.0)
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
    /// (`"C4+50c"`).
    pub text: String,
    /// True when the step's pitch is an exact integer number of semitones
    /// from A4 — i.e. it coincides with the 12-EDO row the name says.
    pub anchor: bool,
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
        };
    }
    // Exact half-semitone ties go to the lower row (tk.floor()); every
    // other offset takes its true nearest. f64 equality is safe here:
    // a true tie is a binary-exact .5, and no true non-tie is within
    // 1e-12 of one for n ≤ 128.
    let nearest = if tk - tk.floor() == 0.5 {
        tk.floor()
    } else {
        tk.round()
    };
    let cents = ((tk - nearest) * 100.0).round() as i64;
    NoteLabel {
        text: format!("{}{:+}c", std_name(nearest as i32 + 69), cents),
        anchor: false,
    }
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
    fn v166_root_A_reanchors() {
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
}
