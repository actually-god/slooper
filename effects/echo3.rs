// ============================================================ slooper ==
// echo3.rs — studio delay with shaped feedback. Covers Fruity Delay 2 /
// Delay 3 (and the single-bank core of Delay Bank). See docs/FL-SUITE.md.
//
// Clean delay line with filtered + saturated feedback (the "tone/damp"
// of tape and digital classics), optional ducking of the wet by the dry
// envelope (Fruity Delay 3's duck), wet trim, mix, output.
// Ping-pong/stereo operation needs the stereo-buffers capability (logged).
include!("effect_api.rs");

const NPARAMS: usize = 9;
const P_TIME: usize = 0; //   5..2000 ms
const P_FB: usize = 1; //    0..0.95
const P_TONE: usize = 2; //  150..18000 Hz feedback LP
const P_HPCUT: usize = 3; // 20..900 Hz feedback HP
const P_SAT: usize = 4; //   0..1 feedback saturation
const P_DUCK: usize = 5; //  0..1 wet ducking by dry
const P_LEVEL: usize = 6; // 0..1.5 wet trim
const P_MIX: usize = 7; //   0..1
const P_OUT: usize = 8;

const MAXD: usize = 1 << 18; // 262144 samples ring (~2.7 s @ 96k)

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
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    Box::into_raw(Box::new(St {
        sr,
        buf: vec![0.0f32; MAXD],
        widx: 0,
        lp: 0.0,
        hp_x: 0.0,
        hp_y: 0.0,
        dry_env: 0.0,
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

#[inline]
fn read_lin(buf: &[f32], mut frac: f64) -> f32 {
    if frac < 0.0 {
        frac = 0.0;
    }
    let d = frac.floor();
    let f = frac - d;
    let w = MAXD - 1;
    let i0 = (((d as i64) % MAXD as i64) as usize) & w;
    let i1 = (((d as i64 + 1) % MAXD as i64) as usize) & w;
    let a = buf[i0] as f64;
    let b = buf[i1] as f64;
    (a + (b - a) * f.min(1.0)) as f32
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
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

    let x = input as f64;
    if !x.is_finite() {
        for v in s.buf.iter_mut() {
            *v = 0.0;
        }
        s.lp = 0.0;
        s.hp_y = 0.0;
        s.dry_env = 0.0;
        return 0.0;
    }

    // Read delayed, shaped feedback FIRST (same-sample writes self-feed).
    let mut echo = read_lin(&s.buf, (time * 0.001 * s.sr).max(1.0)) as f64;

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
        let k = 1.0 + sat * 3.0;
        echo = ((echo * k).tanh()) / k.max(1.0) * k;
    }

    let w = MAXD - 1;
    s.buf[(s.widx & w) as usize] = (x + echo * fb.min(0.995)) as f32;
    s.widx = s.widx.wrapping_add(1);

    // Duck the wet with the dry envelope (Delay 3's "duck").
    let a = x.abs();
    if a > s.dry_env {
        s.dry_env += (a - s.dry_env) * 0.01;
    } else {
        s.dry_env += (a - s.dry_env) * 0.0005;
    }
    let duckg = 1.0 - duck * (s.dry_env * 6.0).min(1.0);

    let wet = echo * level * duckg;
    let o = x * (1.0 - mix) + wet * mix * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-4.0, 4.0) as f32
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
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Echo 3".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Studio delay: filtered + saturated feedback, ducking, wet trim. The Fruity Delay 2/3 role (ping-pong needs the stereo capability).".as_ptr(),
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
