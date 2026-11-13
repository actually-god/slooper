// ============================================================ slooper ==
// pitchshft.rs — pitch shifter, chromatic corrector and tuner meter.
//
// Three modes on one engine:
//   0 SHIFT    — fixed interval shift (semitones + fine cents).
//   1 CORRECT  — an internal YIN pitch detector measures the input and
//                nudges it toward the nearest chromatic semitone (the
//                Pitcher task); strength blends between raw and corrected.
//   2 TUNE     — pass-through audio, detector + meter only (the tuner).
//
// The shift core is the classic dual delay-line modulator: two delay
// taps a half LFO period apart cross-faded sin^2/cos^2 so the sum stays
// unity while each tap's read position slides at (ratio-1) samples per
// sample. A YIN detector (decimated x8, so it costs ~8 MAC/sample)
// supplies the correction target and the GUI atomics. As in any
// pitch shifter, monophonic sources track best.
//
// ABI v3 (tier 5b): TWO delay lines, ONE modulation clock. The R rail
// reads the same saw phase `spread × 0.5` periods further along (one
// clock, two read heads — drift impossible, phaser precedent), through
// its own delay line and tone LP. The detector tracks the MID, so
// identical inputs measure bit-identically to the tier-3 mono circuit;
// hard-panned material never crosses rails (each rail shifts its own
// signal). spread 0 folds the rails bit-exactly.
//
// Params:
//   mode        — 0 shift / 1 correct / 2 tune-only (quantised in process)
//   semitones   — manual shift, -24..24 (used by mode 0, added in 1)
//   fine cents  — manual detune, -100..100
//   strength    — correction depth toward the snapped pitch (mode 1)
//   slew ms     — glide time the shift ratio takes (vibrato-free at 1+)
//   tone        — pre LP 0 dark (600 Hz)..1 open (18 kHz), anti-alias
//   wet         — 0 dry .. 1 shifted only
//   gain        — output trim
//   ref Hz      — A4 reference for detection/snapping (435..445)
//   gate dB     — detector floor; below it nothing tracks (-70..-20)
//   spread      — R-rail modulation-phase offset (x0.5 periods)
// ---------------------------------------------------------------------------

include!("effect_api.rs");

const NPARAMS: usize = 11;
const P_MODE: usize = 0;
const P_SEMI: usize = 1;
const P_FINE: usize = 2;
const P_STR: usize = 3;
const P_SLEW: usize = 4;
const P_TONE: usize = 5;
const P_WET: usize = 6;
const P_GAIN: usize = 7;
const P_REF: usize = 8;
const P_GATE: usize = 9;
const P_SPREAD: usize = 10;

// --- shift core ---
const MOD_BITS: usize = 14; // 16384-sample mod delay (0.34 s at 48k)
const MOD_SZ: usize = 1 << MOD_BITS;
const MOD_MASK: usize = MOD_SZ - 1;
const MOD_D: usize = 6000; // saw amplitude: max delay travel in samples

// --- YIN detector (decimated by 8) ---
const DEC: usize = 8;
const DET_SZ: usize = 2048; // detector ring (decimated samples)
const DET_MASK: usize = DET_SZ - 1;
const YIN_W: usize = 256; // integration window (~43 ms at 6 kHz)
const YIN_MIN: usize = 6; // 1000 Hz at 6 kHz
const YIN_MAX: usize = 150; // 40 Hz at 6 kHz

use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// One-pole lowpass, NaN-guarded (feeds the detector and the shifter).
#[derive(Clone, Copy)]
struct Lp {
    y: f64,
    a: f64,
}
impl Lp {
    fn new(a: f64) -> Lp {
        Lp { y: 0.0, a }
    }
    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        self.y += (x - self.y) * self.a;
        if !self.y.is_finite() {
            self.y = 0.0;
        }
        self.y
    }
}

/// Linear-read delay line, pow2 wrap.
struct Dl {
    b: Vec<f32>,
    wp: usize,
}
impl Dl {
    fn new(bits: usize) -> Dl {
        Dl {
            b: vec![0.0f32; 1 << bits],
            wp: 0,
        }
    }
    #[inline]
    fn push(&mut self, x: f32, mask: usize) {
        self.b[self.wp & mask] = x;
        self.wp = self.wp.wrapping_add(1);
    }
    /// Read at fractional delay `d` samples behind the write head.
    #[inline]
    fn read(&self, d: f64, mask: usize) -> f32 {
        let f = self.wp as f64 - d;
        let i = f.floor();
        let fr = (f - i) as f32;
        let i0 = (i as usize) & mask;
        let i1 = i0.wrapping_add(1) & mask;
        self.b[i0] * (1.0 - fr) + self.b[i1] * fr
    }
}

struct St {
    sr: f64,
    modl: Dl,
    modr: Dl,
    phi: f64,
    lp1: Lp,
    lp2: Lp,
    lp1r: Lp,
    lp2r: Lp,
    // --- detector ---
    det: Vec<f32>, // decimated ring
    dwp: usize,
    acc: f64, // boxcar accumulator for the x8 decimator
    dci: usize,
    rms: f64,
    cents_d: f64,  // smoothed detected cents error (vs nearest semitone)
    conf_d: f64,   // smoothed confidence
    hz_d: f64,     // smoothed detected frequency (for the meter)
    shift_cents: f64, // smoothed applied auto shift
    // GUI atomics (f32 bits): cents error, confidence, Hz.
    g_cents: AtomicU32,
    g_conf: AtomicU32,
    g_hz: AtomicU32,
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate <= 0.0 { 48000.0 } else { sample_rate };
    // ~1.1 kHz two-stage pre-LP for the detector chain (true-Hz pole).
    let a = 1.0 - (-core::f64::consts::TAU * 1100.0 / sr).exp();
    Box::into_raw(Box::new(St {
        sr,
        modl: Dl::new(MOD_BITS),
        modr: Dl::new(MOD_BITS),
        phi: 0.0,
        lp1: Lp::new(a),
        lp2: Lp::new(a),
        lp1r: Lp::new(a),
        lp2r: Lp::new(a),
        det: vec![0.0f32; DET_SZ],
        dwp: 0,
        acc: 0.0,
        dci: 0,
        rms: 0.0,
        cents_d: 0.0,
        conf_d: 0.0,
        hz_d: 0.0,
        shift_cents: 0.0,
        g_cents: AtomicU32::new(0),
        g_conf: AtomicU32::new(0),
        g_hz: AtomicU32::new(0),
    })) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut St) });
    }
}

/// YIN difference + cumulative-mean search on the decimated ring.
/// Returns (cents error vs nearest semitone, confidence, Hz) at `refa`.
/// All stack arrays — safe for the audio thread.
fn yin(det: &[f32], dwp: usize, sr_d: f64, refa: f64) -> (f64, f64, f64) {
    let mut d = [0.0f32; YIN_MAX + 2];
    let mut cm = [0.0f32; YIN_MAX + 2];
    let get = |i: usize| -> f32 {
        det[(dwp.wrapping_sub(YIN_W.wrapping_add(YIN_MAX)).wrapping_add(i)) & DET_MASK]
    };
    for tau in YIN_MIN..=YIN_MAX {
        let mut s = 0.0f64;
        for j in 0..YIN_W {
            let a = get(j) as f64;
            let b = get(j + tau) as f64;
            let e = a - b;
            s += e * e;
        }
        d[tau] = s as f32;
    }
    if !d[YIN_MIN].is_finite() {
        return (0.0, 0.0, 0.0);
    }
    let mut running = 0.0f32;
    for tau in 1..=YIN_MAX {
        running += d[tau];
        cm[tau] = if running > 0.0 {
            d[tau] * (tau as f32) / running
        } else {
            1.0
        };
    }
    // First dip under the threshold; else the global minimum.
    let mut best = YIN_MAX;
    let mut t = YIN_MIN;
    while t <= YIN_MAX {
        if cm[t] < cm[t - 1] && t < YIN_MAX && cm[t + 1] >= cm[t] {
            if cm[t] < 0.25 {
                best = t;
                break;
            } else if best == YIN_MAX || cm[t] < cm[best] {
                best = t;
            }
        }
        t += 1;
    }
    let conf = (1.0 - cm[best].min(1.0)).max(0.0) as f64;
    // Parabolic vertex around the dip for sub-sample tau.
    let a = cm[best.saturating_sub(1).max(YIN_MIN)] as f64;
    let b = cm[best] as f64;
    let c = cm[(best + 1).min(YIN_MAX)] as f64;
    let denom = a + c - 2.0 * b;
    let mut tau = best as f64;
    if denom.abs() > 1e-12 {
        tau += 0.5 * (a - c) / denom;
    }
    if !(1.0..=1e9).contains(&tau) {
        return (0.0, 0.0, 0.0);
    }
    let f = sr_d / tau;
    if !(25.0..=10000.0).contains(&f) || !f.is_finite() {
        return (0.0, 0.0, 0.0);
    }
    let st = 12.0 * (f / refa).log2();
    let err = (st - st.round()) * 100.0; // cents from nearest semitone
    (err, conf, f)
}

/// v1 entry: mono — LEFT of the pair is the tier-3 shifter.
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    let mut or = input;
    process_st(state, input, input, params, 0.0, &mut or)
}

/// v2 entry: mono bus — emit the mid of the two shifters.
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

/// The v3 stereo step: two delay lines, ONE modulation clock (the R
/// rail reads `spread × 0.5` periods further along), per-rail tone LP,
/// a mid-driven detector shared by both rails.
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

    let mode = (unsafe { *params.add(P_MODE) } as i32).clamp(0, 2);
    let semi = unsafe { *params.add(P_SEMI) }.clamp(-24.0, 24.0) as f64;
    let fine = unsafe { *params.add(P_FINE) }.clamp(-100.0, 100.0) as f64;
    let strength = unsafe { *params.add(P_STR) }.clamp(0.0, 1.0) as f64;
    let slew = (unsafe { *params.add(P_SLEW) }.clamp(1.0, 300.0) / 1000.0) as f64;
    let tone = unsafe { *params.add(P_TONE) }.clamp(0.0, 1.0) as f64;
    let wet = unsafe { *params.add(P_WET) }.clamp(0.0, 1.0) as f64;
    let gain = unsafe { *params.add(P_GAIN) }.clamp(0.0, 2.0) as f64;
    let refa = unsafe { *params.add(P_REF) }.clamp(435.0, 445.0) as f64;
    let gate_db = unsafe { *params.add(P_GATE) }.clamp(-70.0, -20.0) as f64;
    let spread = unsafe { *params.add(P_SPREAD) }.clamp(0.0, 1.0) as f64;

    let dry_l = if l.is_finite() { l } else { 0.0 };
    let dry_r = if r.is_finite() { r } else { 0.0 };
    // Detector/RMS ride the MID: identical inputs == the mono circuit.
    let dry_m = (dry_l + dry_r) * 0.5;

    // --- tone LP (cutoff follows the `tone` slider, 600 Hz..18 kHz) ---
    let fc = 600.0f64 * (18000.0f64 / 600.0).powf(tone);
    // True-Hz one-pole step (tier rule: a = 1 - exp(-2*pi*fc/sr)).
    let a = 1.0 - (-core::f64::consts::TAU * fc / sr).exp();
    s.lp1.a = a;
    s.lp2.a = a;
    s.lp1r.a = a;
    s.lp2r.a = a;
    let x_l = s.lp2.run(s.lp1.run(dry_l as f64)) as f32;
    let x_r = s.lp2r.run(s.lp1r.run(dry_r as f64)) as f32;
    let x_m = (x_l + x_r) * 0.5;

    // --- detector feed: decimate x8 by boxcar mean (null at the Nyquist
    // of the decimated rate — cheap anti-alias that kills the worst
    // folding band), hop = YIN_W/2 decimated samples ---
    s.rms = s.rms * 0.999 + (dry_m as f64) * (dry_m as f64) * 0.001;
    s.acc += x_m as f64;
    s.dci += 1;
    if s.dci == DEC {
        s.dci = 0;
        s.det[s.dwp & DET_MASK] = (s.acc / DEC as f64) as f32;
        s.acc = 0.0;
        s.dwp = s.dwp.wrapping_add(1);
    }
    let hop = YIN_W / 2;
    if s.dwp > 0 && s.dwp % hop == 0 {
        let sr_d = sr / DEC as f64;
        let thr = (10.0f64).powf(gate_db / 20.0);
        let lvl_ok = s.rms > thr * thr * 0.25;
        let (err, conf, f) = yin(&s.det, s.dwp, sr_d, refa);
        let conf = if lvl_ok && conf > 0.35 { conf } else { 0.0 };
        // Smooth detector outputs (meter + control).
        s.cents_d += (err - s.cents_d) * 0.35;
        s.conf_d += (conf - s.conf_d) * 0.2;
        s.hz_d += (f - s.hz_d) * 0.3;
        // Auto correction target.
        let target = if mode == 1 && conf > 0.0 {
            (-s.cents_d * strength).clamp(-1200.0, 1200.0) + semi * 100.0 + fine
        } else {
            semi * 100.0 + fine
        };
        let k = 1.0 - (-(hop as f64 * DEC as f64 / sr / slew)).exp();
        s.shift_cents += (target - s.shift_cents) * k;
        s.g_cents.store((s.cents_d as f32).to_bits(), Relaxed);
        s.g_conf.store((s.conf_d as f32).to_bits(), Relaxed);
        s.g_hz.store((s.hz_d as f32).to_bits(), Relaxed);
    }

    // --- dual delay-line modulation shift (per rail) ---
    s.modl.push(x_l, MOD_MASK);
    s.modr.push(x_r, MOD_MASK);
    let cents = if mode == 2 { 0.0 } else { s.shift_cents };
    let ratio = (2.0f64).powf(cents / 1200.0);
    let smag = (ratio - 1.0).abs();
    // Saw travel per period: delay must move `smag` samples per sample,
    // so total travel = smag*sr/fmod. Mod rate grows with |shift| so the
    // crossfade whine stays above audibility for big moves.
    let fmod = (smag * sr / MOD_D as f64).clamp(4.0, 500.0);
    let travel = (smag * sr / fmod).clamp(0.5, MOD_D as f64 - 4.0);
    s.phi += fmod / sr;
    if s.phi >= 1.0 {
        s.phi -= 1.0;
    }
    let wet_s = if mode == 2 { 0.0 } else { wet };
    let off_r = 0.5 * spread;
    let out_l;
    let out_rv;
    if wet_s <= 0.001 {
        out_l = dry_l;
        out_rv = dry_r;
    } else {
        // Up-shift walks the delay DOWN (read chases write), down-shift
        // walks it UP; the two taps share the ramp half a period apart.
        let dir = if ratio < 1.0 { 1.0 } else { -1.0 };
        let mut acc_l = 0.0f32;
        let mut acc_r = 0.0f32;
        for tap in 0..2usize {
            let p = (s.phi + 0.5 * tap as f64) % 1.0;
            let win = (core::f64::consts::PI * p).sin();
            let win = (win * win) as f32;
            let d = MOD_D as f64 + dir * travel * (p - 0.5);
            let d = d.max(1.5).min(MOD_SZ as f64 - 2.0);
            acc_l += win * s.modl.read(d, MOD_MASK);
            // R rail: same clock, read head offset — unity crossfade
            // per side is preserved (the window is re-derived from the
            // offset phase, not reused).
            let pr = (p + off_r) % 1.0;
            let winr = (core::f64::consts::PI * pr).sin();
            let winr = (winr * winr) as f32;
            let dr = MOD_D as f64 + dir * travel * (pr - 0.5);
            let dr = dr.max(1.5).min(MOD_SZ as f64 - 2.0);
            acc_r += winr * s.modr.read(dr, MOD_MASK);
        }
        let w = wet_s as f32;
        out_l = dry_l * (1.0 - w) + acc_l * w;
        out_rv = dry_r * (1.0 - w) + acc_r * w;
    }
    let yl = out_l * gain as f32;
    let yr = out_rv * gain as f32;
    if yl.is_finite() && yr.is_finite() {
        unsafe { *out_r = yr };
        yl
    } else {
        // NaN recovery: flush both delay lines in place (no RT alloc).
        s.modl.b.iter_mut().for_each(|v| *v = 0.0);
        s.modr.b.iter_mut().for_each(|v| *v = 0.0);
        unsafe { *out_r = 0.0 };
        0.0
    }
}

// ===================== scripted editor: tuner meter ========================

/// GUI state: a tiny heap marker — the meter reads the audio-state atomics
/// directly. (gui_create MUST return non-null or the host falls back to
/// the auto slider window.)
struct Gs;

extern "C" fn gui_create(_sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Gs)) as *mut core::ffi::c_void
}

extern "C" fn gui_destroy(gs: *mut core::ffi::c_void) {
    if !gs.is_null() {
        drop(unsafe { Box::from_raw(gs as *mut Gs) });
    }
}

const NAMES: [&str; 12] =
    ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

extern "C" fn gui_draw(
    gs: *mut core::ffi::c_void,
    state: *mut core::ffi::c_void,
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

    // Meter background.
    (api.rect_filled)(c, 0.0, 0.0, w, h, 0.09, 0.10, 0.13, 1.0);
    let mh = 84.0;
    let cx = w * 0.5;
    let y0 = mh + 36.0;

    // Read detector atomics.
    let (mut cents, mut conf, mut hz) = (0.0f32, 0.0f32, 0.0f32);
    if !state.is_null() {
        let st = unsafe { &*(state as *const St) };
        cents = f32::from_bits(st.g_cents.load(Relaxed));
        conf = f32::from_bits(st.g_conf.load(Relaxed));
        hz = f32::from_bits(st.g_hz.load(Relaxed));
    }

    // Cents ruler: -50 .. +50.
    let scale = (w - 40.0) / 100.0;
    let mut t: i32 = -50;
    while t <= 50 {
        let x = cx + t as f32 * scale;
        let big = t % 10 == 0;
        (api.line)(
            c,
            x,
            mh - if big { 18.0 } else { 12.0 },
            x,
            mh - 6.0,
            1.0,
            if big { 0.5 } else { 0.3 },
            0.55,
            0.6,
            0.9,
        );
        t += 5;
    }
    // Centre line.
    (api.line)(c, cx, 10.0, cx, mh - 4.0, 1.5, 0.45, 0.5, 0.6, 1.0);
    // Needle.
    if conf > 0.05 {
        let cl = cents.clamp(-55.0, 55.0);
        let nx = cx + cl * scale;
        let near = cents.abs() < 5.0;
        let (r, g2, b) = if near { (0.35, 0.95, 0.55) } else { (1.0, 0.62, 0.25) };
        (api.line)(c, nx, 8.0, nx, mh - 2.0, 2.5, r, g2, b, 1.0);
        // Note name + readout.
        let refa = (api.get_param)(c, P_REF as u32) as f64;
        if hz > 25.0 {
            let st = 12.0 * (hz as f64 / refa).log2();
            let midi = st.round() as i32 + 69;
            let name = NAMES[((midi % 12) + 12) as usize % 12];
            let oct = midi.div_euclid(12) - 1;
            let s = std::ffi::CString::new(format!(
                "{}{}  {:+.1} cents",
                name,
                oct,
                (st - st.round()) * 100.0
            ))
            .unwrap_or_default();
            (api.text)(c, cx, mh + 16.0, 17.0, 1, 0.9, 0.93, 0.97, 1.0, s.as_ptr());
        }
    } else {
        let s = c"no pitch";
        (api.text)(c, cx, mh + 16.0, 15.0, 1, 0.5, 0.55, 0.62, 1.0, s.as_ptr());
    }

    // Slider block: 2 columns x 5 rows under the meter.
    let colw = (w - 48.0) * 0.5;
    let rh = 34.0;
    macro_rules! sl {
        ($k:expr, $col:expr, $row:expr, $min:expr, $max:expr, $lab:expr, $curve:expr) => {{
            let x = 16.0 + $col as f32 * (colw + 16.0);
            let y = y0 + $row as f32 * rh;
            let v = (api.get_param)(c, $k);
            let mut nv = v;
            if (api.slider)(c, x, y, colw, rh - 8.0, $lab, &mut nv, $min, $max, $curve) != 0 {
                (api.set_param)(c, $k, nv);
            }
        }};
    }
    sl!(0, 0, 0, 0.0, 2.0, c"mode 0shift 1corr 2tune".as_ptr(), 1.0);
    sl!(3, 1, 0, 0.0, 1.0, c"strength".as_ptr(), 1.0);
    sl!(1, 0, 1, -24.0, 24.0, c"semitones".as_ptr(), 1.0);
    sl!(4, 1, 1, 1.0, 300.0, c"slew ms".as_ptr(), 0.5);
    sl!(2, 0, 2, -100.0, 100.0, c"fine cents".as_ptr(), 1.0);
    sl!(5, 1, 2, 0.0, 1.0, c"tone".as_ptr(), 1.0);
    sl!(6, 0, 3, 0.0, 1.0, c"wet".as_ptr(), 1.0);
    sl!(7, 1, 3, 0.0, 2.0, c"gain".as_ptr(), 1.0);
    sl!(8, 0, 4, 435.0, 445.0, c"ref Hz".as_ptr(), 1.0);
    sl!(9, 1, 4, -70.0, -20.0, c"gate dB".as_ptr(), 1.0);
    sl!(10, 1, 5, 0.0, 1.0, c"spread (R phase)".as_ptr(), 1.0);
}

pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: SLOOPER_EFFECT_GUI_ABI,
    want_width: 430.0,
    want_height: 336.0,
    create: gui_create,
    draw: gui_draw,
    destroy: gui_destroy,
};

static PARAMS: [EffectParamV1; NPARAMS] = [
    EffectParamV1 { label: c"mode".as_ptr(), min: 0.0, max: 2.0, default: 0.0 },
    EffectParamV1 { label: c"semitones".as_ptr(), min: -24.0, max: 24.0, default: 0.0 },
    EffectParamV1 { label: c"fine cents".as_ptr(), min: -100.0, max: 100.0, default: 0.0 },
    EffectParamV1 { label: c"strength".as_ptr(), min: 0.0, max: 1.0, default: 0.7 },
    EffectParamV1 { label: c"slew ms".as_ptr(), min: 1.0, max: 300.0, default: 15.0 },
    EffectParamV1 { label: c"tone".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
    EffectParamV1 { label: c"wet".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"ref Hz".as_ptr(), min: 435.0, max: 445.0, default: 440.0 },
    EffectParamV1 { label: c"gate dB".as_ptr(), min: -70.0, max: -20.0, default: -55.0 },
    EffectParamV1 { label: c"spread (R phase x180deg)".as_ptr(), min: 0.0, max: 1.0, default: 0.5 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"pitch shifter".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Pitch shifter / chromatic corrector / tuner meter (Pitch Shifter, Pitcher and Tuner task). Internal YIN detector; mono sources track best. ABI v3: dual delay lines, R rail modulation-phase spread.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"pitch shifter".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Pitch shifter / chromatic corrector / tuner meter (Pitch Shifter, Pitcher and Tuner task). Internal YIN detector; mono sources track best. ABI v3: dual delay lines, R rail modulation-phase spread.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"pitch shifter".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Pitch shifter / chromatic corrector / tuner meter (Pitch Shifter, Pitcher and Tuner task). Internal YIN detector; mono sources track best. ABI v3: dual delay lines, R rail modulation-phase spread.".as_ptr(),
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
    process_st,
};

// ---- ABI v4 (tier 6): param metadata (mode picker + engineering units) ---
// True-stereo from tier 5b (declared here honestly); set_sync stays null.

#[repr(C)]
struct Labels<const N: usize>([*const core::ffi::c_char; N]);
// SAFETY: read-only statics of pointers to literals, shared one way.
unsafe impl<const N: usize> Sync for Labels<N> {}

static MODE_LABELS: Labels<3> =
    Labels([c"shift".as_ptr(), c"correct".as_ptr(), c"tune".as_ptr()]);

static META: [ParamMetaV1; NPARAMS] = [
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: MODE_LABELS.0.as_ptr(), n_labels: 3 },
    ParamMetaV1 { unit: SLOOPER_UNIT_SEMITONES, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: SLOOPER_UNIT_MS, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: SLOOPER_UNIT_HZ, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: SLOOPER_UNIT_DB, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
    ParamMetaV1 { unit: 0, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
];

pub static DESC4: EffectInfoV4 = EffectInfoV4 {
    abi: SLOOPER_EFFECT_ABI_V4,
    name: c"pitch shifter".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC3.description,
    n_params: NPARAMS as u32,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
    process_st,
    stereo: 1,
    set_sync: None,
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

#[no_mangle]
pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 {
    &GUI
}
