// ============================================================ slooper ==
// waveshaper.rs — draw-your-own distortion curve. Covers FL Studio's
// Fruity WaveShaper (draggable curve), the shaping half of Hardcore/LSD,
// and doubles as a generic transfer-function box. See docs/FL-SUITE.md.
//
// The curve is NINE control points (params 0..8, y in -1..1, x fixed on a
// grid) plus two fixed corner knots at (-1,-1) and (1,1). process() runs a
// piecewise-linear lookup over those eleven knots — cheap enough to do per
// sample with zero allocation. The scripted GUI lets you DRAG the nine
// points; presets snap the whole curve in one click.
include!("effect_api.rs");

const NKK: usize = 11; // nine interior knots + two corners
const NPARAMS: usize = 13;
const P_SH0: usize = 0; // P_SH0..P_SH0+8 = nine curve points, -1..1
const P_DRIVE: usize = 9; // 1..16 pre gain
const P_MIX: usize = 10; //  0..1
const P_OUT: usize = 11; //  0..1.5 post trim
const P_DC: usize = 12; //  0/1   remove DC from the output

struct St {
    sr: f64,
    // DC remover (one-pole HP ~24 Hz) on the output.
    dc_x: f64,
    dc_y: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St { sr, dc_x: 0.0, dc_y: 0.0 })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// Knot y-values (params 0..8) expanded with the two corner knots.
/// Knot x positions are fixed: corners at +/-1, interior every 0.2.
#[inline]
fn shape(params: *const f32, x: f64) -> f64 {
    let mut ys = [0.0f64; NKK];
    ys[0] = -1.0;
    for i in 0..9 {
        ys[1 + i] = unsafe { *params.add(P_SH0 + i) } as f64;
    }
    ys[NKK - 1] = 1.0;
    // segment by fixed grid: seg = floor((x+1)/0.2), clamped 0..9
    let u = ((x + 1.0) * 5.0).clamp(0.0, 9.999999);
    let seg = u as usize; // 0..9
    let f = u - seg as f64;
    ys[seg] + (ys[seg + 1] - ys[seg]) * f
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let drive = unsafe { *params.add(P_DRIVE) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;
    let dc = unsafe { *params.add(P_DC) } as f64;

    let mut x = input as f64;
    if !x.is_finite() {
        s.dc_x = 0.0;
        s.dc_y = 0.0;
        return 0.0;
    }

    // Drive into normalized curve space; mix back at a compensating trim
    // so heavy drive does not slam the output.
    let g = if drive <= 1.0 { 1.0 } else { drive };
    let y = shape(params, (x * g).clamp(-1.0, 1.0));
    let mut o = (x * (1.0 - mix) + y * mix) * out * g.sqrt().recip().min(1.0);

    // DC removal: one-pole ~24 Hz highpass ON THE OUTPUT (asymmetric
    // curves bias DC; this drains it). States follow the output signal.
    if dc > 0.5 {
        let a = (core::f64::consts::TAU * 24.0 / s.sr).min(1.0);
        let hp = a * s.dc_y + (o - s.dc_x);
        s.dc_x = o;
        s.dc_y = hp;
        o = hp;
    } else {
        s.dc_x = 0.0;
        s.dc_y = 0.0;
    }

    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-4.0, 4.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"curve p1".as_ptr(), min: -1.0, max: 1.0, default: -0.8 },
    EffectParamV1 { label: c"curve p2".as_ptr(), min: -1.0, max: 1.0, default: -0.6 },
    EffectParamV1 { label: c"curve p3".as_ptr(), min: -1.0, max: 1.0, default: -0.4 },
    EffectParamV1 { label: c"curve p4".as_ptr(), min: -1.0, max: 1.0, default: -0.2 },
    EffectParamV1 { label: c"curve p5".as_ptr(), min: -1.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"curve p6".as_ptr(), min: -1.0, max: 1.0, default: 0.2 },
    EffectParamV1 { label: c"curve p7".as_ptr(), min: -1.0, max: 1.0, default: 0.4 },
    EffectParamV1 { label: c"curve p8".as_ptr(), min: -1.0, max: 1.0, default: 0.6 },
    EffectParamV1 { label: c"curve p9".as_ptr(), min: -1.0, max: 1.0, default: 0.8 },
    EffectParamV1 { label: c"drive".as_ptr(), min: 1.0, max: 16.0, default: 4.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 1.5, default: 0.5 },
    EffectParamV1 { label: c"dc remove".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
];

// ---------------------------------------------------------------------------
// Custom child GUI: curve canvas with nine draggable handles + knobs.
// ---------------------------------------------------------------------------
struct Gs {
    sel: i32, // interior knot index 0..8 while dragging, -1 idle
}

extern "C" fn gui_create(_sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Gs { sel: -1 })) as *mut core::ffi::c_void
}

extern "C" fn gui_destroy(gs: *mut core::ffi::c_void) {
    if !gs.is_null() {
        drop(unsafe { Box::from_raw(gs as *mut Gs) });
    }
}

// Presets: nine interior knot heights (x fixed -0.8..0.8 step 0.2).
static PRESETS: [(&str, [f32; 9]); 6] = [
    ("Linear", [-0.8, -0.6, -0.4, -0.2, 0.0, 0.2, 0.4, 0.6, 0.8]),
    ("Tube", [-0.74, -0.52, -0.3, -0.12, 0.0, 0.12, 0.3, 0.52, 0.74]),
    ("Fuzz", [-0.99, -0.99, -0.99, -0.1, 0.0, 0.1, 0.99, 0.99, 0.99]),
    ("Hard", [-1.0, -1.0, -0.05, -0.05, 0.0, 0.05, 0.05, 1.0, 1.0]),
    ("3-Step", [-1.0, -1.0, -1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0]),
    ("Fold", [-0.6, -0.2, 0.9, 0.2, -0.6, 0.2, 0.9, 0.2, 0.6]),
];

extern "C" fn gui_draw(
    gs: *mut core::ffi::c_void,
    _state: *mut core::ffi::c_void,
    frame: *const GuiFrameV1,
    api: *const GuiApiV1,
) {
    if gs.is_null() || frame.is_null() || api.is_null() {
        return;
    }
    let gs = unsafe { &mut *(gs as *mut Gs) };
    let fr = unsafe { &*frame };
    let api = unsafe { &*api };
    let c = api.ctx;
    let gp = |k: u32| unsafe { (api.get_param)(c, k) };
    let sp = |k: u32, v: f32| unsafe { (api.set_param)(c, k, v) };

    let w = fr.width;
    let h = fr.height;
    let strip = 118.0f32;
    let pad = 26.0f32;
    let plot_w = (w - pad * 2.0).max(40.0);
    let plot_h = (h - strip - pad).max(60.0);
    let mid_y = pad * 0.5 + plot_h * 0.5;
    let px = |x: f32| pad + (x + 1.0) * 0.5 * plot_w;
    let py = |y: f32| mid_y - y * plot_h * 0.5;
    let iy = |p: f32| ((mid_y - p) / (plot_h * 0.5)).clamp(-1.0, 1.0);

    unsafe {
        // canvas + grid
        (api.rect_filled)(c, 8.0, 8.0, w - 16.0, h - 16.0, 0.09, 0.10, 0.13, 1.0);
        for gi in 0..=8 {
            let gx = pad + gi as f32 / 8.0 * plot_w;
            (api.line)(c, gx, mid_y - plot_h * 0.5, gx, mid_y + plot_h * 0.5, 1.0, 0.22, 0.24, 0.30, 0.6);
        }
        for gi in 0..=8 {
            let gy = mid_y - plot_h * 0.5 + gi as f32 / 8.0 * plot_h;
            (api.line)(c, pad, gy, pad + plot_w, gy, 1.0, 0.22, 0.24, 0.30, 0.6);
        }
        (api.line)(c, pad, mid_y, pad + plot_w, mid_y, 1.4, 0.45, 0.5, 0.6, 0.9);

        // curve (sampled through the same knot grid process uses)
        let mut pts: Vec<f32> = Vec::with_capacity(160);
        for i in 0..80 {
            let x = -1.0 + i as f32 / 79.0 * 2.0;
            let segf = (x + 1.0) * 5.0;
            let seg = segf.clamp(0.0, 9.999) as usize;
            let f = segf - seg as f32;
            let y0 = knot_y(gp, seg);
            let y1 = knot_y(gp, seg + 1);
            pts.push(px(x));
            pts.push(py(y0 + (y1 - y0) * f));
        }
        (api.polyline)(c, pts.as_ptr(), 80, 0, 2.0, 0.35, 0.85, 1.0, 0.95);

        // handles
        for i in 0..9 {
            let x = -0.8 + i as f32 * 0.2;
            let y = gp(i as u32);
            let (r, gg, b) = if gs.sel as usize == i { (1.0, 0.85, 0.4) } else { (0.35, 0.85, 1.0) };
            (api.rect_filled)(c, px(x) - 5.0, py(y) - 5.0, 10.0, 10.0, r, gg, b, 1.0);
        }

        // drag interaction: press near a handle grabs it; drag moves y only.
        if fr.mouse_pressed == 1 {
            let mut best = -1i32;
            let mut bd = 14.0f32;
            for i in 0..9 {
                let x = -0.8 + i as f32 * 0.2;
                let y = gp(i as u32);
                let d = (((fr.mouse_x - px(x)).powi(2) + (fr.mouse_y - py(y)).powi(2)).sqrt());
                if d < bd {
                    bd = d;
                    best = i as i32;
                }
            }
            gs.sel = best;
        }
        if gs.sel >= 0 {
            if fr.mouse_down == 1 {
                sp(gs.sel as u32, iy(fr.mouse_y));
            } else if fr.mouse_released == 1 {
                gs.sel = -1;
            }
        }

        // knob row + dc switch
        let ky = h - strip + 14.0;
        let mut v = gp(P_DRIVE as u32);
        let s = std::ffi::CString::new(format!("drive {v:.1}")).unwrap();
        if (api.knob)(c, 20.0, ky, 44.0, s.as_ptr(), &mut v, 1.0, 16.0, 0.0) != 0 {
            sp(P_DRIVE as u32, v);
        }
        let mut v = gp(P_MIX as u32);
        let s = std::ffi::CString::new(format!("mix {:.0}%", v * 100.0)).unwrap();
        if (api.knob)(c, 96.0, ky, 44.0, s.as_ptr(), &mut v, 0.0, 1.0, 1.0) != 0 {
            sp(P_MIX as u32, v);
        }
        let mut v = gp(P_OUT as u32);
        let s = std::ffi::CString::new(format!("out {:.2}", v)).unwrap();
        if (api.knob)(c, 172.0, ky, 44.0, s.as_ptr(), &mut v, 0.0, 1.5, 1.0) != 0 {
            sp(P_OUT as u32, v);
        }
        let mut on = gp(P_DC as u32);
        let s = std::ffi::CString::new("dc-removal").unwrap();
        if (api.checkbox)(c, 250.0, ky + 8.0, s.as_ptr(), &mut on) != 0 {
            sp(P_DC as u32, on);
        }

        // preset buttons
        let py0 = h - 34.0;
        for (pi, (name, vals)) in PRESETS.iter().enumerate() {
            let s = std::ffi::CString::new(*name).unwrap();
            if (api.button)(c, 20.0 + pi as f32 * 86.0, py0, 82.0, s.as_ptr()) != 0 {
                for (k, v) in vals.iter().enumerate() {
                    sp(k as u32, *v);
                }
            }
        }
    }
}

/// Knot y at expanded index 0..10 (corner knots fixed at -1/+1).
#[inline]
fn knot_y(gp: impl Fn(u32) -> f32, k: usize) -> f32 {
    match k {
        0 => -1.0,
        10 => 1.0,
        _ => gp((k - 1) as u32),
    }
}

pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: SLOOPER_EFFECT_GUI_ABI,
    want_width: 620.0,
    want_height: 380.0,
    create: gui_create,
    draw: gui_draw,
    destroy: gui_destroy,
};

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Wave Shaper".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Draw your own distortion curve: nine draggable points + drive/mix/out. Tube, fuzz, hard-clip, step and fold presets included.".as_ptr(),
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

#[no_mangle]
pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 {
    &GUI
}
