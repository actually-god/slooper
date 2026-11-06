// ============================================================ slooper ==
// chorus.rs — modulated-delay chorus / doubler / ensemble for the chain.
// Covers Fruity Chorus, Hyper Chorus and Vintage Chorus (docs/FL-SUITE.md).
//
// The classic: the input is written into a ring buffer and read back
// through sine-LFO-modulated taps; the delayed copy is fed back through
// a one-pole tone filter and mixed with the dry signal. Modulating the
// read position varies the delay length, which detunes the delayed copy
// against the dry one — the chorus effect.
//
// ABI v3 STEREO (tier-4 rework): `voices` taps per side read the same
// ring with evenly spread LFO phases; the R bank runs `spread` half
// turns away from the L bank. At the default spread 1.0 (180 degrees)
// the two ears breathe in opposite directions — the Hyper-Chorus-wide
// sound — while spread 0 collapses both banks onto one phase (classic
// mono chorus). The dry keeps its real (l, r); the side passes through.
//
// Quick recipes (defaults are a relaxed vocal-style chorus):
//   chorus     base 12..25 ms, depth 2..5 ms, feedback <= 0.4
//   doubler    base 20..35 ms, depth 0.3..1 ms, mix ~0.5, voices 1
//   ensemble   voices 3..4, spread 1, depth 4..8 ms
//   flanger    base 0..5 ms,  depth 1..3 ms, feedback 0.5..0.9
//
// TIER-4 FIX (2026-11-06): the tier-1 feedback path was DEAD code —
// `lp += k*(fb - lp)` chased by `fb = lp` froze both at zero, so the
// feedback slider did nothing at any value. The loop now recirculates
// the tone-filtered write (true-Hz one-pole, exp(-2πfc/sr)). The
// nominal delay is slew-limited (slider drags moved the tap by whole
// samples and clicked), and the ring grew to 16384 slots so base+depth
// (45 ms) still fits at 192 kHz.
// ---------------------------------------------------------------------------

include!("effect_api.rs");

const RING_BITS: usize = 14;
const RING: usize = 1 << RING_BITS; // 16384 slots (85 ms @ 192 kHz)
const RING_MASK: usize = RING - 1;

const NPARAMS: usize = 9;
/// Slider indices — keep in sync with PARAMS / n_params. Indices 0..6
/// are the shipped tier-1 set (saved chains keep their values); 7..8
/// were appended by the tier-4 stereo rework.
const P_RATE: usize = 0; //    0.05..8 Hz LFO speed
const P_DEPTH: usize = 1; //   0..15 ms modulation depth
const P_BASE: usize = 2; //    0..30 ms nominal delay time
const P_FEED: usize = 3; //    0..0.9 feedback
const P_TONE: usize = 4; //    400..12000 Hz feedback lowpass
const P_MIX: usize = 5; //     0..1 dry/wet
const P_OUT: usize = 6; //     0..2 output trim
const P_VOICES: usize = 7; //  1..4 modulated taps PER SIDE (tier-4)
const P_SPREAD: usize = 8; //  0..1 L/R bank phase offset (tier-4)

use core::f64::consts::{PI, TAU};

struct Chorus {
    sr: f64,
    buf: [f32; RING],
    wptr: usize,
    /// LFO phase in radians (the L bank; the R bank adds spread*PI).
    phase: f64,
    /// Feedback-path one-pole lowpass state (tier-4: actually used now).
    lp: f32,
    /// Previous frame's tap average — the DELAYED feedback source (the
    /// recirculation period is the tap delay, like the BBD classics).
    pwet: f32,
    /// Slew-limited nominal delay in ms.
    dt: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 44100.0 } else { sample_rate };
    Box::into_raw(Box::new(Chorus {
        sr,
        buf: [0.0; RING],
        wptr: 0,
        phase: 0.0,
        lp: 0.0,
        pwet: 0.0,
        dt: 12.0,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut Chorus) });
    }
}

/// Read the ring `frac` samples behind the write head (linear).
#[inline]
fn read_head(buf: &[f32; RING], wptr: usize, frac: f64) -> f32 {
    let frac = frac.clamp(0.0, (RING - 2) as f64);
    let f = wptr as f64 - 1.0 - frac;
    if f < 0.0 {
        return buf[0];
    }
    let d = f.floor() as usize;
    let fr = (f - d as f64) as f32;
    let i0 = d & RING_MASK;
    let i1 = d.wrapping_add(1) & RING_MASK;
    buf[i0] * (1.0 - fr) + buf[i1] * fr
}

/// v1 entry: mono chorus (the fixed tier-1 behaviour, L bank).
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let mut or = input;
    let ol = process_st(state, input, input, params, 0.0, &mut or);
    (ol + or) * 0.5
}

/// v2 entry (no sidechain key is used).
extern "C" fn process_sc(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
    _sc: f32,
) -> f32 {
    process(state, input, params)
}

/// The v3 stereo step: real pair in, real pair out.
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
    let st = unsafe { &mut *(state as *mut Chorus) };

    let rate = unsafe { *params.add(P_RATE) }.clamp(0.05, 8.0) as f64;
    let depth = unsafe { *params.add(P_DEPTH) }.clamp(0.0, 15.0) as f64;
    let base = unsafe { *params.add(P_BASE) }.clamp(0.0, 30.0) as f64;
    let feed = unsafe { *params.add(P_FEED) }.clamp(0.0, 0.9) as f64;
    let tone = unsafe { *params.add(P_TONE) }.clamp(400.0, 12000.0) as f64;
    let mix = unsafe { *params.add(P_MIX) }.clamp(0.0, 1.0) as f64;
    let out = unsafe { *params.add(P_OUT) }.clamp(0.0, 2.0) as f64;
    let voices = (unsafe { *params.add(P_VOICES) } as i32).clamp(1, 4) as usize;
    let spread = unsafe { *params.add(P_SPREAD) }.clamp(0.0, 1.0) as f64;

    let li = if l.is_finite() { l as f64 } else { 0.0 };
    let ri = if r.is_finite() { r as f64 } else { 0.0 };
    let mid = (li + ri) * 0.5;
    let side = (li - ri) * 0.5;
    let sr = st.sr;

    // Slew the nominal delay (~5 ms) so base-slider drags glide. Depth
    // modulation moves the tap musically fast and is NOT slewed.
    let ks = 1.0 - (-TAU * 35.0 / sr).exp();
    st.dt += (base - st.dt) * ks;

    // LFO tap banks: `voices` taps per side, evenly spread over a half
    // turn inside each bank; the R bank offset by spread * PI. Reads
    // happen BEFORE the write (the head write must not self-feed).
    st.phase = (st.phase + TAU * rate / sr) % TAU;
    let n = voices;
    let mut wetl = 0.0f64;
    let mut wetr = 0.0f64;
    for vi in 0..n {
        let step = if n > 1 { (vi as f64) * PI / (n as f64) } else { 0.0 };
        let lfo_l = 0.5 - 0.5 * (st.phase + step).cos();
        let lfo_r = 0.5 - 0.5 * (st.phase + spread * PI + step).cos();
        wetl += read_head(&st.buf, st.wptr, (st.dt + depth * lfo_l) * 0.001 * sr) as f64;
        wetr += read_head(&st.buf, st.wptr, (st.dt + depth * lfo_r) * 0.001 * sr) as f64;
    }
    wetl /= n as f64;
    wetr /= n as f64;

    // Write the dry mid summed with the tone-filtered feedback. Tier-4:
    // the loop is DELAYED feedback (the tap average recirculates with
    // the tap delay as its period — the BBD chorus tail). The tier-1
    // code chased its own filter state and froze the loop at zero, so
    // the feedback slider did nothing at any value.
    let k = 1.0 - (-TAU * tone / sr).exp();
    st.lp += ((st.pwet as f64 - st.lp as f64) * k) as f32;
    let wr = (mid + st.lp as f64 * feed) as f32;
    st.buf[st.wptr & RING_MASK] = wr;
    st.pwet = ((wetl + wetr) * 0.5) as f32;
    st.wptr = (st.wptr + 1) & RING_MASK;

    let dry = mid * (1.0 - mix);
    let ol = (dry + wetl * mix) * out;
    let orv = (dry + side * (1.0 - mix) + wetr * mix) * out;
    if !(ol.is_finite() && orv.is_finite()) {
        st.lp = 0.0;
        st.pwet = 0.0;
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    unsafe { *out_r = orv as f32 };
    ol as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.05, max: 8.0, default: 0.8 },
    EffectParamV1 { label: c"depth ms".as_ptr(), min: 0.0, max: 15.0, default: 3.5 },
    EffectParamV1 { label: c"base ms".as_ptr(), min: 0.0, max: 30.0, default: 12.0 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.9, default: 0.25 },
    EffectParamV1 { label: c"tone".as_ptr(), min: 400.0, max: 12000.0, default: 5500.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"voices".as_ptr(), min: 1.0, max: 4.0, default: 2.0 },
    EffectParamV1 { label: c"spread".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Chorus Doubler".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Modulated-delay chorus: wide at base 12+ ms, flanger at base 0-5 ms with high feedback. ABI v3: per-side tap banks (Hyper Chorus task).".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"Chorus Doubler".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Modulated-delay chorus: wide at base 12+ ms, flanger at base 0-5 ms with high feedback. ABI v3: per-side tap banks (Hyper Chorus task).".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"Chorus Doubler".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Modulated-delay chorus: wide at base 12+ ms, flanger at base 0-5 ms with high feedback. ABI v3: per-side tap banks (Hyper Chorus task).".as_ptr(),
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
