// ============================================================ slooper ==
// flanger.rs — modulated short-delay flanger with a "voices" unison
// stack. Covers Fruity Flanger and (consolidated) Fruity Flangus. See
// docs/FL-SUITE.md.
//
// Each voice is the same comb with a phase-offset sine LFO; N voices with
// even phase spreading produce the thick flanged-unison Flangus sound.
//
// TIER-4 FIX (2026-11-06): the ring reads used ABSOLUTE slots
// (`sidx(d)`) instead of write-head-relative positions. The taps then
// swept the whole ring once per wrap instead of holding a constant
// delay — the feedback loop starved and an impulse came out as
// (dry * (1−mix)) plus a slow smear. Same bug class as echo3.rs (see
// its header for the full post-mortem). Reads are head-relative now, the
// base time is slew-limited, and the NaN path no longer wipes 256 KB of
// ring per bad sample (the loop self-heals once filters are clean).
// `tier4_feedback_loops_ring` pins that the comb actually rings.
//
// TIER-5 (ABI v3): stereo flanger over ONE shared comb — the ring stores
// the mid, each side reads its voice taps at its own LFO phase, and the
// R bank is offset by `spread` (0..1 of a sweep cycle). A centre source
// therefore flanges with opposite-sweep sides (decorrelated comb tail)
// while the LEFT path is the exact tier-4 mono circuit — the v1 fallback
// returns it, so the tier-4 structure pins keep measuring it unchanged.
// spread = 0 → identical taps both sides → bit-exact mono for identical
// inputs. Params 0..6 are sticky (tier 1/4); `spread` (7) was APPENDED.
include!("effect_api.rs");

const NPARAMS: usize = 8;
const P_RATE: usize = 0; //   0.02..8 Hz
const P_DEPTH: usize = 1; //  0..1 (of 8 ms swing)
const P_BASE: usize = 2; //   0.2..10 ms base delay
const P_FB: usize = 3; //    0..0.9
const P_VOICES: usize = 4; // 1..4 comb voices
const P_MIX: usize = 5; //   0..1
const P_OUT: usize = 6;
const P_SPREAD: usize = 7; // 0..1 LFO phase offset of the R taps (tier 5)

const MAXD: usize = 65536; // power-of-two ring mask (>= 500 ms at 48k+)

struct St {
    sr: f64,
    buf: Vec<f32>,
    widx: usize,
    phase: f64,
    /// Slew-limited nominal delay in ms (tier-4: zipper-click guard).
    bt: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        buf: vec![0.0f32; MAXD],
        widx: 0,
        phase: 0.0,
        bt: 1.8,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

#[inline]
fn sidx(v: i64) -> usize {
    // negative-safe index into the ring
    ((v % MAXD as i64) + MAXD as i64) as usize % MAXD
}

#[inline]
fn read_lin(buf: &[f32], widx: usize, frac: f64) -> f32 {
    // Linear-interpolated read `frac` samples BEHIND the write head.
    // (Tier-1 read buf[d] absolutely — see the file header; keep this
    // head-relative or the flanger stops being a flanger.)
    let frac = frac.max(1.0);
    let p = widx as f64 - frac;
    if p < 1.0 {
        return 0.0;
    }
    let d = p.floor();
    let f = p - d;
    let di = d as i64;
    let i0 = sidx(di);
    let i1 = sidx(di + 1);
    let a = buf[i0] as f64;
    let b = buf[i1] as f64;
    (a + (b - a) * f) as f32
}

/// The v3 stereo step: one shared feedback comb written with the mid,
/// voice taps read per side with a `spread`-offset LFO on the R bank.
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
    let voices = (unsafe { *params.add(P_VOICES) } as i32).clamp(1, 4) as usize;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;
    let spread = (unsafe { *params.add(P_SPREAD) } as f64).clamp(0.0, 1.0);

    let xl = l as f64;
    let xr = r as f64;
    if !(xl.is_finite() && xr.is_finite()) {
        // Re-anchor the slewed time so the first clean sample does not
        // chase a stale target; the ring drains itself within one loop.
        s.bt = base.max(0.2) as f64;
        unsafe { *out_r = 0.0 };
        return 0.0;
    }

    // Slew the nominal delay (~5 ms) so base-slider drags glide instead
    // of stepping every tap by whole samples.
    let ks = 1.0 - (-core::f64::consts::TAU * 35.0 / s.sr).exp();
    s.bt += (base.max(0.2) - s.bt) * ks;
    let bt = s.bt.max(0.15);

    // Write the mid (with feedback read head-relative one base-delay
    // back). For identical sides (l == r) the mid IS the tier-4 mono
    // write — the comb behaves exactly like the historical circuit.
    let xm = (xl + xr) * 0.5;
    let w = MAXD - 1;
    let wr = (s.widx & w) as usize;
    s.buf[wr] = (xm + read_lin(&s.buf, s.widx, bt * 0.001 * s.sr) as f64 * fb) as f32;
    s.widx = s.widx.wrapping_add(1);

    s.phase += rate / s.sr;
    if s.phase > 1.0 {
        s.phase -= (s.phase as i64) as f64;
    }

    // Sum modulated voices with even phase spreading; the R bank is
    // offset by `spread` (one clock, two read heads — never drift).
    let swing_ms = depth * 8.0;
    let mut wet = [0.0f64; 2];
    for (side, wett) in wet.iter_mut().enumerate() {
        let off = if side == 1 { spread } else { 0.0 };
        for vi in 0..voices {
            let mut ph = s.phase + off + vi as f64 / voices as f64;
            if ph > 1.0 {
                ph -= 1.0;
            }
            let modu = 0.5 + 0.5 * (ph * core::f64::consts::TAU).sin();
            let dms = bt + modu * swing_ms;
            *wett += read_lin(&s.buf, s.widx, dms.max(0.15) * 0.001 * s.sr) as f64;
        }
        *wett /= voices as f64;
    }

    let ol = (xl * (1.0 - mix) + wet[0] * mix) * out;
    let or = (xr * (1.0 - mix) + wet[1] * mix) * out;
    if !ol.is_finite() {
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    if !or.is_finite() {
        unsafe { *out_r = 0.0 };
        return ol.clamp(-2.5, 2.5) as f32;
    }
    unsafe { *out_r = or.clamp(-2.5, 2.5) as f32 };
    ol.clamp(-2.5, 2.5) as f32
}

/// v1 entry / graceful fallback: the LEFT side of the pair path — with
/// l == r the mid IS x and the L taps carry the tier-4 phase, so this is
/// the historical mono circuit bit-for-bit.
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
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.02, max: 8.0, default: 0.22 },
    EffectParamV1 { label: c"depth".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
    EffectParamV1 { label: c"base ms".as_ptr(), min: 0.2, max: 10.0, default: 1.8 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.9, default: 0.6 },
    EffectParamV1 { label: c"voices".as_ptr(), min: 1.0, max: 4.0, default: 3.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"spread".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
];

const DESC_TEXT: &core::ffi::CStr =
    c"Modulated comb flanger with 1-4 phase-spread voices (Flangus-style unison), feedback and tone-friendly delay range. ABI v3: shared comb, opposite-sweep sides (spread).";

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Flanger".as_ptr(),
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
    name: c"Flanger".as_ptr(),
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
    name: c"Flanger".as_ptr(),
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
