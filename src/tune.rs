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

#[cfg(test)]
mod tests {
    use super::*;

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
