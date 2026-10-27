// ============================================================ slooper ==
// slooper-synth-version: 6
// pad.rs — a slow, warm analog-style pad.
//
// Per voice: three sine partials (fundamental, a detuned twin for the
// chorus shimmer, and a sub an octave down), run through a Chamberlin
// state-variable low-pass whose cutoff the envelope sweeps open. Long
// attack, sustain while the key is held, long release — the classic pad
// shape. Notes crossfade on overlapping voices; sustain pedal (CC 64)
// holds notes until the pedal lifts.
//
// Params:
//   gain     — output trim
//   attack   — seconds to full level (slow is the point: 0.3..1.5 s)
//   release  — seconds to silence after note-off
//   cutoff   — low-pass ceiling in Hz (brightness ceiling)
//   detune   — cents between the two upper partials (chorus width)
//   bright   — how much velocity and the envelope open the filter
//
// 10 voices, oldest-steal, allocation-free DSP, NaN-guarded.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 10;
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
    pedal_held: bool, // note gone, waiting for pedal-up
    env: f64,
    vel: f64,
    freq: f64,
    p1: f64,
    p2: f64,
    p3: f64,
    // Chamberlin SVF state (low-pass outputs).
    lp1: f64,
    bp1: f64,
    lp2: f64,
    bp2: f64,
    age: f64,
}

struct Pad {
    sr: f64,
    voices: [Voice; VOICES],
    next: usize,
    pedal: bool,
}

impl Voice {
    const DEAD: Voice = Voice {
        alive: false, note: 0, stage: Stage::Dead, pedal_held: false,
        env: 0.0, vel: 0.0, freq: 0.0, p1: 0.0, p2: 0.0, p3: 0.0,
        lp1: 0.0, bp1: 0.0, lp2: 0.0, bp2: 0.0, age: 0.0,
    };
}

extern "C" fn create(sr: f64, _host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let s = Box::new(Pad {
        sr: if sr > 1.0 { sr } else { 48000.0 },
        voices: [Voice::DEAD; VOICES],
        next: 0,
        pedal: false,
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Pad) });
    }
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Pad) };
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
    v.freq = 440.0 * (2.0f64).powf((note as f64 - 69.0) / 12.0);
    v.vel = (vel.max(1) as f64 / 127.0).powf(0.8);
    v.stage = Stage::Attack;
    p.voices[idx] = v;
}

extern "C" fn note_off(s: *mut core::ffi::c_void, _ch: u8, note: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Pad) };
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
    let p = unsafe { &mut *(s as *mut Pad) };
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
    let p = unsafe { &mut *(s as *mut Pad) };
    if num == 64 {
        // Sustain pedal.
        let on = val >= 64;
        if !on && p.pedal {
            // Pedal lifted: everything held by the pedal releases now.
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
    let p = unsafe { &mut *(s as *mut Pad) };
    let sr = p.sr;

    let gain = unsafe { *params.add(0) }.clamp(0.0, 1.0) as f64;
    let attack = unsafe { *params.add(1) }.clamp(0.01, 3.0) as f64;
    let release = unsafe { *params.add(2) }.clamp(0.05, 5.0) as f64;
    let cutoff = unsafe { *params.add(3) }.clamp(150.0, 9000.0) as f64;
    let detune = unsafe { *params.add(4) }.clamp(0.0, 30.0) as f64;
    let bright = unsafe { *params.add(5) }.clamp(0.0, 1.0) as f64;

    let k_att = 1.0 / (attack * sr);
    let k_rel = 1.0 / (release * sr);
    let q = 1.8; // gentle SVF resonance (Chamberlin d = 1/Q)
    let dcoef = 1.0 / q;

    for k in 0..n {
        for v in &mut p.voices {
            if !v.alive {
                continue;
            }
            // --- envelope (linear attack, sustain, exponential release) ---
            match v.stage {
                Stage::Attack => {
                    v.env += k_att;
                    if v.env >= 1.0 {
                        v.env = 0.8;
                        v.stage = Stage::Sustain;
                    }
                }
                Stage::Sustain => {
                    // Settle toward 0.8 so repeated notes stay even.
                    v.env += (0.8 - v.env) * 0.0005;
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

            // --- oscillators: fundamental + detuned twin + sub octave ---
            let f = v.freq;
            let cents = (detune / 1200.0).exp2(); // semitone ratio for the twin
            v.p1 += TAU * f / sr;
            v.p2 += TAU * f * cents / sr;
            v.p3 += TAU * f * 0.5 / sr;
            let osc = v.p1.sin() + 0.7 * v.p2.sin() + 0.35 * v.p3.sin();

            // --- Chamberlin low-pass, cutoff swept by env & velocity ---
            let fcut = (cutoff * (0.25 + 0.75 * v.env) * (0.6 + 0.4 * v.vel * bright.max(0.5))
                + 120.0)
                .min(sr * 0.45);
            let fnk = (TAU * fcut / sr).sin().min(0.85);
            v.bp1 += fnk * (osc - v.lp1 - dcoef * v.bp1);
            v.lp1 += fnk * v.bp1;
            v.bp2 += fnk * (v.lp1 - v.lp2 - dcoef * v.bp2);
            v.lp2 += fnk * v.bp2;

            let smp = v.lp2 * v.env * v.vel * gain * 0.35;
            if smp.is_finite() {
                unsafe { *out.add(k) += smp as f32 };
            }
            v.age += 1.0 / sr;
        }
    }
}

static PARAMS: [SynthParamV1; 6] = [
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
    SynthParamV1 { label: c"attack".as_ptr(), min: 0.01, max: 3.0, default: 0.6 },
    SynthParamV1 { label: c"release".as_ptr(), min: 0.05, max: 5.0, default: 1.4 },
    SynthParamV1 { label: c"cutoff".as_ptr(), min: 150.0, max: 9000.0, default: 2400.0 },
    SynthParamV1 { label: c"detune".as_ptr(), min: 0.0, max: 30.0, default: 9.0 },
    SynthParamV1 { label: c"bright".as_ptr(), min: 0.0, max: 1.0, default: 0.6 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"pad".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Warm analog pad: detuned sines through an envelope-swept low-pass, sustain pedal aware.".as_ptr(),
    n_params: 6,
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
