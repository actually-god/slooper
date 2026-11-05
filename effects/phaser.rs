// ============================================================ slooper ==
// phaser.rs — classic sweeping phaser. Covers FL Studio's Fruity Phaser
// and the Vintage Phaser. See docs/FL-SUITE.md.
//
// N first-order all-pass sections cascaded, their corner frequencies
// swept by one LFO; feedback from the output re-enters the chain, which
// turns the notches into resonant peaks (the "6-stage" sound).
include!("effect_api.rs");

const NSTAGES: usize = 12;

const NPARAMS: usize = 7;
const P_RATE: usize = 0; //   0.02..12 Hz sweep
const P_DEPTH: usize = 1; //  0..1
const P_BASE: usize = 2; //   50..1500 Hz lowest corner
const P_FB: usize = 3; //    -0.9..0.9 feedback
const P_STAGES: usize = 4; // 2..12 sections (even/odd both fine)
const P_MIX: usize = 5; //   0..1
const P_OUT: usize = 6;

struct St {
    sr: f64,
    phase: f64,
    // per-stage allpass state: input-delayed and output-delayed samples
    x1: [f64; NSTAGES],
    y1: [f64; NSTAGES],
    fb_prev: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        phase: 0.0,
        x1: [0.0; NSTAGES],
        y1: [0.0; NSTAGES],
        fb_prev: 0.0,
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
    let depth = unsafe { *params.add(P_DEPTH) } as f64;
    let base = unsafe { *params.add(P_BASE) } as f64;
    let fb = unsafe { *params.add(P_FB) } as f64;
    let stages = (unsafe { *params.add(P_STAGES) } as i32).clamp(1, NSTAGES as i32) as usize;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let x = input as f64;
    if !x.is_finite() {
        s.x1 = [0.0; NSTAGES];
        s.y1 = [0.0; NSTAGES];
        s.fb_prev = 0.0;
        return 0.0;
    }

    // LFO drives the corner: stage k sits at base * mult^k, swept by depth.
    s.phase += rate / s.sr;
    if s.phase > 1.0 {
        s.phase -= 1.0;
    }
    let lfo = 0.5 + 0.5 * (s.phase * core::f64::consts::TAU).sin();

    let mut y = x + s.fb_prev * fb;
    for k in 0..stages {
        // corner for this stage: base * 1.9^k * (1 + depth*lfo*2.5)
        let mult = (1.9f64).powi(k as i32);
        let fc = (base * mult * (1.0 + depth * lfo * 2.5)).min(0.45 * s.sr);
        let t = (core::f64::consts::PI * fc / s.sr).tan();
        let a = (t - 1.0) / (t + 1.0);
        // y = a*x + x1 - a*y1  (first-order allpass)
        let ny = a * y + s.x1[k] - a * s.y1[k];
        s.x1[k] = y;
        s.y1[k] = ny;
        y = ny;
        if !y.is_finite() {
            s.x1 = [0.0; NSTAGES];
            s.y1 = [0.0; NSTAGES];
            return 0.0;
        }
    }
    s.fb_prev = y;

    let o = (x * (1.0 - mix) + y * mix) * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-2.0, 2.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.02, max: 12.0, default: 0.45 },
    EffectParamV1 { label: c"depth".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
    EffectParamV1 { label: c"base Hz".as_ptr(), min: 50.0, max: 1500.0, default: 280.0 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: -0.9, max: 0.9, default: 0.55 },
    EffectParamV1 { label: c"stages".as_ptr(), min: 1.0, max: 12.0, default: 8.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Phaser".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"All-pass sweep phaser, 1-12 stages, bipolar feedback for resonant peaks. The Fruity/Vintage Phaser role.".as_ptr(),
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
