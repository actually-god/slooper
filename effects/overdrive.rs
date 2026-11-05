// ============================================================ slooper ==
// overdrive.rs — the drive family: analog-ish overdrive, clipper, folder.
// Consolidates FL Studio's Fruity Blood Overdrive, Fast Dist and Soft
// Clipper into one shape-able stage. See docs/FL-SUITE.md.
//
// Signal: DC-block -> pre gain -> curve (4 characters) -> tone (tilt LP/HP)
// -> presence (harmonic exciter) -> mix -> output trim. All one-pole, all
// allocation-free, NaN-safe.
include!("effect_api.rs");

const NPARAMS: usize = 6;
const P_DRIVE: usize = 0; //   0..1   pre gain 1..24x
const P_CURVE: usize = 1; //   0..3   0 soft sat, 1 hard clip, 2 fold, 3 silky
const P_TONE: usize = 2; //    0..1   <=0.5 darker (LP), >=0.5 brighter (HP)
const P_PRES: usize = 3; //    0..1   presence (high-harmonic exciter)
const P_MIX: usize = 4; //     0..1   dry/wet
const P_OUT: usize = 5; //     0..2   output trim

struct St {
    sr: f64,
    // DC blocker (one-pole HP under the shaper, keeps clipping symmetric).
    dc_x: f64,
    dc_y: f64,
    // tone state
    lp1: f64,
    // presence band
    lp3: f64,
    // cached one-pole coefficients (rebuilt only when controls move)
    tone_cache: f64,
    ct: f64,
    cp: f64,
}

/// One-pole lowpass coefficient for a TRUE cutoff frequency (exp(-2πfc/sr)).
fn one_c(fc: f64, sr: f64) -> f64 {
    ((-core::f64::consts::TAU * fc / sr).clamp(-30.0, 0.0)).exp()
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        dc_x: 0.0,
        dc_y: 0.0,
        lp1: 0.0,
        lp3: 0.0,
        tone_cache: -1.0,
        ct: 0.5,
        cp: 0.0,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// The four curve characters, x normalized-ish in ±(1+drive).
#[inline]
fn curve(curve: i32, x: f64) -> f64 {
    match curve {
        // soft analog saturation: tanh keeps fundamentals, rounds peaks.
        0 => x.tanh(),
        // hard clipper (Fast Dist vibe) with a 0.9 knee so edges stay sane.
        1 => {
            if x > 0.9 {
                0.9 + (x - 0.9).tanh() * 0.1
            } else if x < -0.9 {
                -0.9 + (x + 0.9).tanh() * 0.1
            } else {
                x
            }
        }
        // wavefolder: peaks reflect back through zero, metallic nulls.
        2 => {
            let mut y = x;
            let mut i = 0;
            while (y > 1.0 || y < -1.0) && i < 8 {
                y = if y > 0.0 { y - 2.0 } else { y + 2.0 };
                y = -y;
                i += 1;
            }
            y
        }
        // silky (Soft Clipper): x/(1+|x|) — no nulls, pure compression.
        _ => x / (1.0 + x.abs()),
    }
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let drive = unsafe { *params.add(P_DRIVE) } as f64;
    let cur = (unsafe { *params.add(P_CURVE) } as f64).round() as i32;
    let tone = unsafe { *params.add(P_TONE) } as f64;
    let pres = unsafe { *params.add(P_PRES) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let mut x = input as f64;
    if !x.is_finite() {
        s.dc_x = 0.0;
        s.dc_y = 0.0;
        s.lp1 = 0.0;
        s.lp3 = 0.0;
        return 0.0;
    }

    // DC block (~35 Hz) before the shaper so asymmetry cannot walk.
    let a = one_c(35.0, s.sr);
    let y = a * s.dc_y + (x - s.dc_x);
    s.dc_x = x;
    s.dc_y = y;
    x = y;

    // ---- shaper -------------------------------------------------------
    // Unity at drive=0 (tanh(x) at small x is ~x); pre-gain to ~24x at 1.
    let g = 1.0 + drive * drive * 23.0;
    let mut y = curve(cur, x * g);
    // compensate so output level tracks input across the drive range
    y /= g.ln().mul_add(0.45, 1.0);

    // ---- tone tilt ------------------------------------------------------
    // tone <= .5: blend the signal toward a 12 dB/oct LOWPASS (dark,
    // Blood-Overdrive "tone"); tone >= .5: blend toward the HIGHPASS
    // (thin/bright). 0.5 is pure straight-through.
    if (tone - s.tone_cache).abs() > 1e-6 {
        s.tone_cache = tone;
        let fc_t = if tone <= 0.5 {
            250.0 + (tone * 2.0) * 15750.0
        } else {
            120.0 + ((tone - 0.5) * 2.0) * 3900.0
        };
        s.ct = one_c(fc_t.min(0.45 * s.sr), s.sr);
    }
    let lp = {
        s.lp1 += (y - s.lp1) * (1.0 - s.ct);
        s.lp1
    };
    y = if tone <= 0.5 {
        lp + (y - lp) * (tone * 2.0)
    } else {
        let hp = y - lp;
        let b = (tone - 0.5) * 2.0;
        y * (1.0 - b) + hp * b
    };

    // ---- presence (harmonic exciter around the SHAPED signal) ----------
    if pres > 0.001 {
        if s.cp <= 0.0 {
            s.cp = one_c(3500.0, s.sr);
        }
        s.lp3 += (y - s.lp3) * (1.0 - s.cp);
        let hi = y - s.lp3;
        y += hi * pres * 1.6;
    }

    // ---- mix + trim ------------------------------------------------------
    let wet = y;
    let o = x * (1.0 - mix) + wet * mix;
    let o = o * out;
    let o = if o > 4.0 {
        4.0
    } else if o < -4.0 {
        -4.0
    } else {
        o
    };
    if !o.is_finite() {
        return 0.0;
    }
    o as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"drive".as_ptr(), min: 0.0, max: 1.0, default: 0.45 },
    EffectParamV1 { label: c"curve (0 sat/1 hard/2 fold/3 silky)".as_ptr(), min: 0.0, max: 3.0, default: 0.0 },
    EffectParamV1 { label: c"tone".as_ptr(), min: 0.0, max: 1.0, default: 0.55 },
    EffectParamV1 { label: c"presence".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 0.8 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Overdrive".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Analog-style drive family: soft saturation, hard clipper, wavefolder and silky soft-clip in one stage. Tone tilts dark/bright, presence adds high harmonics.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}
