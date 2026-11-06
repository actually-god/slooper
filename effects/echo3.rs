// ============================================================ slooper ==
// echo3.rs — studio delay with shaped feedback + stereo ping-pong. Covers
// Fruity Delay 2 / Delay 3 (and the single-bank core of Delay Bank). See
// docs/FL-SUITE.md.
//
// Clean delay line with filtered + saturated feedback (the "tone/damp"
// of tape and digital classics), optional ducking of the wet by the dry
// envelope (Fruity Delay 3's duck), wet trim, mix, output.
//
// ABI v3 STEREO (tier-4 rework): the feedback bus runs on the MID while
// each ear taps the ring at its own delay — right reads `time *
// (1 + ping-pong)` — so successive echoes trail from L to R: 0 keeps both
// ears on the same tap (parallel echoes, image untouched), 1 is hard
// ping-pong (R echoes lag a full repeat), the 0.5..0.9 band is the wide
// modern spot. The dry keeps its real (l, r) including the side, so the
// input image survives untouched.
//
// TIER-4 FIX (2026-11-06): the original `read_lin` indexed the ring by
// ABSOLUTE slot `buf[d]` instead of relative to the write head. The tap
// then heard input only at times t ≡ d (mod MAXD) — the feedback loop
// never got excited (an impulse produced DRY ONLY) and continuous input
// smeared into one scattered copy. The produce-sound gate measures RMS,
// which the smear passed; `tier4_feedback_loops_ring` now pins real
// impulse echoes for every feedback script. Read is head-relative here
// and in flanger.rs; chorus/space_echo/pitchshft already read correctly.
include!("effect_api.rs");

const NPARAMS: usize = 10;
const P_TIME: usize = 0; //   5..2000 ms
const P_FB: usize = 1; //    0..0.95
const P_TONE: usize = 2; //  150..18000 Hz feedback LP
const P_HPCUT: usize = 3; // 20..900 Hz feedback HP
const P_SAT: usize = 4; //   0..1 feedback saturation
const P_DUCK: usize = 5; //  0..1 wet ducking by dry
const P_LEVEL: usize = 6; // 0..1.5 wet trim
const P_MIX: usize = 7; //   0..1
const P_OUT: usize = 8; //   out trim
const P_PP: usize = 9; //    0..1 ping-pong spread (tier-4, appended)

const MAXD: usize = 1 << 18; // 262144 samples ring (~5.9 s @ 44.1 k)
const D_MASK: usize = MAXD - 1;

struct St {
    sr: f64,
    buf: Vec<f32>,
    widx: usize,
    // feedback-path filter states
    lp: f64,
    hp_x: f64,
    hp_y: f64,
    // dry envelope for ducking
    dry_env: f64,
    // slewed delay time in samples (slider drags must not click)
    dt: f64,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 44100.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        buf: vec![0.0f32; MAXD],
        widx: 0,
        lp: 0.0,
        hp_x: 0.0,
        hp_y: 0.0,
        dry_env: 0.0,
        dt: 0.32 * sr,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

#[inline]
fn read_head(buf: &[f32], widx: usize, frac: f64) -> f32 {
    // Linear-interpolated read `frac` samples BEHIND the write head.
    // (The tier-1 bug was reading buf[frac] absolutely — keep this
    // relative or the delay stops being a delay.)
    let frac = frac.clamp(1.0, (MAXD - 4) as f64);
    let f = widx as f64 - frac;
    let d = f.floor() as usize;
    let fr = f - d as f64;
    let i0 = d & D_MASK;
    let i1 = d.wrapping_add(1) & D_MASK;
    let a = buf[i0] as f64;
    let b = buf[i1] as f64;
    (a + (b - a) * fr) as f32
}

/// v1 entry: classic mono delay (the fixed tier-1 behaviour).
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let mut or = input;
    let ol = process_st(state, input, input, params, 0.0, &mut or);
    (ol + or) * 0.5
}

/// v2 entry (no sidechain key is used — the duck is internal by design).
extern "C" fn process_sc(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
    _sc: f32,
) -> f32 {
    process(state, input, params)
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
    let time = unsafe { *params.add(P_TIME) } as f64;
    let fb = unsafe { *params.add(P_FB) } as f64;
    let tone = unsafe { *params.add(P_TONE) } as f64;
    let hpcut = unsafe { *params.add(P_HPCUT) } as f64;
    let sat = unsafe { *params.add(P_SAT) } as f64;
    let duck = unsafe { *params.add(P_DUCK) } as f64;
    let level = unsafe { *params.add(P_LEVEL) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;
    let pp = (unsafe { *params.add(P_PP) } as f64).clamp(0.0, 1.0);

    let li = if l.is_finite() { l as f64 } else { 0.0 };
    let ri = if r.is_finite() { r as f64 } else { 0.0 };
    if !(l.is_finite() && r.is_finite()) {
        // Reset the LOOP FILTERS only — never clear the 262144-slot ring
        // on the audio thread (that is ~2.7 ms of work per NaN sample).
        // With clean filters the ring self-heals within one delay time.
        s.lp = 0.0;
        s.hp_x = 0.0;
        s.hp_y = 0.0;
        s.dry_env = 0.0;
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    let mid = (li + ri) * 0.5;
    let side = (li - ri) * 0.5;

    // Slew the delay time (~3 ms constant): raw slider steps move the
    // tap by whole samples per frame and click.
    let target = (time.max(5.0).min(2000.0) * 0.001 * s.sr).clamp(1.0, (MAXD - 4) as f64);
    let k = 1.0 - (-core::f64::consts::TAU * 50.0 / s.sr).exp();
    s.dt += (target - s.dt) * k;

    // Read delayed, shaped feedback FIRST (same-sample writes self-feed).
    let mut echo = read_head(&s.buf, s.widx, s.dt) as f64;

    // Feedback tone: LP then HP (one-poles, true-Hz via exp(-2πfc/sr)).
    let cl = (-(core::f64::consts::TAU * tone.min(0.45 * s.sr) / s.sr)).exp();
    s.lp += (echo - s.lp) * (1.0 - cl);
    echo = s.lp;
    let ch = (-(core::f64::consts::TAU * hpcut / s.sr)).exp();
    let hp = ch * s.hp_y + (echo - s.hp_x);
    s.hp_x = echo;
    s.hp_y = hp;
    echo = hp;

    // Feedback saturation: gentle tape compression in the loop.
    if sat > 0.001 {
        let kk = 1.0 + sat * 3.0;
        echo = ((echo * kk).tanh()) / kk.max(1.0) * kk;
    }

    s.buf[s.widx & D_MASK] = (mid + echo * fb.min(0.995)) as f32;
    s.widx = s.widx.wrapping_add(1);

    // Duck the wet with the dry envelope (Delay 3's "duck").
    let a = mid.abs();
    if a > s.dry_env {
        s.dry_env += (a - s.dry_env) * 0.01;
    } else {
        s.dry_env += (a - s.dry_env) * 0.0005;
    }
    let duckg = 1.0 - duck * (s.dry_env * 6.0).min(1.0);

    // Ping-pong: the right ear reads a LATER tap — echo n lands at n*T
    // in L and n*T + pp*T in R, so successive repeats trail L→R.
    let wet_l = echo * level * duckg;
    let wet_r = read_head(&s.buf, s.widx, s.dt * (1.0 + pp)) as f64 * level * duckg;

    let ol = mid * (1.0 - mix) + wet_l * mix * out;
    let orv = side + mid * (1.0 - mix) + wet_r * mix * out;
    if !(ol.is_finite() && orv.is_finite()) {
        s.lp = 0.0;
        s.hp_y = 0.0;
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    unsafe { *out_r = orv.clamp(-4.0, 4.0) as f32 };
    ol.clamp(-4.0, 4.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"time ms".as_ptr(), min: 5.0, max: 2000.0, default: 320.0 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.95, default: 0.55 },
    EffectParamV1 { label: c"tone Hz".as_ptr(), min: 150.0, max: 18000.0, default: 6000.0 },
    EffectParamV1 { label: c"low cut Hz".as_ptr(), min: 20.0, max: 900.0, default: 110.0 },
    EffectParamV1 { label: c"saturation".as_ptr(), min: 0.0, max: 1.0, default: 0.15 },
    EffectParamV1 { label: c"duck".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"level".as_ptr(), min: 0.0, max: 1.5, default: 1.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.35 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"ping-pong".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Echo 3".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Studio delay: filtered + saturated feedback, ducking, wet trim, stereo ping-pong (ABI v3). The Fruity Delay 2/3 role.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"Echo 3".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Studio delay: filtered + saturated feedback, ducking, wet trim, stereo ping-pong (ABI v3). The Fruity Delay 2/3 role.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"Echo 3".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Studio delay: filtered + saturated feedback, ducking, wet trim, stereo ping-pong (ABI v3). The Fruity Delay 2/3 role.".as_ptr(),
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
