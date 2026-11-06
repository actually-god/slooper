// ============================================================ slooper ==
// flanger.rs — modulated short-delay flanger with a "voices" unison
// stack. Covers Fruity Flanger and (consolidated) Fruity Flangus. See
// docs/FL-SUITE.md.
//
// Each voice is the same comb with a phase-offset sine LFO; N voices with
// even phase spreading produce the thick flanged-unison Flangus sound.
// Stereo width is deliberately NOT done here — spread.rs (ABI v3) owns
// the image task for the whole suite, so this stays the mono thick-flanger.
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
include!("effect_api.rs");

const NPARAMS: usize = 7;
const P_RATE: usize = 0; //   0.02..8 Hz
const P_DEPTH: usize = 1; //  0..1 (of 8 ms swing)
const P_BASE: usize = 2; //   0.2..10 ms base delay
const P_FB: usize = 3; //    0..0.9
const P_VOICES: usize = 4; // 1..4 comb voices
const P_MIX: usize = 5; //   0..1
const P_OUT: usize = 6;

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

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let rate = unsafe { *params.add(P_RATE) } as f64;
    let depth = unsafe { *params.add(P_DEPTH) } as f64;
    let base = unsafe { *params.add(P_BASE) } as f64;
    let fb = unsafe { *params.add(P_FB) } as f64;
    let voices = (unsafe { *params.add(P_VOICES) } as i32).clamp(1, 4) as usize;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let x = input as f64;
    if !x.is_finite() {
        // Re-anchor the slewed time so the first clean sample does not
        // chase a stale target; the ring drains itself within one loop.
        s.bt = base.max(0.2) as f64;
        return 0.0;
    }

    // Slew the nominal delay (~5 ms) so base-slider drags glide instead
    // of stepping every tap by whole samples.
    let ks = 1.0 - (-core::f64::consts::TAU * 35.0 / s.sr).exp();
    s.bt += (base.max(0.2) - s.bt) * ks;
    let bt = s.bt.max(0.15);

    // Write (with feedback read head-relative one base-delay back).
    let w = MAXD - 1;
    let wr = (s.widx & w) as usize;
    s.buf[wr] = (x + read_lin(&s.buf, s.widx, bt * 0.001 * s.sr) as f64 * fb) as f32;
    s.widx = s.widx.wrapping_add(1);

    s.phase += rate / s.sr;
    if s.phase > 1.0 {
        s.phase -= (s.phase as i64) as f64;
    }

    // Sum modulated voices with even phase spreading.
    let swing_ms = depth * 8.0;
    let mut wet = 0.0f64;
    for vi in 0..voices {
        let ph = s.phase + vi as f64 / voices as f64;
        let modu = 0.5 + 0.5 * (ph * core::f64::consts::TAU).sin();
        let dms = bt + modu * swing_ms;
        wet += read_lin(&s.buf, s.widx, dms.max(0.15) * 0.001 * s.sr) as f64;
    }
    wet /= voices as f64;

    let o = (x * (1.0 - mix) + wet * mix) * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-2.5, 2.5) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.02, max: 8.0, default: 0.22 },
    EffectParamV1 { label: c"depth".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
    EffectParamV1 { label: c"base ms".as_ptr(), min: 0.2, max: 10.0, default: 1.8 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.9, default: 0.6 },
    EffectParamV1 { label: c"voices".as_ptr(), min: 1.0, max: 4.0, default: 3.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Flanger".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Modulated comb flanger with 1-4 phase-spread voices (Flangus-style unison), feedback and tone-friendly delay range.".as_ptr(),
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
