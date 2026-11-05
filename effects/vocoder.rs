// ============================================================ slooper ==
// vocoder.rs — 18-band vocoder. Covers FL Studio's Fruity Vocoder and
// Vocodex (consolidated). See docs/FL-SUITE.md.
//
// ABI v2 showcase: the audio THIS plugin sits on is the CARRIER (the
// sound you hear), the track's sidechain feed is the MODULATOR (the
// articulation — put the voice on another track and route it via the
// mixer "sidechain in"). Each band's modulator envelope rides the same
// band on the carrier; the band-sum is reshaped by `peakiness` — the
// classic Vocodex "peak" knob.
include!("effect_api.rs");

const NBANDS: usize = 18;

const NPARAMS: usize = 8;
const P_ATT: usize = 0; //     0.5..50 ms envelope attack
const P_REL: usize = 1; //     5..300 ms envelope release
const P_PEAK: usize = 2; //    0.5..2.5 band reshaping
const P_FLO: usize = 3; //     100..1500 Hz lowest band
const P_FHI: usize = 4; //     3000..12000 Hz highest band
const P_BRIGHT: usize = 5; //  -12..12 tilt on the summed output
const P_MIX: usize = 6; //     0..1
const P_OUT: usize = 7; //     0..2

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
    const BYPASS: Biquad = Biquad {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
        x1: 0.0,
        x2: 0.0,
        y1: 0.0,
        y2: 0.0,
    };
    #[inline]
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

#[derive(Clone, Copy)]
struct Band {
    m: Biquad,
    c: Biquad,
    env: f64,
}

struct St {
    sr: f64,
    bands: Vec<Band>,
    k_flo: f32,
    k_fhi: f32,
    c_att: f64,
    c_rel: f64,
    k_att: f32,
    k_rel: f32,
    lp: f64,
}

#[inline]
fn bpf(f0: f64, q: f64, sr: f64) -> Biquad {
    let f0 = f0.clamp(20.0, 0.45 * sr);
    let q = q.max(0.5);
    let w0 = core::f64::consts::TAU * f0 / sr;
    let (sw, cw) = w0.sin_cos();
    let alpha = sw / (2.0 * q);
    let a0 = 1.0 + alpha;
    Biquad {
        b0: alpha / a0,
        b1: 0.0,
        b2: -alpha / a0,
        a1: -2.0 * cw / a0,
        a2: (1.0 - alpha) / a0,
        x1: 0.0,
        x2: 0.0,
        y1: 0.0,
        y2: 0.0,
    }
}

fn rebuild(s: &mut St) {
    let lo = s.k_flo.max(20.0) as f64;
    let hi = (s.k_fhi as f64).max(lo * 3.0);
    let ratio = (hi / lo).powf(1.0 / (NBANDS - 1) as f64);
    let q = (1.0 / (ratio - 1.0)).clamp(1.0, 12.0);
    if s.bands.len() != NBANDS {
        s.bands = vec![
            Band { m: Biquad::BYPASS, c: Biquad::BYPASS, env: 0.0 };
            NBANDS
        ];
    }
    for i in 0..NBANDS {
        let f = lo * ratio.powi(i as i32);
        let mk = |b: &mut Biquad| {
            let n = bpf(f, q, s.sr);
            b.b0 = n.b0;
            b.b1 = n.b1;
            b.b2 = n.b2;
            b.a1 = n.a1;
            b.a2 = n.a2;
        };
        mk(&mut s.bands[i].m);
        mk(&mut s.bands[i].c);
    }
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    let mut st = St {
        sr,
        bands: Vec::new(),
        k_flo: 220.0,
        k_fhi: 7500.0,
        c_att: (-(1.0 / (5.0 * 0.001 * sr))).exp(),
        c_rel: (-(1.0 / (60.0 * 0.001 * sr))).exp(),
        k_att: 5.0,
        k_rel: 60.0,
        lp: 0.0,
    };
    rebuild(&mut st);
    Box::into_raw(Box::new(st)) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// v1 fallback: no sidechain exists in v1 hosts, so pass audio through.
extern "C" fn process(_state: *mut core::ffi::c_void, input: f32, _params: *const f32) -> f32 {
    input
}

extern "C" fn process_sc(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
    sc: f32,
) -> f32 {
    if state.is_null() {
        return input;
    }
    let s = unsafe { &mut *(state as *mut St) };
    let att = unsafe { *params.add(P_ATT) };
    let rel = unsafe { *params.add(P_REL) };
    let peak = unsafe { *params.add(P_PEAK) } as f64;
    let flo = unsafe { *params.add(P_FLO) };
    let fhi = unsafe { *params.add(P_FHI) };
    let bright = unsafe { *params.add(P_BRIGHT) } as f64;
    let mix = unsafe { *params.add(P_MIX) } as f64;
    let out = unsafe { *params.add(P_OUT) } as f64;

    let car = input as f64;
    let mut modu = sc as f64;
    if !car.is_finite() {
        for b in s.bands.iter_mut() {
            b.env = 0.0;
        }
        return 0.0;
    }
    if !modu.is_finite() {
        modu = 0.0;
    }

    if att != s.k_att {
        s.k_att = att;
        s.c_att = (-(1.0 / ((att.max(0.1) as f64) * 0.001 * s.sr))).exp();
    }
    if rel != s.k_rel {
        s.k_rel = rel;
        s.c_rel = (-(1.0 / ((rel.max(1.0) as f64) * 0.001 * s.sr))).exp();
    }
    if flo != s.k_flo || fhi != s.k_fhi {
        s.k_flo = flo;
        s.k_fhi = fhi;
        rebuild(s);
    }

    // Per band: modulator envelope rides the carrier band.
    let mut sum = 0.0f64;
    for b in s.bands.iter_mut() {
        let m = b.m.run(modu);
        let ma = m.abs();
        if ma > b.env {
            b.env += (ma - b.env) * (1.0 - s.c_att);
        } else {
            b.env += (ma - b.env) * (1.0 - s.c_rel);
        }
        let c = b.c.run(car);
        // band weight: env^peakiness (peak<1 smears, >1 tightens)
        let w = if b.env <= 1e-9 {
            0.0
        } else if (peak - 1.0).abs() < 0.02 {
            b.env
        } else {
            (peak * b.env.ln()).exp()
        };
        sum += c * w;
    }

    // Brightness tilt on the summed vocoded sound.
    let cl = (-(core::f64::consts::TAU * 3000.0 / s.sr)).exp();
    s.lp += (sum - s.lp) * (1.0 - cl);
    let tilt = if bright >= 0.0 {
        sum + (sum - s.lp) * (bright / 6.0)
    } else {
        sum - (sum - s.lp) * (bright.abs() / 6.0) * 0.9
    };

    let o = car * (1.0 - mix) + tilt * mix * out;
    if !o.is_finite() {
        return 0.0;
    }
    o.clamp(-3.0, 3.0) as f32
}

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"attack ms".as_ptr(), min: 0.5, max: 50.0, default: 5.0 },
    EffectParamV1 { label: c"release ms".as_ptr(), min: 5.0, max: 300.0, default: 60.0 },
    EffectParamV1 { label: c"peakiness".as_ptr(), min: 0.5, max: 2.5, default: 1.3 },
    EffectParamV1 { label: c"freq low".as_ptr(), min: 100.0, max: 1500.0, default: 220.0 },
    EffectParamV1 { label: c"freq high".as_ptr(), min: 3000.0, max: 12000.0, default: 7500.0 },
    EffectParamV1 { label: c"brightness".as_ptr(), min: -12.0, max: 12.0, default: 0.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.1 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Vocoder".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"18-band vocoder (ABI v2): this track's audio is the CARRIER, route a voice/synth via the mixer sidechain-in as MODULATOR.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"Vocoder".as_ptr(),
    author: c"slooper".as_ptr(),
    description: c"18-band vocoder (ABI v2): this track's audio is the CARRIER, route a voice/synth via the mixer sidechain-in as MODULATOR.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}

#[no_mangle]
pub extern "C" fn slooper_effect_info_v2() -> *const EffectInfoV2 {
    &DESC2
}
