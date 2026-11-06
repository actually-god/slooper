// ============================================================ slooper ==
// panmatic.rs — LFO pan + volume wheel over a stereo pair. Covers FL's
// PanOMatic pan duties (and PanMan / Fruity Balance's pan half — the
// trim half is the track fader here). ABI v3 pair effect: the wheel
// shapes the pair the track pan then folds into the mix, so the image
// moves for REAL (FL's PanOMatic on a mono channel does the same thing
// with its two output "pan wheels").
//
// Three LFO shapes drive a smooth equal-power pan angle; the volume
// wheel is a second LFO at another shape/rate mapped through an
// amplitude curve. `slew ms` glides the ratio changes so square wheels
// don't click.
//
// Params:
//   pan shape   — 0 sine / 1 tri / 2 square / 3 saw (quantised)
//   pan rate Hz — 0.005..24
//   pan depth   — 0..1 (0 = centre, 1 = full left/right)
//   centre      — pan bias -1..1
//   phase off   — LFO phase offset 0..1 (L leads R; 0.5 = true L/R alternation)
//   vol shape   — 0 sine / 1 tri / 2 square / 3 saw / 4 hold
//   vol rate Hz — 0.005..24
//   vol depth   — 0..1 tremolo depth
//   slew ms     — glide time for shape steps 1..200
//   gain        — output trim
// ---------------------------------------------------------------------------

include!("effect_api.rs");

const NPARAMS: usize = 10;

struct St {
    sr: f64,
    th1: f64, // pan LFO phase (turns)
    th2: f64, // volume LFO phase
    gl: f32,  // slewed left pan gain
    gr: f32,  // slewed right pan gain
    hold: f32,
    hold_th: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        th1: 0.0,
        th2: 0.0,
        gl: 1.0,
        gr: 1.0,
        hold: 1.0,
        hold_th: 0.0,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// Shape evaluator: -1..1 (shape 4 = "hold" handled by the caller).
fn shape(k: i32, t: f64) -> f64 {
    match k {
        1 => {
            // triangle through -1 at t=0
            let x = if t < 0.5 { t * 2.0 } else { 2.0 - t * 2.0 };
            x * 2.0 - 1.0
        }
        2 => {
            if t < 0.5 {
                -1.0
            } else {
                1.0
            }
        }
        3 => t * 2.0 - 1.0,
        _ => {
            let tau = core::f64::consts::TAU;
            (t * tau - core::f64::consts::FRAC_PI_2).sin()
        }
    }
}

/// v1 entry / graceful fallback on a mono bus: the pan wheel has nothing
/// to pan ACROSS, but the volume wheel still works — apply only the
/// amplitude half.
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let mut or = input;
    let ol = process_st(state, input, input, params, 0.0, &mut or);
    (ol + or) * 0.5
}

/// v2 entry: same half (the host has no pair; volume still applies).
extern "C" fn process_sc(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
    _sc: f32,
) -> f32 {
    process(state, input, params)
}

/// v3 stereo step: real pair through the wheels.
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
    let sr = s.sr;

    let psh = (unsafe { *params.add(0) } as i32).clamp(0, 3);
    let prate = unsafe { *params.add(1) }.clamp(0.005, 24.0) as f64;
    let pdep = unsafe { *params.add(2) }.clamp(0.0, 1.0) as f64;
    let cent = unsafe { *params.add(3) }.clamp(-1.0, 1.0) as f64;
    let phoff = unsafe { *params.add(4) }.clamp(0.0, 1.0);
    let vsh = (unsafe { *params.add(5) } as i32).clamp(0, 4);
    let vrate = unsafe { *params.add(6) }.clamp(0.005, 24.0) as f64;
    let vdep = unsafe { *params.add(7) }.clamp(0.0, 1.0) as f64;
    let slew = (unsafe { *params.add(8) }.clamp(1.0, 200.0) / 1000.0) as f64;
    let gain = unsafe { *params.add(9) }.clamp(0.0, 2.0) as f32;

    // Advance wheels (sample-exact).
    s.th1 += prate / sr;
    if s.th1 >= 1.0 {
        s.th1 -= 1.0;
    }
    s.th2 += vrate / sr;
    if s.th2 >= 1.0 {
        s.th2 -= 1.0;
    }

    // Pan wheel -> equal-power angle in [-1,1] per side.
    let w = shape(psh, s.th1); // -1..1
    let pan_l = (cent + w * pdep).clamp(-1.0, 1.0);
    let pan_r = (cent - w * pdep).clamp(-1.0, 1.0);
    // Constant-power pair: a = 0.5 - pan/2 (left angle), equal-power gains.
    let al = (0.5 - pan_l * 0.5) * core::f64::consts::FRAC_PI_2;
    let ar = (0.5 - pan_r * 0.5) * core::f64::consts::FRAC_PI_2;
    let gl = al.cos() as f32;
    let gr = ar.cos() as f32;

    // Volume wheel (phase-offset L/R; hold shape latches per cycle).
    let t2l = s.th2;
    let t2r = (s.th2 as f32 + phoff).fract();
    let v2l = if vsh == 4 {
        if (s.th2 + 1e-12) < s.hold_th {
            s.hold = shape(0, s.th2).mul_add(0.5, 0.5) as f32;
            s.hold_th = s.th2 + 1.0;
        }
        s.hold
    } else {
        (shape(vsh, t2l).mul_add(0.5, 0.5)) as f32
    };
    let v2r = if vsh == 4 {
        v2l
    } else {
        (shape(vsh, t2r as f64).mul_add(0.5, 0.5)) as f32
    };
    let vol_l = 1.0 - vdep as f32 + vdep as f32 * v2l;
    let vol_r = 1.0 - vdep as f32 + vdep as f32 * v2r;

    // Slew gains (click-free squares) — one pole toward the targets.
    let a = 1.0 - (-1.0 / (slew * sr)).exp() as f32;
    s.gl += (gl - s.gl) * a;
    s.gr += (gr - s.gr) * a;

    let fl = s.gl * vol_l * l * gain;
    let fr = s.gr * vol_r * r * gain;

    if !fl.is_finite() || !fr.is_finite() {
        s.gl = 0.0;
        s.gr = 0.0;
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    unsafe { *out_r = fr };
    fl
}

// ===================== scripted editor ========================

struct Gs {
    _unused: u8,
}

extern "C" fn gui_create(_sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Gs { _unused: 0 })) as *mut core::ffi::c_void
}

extern "C" fn gui_destroy(gs: *mut core::ffi::c_void) {
    if !gs.is_null() {
        drop(unsafe { Box::from_raw(gs as *mut Gs) });
    }
}

extern "C" fn gui_draw(
    gs: *mut core::ffi::c_void,
    _state: *mut core::ffi::c_void,
    frame: *const GuiFrameV1,
    api: *const GuiApiV1,
) {
    let _ = gs;
    if frame.is_null() || api.is_null() {
        return;
    }
    let f = unsafe { &*frame };
    let api = unsafe { &*api };
    let c = api.ctx;
    let w = f.width;
    let h = f.height;
    (api.rect_filled)(c, 0.0, 0.0, w, h, 0.09, 0.10, 0.13, 1.0);

    let y0 = 14.0;
    let colw = (w - 48.0) * 0.5;
    let rh = 32.0;
    macro_rules! sl {
        ($k:expr, $col:expr, $row:expr, $min:expr, $max:expr, $lab:expr, $curve:expr) => {{
            let x = 16.0 + $col as f32 * (colw + 16.0);
            let y = y0 + $row as f32 * rh;
            let mut v = (api.get_param)(c, $k);
            if (api.slider)(c, x, y, colw, rh - 8.0, $lab, &mut v, $min, $max, $curve) != 0 {
                (api.set_param)(c, $k, v);
            }
        }};
    }
    sl!(0, 0, 0, 0.0, 3.0, c"pan 0sin 1tri 2sq 3saw".as_ptr(), 1.0);
    sl!(5, 1, 0, 0.0, 4.0, c"vol 0sin 1tri 2sq 3saw 4hold".as_ptr(), 1.0);
    sl!(1, 0, 1, 0.005, 24.0, c"pan rate hz".as_ptr(), 0.0);
    sl!(6, 1, 1, 0.005, 24.0, c"vol rate hz".as_ptr(), 0.0);
    sl!(2, 0, 2, 0.0, 1.0, c"pan depth".as_ptr(), 1.0);
    sl!(7, 1, 2, 0.0, 1.0, c"vol depth".as_ptr(), 1.0);
    sl!(3, 0, 3, -1.0, 1.0, c"centre".as_ptr(), 1.0);
    sl!(4, 1, 3, 0.0, 1.0, c"phase off".as_ptr(), 1.0);
    sl!(8, 0, 4, 1.0, 200.0, c"slew ms".as_ptr(), 0.5);
    sl!(9, 1, 4, 0.0, 2.0, c"gain".as_ptr(), 1.0);
}

pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: SLOOPER_EFFECT_GUI_ABI,
    want_width: 430.0,
    want_height: 185.0,
    create: gui_create,
    draw: gui_draw,
    destroy: gui_destroy,
};

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"pan shape (0sin 1tri 2sq 3saw)".as_ptr(), min: 0.0, max: 3.0, default: 0.0 },
    EffectParamV1 { label: c"pan rate hz".as_ptr(), min: 0.005, max: 24.0, default: 1.5 },
    EffectParamV1 { label: c"pan depth".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
    EffectParamV1 { label: c"centre".as_ptr(), min: -1.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"phase off".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"vol shape (0sin 1tri 2sq 3saw 4hold)".as_ptr(), min: 0.0, max: 4.0, default: 0.0 },
    EffectParamV1 { label: c"vol rate hz".as_ptr(), min: 0.005, max: 24.0, default: 1.5 },
    EffectParamV1 { label: c"vol depth".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"slew ms".as_ptr(), min: 1.0, max: 200.0, default: 8.0 },
    EffectParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

static DESC_TXT: &core::ffi::CStr =
    c"LFO pan + volume wheels (PanOMatic task). Equal-power pan, phase-offset tremolo, slewed squares. ABI v3.";

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"pan matic".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"pan matic".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"pan matic".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TXT.as_ptr(),
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

#[no_mangle]
pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 {
    &GUI
}
