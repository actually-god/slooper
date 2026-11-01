// ============================================================ slooper ==
// sidechain_comp.rs — the stock SIDECHAIN compressor (ABI v2).
//
// A compressor whose detector listens to ANOTHER track: route one or
// more source tracks into this strip's "Sidechain in" row (mixer CHAIN
// window), and every hit on the source makes this track duck — the
// classic pump of a pad under a kick, or a bed under a voice.
//
// This is slooper's reference ABI-v2 effect: it exports BOTH entry
// points, so it also loads in older hosts (there it simply passes
// audio through — a sidechain compressor with no key does exactly
// that). The engine delivers `sc`, one sample per sample, as the
// mono sum of the selected source tracks from the previous audio
// block (docs/SIDECHAIN.md). No routing → sc is 0.0 → no ducking.
//
//   sc  --|det|--> gain computer --> g ── x input ──> out
//                                     \--- (1-depth) dry leg
//
// ---------------------------------------------------------------------------
// The five pieces + the v2 extra: include!("effect_api.rs"), state,
// create/process/destroy, DESC, `slooper_effect_info`, and the ONE v2
// addition: DESC2 + `slooper_effect_info_v2` with `process_sc`.
// process runs on the AUDIO THREAD: no allocation, no locks, no I/O,
// no panics — and NaN must never escape.
// ---------------------------------------------------------------------------

include!("effect_api.rs");

struct ScComp {
    sr: f64,
    /// Detector envelope (linear, follows |sc|).
    env: f32,
}

/// Slider indices — keep in sync with PARAMS / n_params below.
const P_THRESHOLD: usize = 0; // -60..0 dBFS on the KEY signal
const P_RATIO: usize = 1;     // 1..24
const P_ATTACK: usize = 2;    // 0.1..50 ms
const P_RELEASE: usize = 3;   // 10..800 ms (the "pump" knob)
const P_KNEE: usize = 4;      // 0..24 dB
const P_DEPTH: usize = 5;     // 0..1 duck amount

static NAME: &core::ffi::CStr = c"Sidechain Compressor";
static AUTHOR: &core::ffi::CStr = c"slooper";
static DESC_TEXT: &core::ffi::CStr =
    c"Compressor whose detector hears other tracks (mixer: Sidechain in). Keys duck this track.";

static PARAMS: [EffectParamV1; 6] = [
    EffectParamV1 { label: c"Threshold (key dB)".as_ptr(), min: -60.0, max: 0.0, default: -24.0 },
    EffectParamV1 { label: c"Ratio".as_ptr(), min: 1.0, max: 24.0, default: 4.0 },
    EffectParamV1 { label: c"Attack ms".as_ptr(), min: 0.1, max: 50.0, default: 5.0 },
    EffectParamV1 { label: c"Release ms".as_ptr(), min: 10.0, max: 800.0, default: 180.0 },
    EffectParamV1 { label: c"Knee dB".as_ptr(), min: 0.0, max: 24.0, default: 6.0 },
    EffectParamV1 { label: c"Duck amount".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
];

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(ScComp { sr: sample_rate.max(1.0), env: 0.0 })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut ScComp) });
    }
}

/// v1 entry / graceful fallback: with no key there is nothing to duck.
extern "C" fn process(_state: *mut core::ffi::c_void, input: f32, _params: *const f32) -> f32 {
    input
}

#[inline]
fn db(x: f32) -> f32 {
    (if x < 1.0e-7 { 1.0e-7 } else { x }).ln() * 8.685889638
}

extern "C" fn process_sc(state: *mut core::ffi::c_void, input: f32, params: *const f32, sc: f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let st = unsafe { &mut *(state as *mut ScComp) };

    let threshold = unsafe { *params.add(P_THRESHOLD) }.clamp(-60.0, 0.0);
    let ratio = unsafe { *params.add(P_RATIO) }.clamp(1.0, 24.0);
    let attack = unsafe { *params.add(P_ATTACK) }.clamp(0.1, 50.0);
    let release = unsafe { *params.add(P_RELEASE) }.clamp(10.0, 800.0);
    let knee = unsafe { *params.add(P_KNEE) }.clamp(0.0, 24.0);
    let depth = unsafe { *params.add(P_DEPTH) }.clamp(0.0, 1.0);

    let x = if input.is_finite() { input } else { 0.0 };
    let key = if sc.is_finite() { sc.abs() } else { 0.0 };

    // --- detector: one-pole peak follower on the KEY signal -------------
    let sr = st.sr;
    let a = (-(1.0 / (attack as f64 * 0.001 * sr))).exp() as f32;
    let r = (-(1.0 / (release as f64 * 0.001 * sr))).exp() as f32;
    let k = if key > st.env { a } else { r };
    st.env = key + (st.env - key) * k;
    if !st.env.is_finite() {
        st.env = 0.0;
    }

    // --- soft-knee gain computer on the envelope level (dB domain) ------
    let lvl = db(st.env);
    let over = lvl - threshold;
    let half = (knee * 0.5).max(0.001);
    let reduction_db = if over <= 0.0 {
        0.0
    } else if over < half && knee > 0.0 {
        ((ratio - 1.0) * over * over) / (2.0 * half * ratio.max(1.0001))
    } else {
        over * (1.0 - 1.0 / ratio)
    };
    // Duck amount scales the computed reduction; silence on the key can
    // never request gain (reduction >= 0), so this can only attenuate.
    let gain = (-(reduction_db * depth) * 0.11512925465f32).exp(); // e^(ln10/20 * -x)
    let out = x * gain;
    if out.is_finite() {
        out
    } else {
        st.env = 0.0;
        0.0
    }
}

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: NAME.as_ptr(),
    author: AUTHOR.as_ptr(),
    description: DESC_TEXT.as_ptr(),
    n_params: PARAMS.len() as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: NAME.as_ptr(),
    author: AUTHOR.as_ptr(),
    description: DESC_TEXT.as_ptr(),
    n_params: PARAMS.len() as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}

#[no_mangle]
pub extern "C" fn slooper_effect_info_v2() -> *const EffectInfoV2 {
    &DESC2
}
