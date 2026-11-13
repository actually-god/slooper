// ============================================================ slooper ==
// multiband.rs — three-band split dynamics: low/mid/high crossovers
// with independent compressor + output gain per band, plus a limiter
// per band for maximising.
//
// Crossovers are Linkwitz-Riley (2nd-order Butterworth pairs) — the summed
// output stays near flat, which is what mastering needs. Each band runs a
// peak-envelope compressor/limiter; bands are recombined and trimmed.
include!("effect_api.rs");

const NPARAMS: usize = 14;
// Crossovers
const P_LO: usize = 0; //   40..800   Hz  low/mid
const P_HI: usize = 1; //   1000..8000 Hz mid/high
// Per band: threshold + gain; ratio/attack/release shared across bands.
const P_THR_LO: usize = 2;
const P_THR_MID: usize = 3;
const P_THR_HI: usize = 4;
const P_GAIN_LO: usize = 5;
const P_GAIN_MID: usize = 6;
const P_GAIN_HI: usize = 7;
const P_RATIO: usize = 8; // 1..12
const P_ATT: usize = 9; //  0.05..50 ms
const P_REL: usize = 10; // 5..800 ms
const P_MIX: usize = 11; // 0..1
const P_OUT: usize = 12; // 0..2
const P_MODE: usize = 13; // 0..1 0 = compress down, 1 = maximise (limit up)

/// A 2nd-order Butterworth section (low or high). Coefficients recomputed
/// only when the crossover frequency moves.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        if !y.is_finite() {
            self.x1 = 0.0;
            self.x2 = 0.0;
            self.y1 = 0.0;
            self.y2 = 0.0;
            return 0.0;
        }
        y
    }
}

fn butter_lp(fc: f64, sr: f64) -> Biquad {
    let w = (core::f64::consts::TAU * fc / sr).min(core::f64::consts::PI * 0.95);
    let (sn, cs) = w.sin_cos();
    let q = core::f64::consts::FRAC_1_SQRT_2; // 0.7071 Butterworth
    let alpha = sn / (2.0 * q);
    let a0 = 1.0 + alpha;
    Biquad {
        b0: ((1.0 - cs) / 2.0) / a0,
        b1: (1.0 - cs) / a0,
        b2: ((1.0 - cs) / 2.0) / a0,
        a1: (-2.0 * cs) / a0,
        a2: (1.0 - alpha) / a0,
        x1: 0.0,
        x2: 0.0,
        y1: 0.0,
        y2: 0.0,
    }
}

fn butter_hp(fc: f64, sr: f64) -> Biquad {
    let w = (core::f64::consts::TAU * fc / sr).min(core::f64::consts::PI * 0.95);
    let (sn, cs) = w.sin_cos();
    let q = core::f64::consts::FRAC_1_SQRT_2;
    let alpha = sn / (2.0 * q);
    let a0 = 1.0 + alpha;
    Biquad {
        b0: ((1.0 + cs) / 2.0) / a0,
        b1: (-(1.0 + cs)) / a0,
        b2: ((1.0 + cs) / 2.0) / a0,
        a1: (-2.0 * cs) / a0,
        a2: (1.0 - alpha) / a0,
        x1: 0.0,
        x2: 0.0,
        y1: 0.0,
        y2: 0.0,
    }
}

/// Peak-envelope compressor for one band.
#[derive(Clone, Copy)]
struct Band {
    env: f64,
    gain: f64,
}

impl Band {
    fn run(&mut self, x: f64, thr: f64, ratio: f64, c_att: f64, c_rel: f64, maxim: bool) -> f64 {
        let a = x.abs();
        if a > self.env {
            self.env += (a - self.env) * (1.0 - c_att);
        } else {
            self.env += (a - self.env) * (1.0 - c_rel);
        }
        if !self.env.is_finite() {
            self.env = 0.0;
        }
        let target = if maxim {
            // Maximise: push UP to the threshold floor (expander-ish), then
            // the shared soft ceiling catches peaks.
            if self.env < thr && self.env > 1e-6 {
                (thr / self.env).min(8.0)
            } else {
                1.0
            }
        } else if self.env > thr && thr > 1e-6 {
            ((thr / self.env).powf(1.0 - 1.0 / ratio.max(1.0001))).max(0.01)
        } else {
            1.0
        };
        let gc = if target < self.gain { c_att } else { c_rel };
        self.gain += (target - self.gain) * (1.0 - gc.clamp(0.0, 0.999999));
        if !self.gain.is_finite() {
            self.gain = 1.0;
        }
        x * self.gain
    }
}

struct St {
    sr: f64,
    lo1: Biquad,
    lo2: Biquad, // LR = two Butterworth sections
    hp1: Biquad,
    hp2: Biquad,
    b2lo1: Biquad, // mid split stage 2 (high side split again at hi)
    b2lo2: Biquad,
    b2hi1: Biquad,
    b2hi2: Biquad,
    k_lo: f32,
    k_hi: f32,
    lo_env: Band,
    mid_env: Band,
    hi_env: Band,
    c_att: f64,
    c_rel: f64,
    k_att: f32,
    k_rel: f32,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    let f1 = 160.0f64;
    let f2 = 3000.0f64;
    let st = St {
        sr,
        lo1: butter_lp(f1, sr),
        lo2: butter_lp(f1, sr),
        hp1: butter_hp(f1, sr),
        hp2: butter_hp(f1, sr),
        b2lo1: butter_lp(f2, sr),
        b2lo2: butter_lp(f2, sr),
        b2hi1: butter_hp(f2, sr),
        b2hi2: butter_hp(f2, sr),
        k_lo: f1 as f32,
        k_hi: f2 as f32,
        lo_env: Band { env: 0.0, gain: 1.0 },
        mid_env: Band { env: 0.0, gain: 1.0 },
        hi_env: Band { env: 0.0, gain: 1.0 },
        c_att: tc(3.0, sr),
        c_rel: tc(120.0, sr),
        k_att: 3.0,
        k_rel: 120.0,
    };
    Box::into_raw(Box::new(st)) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

#[inline]
fn tc(ms: f64, sr: f64) -> f64 {
    ((-1.0) * (ms.max(0.01) * 0.001) * sr).recip().exp().clamp(0.0, 0.999999)
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let lo_x = unsafe { *params.add(P_LO) };
    let hi_x = unsafe { *params.add(P_HI) };
    let ratio = unsafe { *params.add(P_RATIO) } as f64;
    let att = unsafe { *params.add(P_ATT) };
    let rel = unsafe { *params.add(P_REL) };
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;
    let maxim = (unsafe { *params.add(P_MODE) } as f64) > 0.5;

    let x = input as f64;
    if !x.is_finite() {
        s.lo_env.env = 0.0;
        s.mid_env.env = 0.0;
        s.hi_env.env = 0.0;
        return 0.0;
    }

    // Rebuild crossover coefficients when a knob moves.
    if (lo_x - s.k_lo).abs() > 0.01 {
        s.k_lo = lo_x;
        s.lo1 = butter_lp(lo_x as f64, s.sr);
        s.lo2 = butter_lp(lo_x as f64, s.sr);
        s.hp1 = butter_hp(lo_x as f64, s.sr);
        s.hp2 = butter_hp(lo_x as f64, s.sr);
    }
    if (hi_x - s.k_hi).abs() > 0.01 {
        s.k_hi = hi_x;
        s.b2lo1 = butter_lp(hi_x as f64, s.sr);
        s.b2lo2 = butter_lp(hi_x as f64, s.sr);
        s.b2hi1 = butter_hp(hi_x as f64, s.sr);
        s.b2hi2 = butter_hp(hi_x as f64, s.sr);
    }
    if att != s.k_att {
        s.k_att = att;
        s.c_att = tc(att as f64, s.sr);
    }
    if rel != s.k_rel {
        s.k_rel = rel;
        s.c_rel = tc(rel as f64, s.sr);
    }

    // ---- split: LR crossover lo, then hi splits the high side -----------
    let mut low = s.lo2.run(s.lo1.run(x));
    let hs = s.hp2.run(s.hp1.run(x));
    let mut mid = s.b2lo2.run(s.b2lo1.run(hs));
    let mut hi = s.b2hi2.run(s.b2hi1.run(hs));

    // ---- per-band dynamics ------------------------------------------------
    let db2lin = |d: f32| (d as f64 * 0.05).exp2();
    low = s.lo_env.run(low, db2lin(unsafe { *params.add(P_THR_LO) }), ratio, s.c_att, s.c_rel, maxim);
    mid = s.mid_env.run(mid, db2lin(unsafe { *params.add(P_THR_MID) }), ratio, s.c_att, s.c_rel, maxim);
    hi = s.hi_env.run(hi, db2lin(unsafe { *params.add(P_THR_HI) }), ratio, s.c_att, s.c_rel, maxim);

    low *= unsafe { *params.add(P_GAIN_LO) } as f64;
    mid *= unsafe { *params.add(P_GAIN_MID) } as f64;
    hi *= unsafe { *params.add(P_GAIN_HI) } as f64;

    let wet = (low + mid + hi) * out;
    let o = x * (1.0 - mix) + wet * mix;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-4.0, 4.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"low/mid Hz".as_ptr(), min: 40.0, max: 800.0, default: 160.0 },
    EffectParamV1 { label: c"mid/high Hz".as_ptr(), min: 1000.0, max: 8000.0, default: 3000.0 },
    EffectParamV1 { label: c"low thr dB".as_ptr(), min: -60.0, max: 0.0, default: -18.0 },
    EffectParamV1 { label: c"mid thr dB".as_ptr(), min: -60.0, max: 0.0, default: -20.0 },
    EffectParamV1 { label: c"high thr dB".as_ptr(), min: -60.0, max: 0.0, default: -22.0 },
    EffectParamV1 { label: c"low gain".as_ptr(), min: 0.0, max: 4.0, default: 1.0 },
    EffectParamV1 { label: c"mid gain".as_ptr(), min: 0.0, max: 4.0, default: 1.0 },
    EffectParamV1 { label: c"high gain".as_ptr(), min: 0.0, max: 4.0, default: 1.0 },
    EffectParamV1 { label: c"ratio".as_ptr(), min: 1.0, max: 12.0, default: 3.0 },
    EffectParamV1 { label: c"attack ms".as_ptr(), min: 0.05, max: 50.0, default: 3.0 },
    EffectParamV1 { label: c"release ms".as_ptr(), min: 5.0, max: 800.0, default: 120.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"maximise".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Multiband".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"Three-band dynamics (LR crossovers): independent compressor or maximiser per band with per-band gain.".as_ptr(),
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
