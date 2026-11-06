// ============================================================ slooper ==
// phaser.rs — classic sweeping phaser. Covers FL Studio's Fruity Phaser
// and the Vintage Phaser. See docs/FL-SUITE.md.
//
// N first-order all-pass sections cascaded, their corner frequencies
// swept by one LFO; feedback from the output re-enters the chain, which
// turns the notches into resonant peaks (the "6-stage" sound).
//
// TIER-5 (ABI v3): dual-path stereo phaser (the Vintage-Phaser job). The
// cascade runs TWICE over a shared-mid input; the R path's LFO is phase-
// offset by `spread` (0..1 of a sweep cycle) so the two sides' notches
// travel in opposite directions — a centre source decorrelates across
// the sweep while the LEFT path stays the exact mono circuit (the v1
// fallback returns it, so every pre-tier-5 mono behaviour is unchanged).
// spread = 0 → both paths run identical phase → bit-exact mono output.
// Params 0..6 are sticky (tier 1); `spread` (7) was APPENDED — chain
// slots pad appended params with the descriptor default.
include!("effect_api.rs");

const NSTAGES: usize = 12;

const NPARAMS: usize = 8;
const P_RATE: usize = 0; //   0.02..12 Hz sweep
const P_DEPTH: usize = 1; //  0..1
const P_BASE: usize = 2; //   50..1500 Hz lowest corner
const P_FB: usize = 3; //    -0.9..0.9 feedback
const P_STAGES: usize = 4; // 2..12 sections (even/odd both fine)
const P_MIX: usize = 5; //   0..1
const P_OUT: usize = 6;
const P_SPREAD: usize = 7; // 0..1 LFO phase offset of the R path (tier 5)

struct St {
    sr: f64,
    phase: f64,
    // per-side, per-stage allpass state: input-delayed and output-delayed
    x1: [[f64; NSTAGES]; 2],
    y1: [[f64; NSTAGES]; 2],
    fb_prev: [f64; 2],
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        phase: 0.0,
        x1: [[0.0; NSTAGES]; 2],
        y1: [[0.0; NSTAGES]; 2],
        fb_prev: [0.0; 2],
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// One all-pass cascade + bipolar feedback, `side`-indexed state. `lfo`
/// is the side's own sweep value (0..1, already phase-offset).
#[inline]
fn path(s: &mut St, side: usize, x: f64, lfo: f64, base: f64, fb: f64, stages: usize) -> f64 {
    let mut y = x + s.fb_prev[side] * fb;
    for k in 0..stages {
        // corner for this stage: base * 1.9^k * (1 + depth*lfo*2.5)
        let mult = (1.9f64).powi(k as i32);
        let fc = (base * mult * lfo).min(0.45 * s.sr);
        let t = (core::f64::consts::PI * fc / s.sr).tan();
        let a = (t - 1.0) / (t + 1.0);
        // y = a*x + x1 - a*y1  (first-order allpass)
        let ny = a * y + s.x1[side][k] - a * s.y1[side][k];
        s.x1[side][k] = y;
        s.y1[side][k] = ny;
        y = ny;
        if !y.is_finite() {
            s.x1[side] = [0.0; NSTAGES];
            s.y1[side] = [0.0; NSTAGES];
            s.fb_prev[side] = 0.0;
            return 0.0;
        }
    }
    s.fb_prev[side] = y;
    y
}

/// The v3 stereo step: real pair in, real pair out. Each side runs the
/// identical cascade on its own input with its own LFO phase; the dry
/// path stays per-side so the input image survives.
extern "C" fn process_st(
    state: *mut core::ffi::c_void,
    l: f32,
    r: f32,
    params: *const f32,
    _sc: f32,
    out_r: *mut f32,
) -> f32 {
    if state.is_null() {
        unsafe { *out_r = r };
        return l;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let rate = unsafe { *params.add(P_RATE) } as f64;
    let depth = unsafe { *params.add(P_DEPTH) } as f64;
    let base = unsafe { *params.add(P_BASE) } as f64;
    let fb = unsafe { *params.add(P_FB) } as f64;
    let stages = (unsafe { *params.add(P_STAGES) } as i32).clamp(1, NSTAGES as i32) as usize;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;
    let spread = (unsafe { *params.add(P_SPREAD) } as f64).clamp(0.0, 1.0);

    let xl = l as f64;
    let xr = r as f64;
    if !(xl.is_finite() && xr.is_finite()) {
        s.x1 = [[0.0; NSTAGES]; 2];
        s.y1 = [[0.0; NSTAGES]; 2];
        s.fb_prev = [0.0; 2];
        unsafe { *out_r = 0.0 };
        return 0.0;
    }

    // One LFO clock; the R path reads it `spread` further along. Both
    // paths share `phase` so the sweep stays locked even across long
    // rate changes (two free-running clocks would drift apart).
    s.phase += rate / s.sr;
    if s.phase > 1.0 {
        s.phase -= 1.0;
    }
    let mut ph_r = s.phase + spread;
    if ph_r > 1.0 {
        ph_r -= 1.0;
    }
    let lfo_l = 1.0 + depth * (0.5 + 0.5 * (s.phase * core::f64::consts::TAU).sin()) * 2.5;
    let lfo_r = 1.0 + depth * (0.5 + 0.5 * (ph_r * core::f64::consts::TAU).sin()) * 2.5;

    let yl = path(s, 0, xl, lfo_l, base, fb, stages);
    let yr = path(s, 1, xr, lfo_r, base, fb, stages);

    let ol = (xl * (1.0 - mix) + yl * mix) * out;
    let or = (xr * (1.0 - mix) + yr * mix) * out;
    if !ol.is_finite() {
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    if !or.is_finite() {
        unsafe { *out_r = 0.0 };
        return ol.clamp(-2.0, 2.0) as f32;
    }
    unsafe { *out_r = or.clamp(-2.0, 2.0) as f32 };
    ol.clamp(-2.0, 2.0) as f32
}

/// v1 entry / graceful fallback: mono bus — the LEFT path IS the classic
/// mono phaser (identical state, identical phase), return it.
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let mut or = 0.0f32;
    process_st(state, input, input, params, 0.0, &mut or)
}

/// v2 entry: no pair on a mono bus — same mid path as v1.
extern "C" fn process_sc(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
    _sc: f32,
) -> f32 {
    process(state, input, params)
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.02, max: 12.0, default: 0.45 },
    EffectParamV1 { label: c"depth".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
    EffectParamV1 { label: c"base Hz".as_ptr(), min: 50.0, max: 1500.0, default: 280.0 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: -0.9, max: 0.9, default: 0.55 },
    EffectParamV1 { label: c"stages".as_ptr(), min: 1.0, max: 12.0, default: 8.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"spread".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
];

const DESC_TEXT: &core::ffi::CStr =
    c"All-pass sweep phaser, 1-12 stages, bipolar feedback for resonant peaks. The Fruity/Vintage Phaser role. ABI v3: dual-path stereo sweep (spread offsets the R path's LFO).";

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Phaser".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TEXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"Phaser".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TEXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"Phaser".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TEXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
    process_st,
};

#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}

#[no_mangle]
pub extern "C" fn slooper_effect_info_v2() -> *const EffectInfoV2 {
    &DESC2
}

#[no_mangle]
pub extern "C" fn slooper_effect_info_v3() -> *const EffectInfoV3 {
    &DESC3
}
