// ============================================================ slooper ==
// vfilter.rs — resonant state-variable filter with envelope "auto-wah".
// Covers Fruity Filter (automation-optimized VCF), the single-filter core
// of Fruity Love Philter, and Fruity Wah. See docs/FL-SUITE.md.
//
// Chamberlin state-variable filter: one pole pair (12 dB/oct) or two in
// cascade (24). LP / BP / HP / notch picked by mode. An envelope follower
// on the input pushes cutoff upward — env=1 is the classic wah sound.
include!("effect_api.rs");

const NPARAMS: usize = 10;
const P_MODE: usize = 0; //  0 LP, 1 BP, 2 HP, 3 notch
const P_CUT: usize = 1; //   20..18000 Hz
const P_RES: usize = 2; //   0..1 resonance
const P_ENV: usize = 3; //   0..1 envelope amount
const P_ATT: usize = 4; //   0.1..30 ms follower attack
const P_REL: usize = 5; //   5..600 ms follower release
const P_DRIVE: usize = 6; // 1..8 pre saturation
const P_SLOPE: usize = 7; // 0=12dB 1=24dB
const P_MIX: usize = 8; //   0..1
const P_OUT: usize = 9; //   0..2

struct Stage {
    vl: f64,
    vb: f64,
}

struct St {
    sr: f64,
    s1: Stage,
    s2: Stage,
    env: f64,
    c_att: f64,
    c_rel: f64,
    k_att: f32,
    k_rel: f32,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        s1: Stage { vl: 0.0, vb: 0.0 },
        s2: Stage { vl: 0.0, vb: 0.0 },
        env: 0.0,
        c_att: (-(1.0 / (1.0 * 0.001 * sr))).exp(),
        c_rel: (-(1.0 / (120.0 * 0.001 * sr))).exp(),
        k_att: 1.0,
        k_rel: 120.0,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

#[inline]
fn tc(ms: f64, sr: f64) -> f64 {
    ((-1.0) / ((ms.max(0.05) * 0.001) * sr)).exp()
}

#[inline]
fn svf(st: &mut Stage, x: f64, f: f64, q: f64, mode: i32) -> f64 {
    // Chamberlin SVF; f < 1, q in [0,1).
    let d = 1.0 - q;
    st.vl += f * st.vb;
    let vh = x - st.vl - d * st.vb;
    st.vb += f * vh;
    let out = match mode {
        0 => st.vl,
        1 => st.vb,
        2 => vh,
        _ => st.vl + vh,
    };
    if !out.is_finite() {
        st.vl = 0.0;
        st.vb = 0.0;
        return 0.0;
    }
    out
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let mode = (unsafe { *params.add(P_MODE) } as f64).round() as i32;
    let cut = unsafe { *params.add(P_CUT) } as f64;
    let res = unsafe { *params.add(P_RES) } as f64;
    let env_amt = unsafe { *params.add(P_ENV) } as f64;
    let att = unsafe { *params.add(P_ATT) };
    let rel = unsafe { *params.add(P_REL) };
    let drive = unsafe { *params.add(P_DRIVE) } as f64;
    let two = (unsafe { *params.add(P_SLOPE) } as f64) > 0.5;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let xi = input as f64;
    if !xi.is_finite() {
        s.s1 = Stage { vl: 0.0, vb: 0.0 };
        s.s2 = Stage { vl: 0.0, vb: 0.0 };
        s.env = 0.0;
        return 0.0;
    }

    if att != s.k_att {
        s.k_att = att;
        s.c_att = tc(att as f64, s.sr);
    }
    if rel != s.k_rel {
        s.k_rel = rel;
        s.c_rel = tc(rel as f64, s.sr);
    }

    // Envelope follower (peak, separate a/r).
    let a = xi.abs();
    if a > s.env {
        s.env += (a - s.env) * (1.0 - s.c_att);
    } else {
        s.env += (a - s.env) * (1.0 - s.c_rel);
    }

    // Drive into a soft saturator so resonance peaks stay bounded.
    let mut x = (xi * drive).tanh();
    // resonance -> filter q in [0, 0.99]; 24 dB mode caps q lower so the
    // cascade never self-oscillates hot.
    let q = res * if two { 0.85 } else { 0.97 };

    // Cutoff, pushed by the envelope (wah).
    let mut fc = cut;
    if env_amt > 0.0 {
        fc = cut * (1.0 + s.env * env_amt * 9.0);
    }
    let f = (core::f64::consts::PI * fc.min(0.45 * s.sr) / s.sr).sin();

    x = svf(&mut s.s1, x, f, q, mode);
    if two {
        x = svf(&mut s.s2, x, f, q, mode);
    }
    // mode's LP/BP/HP gains differ a lot in level; keep it musical.
    let o = (xi * (1.0 - mix) + x * mix) * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-2.0, 2.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"mode (0 LP/1 BP/2 HP/3 notch)".as_ptr(), min: 0.0, max: 3.0, default: 0.0 },
    EffectParamV1 { label: c"cutoff".as_ptr(), min: 20.0, max: 18000.0, default: 4000.0 },
    EffectParamV1 { label: c"resonance".as_ptr(), min: 0.0, max: 1.0, default: 0.35 },
    EffectParamV1 { label: c"env (auto-wah)".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"attack ms".as_ptr(), min: 0.1, max: 30.0, default: 1.0 },
    EffectParamV1 { label: c"release ms".as_ptr(), min: 5.0, max: 600.0, default: 120.0 },
    EffectParamV1 { label: c"drive".as_ptr(), min: 1.0, max: 8.0, default: 1.0 },
    EffectParamV1 { label: c"slope (0=12/1=24)".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 0.9 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Filter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"State-variable resonant filter: LP/BP/HP/notch, 12 or 24 dB/oct, drive, and an envelope follower for auto-wah. The Fruity Filter / Wah role.".as_ptr(),
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
