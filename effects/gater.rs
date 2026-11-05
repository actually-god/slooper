// ============================================================ slooper ==
// gater.rs — tempo volume-gating. Covers Fruity PanOMatic's volume side
// and the gating half of Gross Beat. See docs/FL-SUITE.md.
//
// A shape LFO multiplies the signal through click-free smoothed ramps.
// Shapes: sine, triangle, pulse (duty = width), pluck (exponential
// decay), and random sample-&-hold — the four Groove-gate staples.
// Pan modulation (PanOMatic's other side) needs the stereo/pan capability
// logged in docs/FL-SUITE.md.
include!("effect_api.rs");

const NPARAMS: usize = 7;
const P_RATE: usize = 0; //  0.05..40 Hz gate rate
const P_SHAPE: usize = 1; // 0 sine 1 tri 2 pulse 3 pluck 4 random
const P_WIDTH: usize = 2; // duty / decay scale
const P_SMOOTH: usize = 3; // 0.1..80 ms edge smoothing
const P_PHASE: usize = 4; // 0..1 phase offset
const P_MIX: usize = 5; //  0..1 (wet = gated amount)
const P_OUT: usize = 6;

struct St {
    sr: f64,
    phase: f64,
    gate: f64,
    rng: u32,
    rnd: f64,
    last_shape: i32,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        phase: 0.0,
        gate: 1.0,
        rng: 0x2545_F491,
        rnd: 1.0,
        last_shape: -999,
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
    let rate = unsafe { *params.add(P_RATE) } as f64;
    let shape = (unsafe { *params.add(P_SHAPE) } as f64).round() as i32;
    let width = unsafe { *params.add(P_WIDTH) } as f64;
    let smooth = unsafe { *params.add(P_SMOOTH) } as f64;
    let phase = unsafe { *params.add(P_PHASE) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let x = input as f64;
    if !x.is_finite() {
        s.gate = 1.0;
        return 0.0;
    }

    // Advance the LFO.
    let prev = s.phase;
    s.phase += rate / s.sr;
    let wrapped = s.phase >= 1.0;
    if wrapped {
        s.phase -= s.phase.floor();
        // resample on every cycle for the random shape
        s.rng = s.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        s.rnd = ((s.rng >> 8) as f64 / 16_777_216.0) * 0.9 + 0.1;
    }
    let _ = prev;

    // Raw shape in [0,1], phase-offset.
    let p = (s.phase + phase) % 1.0;
    let raw = match shape {
        0 => 0.5 + 0.5 * (p * core::f64::consts::TAU).sin(),
        1 => 1.0 - (p * 2.0 - 1.0).abs(),
        2 => {
            let duty = width.clamp(0.01, 0.99);
            if p < duty {
                1.0
            } else {
                0.0
            }
        }
        3 => {
            // pluck: instant open, exp decay over width*period
            let t = p / width.max(0.02);
            (-(t * 4.0)).exp()
        }
        _ => {
            // random S&H with a short decay so it still breathes
            let t = p * 6.0;
            s.rnd * (-(t).exp().recip()).exp() * 0.0 + s.rnd * (-(t * 0.35)).exp()
        }
    };

    // Edge smoothing: separate attack/release one-poles from `smooth` ms.
    // (A hard square gate clicks; this is the Gross-Beat "soft" feel.)
    let tau = (smooth.max(0.1) * 0.001 * s.sr).max(1.0);
    let c = if raw > s.gate {
        (-(1.0 / (tau * 0.35))).exp()
    } else {
        (-(1.0 / tau)).exp()
    };
    s.gate = c * s.gate + (1.0 - c) * raw;
    s.last_shape = shape;

    let o = x * (1.0 - mix + mix * s.gate) * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-4.0, 4.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"rate Hz".as_ptr(), min: 0.05, max: 40.0, default: 2.0 },
    EffectParamV1 { label: c"shape (0 sine/1 tri/2 pulse/3 pluck/4 rnd)".as_ptr(), min: 0.0, max: 4.0, default: 2.0 },
    EffectParamV1 { label: c"width/decay".as_ptr(), min: 0.02, max: 1.0, default: 0.5 },
    EffectParamV1 { label: c"smooth ms".as_ptr(), min: 0.1, max: 80.0, default: 4.0 },
    EffectParamV1 { label: c"phase".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Gater".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Tempo volume-gating: sine/triangle/pulse/pluck/random LFO with click-free edges. The PanOMatic-volume / Gross-Beat-gate role.".as_ptr(),
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
