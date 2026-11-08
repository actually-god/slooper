// ============================================================ slooper ==
// limiter.rs — one box for the Fruity Limiter duties: single-band
// compressor, transparent brickwall limiter, and noise gate. Also covers
// the Emphasis mastering-limiter role. See docs/FL-SUITE.md.
//
// mode 0 = compressor, 1 = limiter (hard, fast, soft-knee ceiling),
// mode 2 = noise gate. Same envelope machinery drives all three.
include!("effect_api.rs");

const NPARAMS: usize = 6;
const P_MODE: usize = 0; //  0..2
const P_THR: usize = 1; //   -48..0 dB
const P_RATIO: usize = 2; // 1..20 (compressor)
const P_ATT: usize = 3; //   0.05..100 ms
const P_REL: usize = 4; //   5..2000 ms
const P_GAIN: usize = 5; //  0..2   makeup / output

struct Env {
    e: f64, // linear peak envelope
}

struct St {
    sr: f64,
    env: Env,
    gain: f64, // smoothed applied gain
    // cached control coefficients (recomputed when the knobs move)
    k_thr: f32,
    k_att: f32,
    k_rel: f32,
    c_att: f64,
    c_rel: f64,
    thr_lin: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        env: Env { e: 0.0 },
        gain: 1.0,
        k_thr: -99.0,
        k_att: -99.0,
        k_rel: -99.0,
        c_att: 0.5,
        c_rel: 0.99,
        thr_lin: 0.7,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

#[inline]
fn tc(ms: f64, sr: f64) -> f64 {
    // one-pole coefficient for a time constant of `ms` milliseconds.
    ((-1.0) * (ms.max(0.01) * 0.001) * sr).recip().max(-1e9).exp().clamp(0.0, 0.999999)
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let mode = (unsafe { *params.add(P_MODE) } as f64).round() as i32;
    let thr = unsafe { *params.add(P_THR) };
    let ratio = unsafe { *params.add(P_RATIO) } as f64;
    let att = unsafe { *params.add(P_ATT) };
    let rel = unsafe { *params.add(P_REL) };
    let gain = unsafe { *params.add(P_GAIN) } as f64;

    let x = input as f64;
    if !x.is_finite() {
        s.env.e = 0.0;
        s.gain = 1.0;
        return 0.0;
    }

    // Recompute coefficients only when the knobs actually move.
    if thr != s.k_thr {
        s.k_thr = thr;
        s.thr_lin = (thr as f64 * 0.05).exp2().clamp(1e-4, 1.6); // dB->lin
    }
    if att != s.k_att {
        s.k_att = att;
        s.c_att = tc(att as f64, s.sr);
    }
    if rel != s.k_rel {
        s.k_rel = rel;
        s.c_rel = tc(rel as f64, s.sr);
    }

    // Peak envelope with separate attack/release one-poles.
    let a = x.abs();
    if a > s.env.e {
        s.env.e += (a - s.env.e) * (1.0 - s.c_att);
    } else {
        s.env.e += (a - s.env.e) * (1.0 - s.c_rel);
    }
    let e = s.env.e;

    let target = match mode {
        // --- compressor: y = thr + (env-thr)/ratio, in linear terms ------
        0 => {
            if e > s.thr_lin && s.thr_lin > 1e-6 && ratio > 1.0 {
                ((s.thr_lin / e).powf(1.0 - 1.0 / ratio) as f64).max(0.01)
            } else {
                1.0
            }
        }
        // --- limiter: never above the threshold, near-instant -------------
        1 => {
            if e > s.thr_lin && s.thr_lin > 1e-6 {
                (s.thr_lin / e).max(0.01)
            } else {
                1.0
            }
        }
        // --- noise gate: open above threshold, hard-but-smooth close -------
        _ => {
            if e > s.thr_lin {
                1.0
            } else {
                0.0
            }
        }
    };

    // Smooth the gain move. Dynamics (mode<2): duck at attack speed,
    // recover at release speed. Gate: OPENS at attack speed, CLOSES at
    // release speed — opposite direction, same two time constants.
    let gc = if mode == 2 {
        if target < s.gain {
            s.c_rel
        } else {
            s.c_att
        }
    } else if target < s.gain {
        s.c_att
    } else {
        s.c_rel
    };
    s.gain += (target - s.gain) * (1.0 - gc.clamp(0.0, 0.999999));
    let mut y = x * s.gain * gain;

    // Safety ceiling for limiter mode: soft-knee at ~0.98 FS.
    let ay = y.abs();
    if ay > 0.98 {
        y = (0.98 + (ay - 0.98).tanh() * 0.02) * y.signum();
    }
    if !y.is_finite() {
        return 0.0;
    }
    y.clamp(-1.0, 1.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"mode".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"threshold dB".as_ptr(), min: -48.0, max: 0.0, default: -3.0 },
    EffectParamV1 { label: c"ratio".as_ptr(), min: 1.0, max: 20.0, default: 4.0 },
    EffectParamV1 { label: c"attack ms".as_ptr(), min: 0.05, max: 100.0, default: 0.7 },
    EffectParamV1 { label: c"release ms".as_ptr(), min: 5.0, max: 2000.0, default: 160.0 },
    EffectParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

// ---- ABI v4 (tier 6): mono graceful prefix + param metadata --------------
// Metadata-only trip: mode picker + dB/ms engineering units in the auto
// window; process_sc/process_st are the documented mid-channel fallbacks.

extern "C" fn process_sc(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
    _sc: f32,
) -> f32 {
    process(state, input, params)
}

extern "C" fn process_st(
    state: *mut core::ffi::c_void,
    l: f32,
    r: f32,
    params: *const f32,
    _sc: f32,
    out_r: *mut f32,
) -> f32 {
    let y = process(state, ((l + r) * 0.5) as f32, params);
    unsafe { *out_r = y };
    y
}

#[repr(C)]
struct Labels<const N: usize>([*const core::ffi::c_char; N]);
// SAFETY: read-only statics of pointers to literals, shared one way.
unsafe impl<const N: usize> Sync for Labels<N> {}

static MODE_LABELS: Labels<3> =
    Labels([c"compress".as_ptr(), c"limit".as_ptr(), c"gate".as_ptr()]);

static META: [ParamMetaV1; NPARAMS] = [
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: MODE_LABELS.0.as_ptr(), n_labels: 3 },
    ParamMetaV1 { unit: SLOOPER_UNIT_DB, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: SLOOPER_UNIT_TIMES, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: SLOOPER_UNIT_MS, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: SLOOPER_UNIT_MS, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
];

static DESC_TXT: &core::ffi::CStr =
    c"Single-band dynamics three ways: compressor, transparent brickwall limiter (soft 0.98 ceiling) and noise gate.";

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Limiter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"Limiter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"Limiter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
    process_st,
};

pub static DESC4: EffectInfoV4 = EffectInfoV4 {
    abi: SLOOPER_EFFECT_ABI_V4,
    name: c"Limiter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
    process_st,
    // Mono by design: process_st above is the documented graceful fallback.
    stereo: 0,
    set_sync: None,
    param_meta: META.as_ptr(),
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

#[no_mangle]
pub extern "C" fn slooper_effect_info_v4() -> *const EffectInfoV4 {
    &DESC4
}
