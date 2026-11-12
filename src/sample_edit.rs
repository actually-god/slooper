//! v50 R1: sample-editing engine — the FL Studio / Ableton "sample
//! properties" feature set, rendered OFFLINE into a new buffer.
//!
//! Research summary (FL Studio sample controller, Ableton Simpler/clip):
//! * FL: reverse, per-asset pitch (coarse + fine cents), pitch-bend
//!   envelope, fade-in/out curves (Lin/Sin/Exp/Log/Sq/Sqrt/Flat), and a
//!   hardware-stretch control (time stretch preserving pitch).
//! * Ableton: reverse, fades with curve types (Sine/Log/Lin/Exp), pitch
//!   in semitones+cents, a pitch envelope (bend), and warp-free
//!   stretch/compress.
//! Pipeline order (documented + pinned by tests):
//!   stretch -> pitch/bend -> reverse -> ping-pong -> fades.
//! Stretch first so pitch sees a stable length; fades last so they always
//! hug the audible head/tail regardless of what came before.

use serde::{Deserialize, Serialize};

/// Fade-curve shapes (FL Studio's fade dropdown plus the common extras).
/// `f(t)` maps t in [0,1] to gain, always 0 -> 1 for fade-in; fade-out
/// mirrors it (`1 - f` of the reversed axis is applied by the caller via
/// the same curve evaluated on the tail index).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Curve {
    /// Straight line.
    Linear,
    /// Sine — Ableton's default (smooth at both ends).
    Sin,
    /// Exponential — slow start (FL "Exp").
    Exp,
    /// Logarithmic — fast start (FL "Log").
    Log,
    /// Squared (gentle quadratic).
    Sq,
    /// Square-root (front-loaded).
    Sqrt,
    /// Full instantly (gain 1 everywhere).
    Flat,
}

impl Curve {
    pub const ALL: [Curve; 7] = [
        Curve::Linear,
        Curve::Sin,
        Curve::Exp,
        Curve::Log,
        Curve::Sq,
        Curve::Sqrt,
        Curve::Flat,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Curve::Linear => "linear",
            Curve::Sin => "sine",
            Curve::Exp => "exp",
            Curve::Log => "log",
            Curve::Sq => "sq",
            Curve::Sqrt => "sqrt",
            Curve::Flat => "flat",
        }
    }
    /// Gain at t in [0,1]: 0 = silent, 1 = full.
    pub fn gain(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Curve::Linear => t,
            Curve::Sin => (t * std::f32::consts::FRAC_PI_2).sin(),
            Curve::Exp => t * t * t,
            Curve::Log => t.sqrt(),
            Curve::Sq => t * t,
            Curve::Sqrt => t.powf(0.75),
            Curve::Flat => 1.0,
        }
    }
}

/// Per-asset sample settings; stored in the project keyed by asset path.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct SampleEdit {
    pub reverse: bool,
    /// Append the reversed copy after the forward one (FL ping-loop).
    pub pingpong: bool,
    pub pitch_cents: f32,
    /// Linear pitch-bend sweep added on top of `pitch_cents`, evolving
    /// from start to end across the clip (Ableton pitch envelope).
    pub bend_start_cents: f32,
    pub bend_end_cents: f32,
    /// Output length multiplier. 2.0 = twice as long.
    pub stretch: f32,
    /// true: WSOLA time stretch (pitch preserved, FL hardware-stretch
    /// style). false: plain resample (length and pitch ride together).
    pub preserve_pitch: bool,
    pub fade_in_ms: f32,
    pub fade_in: Curve,
    pub fade_out_ms: f32,
    pub fade_out: Curve,
}

impl Default for SampleEdit {
    fn default() -> Self {
        Self {
            reverse: false,
            pingpong: false,
            pitch_cents: 0.0,
            bend_start_cents: 0.0,
            bend_end_cents: 0.0,
            stretch: 1.0,
            preserve_pitch: true,
            fade_in_ms: 0.0,
            fade_in: Curve::Sin,
            fade_out_ms: 0.0,
            fade_out: Curve::Sin,
        }
    }
}

impl SampleEdit {
    /// True when the edit would return the buffer untouched.
    pub fn is_identity(&self) -> bool {
        *self == SampleEdit::default()
    }
}

fn cubic_at(x: &[f32], pos: f64) -> f32 {
    // Catmull-Rom-ish cubic interpolation with edge clamping.
    let i = pos.floor();
    let f = (pos - i) as f32;
    let i = i as usize;
    let g = |k: isize| x[(k.max(0) as usize).min(x.len() - 1)];
    let (a, b, c, d) = (
        g(i as isize - 1),
        g(i as isize),
        g(i as isize + 1),
        g(i as isize + 2),
    );
    let p = -0.5 * a + 1.5 * b - 1.5 * c + 0.5 * d;
    let q = a - 2.5 * b + 2.0 * c - 0.5 * d;
    let r = -0.5 * a + 0.5 * c;
    ((p * f + q) * f + r) * f + b
}

/// Fractional-index resample to EXACTLY `out_len` samples (linear ramp on
/// the read position, cubic on values).
fn resample_to(x: &[f32], out_len: usize) -> Vec<f32> {
    if x.is_empty() || out_len == 0 {
        return vec![0.0; out_len];
    }
    if x.len() == 1 {
        return vec![x[0]; out_len];
    }
    let ratio = (x.len() - 1) as f64 / out_len.max(2) as f64;
    (0..out_len)
        .map(|k| cubic_at(x, k as f64 * ratio))
        .collect()
}

/// Classic WSOLA: Hann-windowed overlap-add with best-continuity segment
/// search (no FFT). `factor` = output length / input length.
fn wsola(x: &[f32], factor: f64) -> Vec<f32> {
    let n = x.len();
    let w = n.min(2048) & !1; // window size (even)
    if w < 64 || (factor - 1.0).abs() < 1e-3 {
        return x.to_vec();
    }
    let h = w / 2; // synthesis hop
    let hann: Vec<f32> = (0..w)
        .map(|k| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / (w - 1) as f32).cos())
        .collect();
    let target = (h as f64 / factor).max(1.0); // analysis hop for this factor
    let mut out = vec![0.0f32; (n as f64 * factor) as usize + 2 * h + 4];
    let mut s = 0usize; // synthesis cursor
    let mut a = 0usize; // analysis cursor
    let mut guard = 0usize;
    let search = h / 2; // candidate deviation for best continuity
    while a + w <= n && s + w < out.len() && guard < 100_000 {
        guard += 1;
        // Best-continuity: minimize the diff of the h/2 samples BEFORE
        // the previous frame's position against the h/2 samples before
        // each candidate (they overlap at the same synth point, so the
        // cheapest continuation is the right one).
        let base = a + target as usize;
        let mut best = base.min(n.saturating_sub(w));
        let mut best_err = f64::MAX;
        let lo = base.saturating_sub(search).max(h / 2);
        let hi = (base + search).min(n.saturating_sub(w));
        if hi > lo && a >= h / 2 {
            for cand in (lo..=hi).step_by(2) {
                let mut err = 0.0f64;
                for u in 0..(h / 2) {
                    let d = x[a - h / 2 + u] as f64 - x[cand - h / 2 + u] as f64;
                    err += d * d;
                }
                if err < best_err {
                    best_err = err;
                    best = cand;
                }
            }
        }
        for k in 0..w {
            out[s + k] += hann[k] * x[best + k];
        }
        // The next frame continues `target` input samples after the
        // chosen point (search re-centres on that each iteration).
        a = best;
        s += h;
    }
    out.truncate(s + w);
    if out.is_empty() {
        return vec![0.0];
    }
    out
}

/// Render `src` (mono, at `sr`) through `e`. Deterministic; identity
/// edits clone. See module docs for the pipeline order.
pub fn render(src: &[f32], sr: u32, e: &SampleEdit) -> Vec<f32> {
    if src.is_empty() || e.is_identity() {
        return src.to_vec();
    }
    let sr = sr.max(1) as f64;
    let mut buf: Vec<f32> = src.to_vec();

    // 1) stretch.
    let st = if e.stretch.is_finite() {
        e.stretch.clamp(0.05, 20.0)
    } else {
        1.0
    };
    if (st - 1.0).abs() > 1e-3 {
        buf = if e.preserve_pitch {
            wsola(&buf, st as f64)
        } else {
            resample_to(&buf, ((buf.len() as f64) * st as f64).max(1.0) as usize)
        };
    }

    // 2) pitch + linear bend sweep. FL sample-window semantics: pitch is
    // a PLAYBACK-RATE change, so the buffer length follows the rate
    // (one octave up = half as long). The bend ramp spans the output.
    let p0 = e.pitch_cents + e.bend_start_cents;
    let p1 = e.pitch_cents + e.bend_end_cents;
    let bent = p0.abs() > 0.01 || p1.abs() > 0.01;
    if bent && buf.len() > 2 {
        let n = buf.len();
        let avg = 0.5 * (p0 + p1) as f64;
        let exp_len = ((n as f64) / (2f64).powf(avg / 1200.0)).max(1.0) as usize;
        let mut out = Vec::with_capacity(exp_len + 8);
        let mut pos = 0.0f64;
        let mut k = 0usize;
        while pos + 2.0 < n as f64 && k <= exp_len * 2 + 8 {
            let tt = (k as f64 / exp_len.max(1) as f64).clamp(0.0, 1.0);
            let cents = p0 as f64 + (p1 - p0) as f64 * tt;
            out.push(cubic_at(&buf, pos));
            pos += (2f64).powf(cents / 1200.0);
            k += 1;
        }
        if out.is_empty() {
            out.push(buf[0]);
        }
        buf = out;
    }

    // 3) reverse.
    if e.reverse {
        buf.reverse();
    }

    // 4) ping-pong: forward then reversed.
    if e.pingpong && buf.len() > 1 {
        let mut pp = Vec::with_capacity(buf.len() * 2);
        pp.extend_from_slice(&buf);
        pp.extend(buf.iter().rev().copied());
        buf = pp;
    }

    // 5) fades.
    let ms2n = |ms: f32, len: usize| -> usize {
        if ms.is_finite() && ms > 0.0 {
            ((ms as f64 * sr / 1000.0) as usize).min(len)
        } else {
            0
        }
    };
    let n_in = ms2n(e.fade_in_ms, buf.len());
    for (k, v) in buf.iter_mut().enumerate().take(n_in) {
        *v *= e.fade_in.gain(k as f32 / n_in.max(1) as f32);
    }
    let n_out = ms2n(e.fade_out_ms, buf.len());
    let ntot = buf.len();
    for k in 0..n_out {
        let idx = ntot - 1 - k;
        buf[idx] *= e.fade_out.gain(k as f32 / n_out.max(1) as f32);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, freq_hz: f32, sr: u32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq_hz * i as f32 / sr as f32).sin())
            .collect()
    }
    fn zc_freq(x: &[f32], sr: u32) -> f32 {
        let mut z = 0.0f32;
        for w in x.windows(2) {
            if w[0] <= 0.0 && w[1] > 0.0 {
                z += 1.0;
            }
        }
        z / (x.len() as f32 / sr as f32)
    }

    #[test]
    fn identity_is_a_clone() {
        let s: Vec<f32> = vec![0.25, -0.1, 0.8, 0.0];
        // sr 0 -> render keeps the guard path.
        assert_eq!(render(&s, 0, &SampleEdit::default()), s);
    }

    #[test]
    fn reverse_flips() {
        let s: Vec<f32> = (0..64).map(|i| i as f32).collect();
        let o = render(&s, 48000, &SampleEdit { reverse: true, ..Default::default() });
        assert_eq!(o.first(), Some(&63.0));
        assert_eq!(o.last(), Some(&0.0));
    }

    #[test]
    fn pingpong_doubles_and_mirrors() {
        let s: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let o = render(&s, 48000, &SampleEdit { pingpong: true, ..Default::default() });
        assert_eq!(o.len(), 200);
        assert_eq!(o[100], 99.0);
        assert_eq!(o[199], 0.0);
        assert_eq!(&o[..100], &s[..]);
    }

    #[test]
    fn fade_in_curves_have_expected_shapes() {
        let s = vec![1.0f32; 4800]; // 100 ms @ 48k
        for c in Curve::ALL {
            let e = SampleEdit {
                fade_in_ms: 100.0,
                fade_in: c,
                ..Default::default()
            };
            let o = render(&s, 48000, &e);
            assert!(o[0].abs() < 1e-6 || c == Curve::Flat, "{c:?} head silent");
            assert!(o[s.len() - 1] > 0.99, "{c:?} tail ~full");
            assert!(o[2400] > 0.0 && o[2400] <= 1.0, "{c:?} mid in range");
        }
        // linear mid is exactly half.
        let o = render(
            &s,
            48000,
            &SampleEdit { fade_in_ms: 100.0, fade_in: Curve::Linear, ..Default::default() },
        );
        assert!((o[2400] - 0.5).abs() < 0.01);
    }

    #[test]
    fn fade_out_silences_the_tail() {
        let s = vec![1.0f32; 4800];
        let o = render(
            &s,
            48000,
            &SampleEdit { fade_out_ms: 50.0, fade_out: Curve::Linear, ..Default::default() },
        );
        assert!(o[4799].abs() < 0.01);
        assert!((o[4799 - 2400] - 1.0).abs() < 1e-6, "before the fade: full");
    }

    #[test]
    fn pitch_up_octave_doubles_zero_crossings() {
        let s = sine(48000, 220.0, 48000);
        let o = render(
            &s,
            48000,
            &SampleEdit { pitch_cents: 1200.0, ..Default::default() },
        );
        let f = zc_freq(&o, 48000);
        assert!((f - 440.0).abs() < 12.0, "got {f}");
    }

    #[test]
    fn pitch_bend_sweeps_upward() {
        let s = sine(48000, 200.0, 48000);
        let o = render(
            &s,
            48000,
            &SampleEdit {
                bend_start_cents: 0.0,
                bend_end_cents: 1200.0,
                ..Default::default()
            },
        );
        let q = o.len() / 4;
        let head = zc_freq(&o[..q], 48000);
        let tail = zc_freq(&o[3 * q..], 48000);
        assert!(tail > head * 1.8, "sweep up: {head} -> {tail}");
    }

    #[test]
    fn stretch_compress_changes_length() {
        let s: Vec<f32> = sine(48000, 200.0, 48000)
            .into_iter()
            // add noise so WSOLA has real material to align on
            .map(|v| v + 0.3 * (v * 913.7).sin())
            .collect();
        let e = SampleEdit { stretch: 1.6, preserve_pitch: true, ..Default::default() };
        let o = render(&s, 48000, &e);
        let ratio = o.len() as f32 / s.len() as f32;
        assert!((ratio - 1.6).abs() < 0.25, "stretch ratio {ratio}");
    }

    #[test]
    fn stretch_no_pitch_preserve_acts_as_resample() {
        let s = sine(48000, 300.0, 48000);
        let o = render(
            &s,
            48000,
            &SampleEdit { stretch: 2.0, preserve_pitch: false, ..Default::default() },
        );
        assert!((o.len() as f32 / s.len() as f32 - 2.0).abs() < 0.02);
        assert!((zc_freq(&o, 48000) - 150.0).abs() < 6.0, "pitch halves");
    }

    #[test]
    fn pipeline_order_is_stable() {
        // reverse + pingpong + fades: fades hug the FINAL head/tail.
        let s = vec![1.0f32; 2400];
        let e = SampleEdit {
            reverse: true,
            pingpong: true,
            fade_in_ms: 100.0,
            fade_in: Curve::Linear,
            fade_out_ms: 100.0,
            fade_out: Curve::Linear,
            ..Default::default()
        };
        let o = render(&s, 48000, &e);
        assert_eq!(o.len(), 4800);
        assert!(o[0].abs() < 1e-6);
        assert!(o[4799].abs() < 1e-6);
    }
}
