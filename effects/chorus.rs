// ============================================================ slooper ==
// chorus.rs — modulated-delay chorus / doubler / flanger for the chain.
//
// The classic: the input is written into a ring buffer and read back
// through a sine-LFO-modulated tap; the delayed copy is fed back through
// a one-pole tone filter and mixed with the dry signal. Modulating the
// read position varies the delay length, which detunes the delayed copy
// against the dry one — the chorus effect.
//
// Quick recipes (defaults are a relaxed vocal-style chorus):
//   chorus     base 12..25 ms, depth 2..5 ms, feedback <= 0.4
//   doubler    base 20..35 ms, depth 0.3..1 ms, mix ~0.5
//   flanger    base 0..5 ms,  depth 1..3 ms, feedback 0.5..0.9
//
// ---------------------------------------------------------------------------
// Standard five-piece slooper effect (see template.rs). process() runs on
// the audio thread: allocation-free, NaN-guarded. The ring buffer is a
// fixed inline array (power-of-two so wrap is a mask); at 192 kHz the
// 8192 slots cover 42 ms, well past the longest base delay offered.
// ---------------------------------------------------------------------------

include!("effect_api.rs");

const RING_BITS: usize = 13;
const RING: usize = 1 << RING_BITS; // 8192 slots
const RING_MASK: usize = RING - 1;

struct Chorus {
    sr: f64,
    buf: [f32; RING],
    wptr: usize,
    /// LFO phase in radians.
    phase: f64,
    /// Feedback-path one-pole lowpass state.
    lp: f32,
    /// Last feedback sample (delayed read is at the write head, so keep
    /// the exact processed value that entered the ring).
    fb: f32,
}

/// Slider indices — keep in sync with PARAMS / n_params.
const P_RATE: usize = 0;    // 0.05..8 Hz LFO speed
const P_DEPTH: usize = 1;   // 0..15 ms modulation depth
const P_BASE: usize = 2;    // 0..30 ms nominal delay time
const P_FEED: usize = 3;    // 0..0.9 feedback
const P_TONE: usize = 4;    // 400..12000 Hz feedback lowpass
const P_MIX: usize = 5;     // 0..1 dry/wet
const P_OUT: usize = 6;     // 0..2 output trim

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Chorus {
        sr: sample_rate.max(1.0),
        buf: [0.0; RING],
        wptr: 0,
        phase: 0.0,
        lp: 0.0,
        fb: 0.0,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut Chorus) });
    }
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let st = unsafe { &mut *(state as *mut Chorus) };

    let rate = unsafe { *params.add(P_RATE) }.clamp(0.05, 8.0);
    let depth = unsafe { *params.add(P_DEPTH) }.clamp(0.0, 15.0);
    let base = unsafe { *params.add(P_BASE) }.clamp(0.0, 30.0);
    let feed = unsafe { *params.add(P_FEED) }.clamp(0.0, 0.9);
    let tone = unsafe { *params.add(P_TONE) }.clamp(400.0, 12000.0);
    let mix = unsafe { *params.add(P_MIX) }.clamp(0.0, 1.0);
    let out = unsafe { *params.add(P_OUT) }.clamp(0.0, 2.0);

    let x = if input.is_finite() { input } else { 0.0 };
    let sr = st.sr;

    // Depth 0 + no feedback + dry-only => plain trim; skip the oscillator.
    if mix <= 0.0 {
        let o = x * out;
        return if o.is_finite() { o } else { 0.0 };
    }

    // Write the dry input summed with the (filtered) feedback tap.
    let tone_k = (-(core::f64::consts::LN_2 * 2.0 * tone as f64 / sr)).exp() as f32;
    st.lp += tone_k * (st.fb - st.lp); // one-pole LP on feedback
    st.buf[st.wptr & RING_MASK] = x + st.lp * feed;
    st.fb = st.lp;

    // LFO-modulated read position (linear interpolation between slots).
    st.phase = (st.phase + core::f64::consts::TAU * rate as f64 / sr)
        % core::f64::consts::TAU;
    let lfo = 0.5 - 0.5 * st.phase.cos(); // 0..1
    let delay_smp =
        (base + depth * lfo as f32) as f64 * 0.001 * sr;
    let total = (RING as f64 - 1.0).min(delay_smp.max(0.0));
    let ri = (st.wptr as i64 - 1 - total as i64) & (RING as i64 - 1) ;
    let rj = (ri + 1) & (RING as i64 - 1);
    let frac = (total - total.floor()) as f32;
    let wet = st.buf[ri as usize] * (1.0 - frac) + st.buf[rj as usize] * frac;

    let o = (x * (1.0 - mix) + wet * mix) * out;
    st.wptr = (st.wptr + 1) & RING_MASK;
    if o.is_finite() {
        o
    } else {
        st.lp = 0.0;
        st.fb = 0.0;
        0.0
    }
}

static PARAMS: [EffectParamV1; 7] = [
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.05, max: 8.0, default: 0.8 },
    EffectParamV1 { label: c"depth ms".as_ptr(), min: 0.0, max: 15.0, default: 3.5 },
    EffectParamV1 { label: c"base ms".as_ptr(), min: 0.0, max: 30.0, default: 12.0 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.9, default: 0.25 },
    EffectParamV1 { label: c"tone".as_ptr(), min: 400.0, max: 12000.0, default: 5500.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Chorus Doubler".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Modulated-delay chorus: wide at base 12+ ms, flanger at base 0-5 ms with high feedback.".as_ptr(),
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
