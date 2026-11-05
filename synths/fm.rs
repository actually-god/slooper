// ============================================================ slooper ==
// slooper-synth-version: 6
// fm.rs — two-operator FM with feedback and a sub sine.
//
// Covers the FL Suite FM task: Fruity DX10's bell/EP/organ world and the
// FM half of Sytrus / Toxic Biohazard live in the same two-operator
// skeleton — a modulator sine (its own A/D envelope and index) feeding a
// carrier with self feedback, plus an independent carrier envelope and a
// sine sub for body. Ratio 1 + index 0 is a pure sine; ratio 2/3/4 with a
// fast mod envelope is bells and mallets; slow envelopes are growls.
//
// Params:
//   gain        — output trim
//   ratio       — modulator:carrier frequency ratio (0.25..8)
//   index       — modulation index at full envelope (0..12, in carrier Hz)
//   mod_attack  — seconds for the mod envelope to peak
//   mod_decay   — seconds for the mod envelope to fall to sustain level
//   mod_hold    — sustain fraction of the mod envelope while held (0..1)
//   car_decay   — carrier decay to silence (0 = hold until note-off)
//   feedback    — carrier self-feedback (0..1): adds harmonics/buzz
//   sub         — sub-sine level an octave below (0..1)
//   bright      — output one-pole tilt (0 dark..1 open)
//
// 8 voices, oldest-steal, allocation-free DSP, NaN-guarded, pedal aware.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 8;
const TAU: f64 = core::f64::consts::TAU;

#[derive(Clone, Copy, PartialEq)]
enum MStage {
    Attack,
    Decay,
    Hold,
    Off,
}

#[derive(Clone, Copy)]
struct Voice {
    alive: bool,
    note: u8,
    held: bool,
    pedal_held: bool,
    // carrier
    cph: f64,
    cenv: f64,
    releasing: bool,
    // modulator
    mph: f64,
    menv: f64,
    mstage: MStage,
    fb: f64,     // feedback state (last modulated output)
    lp: f64,    // output tilt state
    vel: f64,
    freq: f64,
    age: f64,
}

struct Fm {
    sr: f64,
    voices: [Voice; VOICES],
    next: usize,
    pedal: bool,
}

impl Voice {
    const DEAD: Voice = Voice {
        alive: false, note: 0, held: false, pedal_held: false,
        cph: 0.0, cenv: 0.0, releasing: false,
        mph: 0.0, menv: 0.0, mstage: MStage::Attack,
        fb: 0.0, lp: 0.0, vel: 0.0, freq: 0.0, age: 0.0,
    };
}

extern "C" fn create(sr: f64, _host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let s = Box::new(Fm {
        sr: if sr > 1.0 { sr } else { 48000.0 },
        voices: [Voice::DEAD; VOICES],
        next: 0,
        pedal: false,
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Fm) });
    }
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Fm) };
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
    v.held = true;
    v.freq = 440.0 * (2.0f64).powf((note as f64 - 69.0) / 12.0);
    v.vel = (vel.max(1) as f64 / 127.0).powf(0.6);
    p.voices[idx] = v;
}

extern "C" fn note_off(s: *mut core::ffi::c_void, _ch: u8, note: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Fm) };
    for v in &mut p.voices {
        if v.alive && v.note == note {
            v.held = false;
            if p.pedal {
                v.pedal_held = true;
            } else {
                v.releasing = true;
                v.mstage = MStage::Off;
            }
        }
    }
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Fm) };
    for v in &mut p.voices {
        v.alive = false;
    }
    p.pedal = false;
}

extern "C" fn cc(s: *mut core::ffi::c_void, _ch: u8, num: u8, val: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Fm) };
    if num == 64 {
        let on = val >= 64;
        if !on && p.pedal {
            for v in &mut p.voices {
                if v.alive && v.pedal_held && !v.held {
                    v.pedal_held = false;
                    v.releasing = true;
                    v.mstage = MStage::Off;
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
    let p = unsafe { &mut *(s as *mut Fm) };
    let sr = p.sr;

    let gain = unsafe { *params.add(0) }.clamp(0.0, 1.0) as f64;
    let ratio = unsafe { *params.add(1) }.clamp(0.25, 8.0) as f64;
    let index = unsafe { *params.add(2) }.clamp(0.0, 12.0) as f64;
    let m_att = unsafe { *params.add(3) }.clamp(0.0, 2.0) as f64;
    let m_dec = unsafe { *params.add(4) }.clamp(0.01, 6.0) as f64;
    let m_hold = unsafe { *params.add(5) }.clamp(0.0, 1.0) as f64;
    let c_dec = unsafe { *params.add(6) }.clamp(0.0, 8.0) as f64;
    let fb = unsafe { *params.add(7) }.clamp(0.0, 1.0) as f64;
    let sub = unsafe { *params.add(8) }.clamp(0.0, 1.0) as f64;
    let bright = unsafe { *params.add(9) }.clamp(0.0, 1.0) as f64;

    let k_matt = if m_att > 0.0005 { 1.0 / (m_att * sr) } else { 1.0 };
    let k_mdec = 1.0 - (-3.0 / (m_dec * sr)).exp(); // to sustain in ~m_dec
    let k_crel = if c_dec > 0.01 {
        -3.0 / (c_dec * sr)
    } else {
        0.0
    };
    // Output tilt: one-pole mixing the raw sample with its lowpassed copy.
    let tilt = 0.02 + 0.98 * bright;

    for k in 0..n {
        for v in &mut p.voices {
            if !v.alive {
                continue;
            }
            let f = v.freq;
            if f < 6.0 || f > sr * 0.4 {
                v.alive = false;
                continue;
            }

            // --- modulator envelope (attack / decay-to-hold / off) ---
            match v.mstage {
                MStage::Attack => {
                    v.menv += k_matt;
                    if v.menv >= 1.0 {
                        v.menv = 1.0;
                        v.mstage = MStage::Decay;
                    }
                }
                MStage::Decay => {
                    v.menv += (m_hold - v.menv) * k_mdec;
                    if !v.held && !p.pedal {
                        v.mstage = MStage::Off;
                    } else if !v.held && v.pedal_held {
                        // Pedal keeps it in hold until the pedal lifts.
                    } else if (v.menv - m_hold).abs() < 1e-3 {
                        v.mstage = MStage::Hold;
                    }
                }
                MStage::Hold => {
                    if !v.held && !p.pedal {
                        v.mstage = MStage::Off;
                    }
                }
                MStage::Off => {
                    v.menv *= (-k_mdec * 4.0).exp().min(0.9999);
                }
            }

            // --- carrier envelope (linear attack-ish, hold or decay) ---
            if !v.releasing {
                v.cenv += (1.0 - v.cenv) * 0.004 + 0.002; // quick-ish rise
                if v.cenv > 1.0 {
                    v.cenv = 1.0;
                }
            } else {
                if c_dec > 0.01 {
                    v.cenv *= k_crel.exp();
                } else {
                    v.cenv -= 1.0 / (0.06 * sr); // no decay: 60 ms gate-off
                }
                if v.cenv <= 0.001 {
                    v.alive = false;
                    continue;
                }
            }

            // --- FM core: sine modulator, DX-style carrier feedback ---
            let mfreq = f * ratio as f64;
            v.mph += TAU * mfreq / sr;
            if v.mph > TAU {
                v.mph -= TAU;
            }
            // Modulation: frequency deviation (idx_hz) -> phase deviation
            // radians is TAU * idx_hz / sr scaled by the mod sine.
            let idx_hz = index as f64 * f * v.menv * (0.3 + 0.7 * v.vel);
            let dev = TAU * idx_hz / sr;
            let phase = v.cph + dev * v.mph.sin() + TAU * fb * v.fb * 0.25;
            let o0 = phase.sin().clamp(-1.2, 1.2);
            v.fb = o0;
            v.cph += TAU * f / sr;
            if v.cph > TAU {
                v.cph -= TAU;
            }
            let mut o = o0;

            // Sub sine an octave below keeps bass patches anchored.
            o += sub * (v.cph * 0.5).sin() * 0.8;

            // Velocity-scaled output through a tilt one-pole.
            let raw = o * v.cenv * v.vel * gain * 0.35;
            v.lp += (raw - v.lp) * tilt;
            let smp = v.lp + (raw - v.lp) * (0.15 + 0.85 * bright);
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

static PARAMS: [SynthParamV1; 10] = [
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
    SynthParamV1 { label: c"ratio x".as_ptr(), min: 0.25, max: 8.0, default: 2.0 },
    SynthParamV1 { label: c"mod index".as_ptr(), min: 0.0, max: 12.0, default: 3.0 },
    SynthParamV1 { label: c"mod attack s".as_ptr(), min: 0.0, max: 2.0, default: 0.002 },
    SynthParamV1 { label: c"mod decay s".as_ptr(), min: 0.01, max: 6.0, default: 0.6 },
    SynthParamV1 { label: c"mod hold".as_ptr(), min: 0.0, max: 1.0, default: 0.1 },
    SynthParamV1 { label: c"car decay s (0=hold)".as_ptr(), min: 0.0, max: 8.0, default: 0.0 },
    SynthParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 1.0, default: 0.2 },
    SynthParamV1 { label: c"sub".as_ptr(), min: 0.0, max: 1.0, default: 0.2 },
    SynthParamV1 { label: c"bright".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"fm".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Two-operator FM (DX10 / Sytrus FM task): ratio, index, dual envelopes, feedback, sub sine. Bells, EPs, growls, basses.".as_ptr(),
    n_params: 10,
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
