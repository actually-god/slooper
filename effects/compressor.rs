// ============================================================ slooper ==
// compressor.rs — a real compressor/leveler for the mixer chain.
//
// Classic feedback-free feedforward compressor, processed sample by
// sample: a peak envelope follower (separate attack/release time
// constants) drives a soft-knee gain computer in the dB domain, then
// makeup gain restores level and a dry/wet crossfader folds in the
// untouched signal (parallel compression at mix < 1).
//
//   signal --+--> gain cell (x g) --> * makeup --+
//            |                                   |--> out
//            +--> detector --> gain computer -->-+
//                 (dry leg = input * (1 - mix))
//
// With threshold at 0 dB the unit is an exact pass-through, so it can
// sit on any chain while you set it up.
//
// ---------------------------------------------------------------------------
// The five pieces of a slooper effect: include!("effect_api.rs"), a state
// struct, create/process/destroy, a DESC descriptor, the entry point.
// process() runs on the AUDIO THREAD: no allocation, no locks, no I/O,
// no panics — and NaN must never escape (a NaN poisons the loop buffer
// forever, so every state update is guarded).
// ---------------------------------------------------------------------------

include!("effect_api.rs");

struct Compressor {
    sr: f64,
    /// Detector envelope (linear), smoothed toward |input|.
    env: f32,
}

/// Slider indices — keep in sync with PARAMS / n_params below.
const P_THRESHOLD: usize = 0; // -60..0 dB
const P_RATIO: usize = 1;     // 1..24
const P_ATTACK: usize = 2;    // 0.1..50 ms
const P_RELEASE: usize = 3;   // 10..800 ms
const P_KNEE: usize = 4;      // 0..24 dB
const P_MAKEUP: usize = 5;    // 0..4 linear
const P_MIX: usize = 6;       // 0..1 dry/wet

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Compressor { sr: sample_rate.max(1.0), env: 0.0 }))
        as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut Compressor) });
    }
}

/// Amplitude -> dBFS (20*log10) with a hard floor so silence can never
/// produce -inf/NaN.
#[inline]
fn db(x: f32) -> f32 {
    (if x < 1.0e-7 { 1.0e-7 } else { x }).ln() * 8.685889638
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let st = unsafe { &mut *(state as *mut Compressor) };

    let threshold = unsafe { *params.add(P_THRESHOLD) }.clamp(-60.0, 0.0);
    let ratio = unsafe { *params.add(P_RATIO) }.clamp(1.0, 24.0);
    let attack = unsafe { *params.add(P_ATTACK) }.clamp(0.1, 50.0);
    let release = unsafe { *params.add(P_RELEASE) }.clamp(10.0, 800.0);
    let knee = unsafe { *params.add(P_KNEE) }.clamp(0.0, 24.0);
    let makeup = unsafe { *params.add(P_MAKEUP) }.clamp(0.0, 4.0) as f64;
    let mix = unsafe { *params.add(P_MIX) }.clamp(0.0, 1.0);

    // Threshold 0 dB with makeup at unity = pass-through (skip the maths).
    if threshold >= -0.01 && (makeup - 1.0).abs() < 0.01 {
        st.env = 0.0;
        return input;
    }

    let x = if input.is_finite() { input } else { 0.0 };
    let sr = st.sr;

    // --- detector: one-pole peak follower, attack on rising, release on
    // falling. Coefficients are time constants at the current sample rate.
    let a = (-(1.0 / (attack as f64 * 0.001 * sr))).exp() as f32;
    let r = (-(1.0 / (release as f64 * 0.001 * sr))).exp() as f32;
    let target = x.abs();
    let k = if target > st.env { a } else { r };
    st.env = target + (st.env - target) * k;
    if !st.env.is_finite() {
        st.env = 0.0;
    }

    // --- soft-knee gain computer (W3C dynamic-compressor formulas, dB):
    //   over <= 0        -> 0 dB
    //   0 < over < knee  -> (1/R-1) * over^2 / (2*knee)   (quadratic knee)
    //   over >= knee     -> (1/R-1) * (over - knee/2)     (hard slope)
    let over = db(st.env) - threshold;
    let gr = if over <= 0.0 {
        0.0
    } else if knee > 0.0 && over < knee {
        (1.0 / ratio - 1.0) * over * over / (2.0 * knee)
    } else {
        (1.0 / ratio - 1.0) * (over - 0.5 * knee)
    };
    let gain = (gr as f64 * 0.16610).exp2() as f32; // 10^(gr/20) = 2^(gr*log2(10)/20)

    let wet = x * gain * makeup as f32;
    let out = x * (1.0 - mix) + wet * mix;
    if out.is_finite() {
        out
    } else {
        st.env = 0.0;
        0.0
    }
}

static PARAMS: [EffectParamV1; 7] = [
    EffectParamV1 { label: c"threshold".as_ptr(), min: -60.0, max: 0.0, default: -18.0 },
    EffectParamV1 { label: c"ratio".as_ptr(), min: 1.0, max: 24.0, default: 4.0 },
    EffectParamV1 { label: c"attack ms".as_ptr(), min: 0.1, max: 50.0, default: 5.0 },
    EffectParamV1 { label: c"release ms".as_ptr(), min: 10.0, max: 800.0, default: 140.0 },
    EffectParamV1 { label: c"knee dB".as_ptr(), min: 0.0, max: 24.0, default: 6.0 },
    EffectParamV1 { label: c"makeup".as_ptr(), min: 0.0, max: 4.0, default: 1.3 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Compressor".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Leveler: peak detector, soft-knee ratio, makeup and parallel mix. Threshold at 0 dB passes through.".as_ptr(),
    n_params: 7,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}
