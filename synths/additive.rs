// ============================================================ slooper ==
// slooper-synth-version: 6
// additive.rs — harmonic partial bank with two morphable spectra.
//
// The additive/harmonic synth family lives in one skeleton — a bank
// of sine partials at integer multiples of the fundamental, a spectral
// level profile over the bank, and a real-time morph between TWO
// profiles.
// Profile A and B are parameter-generated — tilt (harmonic rolloff) and
// even/odd balance each — and the morph slider cross-fades the partial
// levels continuously while a note plays. A formant bell adds vowel-ish
// resonances, and a decay tilt makes high partials die faster than low
// ones — the single biggest "acoustic brightness bloom" cue (pluck-
// like at 1, organ-like at 0).
//
// Params:
//   gain        — output trim
//   partials    — bank size (4..32 harmonics; more = brighter + pricier)
//   tilt A      — profile A rolloff (-1 dark .. +1 bright)
//   odd A       — profile A even/odd balance (0 even-only .. 1 odd-only)
//   tilt B      — profile B rolloff
//   odd B       — profile B even/odd balance
//   morph A->B  — live cross-fade between the profiles (0 = A .. 1 = B)
//   formant Hz  — formant band centre (0 = off)
//   formant amt — formant bell depth around the centre (0..1)
//   attack s    — master attack to full level
//   decay s     — decay toward sustain while held
//   sustain     — sustain level (0..1)
//   release s   — release to silence after note-off
//   decay tilt  — extra per-partial decay spread with harmonic number
//   vib rate Hz — shared vibrato LFO rate
//   vib depth   — vibrato depth in cents (0..200)
//
// 8 voices, oldest-steal, table-sine DSP (allocation-free), pedal aware,
// NaN-guarded. All time-based math is per-voice state advanced once per
// SAMPLE (never per block) — host block sizes run from ~1 ms to seconds.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 8;
const PARTIALS: usize = 32;
const TAU: f64 = core::f64::consts::TAU;

// Table sine: 4096-entry linear-interpolated unit circle, phase in turns
// [0,1). ~10x cheaper than f64 sin per partial and band-clean enough for
// additive partials (each bin is a single sine).
const TLOG: usize = 12;
const TLEN: usize = 1 << TLOG;
const TMASK: usize = TLEN - 1;

struct Tab {
    v: [f32; TLEN + 1],
}

impl Tab {
    fn new() -> Tab {
        let mut v = [0.0f32; TLEN + 1];
        let mut i = 0;
        while i <= TLEN {
            v[i] = ((i as f64 / TLEN as f64) * TAU).sin() as f32;
            i += 1;
        }
        Tab { v }
    }
    #[inline(always)]
    fn sin(&self, turns: f32) -> f32 {
        let x = turns * TLEN as f32;
        let i = x as i32;
        let f = x - i as f32;
        let a = self.v[(i as usize) & TMASK];
        let b = self.v[((i as usize) + 1) & TMASK];
        a + (b - a) * f
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Attack,
    Decay,
    Sustain,
    Release,
    Off,
}

#[derive(Clone, Copy)]
struct Voice {
    alive: bool,
    note: u8,
    held: bool,
    pedal_held: bool,
    seeded: bool,
    stage: Stage,
    env: f64,
    vel: f64,
    freq: f64,
    phase: [f32; PARTIALS],
    // Per-partial amplitude: profile level × per-partial decay state.
    amp: [f32; PARTIALS],
    vibph: f64,
    age: f64,
}

impl Voice {
    const DEAD: Voice = Voice {
        alive: false, note: 0, held: false, pedal_held: false,
        seeded: false, stage: Stage::Off, env: 0.0, vel: 0.0, freq: 0.0,
        phase: [0.0; PARTIALS],
        amp: [0.0; PARTIALS],
        vibph: 0.0, age: 0.0,
    };
}

struct Add {
    sr: f64,
    tab: Tab,
    voices: [Voice; VOICES],
    next: usize,
    pedal: bool,
    // Cached morph profile (recomputed at block rate when the controlling
    // params move — params are control-rate inputs anyway).
    prof: [f32; PARTIALS],
    prof_key: [i32; 8],
}

// Profile A/B: level of harmonic k (1-based) from rolloff tilt and odd
// balance. tilt -1 => ~1/k^2 dark; +1 => rising; odd 0.5 balanced.
fn profile_at(k: usize, tilt: f64, odd: f64) -> f32 {
    let n = k as f64;
    let rolloff = n.powf(-1.0 - tilt * 1.5);
    let oddw = if k & 1 == 1 { odd * 2.0 } else { (1.0 - odd) * 2.0 };
    // Floor on the suppressed side so odd-only still keeps sub body.
    (rolloff * (0.25 + 0.75 * oddw.min(2.0))) as f32
}

extern "C" fn create(sr: f64, _host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let sr = if sr > 1.0 { sr } else { 48000.0 };
    let s = Box::new(Add {
        sr,
        tab: Tab::new(),
        voices: [Voice::DEAD; VOICES],
        next: 0,
        pedal: false,
        prof: [0.0; PARTIALS],
        prof_key: [i32::MIN; 8],
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Add) });
    }
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Add) };
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
    v.stage = Stage::Attack;
    v.freq = 440.0 * (2.0f64).powf((note as f64 - 69.0) / 12.0);
    v.vel = (vel.max(1) as f64 / 127.0).powf(0.6);
    // Random start phases: stacked sines at phase 0 sum to a huge transient.
    let mut seed = (note as u32)
        .wrapping_mul(2654435761)
        ^ ((idx as u32) << 8)
        ^ 0x9e37_79b9;
    for ph in v.phase.iter_mut() {
        seed = seed
            .wrapping_mul(1664525)
            .wrapping_add(1013904223);
        *ph = (seed >> 8) as f32 / 16777216.0;
    }
    p.voices[idx] = v;
}

extern "C" fn note_off(s: *mut core::ffi::c_void, _ch: u8, note: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Add) };
    for v in &mut p.voices {
        if v.alive && v.note == note {
            v.held = false;
            if p.pedal {
                v.pedal_held = true;
            } else if v.stage != Stage::Release {
                v.stage = Stage::Release;
            }
        }
    }
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Add) };
    for v in &mut p.voices {
        v.alive = false;
    }
    p.pedal = false;
}

extern "C" fn cc(s: *mut core::ffi::c_void, _ch: u8, num: u8, val: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Add) };
    if num == 64 {
        let on = val >= 64;
        if !on && p.pedal {
            for v in &mut p.voices {
                if v.alive && v.pedal_held && !v.held && v.stage != Stage::Release {
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
    let p = unsafe { &mut *(s as *mut Add) };
    let Add {
        sr,
        tab,
        voices,
        pedal,
        prof,
        prof_key,
        ..
    } = p;
    let sr = *sr;
    let pedal = *pedal;

    let gain = unsafe { *params.add(0) }.clamp(0.0, 1.0) as f64;
    let np = unsafe { *params.add(1) }.clamp(4.0, PARTIALS as f32) as usize;
    let tilt_a = unsafe { *params.add(2) }.clamp(-1.0, 1.0) as f64;
    let odd_a = unsafe { *params.add(3) }.clamp(0.0, 1.0) as f64;
    let tilt_b = unsafe { *params.add(4) }.clamp(-1.0, 1.0) as f64;
    let odd_b = unsafe { *params.add(5) }.clamp(0.0, 1.0) as f64;
    let morph = unsafe { *params.add(6) }.clamp(0.0, 1.0) as f64;
    let form_hz = unsafe { *params.add(7) }.clamp(0.0, 5000.0) as f64;
    let form_amt = unsafe { *params.add(8) }.clamp(0.0, 1.0) as f64;
    let att = unsafe { *params.add(9) }.clamp(0.0, 4.0) as f64;
    let dec = unsafe { *params.add(10) }.clamp(0.0, 8.0) as f64;
    let sus = unsafe { *params.add(11) }.clamp(0.0, 1.0) as f64;
    let rel = unsafe { *params.add(12) }.clamp(0.01, 12.0) as f64;
    let dtilt = unsafe { *params.add(13) }.clamp(0.0, 1.0) as f64;
    let vr = unsafe { *params.add(14) }.clamp(0.0, 20.0) as f64;
    let vdep = unsafe { *params.add(15) }.clamp(0.0, 200.0) as f64;

    // --- profile (block rate; every slider that shapes it is in the key,
    // INCLUDING morph, or dragging morph would do nothing) ---
    let key = [
        (tilt_a * 200.0) as i32,
        (odd_a * 200.0) as i32,
        (tilt_b * 200.0) as i32,
        (odd_b * 200.0) as i32,
        (morph * 200.0) as i32,
        (form_hz / 4.0) as i32,
        (form_amt * 100.0) as i32,
        np as i32,
    ];
    if key != *prof_key {
        for k in 1..=PARTIALS {
            let mut l = profile_at(k, tilt_a, odd_a) * (1.0 - morph) as f32
                + profile_at(k, tilt_b, odd_b) * morph as f32;
            if form_amt > 0.001 && form_hz > 1.0 {
                // Gaussian formant bell; profile-space reference pitch is
                // A4 so the bell sits at a fixed reference frequency.
                let f = k as f64 * 440.0;
                let w = (form_hz / 3.0).max(40.0);
                let d = (f - form_hz) / w;
                l *= (1.0 + (form_amt * 4.0) * (-d * d).exp()) as f32;
            }
            prof[k - 1] = l;
        }
        // ENERGY-normalise (tier-2 rule: peak-normalising a broad spectrum
        // is far too hot once the partials sum).
        let mut e = 0.0f64;
        for k in 0..np {
            e += (prof[k] as f64) * (prof[k] as f64);
        }
        let norm = 1.0 / (e.max(1e-9)).sqrt();
        for k in 0..PARTIALS {
            prof[k] = if k < np { prof[k] * norm as f32 } else { 0.0 };
        }
        *prof_key = key;
    }

    // Per-partial decay factors: harmonic n gets `dtilt`-scaled extra
    // decay (audible ~0.4 s collapse for the 32nd partial at dtilt 1).
    let mut dpart = [1.0f32; PARTIALS];
    for k in 1..=PARTIALS {
        let n = k as f64;
        dpart[k - 1] =
            (-dtilt * 0.06 * (n - 1.0) * n / sr).exp() as f32;
    }

    let k_att = if att > 0.0005 { 1.0 / (att * sr) } else { 4.0 };
    let k_dec = if dec > 0.005 {
        (1.0 - sus).min(1.0) / (dec * sr)
    } else {
        0.0
    };
    let k_rel = 1.0 - (-5.0 / (rel * sr)).exp();
    let vib_inc = vr / sr;

    for k in 0..n {
        for v in voices.iter_mut() {
            if !v.alive {
                continue;
            }
            let f = v.freq;
            if f < 6.0 || f > sr * 0.4 {
                v.alive = false;
                continue;
            }

            // --- master ADSR (sample-exact, block-size honest) ---
            match v.stage {
                Stage::Attack => {
                    v.env += k_att;
                    if v.env >= 1.0 {
                        v.env = 1.0;
                    }
                    if v.env >= 1.0 || k_dec == 0.0 {
                        v.stage = Stage::Sustain;
                    } else {
                        v.stage = Stage::Decay;
                    }
                }
                Stage::Decay => {
                    v.env -= k_dec;
                    if v.env <= sus {
                        v.env = sus;
                        v.stage = Stage::Sustain;
                    }
                }
                Stage::Sustain => {}
                Stage::Release => {
                    v.env -= v.env * k_rel;
                    if v.env <= 0.001 {
                        v.alive = false;
                        continue;
                    }
                }
                Stage::Off => {
                    v.alive = false;
                    continue;
                }
            }
            if !v.held && !pedal && v.stage != Stage::Release {
                v.stage = Stage::Release;
            }

            // First sounding sample: seed partial amps from the profile.
            if !v.seeded {
                v.seeded = true;
                for i in 0..PARTIALS {
                    v.amp[i] = prof[i];
                }
            }

            // --- shared vibrato: one LFO, ratio deviation around f0 ---
            v.vibph += vib_inc;
            if v.vibph >= 1.0 {
                v.vibph -= 1.0;
            }
            let vib_mul = if vdep < 0.5 || vr < 0.05 {
                1.0f64
            } else {
                1.0 + (vdep / 1200.0) * (v.vibph * TAU).sin()
            };

            let inc = (f * vib_mul / sr) as f32;
            let mut acc = 0.0f32;
            for i in 0..np {
                let mut x = v.phase[i] + inc * (i as f32 + 1.0);
                x -= x.floor();
                v.phase[i] = x;
                v.amp[i] *= dpart[i];
                acc += tab.sin(x) * v.amp[i];
            }
            let raw = acc * (v.env * v.vel * gain * 0.35) as f32;
            if raw.is_finite() {
                unsafe { *out.add(k) += raw };
            } else {
                *v = Voice::DEAD;
                continue;
            }
            v.age += 1.0 / sr;
        }
    }
}

static PARAMS: [SynthParamV1; 16] = [
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
    SynthParamV1 { label: c"partials".as_ptr(), min: 4.0, max: 32.0, default: 16.0 },
    SynthParamV1 { label: c"tilt A".as_ptr(), min: -1.0, max: 1.0, default: 0.0 },
    SynthParamV1 { label: c"odd A".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    SynthParamV1 { label: c"tilt B".as_ptr(), min: -1.0, max: 1.0, default: 0.6 },
    SynthParamV1 { label: c"odd B".as_ptr(), min: 0.0, max: 1.0, default: 0.9 },
    SynthParamV1 { label: c"morph A->B".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    SynthParamV1 { label: c"formant Hz".as_ptr(), min: 0.0, max: 5000.0, default: 0.0 },
    SynthParamV1 { label: c"formant amt".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    SynthParamV1 { label: c"attack s".as_ptr(), min: 0.0, max: 4.0, default: 0.005 },
    SynthParamV1 { label: c"decay s".as_ptr(), min: 0.0, max: 8.0, default: 1.2 },
    SynthParamV1 { label: c"sustain".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    SynthParamV1 { label: c"release s".as_ptr(), min: 0.01, max: 12.0, default: 0.35 },
    SynthParamV1 { label: c"decay tilt".as_ptr(), min: 0.0, max: 1.0, default: 0.35 },
    SynthParamV1 { label: c"vib rate".as_ptr(), min: 0.0, max: 20.0, default: 5.4 },
    SynthParamV1 { label: c"vib depth c".as_ptr(), min: 0.0, max: 200.0, default: 12.0 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"additive".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Harmonic partial bank with two morphable spectra, formant bell and per-partial decay tilt. Organ, strings, vocal pads, brass bloom.".as_ptr(),
    n_params: 16,
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
