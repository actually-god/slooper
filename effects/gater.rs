// ============================================================ slooper ==
// gater.rs — tempo volume-gating (LFO gate / stutter-lite).
//
// A shape LFO multiplies the signal through click-free smoothed ramps.
// Shapes: sine, triangle, pulse (duty = width), pluck (exponential
// decay), and random sample-&-hold — the four Groove-gate staples.
//
// ABI v3 (tier 5b): dual-lane gating, the Gross-Beat stereo role. ONE
// LFO clock drives both rails; the R rail reads it `spread × 0.5` cycles
// further along (1.0 = full antiphase: L open while R closed), with its
// own edge smoother. spread 0 folds the rails bit-exactly. The dry image
// never crosses rails — each side multiplies its own input.
//
// ABI v4 (tier 6): HOST SYNC mode + param metadata. `host = song` drives
// the LFO phase from the transport push (`rate` then means CYCLES PER
// BEAT): gating locks to the song grid and seeks/loops follow the
// playhead, the Gross-Beat role proper. host = free (default — every
// pre-v4 session gates exactly as before) keeps the free-running clock.
// Param metadata renders `shape` as a label picker and units on sliders.
include!("effect_api.rs");

const NPARAMS: usize = 9;
const P_RATE: usize = 0; //  0.05..40 Hz gate rate (song: cycles/beat)
const P_SHAPE: usize = 1; // 0 sine 1 tri 2 pulse 3 pluck 4 random
const P_WIDTH: usize = 2; // duty / decay scale
const P_SMOOTH: usize = 3; // 0.1..80 ms edge smoothing
const P_PHASE: usize = 4; // 0..1 phase offset
const P_MIX: usize = 5; //  0..1 (wet = gated amount)
const P_OUT: usize = 6;
const P_SPREAD: usize = 7; // 0..1 R-rail phase offset (x0.5 cycles)
const P_HOST: usize = 8; //  0 free clock / 1 host song phase (tier 6)

struct St {
    sr: f64,
    phase: f64,
    gate: f64,
    gate_r: f64,
    rng: u32,
    rnd: f64,
    last_shape: i32,
    // Tier-6 sync state (written ONLY in set_sync / read+advanced in
    // process — both audio thread; the GUI never touches these).
    sync_beats: f64,  // pos_beats at the last push
    sync_bps: f64,    // beats per sample (bpm / (60 * sr))
    sync_playing: bool,
    sync_in: i64,     // samples since the last push
    sync_ready: bool, // at least one push has arrived
    last_cyc: f64,    // whole-cycle counter for S&H resampling
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        phase: 0.0,
        gate: 1.0,
        gate_r: 1.0,
        rng: 0x2545_F491,
        rnd: 1.0,
        last_shape: -999,
        sync_beats: 0.0,
        sync_bps: 0.0,
        sync_playing: false,
        sync_in: 0,
        sync_ready: false,
        last_cyc: f64::MIN,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// Tier-6: block-start transport push (see the ABI v4 contract): store
/// the beat anchor; process self-advances it sample-exactly afterwards.
extern "C" fn set_sync(state: *mut core::ffi::c_void, sync: *const SyncInfoV1) {
    if state.is_null() || sync.is_null() {
        return;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let sy = unsafe { &*sync };
    if sy.abi == SLOOPER_SYNC_ABI && sy.sample_rate > 0.0 {
        s.sync_beats = if sy.pos_beats.is_finite() && sy.pos_beats >= 0.0 {
            sy.pos_beats
        } else {
            0.0
        };
        s.sync_bps = if sy.bpm > 0.0 && sy.bpm.is_finite() {
            sy.bpm / (60.0 * sy.sample_rate)
        } else {
            0.0
        };
        s.sync_playing = sy.playing != 0;
        s.sync_in = 0;
        s.sync_ready = true;
    }
}

/// Raw shape in [0,1] at phase `p` (0..1). Shared by both rails.
#[inline]
fn shape_at(shape: i32, p: f64, width: f64, rnd: f64) -> f64 {
    match shape {
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
            rnd * (-(t * 0.35)).exp()
        }
    }
}

/// v1 entry: mono — LEFT of the pair is the historical gate.
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    let mut or = input;
    process_st(state, input, input, params, 0.0, &mut or)
}

/// v2 entry: mono bus — emit the mid of the two gate lanes.
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

/// The v3 stereo step: one clock, two gate lanes, per-rail smoothing.
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
    let rate = unsafe { *params.add(P_RATE) } as f64;
    let shape = (unsafe { *params.add(P_SHAPE) } as f64).round() as i32;
    let width = unsafe { *params.add(P_WIDTH) } as f64;
    let smooth = unsafe { *params.add(P_SMOOTH) } as f64;
    let phase = unsafe { *params.add(P_PHASE) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;
    let spread = unsafe { *params.add(P_SPREAD) }.clamp(0.0, 1.0) as f64;

    if !l.is_finite() {
        // Preserve the v1 NaN semantics on the primary rail; the pair
        // simply rides the ramps down from here.
        s.gate = 1.0;
        s.gate_r = if r.is_finite() { s.gate_r } else { 1.0 };
        let yr = if r.is_finite() {
            (r as f64 * (1.0 - mix + mix * s.gate_r) * out) as f32
        } else {
            0.0
        };
        unsafe { *out_r = yr };
        return 0.0;
    }
    let x = l as f64;
    let xr = if r.is_finite() { r as f64 } else { 0.0 };

    // LFO clock (ONE for both rails): free-running, or host-song phase.
    let host = unsafe { *params.add(P_HOST) } > 0.5;
    let cyc = if host && s.sync_ready {
        // `rate` = cycles per BEAT; the phase follows the transport and
        // self-advances sample-exactly between pushes (ABI v4 contract).
        // While the transport is stopped the phase freezes; the next
        // push re-anchors (so seeks and pauses stay in phase).
        let b = s.sync_beats + if s.sync_playing { s.sync_in as f64 * s.sync_bps } else { 0.0 };
        if s.sync_playing && s.sync_in < i64::MAX / 2 {
            s.sync_in += 1;
        }
        let c = b * rate;
        let ci = c.floor();
        if ci != s.last_cyc {
            s.last_cyc = ci;
            s.rng = s.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            s.rnd = ((s.rng >> 8) as f64 / 16_777_216.0) * 0.9 + 0.1;
        }
        c
    } else {
        s.phase += rate / s.sr;
        if s.phase >= 1.0 {
            s.phase -= s.phase.floor();
            // resample on every cycle for the random shape
            s.rng = s.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            s.rnd = ((s.rng >> 8) as f64 / 16_777_216.0) * 0.9 + 0.1;
        }
        s.phase
    };

    // Raw shapes at the two rail phases (R is `spread x 0.5` cycles late).
    let p = (cyc + phase) % 1.0;
    let raw = shape_at(shape, p, width, s.rnd);
    let pr = (p + 0.5 * spread) % 1.0;
    let raw_r = shape_at(shape, pr, width, s.rnd);

    // Edge smoothing: separate attack/release one-poles from `smooth` ms.
    // (A hard square gate clicks; this is the Gross-Beat "soft" feel.)
    let tau = (smooth.max(0.1) * 0.001 * s.sr).max(1.0);
    let c = if raw > s.gate {
        (-(1.0 / (tau * 0.35))).exp()
    } else {
        (-(1.0 / tau)).exp()
    };
    s.gate = c * s.gate + (1.0 - c) * raw;
    let cr = if raw_r > s.gate_r {
        (-(1.0 / (tau * 0.35))).exp()
    } else {
        (-(1.0 / tau)).exp()
    };
    s.gate_r = cr * s.gate_r + (1.0 - cr) * raw_r;
    s.last_shape = shape;

    let ol = x * (1.0 - mix + mix * s.gate) * out;
    let or = xr * (1.0 - mix + mix * s.gate_r) * out;
    if !ol.is_finite() || !or.is_finite() {
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    unsafe { *out_r = (or.clamp(-4.0, 4.0)) as f32 };
    (ol.clamp(-4.0, 4.0)) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.05, max: 40.0, default: 2.0 },
    EffectParamV1 { label: c"shape".as_ptr(), min: 0.0, max: 4.0, default: 2.0 },
    EffectParamV1 { label: c"width/decay".as_ptr(), min: 0.02, max: 1.0, default: 0.5 },
    EffectParamV1 { label: c"smooth ms".as_ptr(), min: 0.1, max: 80.0, default: 4.0 },
    EffectParamV1 { label: c"phase".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"spread (R lane offset x180deg)".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
    EffectParamV1 { label: c"host".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
];

// Tier-6 param metadata: `shape` and `host` become label pickers (the
// 5 / 2 equally spaced slider points), units ride the numeric readouts.
// (Raw-pointer arrays need the read-only-static Sync promise the ABI
// file already makes for DESC structs — same reasoning, local wrapper.)
#[repr(C)]
struct Labels<const N: usize>([*const core::ffi::c_char; N]);
// SAFETY: read-only statics of pointers to literals, shared one way.
unsafe impl<const N: usize> Sync for Labels<N> {}

static SHAPE_LABELS: Labels<5> = Labels([
    c"sine".as_ptr(),
    c"tri".as_ptr(),
    c"pulse".as_ptr(),
    c"pluck".as_ptr(),
    c"rnd".as_ptr(),
]);
static HOST_LABELS: Labels<2> = Labels([c"free".as_ptr(), c"song".as_ptr()]);

static META: [ParamMetaV1; NPARAMS] = [
    ParamMetaV1 { unit: SLOOPER_UNIT_HZ, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: SHAPE_LABELS.0.as_ptr(), n_labels: 5 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: SLOOPER_UNIT_MS, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: HOST_LABELS.0.as_ptr(), n_labels: 2 },
];

const DESC_TXT: &core::ffi::CStr =
    c"Tempo volume-gating: sine/triangle/pulse/pluck/random LFO with click-free edges. ABI v3: dual-lane gating (R lane phase-spread, 1 = antiphase). ABI v4: host = song locks the gate phase to the transport (rate = cycles per beat).";

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Gater".as_ptr(),
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
    name: c"Gater".as_ptr(),
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
    name: c"Gater".as_ptr(),
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

pub static DESC4: EffectInfoV4 = EffectInfoV4 {
    abi: SLOOPER_EFFECT_ABI_V4,
    name: c"Gater".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TXT.as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
    process_st,
    stereo: 1,
    set_sync: Some(set_sync),
    param_meta: META.as_ptr(),
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
pub extern "C" fn slooper_effect_info_v4() -> *const EffectInfoV4 {
    &DESC4
}
