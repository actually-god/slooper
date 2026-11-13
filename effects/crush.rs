// ============================================================ slooper ==
// crush.rs — bit depth + sample-rate destruction (lo-fi digital
// distortion).
//
// Signal: optional pre drive -> downsample (sample & hold) -> bit
// quantize -> smoothing (recon filter approx) -> mix -> trim.
include!("effect_api.rs");

const NPARAMS: usize = 6;
const P_BITS: usize = 0; //   1..16   quantization bits
const P_DOWN: usize = 1; //  0..40    hold time in ms (0 = off)
const P_SMOOTH: usize = 2; // 0..1    output smoothing amount
const P_MIX: usize = 3; //   0..1
const P_DRIVE: usize = 4; // 1..4    pre gain (crush INTO the quantizer)
const P_OUT: usize = 5; //   0..2

struct St {
    sr: f64,
    held: f64,
    hold_left: i32,
    lp: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St { sr, held: 0.0, hold_left: 0, lp: 0.0 })) as *mut core::ffi::c_void
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
    let bits = unsafe { *params.add(P_BITS) } as f64;
    let down = unsafe { *params.add(P_DOWN) } as f64;
    let smooth = unsafe { *params.add(P_SMOOTH) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let drive = unsafe { *params.add(P_DRIVE) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let mut x = input as f64;
    if !x.is_finite() {
        s.held = 0.0;
        s.hold_left = 0;
        s.lp = 0.0;
        return 0.0;
    }
    x *= drive;

    // Downsample: hold each sample for `down` ms (sample & hold = the
    // aliasing fizz that makes lo-fi sparkle).
    if down > 0.05 {
        if s.hold_left <= 0 {
            s.held = x;
            let n = (down * 0.001 * s.sr) as i32;
            s.hold_left = if n < 1 { 1 } else { n };
        }
        s.hold_left -= 1;
        x = s.held;
    } else {
        s.held = x;
        s.hold_left = 0;
    }

    // Bit quantize (uniform, no dither — the grit IS the point).
    let q = (1i64 << bits.clamp(1.0, 15.0) as u32) as f64 * 0.5;
    let mut y = (x * q).round() / q;

    // Optional smoothing: one-pole LP, 20 kHz..1.5 kHz across the dial.
    if smooth > 0.001 {
        let fc = 20000.0 - smooth * smooth * 18500.0;
        // true one-pole coefficient: exp(-2π fc/sr)
        let c = (-(core::f64::consts::TAU * fc / s.sr)).exp();
        s.lp += (y - s.lp) * (1.0 - c);
        y = s.lp;
    }

    let o = (input as f64) * (1.0 - mix) + y * mix * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-4.0, 4.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"bits".as_ptr(), min: 1.0, max: 16.0, default: 4.0 },
    EffectParamV1 { label: c"downsample ms".as_ptr(), min: 0.0, max: 40.0, default: 0.0 },
    EffectParamV1 { label: c"smooth".as_ptr(), min: 0.0, max: 1.0, default: 0.15 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"drive".as_ptr(), min: 1.0, max: 4.0, default: 1.6 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 0.9 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Crusher".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Bit depth + downsample destruction: lo-fi quantize, sample & hold fizz, optional smoothing and drive.".as_ptr(),
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
