// ============================================================ slooper ==
// transient.rs — transient / sustain shaper. Covers FL Studio's Transient
// Processor (and the spirit of the 2026 Transmitter split). See
// docs/FL-SUITE.md.
//
// A fast envelope vs a slow envelope tells onsets (transients) from body
// (sustain); each side gets its own gain, crossfaded click-free. Boost
// transients for punch, cut them for smoothness — after the fact.
include!("effect_api.rs");

const NPARAMS: usize = 6;
const P_DETECT: usize = 0; // 2..100 ms onset window
const P_SMOOTH: usize = 1; // 1..60 ms crossfade smoothing
const P_TGAIN: usize = 2; // 0..2.5 transient gain
const P_SGAIN: usize = 3; // 0..2.5 sustain gain
const P_MIX: usize = 4; //   0..1
const P_OUT: usize = 5; //   0..2

struct St {
    sr: f64,
    fast: f64,
    slow: f64,
    tg: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St { sr, fast: 0.0, slow: 0.0, tg: 0.0 })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

#[inline]
fn one_c_tau(tau_samples: f64) -> f64 {
    (-(1.0 / tau_samples.max(1.0))).exp()
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let detect = unsafe { *params.add(P_DETECT) } as f64;
    let smooth = unsafe { *params.add(P_SMOOTH) } as f64;
    let tgain = unsafe { *params.add(P_TGAIN) } as f64;
    let sgain = unsafe { *params.add(P_SGAIN) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let x = input as f64;
    if !x.is_finite() {
        s.fast = 0.0;
        s.slow = 0.0;
        return 0.0;
    }

    // Fast envelope: hard-attack, detect-time release. Slow: a longer
    // average of the fast envelope. Ratio > 1 == onset.
    let a = x.abs();
    if a > s.fast {
        s.fast += (a - s.fast) * 0.35;
    } else {
        let c = one_c_tau(detect * 0.5 * 0.001 * s.sr);
        s.fast += (a - s.fast) * (1.0 - c);
    }
    let cs = one_c_tau(detect * 2.0 * 0.001 * s.sr);
    s.slow += (s.fast - s.slow) * (1.0 - cs);

    let r = s.fast / (s.slow + 1e-9);
    let target = ((r - 1.0) * 1.5).clamp(0.0, 1.0);
    let cm = one_c_tau(smooth * 0.001 * s.sr);
    s.tg += (target - s.tg) * (1.0 - cm);

    let gain = tgain * s.tg + sgain * (1.0 - s.tg);
    let o = (x * (1.0 - mix) + x * gain * mix) * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-4.0, 4.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"detect ms".as_ptr(), min: 2.0, max: 100.0, default: 18.0 },
    EffectParamV1 { label: c"smooth ms".as_ptr(), min: 1.0, max: 60.0, default: 10.0 },
    EffectParamV1 { label: c"transient".as_ptr(), min: 0.0, max: 2.5, default: 1.5 },
    EffectParamV1 { label: c"sustain".as_ptr(), min: 0.0, max: 2.5, default: 0.8 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Transient".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Separate transient and sustain content with two envelopes and re-balance them: more attack punch or more body, after the fact.".as_ptr(),
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
