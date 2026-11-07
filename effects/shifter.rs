// ============================================================ slooper ==
// shifter.rs — single-sideband frequency shifter. Covers FL Studio's
// Frequency Shifter. See docs/FL-SUITE.md.
//
// Hilbert-transform SSB: two wideband all-pass chains give a 0/-90 degree
// quadrature pair, complex-rotated by an internal oscillator. Unlike ring
// modulation every partial moves by the SAME amount (harmonically
// inharmonic = the metallic/vinyl-vibrato character). Best tracking above
// ~100 Hz — the classic trade of a 12th-order Hilbert.
//
// ABI v3 (tier 5b): TRUE STEREO per-side shifters, the FL Frequency
// Shifter role. L runs `shift`, R runs `shift × (1 + spread)` with a
// fully independent Hilbert pair, oscillator and feedback loop — one
// clock-less circuit per side, so a hard-panned input NEVER crosses to
// the other rail. spread 0 makes the sides exact copies (bit-exact) and
// the v1 fallback is `process_st(x, x)` → LEFT = the tier-1 mono circuit.
include!("effect_api.rs");

const NPARAMS: usize = 5;
const P_SHIFT: usize = 0; // -8000..8000 Hz (L side; R = ×(1+spread))
const P_FB: usize = 1; //    0..0.85 recirculation
const P_MIX: usize = 2; //   0..1
const P_OUT: usize = 3; //   0..2
const P_SPREAD: usize = 4; // 0..2 R-side shift multiplier (1 + spread)

/// First-order allpass: H = (a + z^-1)/(1 + a z^-1).
#[derive(Clone, Copy)]
struct Ap {
    a: f64,
    x1: f64,
    y1: f64,
}

impl Ap {
    fn new(a: f64) -> Ap {
        Ap { a, x1: 0.0, y1: 0.0 }
    }
    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.a * (x - self.y1) + self.x1;
        self.x1 = x;
        self.y1 = y;
        if !y.is_finite() {
            self.x1 = 0.0;
            self.y1 = 0.0;
            return 0.0;
        }
        y
    }
}

/// One independent SSB channel: Hilbert pair + rotator + feedback tap.
struct Ch {
    // 0-degree path (4 all-pass), 90-degree path (1-sample delay + 3).
    a0: [Ap; 4],
    b0: [Ap; 3],
    d_x: f64,
    theta: f64,
    fb_out: f64,
}

impl Ch {
    fn new() -> Ch {
        Ch {
            a0: [Ap::new(0.7706), Ap::new(0.3237), Ap::new(0.0517), Ap::new(0.00066)],
            b0: [Ap::new(0.6985), Ap::new(0.19), Ap::new(0.014)],
            d_x: 0.0,
            theta: 0.0,
            fb_out: 0.0,
        }
    }
    /// The tier-1 mono circuit, verbatim — the L path runs exactly this.
    #[inline]
    fn run(&mut self, x_in: f64, shift: f64, fb: f64, mix: f64, out: f64, sr: f64) -> f64 {
        if !x_in.is_finite() {
            self.fb_out = 0.0;
            self.d_x = 0.0;
            return 0.0;
        }
        let x = x_in + self.fb_out * fb;

        // Hilbert pair.
        let mut ha = x;
        for ap in self.a0.iter_mut() {
            ha = ap.run(ha);
        }
        let i0 = 0.5 * (x + ha);

        let d = self.d_x;
        self.d_x = x;
        let mut hb = d;
        for ap in self.b0.iter_mut() {
            hb = ap.run(hb);
        }
        let q0 = 0.5 * (d - hb);

        // Complex rotation at the shift frequency.
        self.theta += core::f64::consts::TAU * shift.abs() / sr;
        if self.theta > core::f64::consts::TAU {
            self.theta -= core::f64::consts::TAU;
        }
        let (cs, sn) = self.theta.sin_cos();
        let dir = if shift < 0.0 { -1.0 } else { 1.0 };
        let wet = i0 * cs - q0 * sn * dir;

        self.fb_out = wet;
        let o = (x_in * (1.0 - mix) + wet * mix) * out;
        if !o.is_finite() {
            return 0.0;
        }
        o.clamp(-3.0, 3.0)
    }
}

struct St {
    sr: f64,
    l: Ch,
    r: Ch,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        l: Ch::new(),
        r: Ch::new(),
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// v1 entry: mono pair of one signal — LEFT is the historical circuit.
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    let mut or = input;
    process_st(state, input, input, params, 0.0, &mut or)
}

/// v2 entry: no pair on a mono bus — emit the mid of the two shifters.
extern "C" fn process_sc(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
    _sc: f32,
) -> f32 {
    if state.is_null() {
        return input;
    }
    let mut or = input;
    let ol = process_st(state, input, input, params, 0.0, &mut or);
    (ol + or) * 0.5
}

/// The v3 stereo step: two independent shifters, no cross-feed.
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
    let shift = unsafe { *params.add(P_SHIFT) } as f64;
    let fb = unsafe { *params.add(P_FB) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;
    let spread = unsafe { *params.add(P_SPREAD) }.clamp(0.0, 2.0) as f64;
    let shift_r = shift * (1.0 + spread);

    let yl = s.l.run(l as f64, shift, fb, mix, out, s.sr);
    let yr = s.r.run(r as f64, shift_r, fb, mix, out, s.sr);
    unsafe { *out_r = yr as f32 };
    yl as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"shift Hz".as_ptr(), min: -8000.0, max: 8000.0, default: 240.0 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.85, default: 0.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"spread".as_ptr(), min: 0.0, max: 2.0, default: 0.5 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Shifter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"SSB frequency shifter (Hilbert): moves every partial by the same Hz. Negative/positive shifts, feedback for resonant metal. ABI v3: independent R-side shifter at shift*(1+spread).".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"Shifter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"SSB frequency shifter (Hilbert): moves every partial by the same Hz. Negative/positive shifts, feedback for resonant metal. ABI v3: independent R-side shifter at shift*(1+spread).".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"Shifter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"SSB frequency shifter (Hilbert): moves every partial by the same Hz. Negative/positive shifts, feedback for resonant metal. ABI v3: independent R-side shifter at shift*(1+spread).".as_ptr(),
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
