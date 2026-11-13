// ============================================================ slooper ==
// slooper-synth-version: 6
// pluck.rs — Karplus-Strong plucked strings.
//
// Physical-modeled string instruments reduce to the classic extended
// Karplus-Strong loop
// (fractional delay + loop damping + pick-position comb + body resonator)
// gets you harp, guitar, koto and bass-pluck from six sliders. Strings
// KEEP RINGING after note-off (real string behavior); all_off kills them.
//
// Params:
//   gain   — output trim
//   decay  — seconds for the string to die ~60 dB (0.05..8)
//   bright — loop low-pass tightness: 1 = fresh pick, 0 = muted
//   pick   — pick/plectrum position along the string (0..0.5); comb
//            notches (0 = center = hollow, higher = open)
//   body   — body-resonance level (low-passed feedback, 0..1)
//   stiff  — high-frequency loop retention: string stiffness / shimmer
//
// 10 voices, round-robin steal, allocation-free DSP (delay lines built in
// create), NaN-guarded.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 10;
const SEG: usize = 8192; // delay-line capacity per voice (power of two)

struct Voice {
    alive: bool,
    note: u8,
    buf: Vec<f32>,
    wp: usize,   // write position
    len: f64,    // delay length in samples (fractional)
    damp: f64,   // loop one-pole state
    bstate: f64, // body resonator state
    energy: f64,
}

struct Pluck {
    sr: f64,
    voices: Vec<Voice>,
    next: usize,
}

/// Linear-interpolated read at fractional `pos`, wrapped to [0, SEG).
fn lerp(buf: &[f32], pos: f64) -> f64 {
    let mut p = pos % SEG as f64;
    if p < 0.0 {
        p += SEG as f64;
    }
    let i0 = p as usize; // p in [0, SEG): i0 in [0, SEG-1], i0+1 wraps
    let i0 = i0 & (SEG - 1);
    let i1 = (i0 + 1) & (SEG - 1);
    let f = p - p.floor();
    buf[i0] as f64 * (1.0 - f) + buf[i1] as f64 * f
}

extern "C" fn create(sr: f64, _host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let voices = (0..VOICES)
        .map(|_| Voice {
            alive: false,
            note: 0,
            buf: vec![0.0f32; SEG],
            wp: 0,
            len: 100.0,
            damp: 0.0,
            bstate: 0.0,
            energy: 0.0,
        })
        .collect::<Vec<_>>();
    let s = Box::new(Pluck {
        sr: if sr > 1.0 { sr } else { 48000.0 },
        voices,
        next: 0,
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Pluck) });
    }
}

/// Deterministic cheap LCG (no rand crate in script land).
struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f64 / 16_777_216.0 * 2.0 - 1.0
    }
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Pluck) };
    let sr = p.sr;
    let amp = (vel.max(1) as f64 / 127.0).powf(0.6);
    let f = 440.0f64 * (2.0f64).powf((note as f64 - 69.0) / 12.0);
    let idx = p.next;
    p.next = (p.next + 1) % VOICES;
    let v = &mut p.voices[idx];
    v.alive = true;
    v.note = note;
    v.damp = 0.0;
    v.bstate = 0.0;
    let len = (sr / f.max(6.0)).clamp(3.0, (SEG / 2 - 64) as f64);
    v.len = len;
    // Excitation: noise burst windowed over one period (the classic
    // "pluck the string" initialization).
    let mut rng = Lcg(0x9e37_79b9 ^ ((note as u32) << 7) ^ (idx as u32) ^ 0x5bd1);
    for x in v.buf.iter_mut() {
        *x = 0.0;
    }
    let li = len as usize;
    for k in 0..li {
        let shape = (k as f64 / li as f64 * std::f64::consts::PI).sin();
        v.buf[k] = (rng.next() * shape) as f32 * amp as f32;
    }
    v.wp = li & (SEG - 1);
    v.energy = amp;
}

extern "C" fn note_off(_s: *mut core::ffi::c_void, _ch: u8, _note: u8) {
    // A plucked string keeps ringing. Decay is the decay parameter.
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Pluck) };
    for v in &mut p.voices {
        v.alive = false;
        v.energy = 0.0;
        for x in v.buf.iter_mut() {
            *x = 0.0;
        }
    }
}

extern "C" fn cc(_s: *mut core::ffi::c_void, _ch: u8, _num: u8, _val: u8) {}
extern "C" fn press(_s: *mut core::ffi::c_void, _ch: u8, _val: u8) {}

extern "C" fn process(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Pluck) };

    let gain = unsafe { *params.add(0) }.clamp(0.0, 1.0) as f64;
    let t60 = unsafe { *params.add(1) }.clamp(0.05, 8.0) as f64;
    let bright = unsafe { *params.add(2) }.clamp(0.0, 1.0) as f64;
    let pick = unsafe { *params.add(3) }.clamp(0.0, 0.49) as f64;
    let body = unsafe { *params.add(4) }.clamp(0.0, 1.0) as f64;
    let stiff = unsafe { *params.add(5) }.clamp(0.0, 1.0) as f64;

    // Loop gain reaching -60 dB in t60 seconds.
    let decay = (-9.21034 / (t60 * p.sr)).exp();
    let a = 0.15 + 0.85 * bright; // loop one-pole coefficient (1 = open)
    let hf = 0.2 + 0.8 * stiff;   // HF floor lifted by string stiffness

    for k in 0..n {
        let mut acc = 0.0f64;
        for v in &mut p.voices {
            if !v.alive {
                continue;
            }
            if v.len < 3.0 {
                v.alive = false;
                continue;
            }
            // Read the loop delay at wp - len (fractional, wrapped).
            let rp = v.wp as f64 + 2.0 * SEG as f64 - v.len;
            let s1 = lerp(&v.buf, rp);
            let s2 = lerp(&v.buf, rp - 1.0);
            // Loop low-pass on the two-tap mean; split into LP and HP
            // parts so `bright` darkens and `stiff` saves the shimmer.
            let mean = (s1 + s2) * 0.5;
            v.damp += (mean - v.damp) * a;
            let hp_part = mean - v.damp;
            // LP passes at unity; HP passes between `a` (dark) and 1.0
            // (stiff=1 keeps every partial).
            let hp_gain = a + (1.0 - a) * hf;
            let x = decay * (v.damp + hp_part * hp_gain);
            if !x.is_finite() {
                v.alive = false;
                for y in v.buf.iter_mut() {
                    *y = 0.0;
                }
                continue;
            }
            v.buf[v.wp & (SEG - 1)] = x as f32;
            v.wp = (v.wp + 1) & (SEG - 1);

            // Output: pick-position comb (subtract a delayed copy).
            let wp1 = (v.wp + SEG - 2) & (SEG - 1);
            let mut o = v.buf[wp1] as f64;
            if pick > 0.001 {
                let cl = (v.len * pick) as usize;
                if cl >= 1 {
                    let cp = (wp1 + SEG - (cl & (SEG - 1))) & (SEG - 1);
                    o -= v.buf[cp] as f64 * (1.0 - pick * 2.0).max(0.0);
                }
            }
            // Body resonance: gentle low-passed feedback.
            v.bstate += (o - v.bstate) * 0.08;
            o = o * (1.0 - body * 0.6) + v.bstate * body * 0.9;

            acc += o;
            v.energy = v.energy * 0.9995 + o.abs() * 0.0005;
            if v.energy < 1.0e-5 {
                v.alive = false;
            }
        }
        let smp = acc * gain * 0.5;
        if smp.is_finite() {
            unsafe { *out.add(k) += smp as f32 };
        }
    }
}

static PARAMS: [SynthParamV1; 6] = [
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
    SynthParamV1 { label: c"decay s (T60)".as_ptr(), min: 0.05, max: 8.0, default: 1.8 },
    SynthParamV1 { label: c"bright".as_ptr(), min: 0.0, max: 1.0, default: 0.65 },
    SynthParamV1 { label: c"pick pos".as_ptr(), min: 0.0, max: 0.49, default: 0.13 },
    SynthParamV1 { label: c"body".as_ptr(), min: 0.0, max: 1.0, default: 0.25 },
    SynthParamV1 { label: c"stiff shimmer".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"pluck".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Karplus-Strong plucked strings: fractional delay loop, pick comb, body resonance. Strings ring through note-off.".as_ptr(),
    n_params: 6,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    note_on,
    note_off,
    all_off,
    cc,
    press,
    process,
};

#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 {
    &DESC
}
