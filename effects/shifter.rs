// ============================================================ slooper ==
// shifter.rs — single-sideband frequency shifter. Covers FL Studio's
// Frequency Shifter. See docs/FL-SUITE.md.
//
// Hilbert-transform SSB: two wideband all-pass chains give a 0/-90 degree
// quadrature pair, complex-rotated by an internal oscillator. Unlike ring
// modulation every partial moves by the SAME amount (harmonically
// inharmonic = the metallic/vinyl-vibrato character). Best tracking above
// ~100 Hz — the classic trade of a 12th-order Hilbert.
include!("effect_api.rs");

const NPARAMS: usize = 4;
const P_SHIFT: usize = 0; // -8000..8000 Hz
const P_FB: usize = 1; //    0..0.85 recirculation
const P_MIX: usize = 2; //   0..1
const P_OUT: usize = 3; //   0..2

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

struct St {
    sr: f64,
    // 0-degree path (4 all-pass), 90-degree path (1-sample delay + 3).
    a0: [Ap; 4],
    b0: [Ap; 3],
    d_x: f64,
    theta: f64,
    fb_out: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        a0: [Ap::new(0.7706), Ap::new(0.3237), Ap::new(0.0517), Ap::new(0.00066)],
        b0: [Ap::new(0.6985), Ap::new(0.19), Ap::new(0.014)],
        d_x: 0.0,
        theta: 0.0,
        fb_out: 0.0,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let shift = unsafe { *params.add(P_SHIFT) } as f64;
    let fb = unsafe { *params.add(P_FB) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let xi = input as f64;
    if !xi.is_finite() {
        s.fb_out = 0.0;
        s.d_x = 0.0;
        return 0.0;
    }

    let x = xi + s.fb_out * fb;

    // Hilbert pair.
    let mut ha = x;
    for ap in s.a0.iter_mut() {
        ha = ap.run(ha);
    }
    let i0 = 0.5 * (x + ha);

    let d = s.d_x;
    s.d_x = x;
    let mut hb = d;
    for ap in s.b0.iter_mut() {
        hb = ap.run(hb);
    }
    let q0 = 0.5 * (d - hb);

    // Complex rotation at the shift frequency.
    s.theta += core::f64::consts::TAU * shift.abs() / s.sr;
    if s.theta > core::f64::consts::TAU {
        s.theta -= core::f64::consts::TAU;
    }
    let (cs, sn) = s.theta.sin_cos();
    let dir = if shift < 0.0 { -1.0 } else { 1.0 };
    let wet = i0 * cs - q0 * sn * dir;

    s.fb_out = wet;
    let o = (xi * (1.0 - mix) + wet * mix) * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-3.0, 3.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"shift Hz".as_ptr(), min: -8000.0, max: 8000.0, default: 240.0 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.85, default: 0.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Shifter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"SSB frequency shifter (Hilbert): moves every partial by the same Hz. Negative/positive shifts, feedback for resonant metal.".as_ptr(),
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
