// ============================================================ slooper ==
// goodizer.rs — the one-knob enhancer. Covers FL Studio's Soundgoodizer
// (A/B/C/D characters), Emphasizer (presence) and Low Lifter (bass weight
// for small speakers). See docs/FL-SUITE.md.
//
// Approach: split into three bands, saturate each band with a character-
// weighted drive, add a parallel harmonic exciter ("air"), glue with a
// soft limiter, trim. One knob (amount) does the work, like the original.
include!("effect_api.rs");

const NPARAMS: usize = 4;
const P_CHAR: usize = 0; //  0..3    A bass / B mid-low / C presence / D air
const P_AMOUNT: usize = 1; // 0..1
const P_AIR: usize = 2; //   0..1    parallel high exciter
const P_OUT: usize = 3; //   0..1.5

struct St {
    sr: f64,
    // 3-band split: one-poles at ~250 Hz and ~4 kHz.
    lo: f64,
    mid: f64,
    pre: f64,
    // air band
    air_lp: f64,
    // smoothed band drives (zipf parameter changes)
    amt: f64,
    air: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        lo: 0.0,
        mid: 0.0,
        pre: 0.0,
        air_lp: 0.0,
        amt: 0.0,
        air: 0.0,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

#[inline]
fn lp_state(p: &mut f64, x: f64, c: f64) -> f64 {
    *p += (x - *p) * (1.0 - c);
    *p
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let ch = (unsafe { *params.add(P_CHAR) } as f64).round() as i32;
    let amount = unsafe { *params.add(P_AMOUNT) } as f64;
    let air = unsafe { *params.add(P_AIR) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let x = input as f64;
    if !x.is_finite() {
        s.lo = 0.0;
        s.mid = 0.0;
        return 0.0;
    }

    // Slew the controls so knob rides never click.
    s.amt += (amount - s.amt) * 0.002;
    s.air += (air - s.air) * 0.002;
    let amt = s.amt;

    // ---- band split ------------------------------------------------------
    let c1 = (-(core::f64::consts::TAU * 250.0 / s.sr)).exp();
    let c2 = (-(core::f64::consts::TAU * 4000.0 / s.sr)).exp();
    let lo = lp_state(&mut s.lo, x, c1);
    let rest = x - lo;
    let mid = lp_state(&mut s.mid, rest, c2);
    let hi = rest - mid;

    // ---- character-weighted saturation -----------------------------------
    // weights per band [lo, mid, hi] per character A..D
    let (wl, wm, wh) = match ch {
        0 => (3.6, 1.2, 0.6), // A: bottom weight
        1 => (1.6, 3.4, 1.0), // B: low-mid fat
        2 => (0.8, 1.8, 3.4), // C: presence
        _ => (0.6, 1.0, 4.2), // D: air / edge
    };
    let k = amt * 6.0;
    let sl = lo * (1.0 + k * wl * 0.5);
    let sm = mid * (1.0 + k * wm * 0.5);
    let sh = hi * (1.0 + k * wh * 0.5);
    // tanh each band, but keep some clean parallel to preserve transients
    let par = 1.0 - amt * 0.35;
    let mut y = (sl.tanh() + sm.tanh() + sh.tanh()) * (1.0 - par * 0.5)
        + x * par * 0.5;

    // makeup: saturation lost level — restore with amount.
    y *= 1.0 + amt * 1.4;

    // ---- parallel air exciter --------------------------------------------
    if s.air > 0.001 {
        let ca = (-(core::f64::consts::TAU * 6000.0 / s.sr)).exp();
        let lp = lp_state(&mut s.air_lp, y, ca);
        let d = y - lp;
        y += d.tanh() * s.air * 1.8;
    }

    // ---- soft glue limiter -------------------------------------------------
    y = y.tanh();

    let o = y * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-2.0, 2.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"char (0 A bass/1 B fat/2 C pres/3 D air)".as_ptr(), min: 0.0, max: 3.0, default: 2.0 },
    EffectParamV1 { label: c"amount".as_ptr(), min: 0.0, max: 1.0, default: 0.6 },
    EffectParamV1 { label: c"air".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 1.5, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Goodizer".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"One-knob enhancer: band-weighted saturation with four characters (A-D), parallel air exciter and soft glue. Turn it up, job done.".as_ptr(),
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
