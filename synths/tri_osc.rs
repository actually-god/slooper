// ============================================================ slooper ==
// slooper-synth-version: 6
// tri_osc.rs — the subtractive synth workhorse.
//
// The classic three-oscillator subtractive analog is all one machine —
// waveform oscs + detune + sub + a resonant low-pass with an envelope on
// it. This is that machine. Three oscillators per voice (main, detuned
// twin, sub an octave down), a 24 dB low-pass, a simple A/R amp envelope
// with filter-envelope and optional glide. Harmonic-limited waveforms keep
// aliasing down without a wavetable engine.
//
// Params:
//   gain     — output trim
//   waveform — 0 sine, 1 triangle, 2 saw, 3 square (quantized in DSP)
//   octave   — transpose in steps: -2, -1, 0, +1 (quantized)
//   sub      — sub-oscillator level (one octave below, sine)
//   detune   — cents between main and twin (width / fatness)
//   cutoff   — low-pass base cutoff in Hz
//   res      — low-pass resonance (Q ~ 0.5..8)
//   fenv     — how far the envelope sweeps the cutoff (0 = none)
//   attack   — seconds to full level
//   release  — seconds to silence after note-off
//   glide    — portamento seconds between notes (0 = off)
//
// 8 voices, oldest-steal, allocation-free DSP, NaN-guarded, pedal aware.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 8;
const TAU: f64 = core::f64::consts::TAU;

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Attack,
    Sustain,
    Release,
    Dead,
}

#[derive(Clone, Copy)]
struct Voice {
    alive: bool,
    note: u8,
    stage: Stage,
    pedal_held: bool,
    env: f64,
    vel: f64,
    freq: f64,     // current pitch (glide slews this toward target)
    target: f64,
    o1: f64,
    o2: f64,
    o3: f64,
    // Two cascaded Chamberlin 2-poles = 24 dB low-pass.
    lp1a: f64, bp1a: f64, lp2a: f64, bp2a: f64,
    lp1b: f64, bp1b: f64, lp2b: f64, bp2b: f64,
    age: f64,
}

struct TriOsc {
    sr: f64,
    voices: [Voice; VOICES],
    next: usize,
    last_freq: f64, // glide source
    pedal: bool,
}

impl Voice {
    const DEAD: Voice = Voice {
        alive: false, note: 0, stage: Stage::Dead, pedal_held: false,
        env: 0.0, vel: 0.0, freq: 0.0, target: 0.0, o1: 0.0, o2: 0.0, o3: 0.0,
        lp1a: 0.0, bp1a: 0.0, lp2a: 0.0, bp2a: 0.0,
        lp1b: 0.0, bp1b: 0.0, lp2b: 0.0, bp2b: 0.0, age: 0.0,
    };
}

/// Harmonic-limited waveforms (6 partials max) — naive waves alias badly
/// at synth pitches; truncating the series is cheap and mostly enough.
/// `mode` is already quantized to 0..=3.
fn osc(p: f64, mode: i32) -> f64 {
    match mode {
        0 => p.sin(),
        1 => {
            // Triangle: odd harmonics, 1/k^2, alternating sign.
            let mut s = 0.0;
            let mut k = 1;
            let mut n = 0;
            while k <= 9 {
                let sign = if n % 2 == 0 { 1.0 } else { -1.0 };
                s += sign * (k as f64 * p).sin() / (k as f64 * k as f64);
                k += 2;
                n += 1;
            }
            s * 8.0 / (core::f64::consts::PI * core::f64::consts::PI) * 1.25
        }
        2 => {
            // Saw: all harmonics 1/k.
            let mut s = 0.0;
            let mut k = 1;
            while k <= 8 {
                s += (k as f64 * p).sin() / k as f64;
                k += 1;
            }
            s * 2.0 / core::f64::consts::PI * 1.05
        }
        _ => {
            // Square: odd harmonics 1/k.
            let mut s = 0.0;
            let mut k = 1;
            while k <= 9 {
                s += (k as f64 * p).sin() / k as f64;
                k += 2;
            }
            s * 4.0 / core::f64::consts::PI * 0.95
        }
    }
}

extern "C" fn create(sr: f64, _host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let s = Box::new(TriOsc {
        sr: if sr > 1.0 { sr } else { 48000.0 },
        voices: [Voice::DEAD; VOICES],
        next: 0,
        last_freq: 440.0,
        pedal: false,
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut TriOsc) });
    }
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut TriOsc) };
    let mut idx = None;
    for k in 0..VOICES {
        if !p.voices[k].alive {
            idx = Some(k);
            break;
        }
    }
    let idx = idx.unwrap_or_else(|| {
        let mut best = p.next;
        let mut best_age = -1.0;
        for k in 0..VOICES {
            if p.voices[k].age > best_age {
                best_age = p.voices[k].age;
                best = k;
            }
        }
        p.next = (best + 1) % VOICES;
        best
    });
    let mut v = Voice::DEAD;
    v.alive = true;
    v.note = note;
    v.target = 440.0 * (2.0f64).powf((note as f64 - 69.0) / 12.0);
    // Glide starts from wherever the synth last was; hard-attack snaps.
    v.freq = p.last_freq;
    v.vel = (vel.max(1) as f64 / 127.0).powf(0.7);
    v.stage = Stage::Attack;
    p.last_freq = v.target;
    p.voices[idx] = v;
}

extern "C" fn note_off(s: *mut core::ffi::c_void, _ch: u8, note: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut TriOsc) };
    for v in &mut p.voices {
        if v.alive && v.note == note {
            if p.pedal {
                v.pedal_held = true;
            } else {
                v.stage = Stage::Release;
            }
        }
    }
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut TriOsc) };
    for v in &mut p.voices {
        v.alive = false;
        v.stage = Stage::Dead;
    }
    p.pedal = false;
}

extern "C" fn cc(s: *mut core::ffi::c_void, _ch: u8, num: u8, val: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut TriOsc) };
    if num == 64 {
        let on = val >= 64;
        if !on && p.pedal {
            for v in &mut p.voices {
                if v.alive && v.pedal_held {
                    v.pedal_held = false;
                    v.stage = Stage::Release;
                }
            }
        }
        p.pedal = on;
    }
}

extern "C" fn press(_s: *mut core::ffi::c_void, _ch: u8, _val: u8) {}

extern "C" fn process(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut TriOsc) };
    let sr = p.sr;

    let gain = unsafe { *params.add(0) }.clamp(0.0, 1.0) as f64;
    let mode = (unsafe { *params.add(1) }.clamp(0.0, 3.0) + 0.5) as i32;
    // Octave slider maps -2..1 to its four steps.
    let oct = ((unsafe { *params.add(2) }.clamp(-2.0, 1.0) + 0.5) as i32) as f64;
    let oct_f = 2.0f64.powf(oct);
    let sub = unsafe { *params.add(3) }.clamp(0.0, 1.0) as f64;
    let detune = unsafe { *params.add(4) }.clamp(0.0, 50.0) as f64;
    let cutoff = unsafe { *params.add(5) }.clamp(40.0, 16000.0) as f64;
    let res = unsafe { *params.add(6) }.clamp(0.0, 1.0) as f64;
    let fenv = unsafe { *params.add(7) }.clamp(0.0, 1.0) as f64;
    let attack = unsafe { *params.add(8) }.clamp(0.001, 2.0) as f64;
    let release = unsafe { *params.add(9) }.clamp(0.01, 6.0) as f64;
    let glide = unsafe { *params.add(10) }.clamp(0.0, 0.5) as f64;

    let k_att = 1.0 / (attack * sr);
    let k_rel = 1.0 / (release * sr);
    let q = 0.5 + res * 7.5;
    let dcoef = 1.0 / q;
    let cents = (detune / 1200.0).exp2();
    let glide_k = if glide > 0.0005 {
        1.0 - (-1.0 / (glide * sr)).exp()
    } else {
        1.0
    };

    for k in 0..n {
        for v in &mut p.voices {
            if !v.alive {
                continue;
            }
            // --- envelope ---
            match v.stage {
                Stage::Attack => {
                    v.env += k_att;
                    if v.env >= 1.0 {
                        v.env = 0.75;
                        v.stage = Stage::Sustain;
                    }
                }
                Stage::Sustain => {
                    v.env += (0.75 - v.env) * 0.0005;
                }
                Stage::Release => {
                    v.env *= (-k_rel).exp();
                    if v.env < 1.0e-4 {
                        v.alive = false;
                        continue;
                    }
                }
                Stage::Dead => {
                    v.alive = false;
                    continue;
                }
            }

            // --- pitch (glide toward target) and oscillators ---
            v.freq += (v.target - v.freq) * glide_k;
            let f = v.freq * oct_f;
            if f < 6.0 || f > sr * 0.4 {
                v.alive = false;
                continue;
            }
            let inc = TAU * f / sr;
            v.o1 += inc;
            v.o2 += inc * cents;
            v.o3 += inc * 0.5;
            if v.o1 > TAU { v.o1 -= TAU; }
            if v.o2 > TAU { v.o2 -= TAU; }
            if v.o3 > TAU { v.o3 -= TAU; }
            let mut mix = osc(v.o1, mode) + 0.7 * osc(v.o2, mode);
            mix += sub * v.o3.sin();

            // --- 24 dB low-pass, cutoff swept by the amp envelope ---
            let sweep = (1.0 - fenv) + fenv * (0.1 + 0.9 * v.env) * (0.5 + 0.5 * v.vel);
            let fcut = (cutoff * sweep).clamp(40.0, sr * 0.45);
            let fnk = (TAU * fcut / sr).sin().min(0.85);
            v.bp1a += fnk * (mix - v.lp1a - dcoef * v.bp1a);
            v.lp1a += fnk * v.bp1a;
            v.bp2a += fnk * (v.lp1a - v.lp2a - dcoef * v.bp2a);
            v.lp2a += fnk * v.bp2a;
            v.bp1b += fnk * (v.lp2a - v.lp1b - dcoef * v.bp1b);
            v.lp1b += fnk * v.bp1b;
            v.bp2b += fnk * (v.lp1b - v.lp2b - dcoef * v.bp2b);
            v.lp2b += fnk * v.bp2b;

            let smp = v.lp2b * v.env * v.vel * gain * 0.22;
            if smp.is_finite() {
                unsafe { *out.add(k) += smp as f32 };
            } else {
                *v = Voice::DEAD;
                continue;
            }
            v.age += 1.0 / sr;
        }
    }
}

static PARAMS: [SynthParamV1; 11] = [
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
    SynthParamV1 { label: c"waveform (0sine 1tri 2saw 3sqr)".as_ptr(), min: 0.0, max: 3.0, default: 2.0 },
    SynthParamV1 { label: c"octave -2..+1".as_ptr(), min: -2.0, max: 1.0, default: 0.0 },
    SynthParamV1 { label: c"sub".as_ptr(), min: 0.0, max: 1.0, default: 0.3 },
    SynthParamV1 { label: c"detune cents".as_ptr(), min: 0.0, max: 50.0, default: 12.0 },
    SynthParamV1 { label: c"cutoff Hz".as_ptr(), min: 40.0, max: 16000.0, default: 4000.0 },
    SynthParamV1 { label: c"resonance".as_ptr(), min: 0.0, max: 1.0, default: 0.2 },
    SynthParamV1 { label: c"filter env".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    SynthParamV1 { label: c"attack s".as_ptr(), min: 0.001, max: 2.0, default: 0.006 },
    SynthParamV1 { label: c"release s".as_ptr(), min: 0.01, max: 6.0, default: 0.35 },
    SynthParamV1 { label: c"glide s".as_ptr(), min: 0.0, max: 0.5, default: 0.0 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"tri_osc".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Subtractive workhorse: 3 oscillators (detune + sub) into a 24 dB env-swept low-pass. The three-osc subtractive workhorse role. Pedal aware.".as_ptr(),
    n_params: 11,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    note_on,
    note_off,
    all_off,
    cc,
    press,
    process,
};

#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 {
    &DESC
}
