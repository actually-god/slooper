// ============================================================ slooper ==
// spread.rs — stereo widener / image processor. Covers the FL Suite
// stereo-image task: Fruity Stereo Shaper, Stereo Enhancer, Spreader and
// the width half of PanOMatic live in one skeleton — a mid/side spreader
// fed by a decorrelating delay (Haas), because every source upstream of a
// slooper chain is MONO: "width" must be MANUFACTURED, not preserved.
// ABI v3 (first stock scripts — see effect_api.rs §ABI v3): the host
// hands process_st a real pair and carries the emitted pair to the
// track pan.
//
// The decorrelator is `side = x - lp(delay(x, haas))`: a lowpass-comb
// whose energy sits in the SIDES only (mid is untouched), so the mono
// downmix never loses body and no Haas comb is audible in mono (the lp
// keeps the mono sum's phase sane). `rotate` slowly walks the L/R image
// with a shared sine; `flip` drops the right side 180 degrees.
//
// Params:
//   width     — side gain applied with opposite signs (0..2)
//   haas ms   — decorrelation delay 0..40 ms (5..25 = the wide spot)
//   side lp   — lowpass on the delayed side 300..12000 Hz
//   rotate Hz — image rotation rate (0 = still; 0.05..0.5 drifts nicely)
//   flip R    — 1 = right side polarity inverted
//   mix       — 0 dry .. 1 wet
//   gain      — output trim
// ---------------------------------------------------------------------------

include!("effect_api.rs");

const NPARAMS: usize = 7;
const D_BITS: usize = 12; // 4096 samples (85 ms at 48k) covers 40 ms
const D_SZ: usize = 1 << D_BITS;
const D_MASK: usize = D_SZ - 1;

use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

struct St {
    sr: f64,
    dl: Vec<f32>,
    wp: usize,
    lp: f64,  // side lowpass state
    th: f64,   // rotation phase (turns)
    // GUI meter atomics (f32 bits): |L-R| side energy (0..1-ish).
    g_side: AtomicU32,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        dl: vec![0.0f32; D_SZ],
        wp: 0,
        lp: 0.0,
        th: 0.0,
        g_side: AtomicU32::new(0),
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// v1 entry / graceful fallback: with a mono bus there is no image to
/// spread — pass through.
extern "C" fn process(_state: *mut core::ffi::c_void, input: f32, _params: *const f32) -> f32 {
    input
}

/// v2 entry: no pair exists on a mono bus, and the side component
/// cancels in the host's fold — emit the mid (graceful dry).
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

/// The v3 stereo step: real pair in, real pair out.
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

    let width = unsafe { *params.add(0) }.clamp(0.0, 2.0) as f64;
    let haas = unsafe { *params.add(1) }.clamp(0.0, 40.0) as f64;
    let lp_hz = unsafe { *params.add(2) }.clamp(300.0, 12000.0) as f64;
    let rot = unsafe { *params.add(3) }.clamp(0.0, 5.0) as f64;
    let flip = unsafe { *params.add(4) } >= 0.5;
    let mix = unsafe { *params.add(5) }.clamp(0.0, 1.0) as f64;
    let gain = unsafe { *params.add(6) }.clamp(0.0, 2.0) as f64;

    let li = if l.is_finite() { l } else { 0.0 };
    let ri = if r.is_finite() { r } else { 0.0 };

    // Delay read (linear) at `haas`.
    let dpos = haas * 1e-3 * sr;
    let di = dpos as usize;
    let df = (dpos - di as f64) as f32;
    let i0 = s.wp.wrapping_sub(1 + di) & D_MASK;
    let i1 = s.wp.wrapping_sub(2 + di) & D_MASK;
    let dly = s.dl[i0] * (1.0 - df) + s.dl[i1] * df;

    // Store input (mid) into the ring.
    let mid_f = (li + ri) * 0.5;
    s.dl[s.wp & D_MASK] = mid_f;
    s.wp = s.wp.wrapping_add(1);

    // Side lowpass (true-Hz one-pole).
    let a = 1.0 - (-core::f64::consts::TAU * lp_hz / sr).exp();
    s.lp += (dly as f64 - s.lp as f64) * a;
    if !s.lp.is_finite() {
        s.lp = 0.0;
    }

    // Decorrelated side: what the comb REMOVED from mid is by
    // construction side-band energy.
    let side = (mid_f as f64 - s.lp) * width;

    // Image rotation: shared angle turns the (mid,side) pair.
    let rot_a = if rot > 0.001 {
        s.th += rot / sr;
        if s.th >= 1.0 {
            s.th -= 1.0;
        }
        let ang = (s.th * core::f64::consts::TAU).sin() * 0.5;
        ang
    } else {
        0.0
    };

    let mut os = side * (1.0 - rot_a.abs());
    if flip {
        os = -os;
    }
    let mut ol = mid_f as f64 + os;
    let mut orv = mid_f as f64 - os;
    // Rotation blends a little cross-coupling in (the image drifts).
    ol += rot_a * os * 0.5;
    orv -= rot_a * os * 0.5;

    let fl = (li as f64 * (1.0 - mix) + ol * mix) * gain as f64;
    let fr = (ri as f64 * (1.0 - mix) + orv * mix) * gain as f64;

    // Meter: running side energy for the GUI bar.
    let e = (li - ri).abs() as f64 * 0.02 + (os.abs() * 0.98).min(1.0);
    let prev = f32::from_bits(s.g_side.load(Relaxed)) as f64;
    s.g_side
        .store(((prev * 0.9 + e * 0.1) as f32).to_bits(), Relaxed);

    let (fl, fr) = (fl as f32, fr as f32);
    if !fl.is_finite() || !fr.is_finite() {
        s.lp = 0.0;
        s.dl.iter_mut().for_each(|v| *v = 0.0);
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    unsafe { *out_r = fr };
    fl
}

// ===================== scripted editor ========================

struct Gs {
    peak: f32,
}

extern "C" fn gui_create(_sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Gs { peak: 0.0 })) as *mut core::ffi::c_void
}

extern "C" fn gui_destroy(gs: *mut core::ffi::c_void) {
    if !gs.is_null() {
        drop(unsafe { Box::from_raw(gs as *mut Gs) });
    }
}

extern "C" fn gui_draw(
    gs: *mut core::ffi::c_void,
    state: *mut core::ffi::c_void,
    frame: *const GuiFrameV1,
    api: *const GuiApiV1,
) {
    if gs.is_null() || frame.is_null() || api.is_null() {
        return;
    }
    let gs = unsafe { &mut *(gs as *mut Gs) };
    let f = unsafe { &*frame };
    let api = unsafe { &*api };
    let c = api.ctx;
    let w = f.width;
    let h = f.height;

    (api.rect_filled)(c, 0.0, 0.0, w, h, 0.09, 0.10, 0.13, 1.0);

    // Meter: mid bar + side bar.
    let side = if state.is_null() {
        0.0
    } else {
        f32::from_bits(unsafe { (*(state as *const St)).g_side.load(Relaxed) })
    };
    let mh = 54.0;
    (api.text)(c, 12.0, 12.0, 12.0, 0, 0.6, 0.65, 0.72, 1.0, c"side energy".as_ptr());
    let bar = ((w - 24.0) * side.min(1.0)).max(0.0);
    (api.rect_filled)(c, 12.0, 26.0, bar, 12.0, 0.35, 0.75, 1.0, 0.9);

    // Sliders: one column of 5 + a short second column.
    let y0 = mh + 14.0;
    let colw = (w - 48.0) * 0.5;
    let rh = 34.0;
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
    sl!(0, 0, 0, 0.0, 2.0, c"width".as_ptr(), 1.0);
    sl!(1, 1, 0, 0.0, 40.0, c"haas ms".as_ptr(), 1.0);
    sl!(2, 0, 1, 300.0, 12000.0, c"side lp".as_ptr(), 0.0);
    sl!(3, 1, 1, 0.0, 5.0, c"rotate hz".as_ptr(), 1.0);
    sl!(4, 0, 2, 0.0, 1.0, c"flip R".as_ptr(), 1.0);
    sl!(5, 1, 2, 0.0, 1.0, c"mix".as_ptr(), 1.0);
    sl!(6, 0, 3, 0.0, 2.0, c"gain".as_ptr(), 1.0);
    let _ = gs.peak; // reserved for peak-hold when meters grow
}

pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: SLOOPER_EFFECT_GUI_ABI,
    want_width: 420.0,
    want_height: 190.0,
    create: gui_create,
    draw: gui_draw,
    destroy: gui_destroy,
};

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"width".as_ptr(), min: 0.0, max: 2.0, default: 1.2 },
    EffectParamV1 { label: c"haas ms".as_ptr(), min: 0.0, max: 40.0, default: 12.0 },
    EffectParamV1 { label: c"side lp".as_ptr(), min: 300.0, max: 12000.0, default: 2500.0 },
    EffectParamV1 { label: c"rotate hz".as_ptr(), min: 0.0, max: 5.0, default: 0.0 },
    EffectParamV1 { label: c"flip R".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"stereo spread".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Haas-comb stereo widener with image rotation (Stereo Shaper / Stereo Enhancer / Spreader task). ABI v3: real L/R to the track pan.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"stereo spread".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Haas-comb stereo widener with image rotation (Stereo Shaper / Stereo Enhancer / Spreader task). ABI v3: real L/R to the track pan.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"stereo spread".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Haas-comb stereo widener with image rotation (Stereo Shaper / Stereo Enhancer / Spreader task). ABI v3: real L/R to the track pan.".as_ptr(),
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
