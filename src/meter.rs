//! v39: master-output metering — ITU-R BS.1770-4 loudness (LUFS), an
//! EBU Tech 3342 loudness range, sample peak/hold and a real-time
//! spectrum analyzer for the MASTER OUT panel.
//!
//! All of the math runs on the GUI thread; the realtime callback only
//! copies post-master-gain stereo samples into the lock-free [`MeterRing`].
//!
//! Standard notes (research 2026-11-04, EBU R 128 / ITU-R BS.1770-4):
//! - K-weighting is two biquad stages: a high shelf (fc 1681.9744509555319 Hz,
//!   +4 dB, Q 0.7071752369554193) then a high-pass (fc 38.13547087613982 Hz,
//!   Q 0.5003270373253953). Coefficients are derived from the RBJ-style
//!   formulas below; at 48 kHz they must match the spec constants
//!   (stage 1 b0 = 1.53512485958697, a1 = −1.69065929318241 — verified by
//!   unit test, matching the reference tables every conformant meter uses).
//! - Loudness in LUFS/LKFS = −0.691 + 10·log10(mean-square of the
//!   K-weighted signal); stereo = plain L+R power sum (both weights 1.0).
//! - Momentary = 400 ms window, Short-term = 3 s, both ungated.
//! - Integrated = mean over 400 ms gating blocks (100 ms hop) that pass
//!   the absolute gate (−70 LUFS) AND the relative gate (10 LU below the
//!   absolutely-gated mean). Silence therefore never drags I down.
//! - LRA (EBU Tech 3342): P95 − P10 of the short-term series sampled
//!   every 100 ms, after dropping values below −70 LUFS.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Offset of the K-weighted loudness scale (BS.1770-4 eq. 1).
const LUFS_OFFSET: f64 = -0.691;

#[derive(Clone, Copy)]
struct Biquad {
    a1: f32,
    a2: f32,
    b0: f32,
    b1: f32,
    b2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    #[inline(always)]
    fn run(&mut self, x0: f32) -> f32 {
        let y0 = self.b0 * x0 + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x0;
        self.y2 = self.y1;
        self.y1 = y0;
        y0
    }
}

/// Stage 1 of the BS.1770-4 K-weighting pre-filter (high shelf).
fn k_stage1(sr: f32) -> Biquad {
    let gain_db = 3.99984385397f64;
    let q = 0.7071752369554193f64;
    let fc = 1_681.974_450_955_532_f64;
    let k = (std::f64::consts::PI * fc / sr as f64).tan();
    let vh = 10.0f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.499666774155);
    let a0 = 1.0 + k / q + k * k;
    Biquad {
        b0: ((vh + vb * k / q + k * k) / a0) as f32,
        b1: (2.0 * (k * k - vh) / a0) as f32,
        b2: ((vh - vb * k / q + k * k) / a0) as f32,
        a1: (2.0 * (k * k - 1.0) / a0) as f32,
        a2: ((1.0 - k / q + k * k) / a0) as f32,
        x1: 0.0,
        x2: 0.0,
        y1: 0.0,
        y2: 0.0,
    }
}

/// Stage 2 of the BS.1770-4 K-weighting pre-filter (high-pass).
fn k_stage2(sr: f32) -> Biquad {
    let q = 0.5003270373253953f64;
    let fc = 38.13547087613982f64;
    let k = (std::f64::consts::PI * fc / sr as f64).tan();
    let a0 = 1.0 + k / q + k * k;
    Biquad {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: (2.0 * (k * k - 1.0) / a0) as f32,
        a2: ((1.0 - k / q + k * k) / a0) as f32,
        x1: 0.0,
        x2: 0.0,
        y1: 0.0,
        y2: 0.0,
    }
}

/// Mean-square power of one 100 ms window, L+R summed.
fn loudness_lufs(power: f64) -> f64 {
    if power <= 0.0 {
        f64::NEG_INFINITY
    } else {
        LUFS_OFFSET + 10.0 * power.log10()
    }
}
fn power_at_lufs(lkfs: f64) -> f64 {
    10.0f64.powf((lkfs - LUFS_OFFSET) / 10.0)
}

/// Streaming BS.1770-4 loudness meter fed with interleaved stereo audio.
///
/// Feed it every master-output sample (GUI thread); query momentary /
/// short-term / integrated / LRA at paint time. Window powers are kept
/// for the whole session (8 B per 100 ms — an hour is ~300 KB), so I and
/// LRA always recompute exactly over the non-gated set.
pub struct LufsMeter {
    s1: [Biquad; 2],
    s2: [Biquad; 2],
    /// Samples per 100 ms window at this rate.
    spd: usize,
    sum: f64,
    cnt: usize,
    /// Summed (L+R) mean-square power per finished 100 ms window.
    windows: Vec<f64>,
}

impl LufsMeter {
    pub fn new(sr: u32) -> Self {
        let sr = sr.max(1) as f32;
        Self {
            s1: [k_stage1(sr), k_stage1(sr)],
            s2: [k_stage2(sr), k_stage2(sr)],
            spd: ((sr as f64 * 0.1).round() as usize).max(1),
            sum: 0.0,
            cnt: 0,
            windows: Vec::new(),
        }
    }

    /// Feed an interleaved stereo block (may be any length).
    pub fn push_stereo(&mut self, data: &[f32]) {
        for pair in data.as_chunks::<2>().0 {
            let (l, r) = (pair[0], pair[1]);
            // Guard: plugins/feedback can hand the master non-finite values.
            let l = if l.is_finite() { l } else { 0.0 };
            let r = if r.is_finite() { r } else { 0.0 };
            let zl = self.s2[0].run(self.s1[0].run(l));
            let zr = self.s2[1].run(self.s1[1].run(r));
            self.sum += (zl * zl + zr * zr) as f64;
            self.cnt += 1;
            if self.cnt == self.spd {
                self.windows.push(self.sum / self.spd as f64);
                self.sum = 0.0;
                self.cnt = 0;
            }
        }
    }

    fn windows_mean(&self, n: usize) -> Option<f64> {
        if self.windows.len() < n || n == 0 {
            return None;
        }
        let tail = &self.windows[self.windows.len() - n..];
        Some(tail.iter().sum::<f64>() / n as f64)
    }

    /// Momentary loudness (400 ms window).
    pub fn momentary(&self) -> Option<f64> {
        self.windows_mean(4).map(loudness_lufs)
    }

    /// Short-term loudness (3 s window).
    pub fn short_term(&self) -> Option<f64> {
        self.windows_mean(30).map(loudness_lufs)
    }

    /// Number of finished 100 ms windows (for tests/telemetry).
    pub fn windows_done(&self) -> usize {
        self.windows.len()
    }

    /// Integrated loudness: mean over 400 ms gating blocks (100 ms hop)
    /// passing the −70 LUFS absolute gate and the −10 LU relative gate.
    pub fn integrated(&self) -> Option<f64> {
        if self.windows.len() < 4 {
            return None;
        }
        let abs_gate = power_at_lufs(-70.0);
        // 400 ms gating blocks, 100 ms hop.
        let blocks: Vec<f64> = self
            .windows
            .windows(4)
            .map(|w| 0.25 * w.iter().sum::<f64>())
            .filter(|p| *p > abs_gate)
            .collect();
        if blocks.is_empty() {
            return None;
        }
        let abs_mean = blocks.iter().sum::<f64>() / blocks.len() as f64;
        let rel_gate = power_at_lufs(loudness_lufs(abs_mean) - 10.0);
        let (mut sum, mut n) = (0.0f64, 0usize);
        for p in blocks {
            if p > rel_gate {
                sum += p;
                n += 1;
            }
        }
        if n == 0 {
            return None;
        }
        Some(loudness_lufs(sum / n as f64))
    }

    /// Loudness range (EBU Tech 3342): P95 − P10 of the gated short-term
    /// series. Returns None until at least 31 windows exist (1 s of ST
    /// history at 100 ms sampling).
    pub fn lra(&self) -> Option<f64> {
        if self.windows.len() < 31 {
            return None;
        }
        let floor = power_at_lufs(-70.0);
        let mut sts: Vec<f64> = Vec::with_capacity(self.windows.len() - 29);
        // ST at window i = mean of windows i-29..=i (30 windows = 3 s).
        let mut run: f64 = self.windows[..30].iter().sum();
        sts.push(run / 30.0);
        for i in 30..self.windows.len() {
            run += self.windows[i] - self.windows[i - 30];
            sts.push(run / 30.0);
        }
        sts.retain(|p| *p > floor);
        if sts.len() < 2 {
            return None;
        }
        sts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |q: f64| -> f64 {
            let f = q * (sts.len() - 1) as f64;
            let i = f as usize;
            if i + 1 < sts.len() {
                sts[i] + (f - i as f64) * (sts[i + 1] - sts[i])
            } else {
                sts[i]
            }
        };
        let (p10, p95) = (pct(0.10), pct(0.95));
        Some((loudness_lufs(p95) - loudness_lufs(p10)).max(0.0))
    }

    /// Zero the integrated/LRA history (Youlean-style I reset). The
    /// K-weighting state keeps running so the next window is seamless.
    pub fn reset_integrated(&mut self) {
        self.windows.clear();
        self.sum = 0.0;
        self.cnt = 0;
    }
}

/// Real-time spectrum analyzer: keeps the last `n` mono-summed samples,
/// applies Hann, and runs an in-place iterative radix-2 FFT on demand
/// (GUI thread, once per paint). Bin magnitudes are dBFS-referenced: a
/// full-scale sine at an exact bin reads 0 dB.
pub struct Spectrum {
    n: usize,
    /// Circular mono history (length n).
    circ: Vec<f32>,
    wpos: usize,
    filled: usize,
    hann: Vec<f32>,
    win_gain: f32,
    wcos: Vec<f32>,
    wsin: Vec<f32>,
    rev: Vec<u32>,
    re: Vec<f32>,
    im: Vec<f32>,
    /// n/2 bin magnitudes in dB, and their decaying peak holds.
    pub db: Vec<f32>,
    pub peak_db: Vec<f32>,
    pub sr: f32,
}

impl Spectrum {
    pub fn new(sr: u32) -> Self {
        // v49 R3: 8192-sample window (≈171 ms @48 kHz) — 4x finer
        // frequency resolution than the old 2048 (bin spacing ≈5.9 Hz
        // instead of ≈23.4 Hz), which is what makes low-frequency tones
        // and narrow cuts/gains readable on the log axis. One 8192-pt
        // FFT per UI frame is ~0.1 ms on this host's release build.
        let n = 8192usize;
        let hann: Vec<f32> = (0..n)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos())
            .collect();
        let win_gain = hann.iter().sum::<f32>();
        let log2n = n.trailing_zeros();
        let rev: Vec<u32> = (0..n as u32)
            .map(|i| i.reverse_bits() >> (32 - log2n))
            .collect();
        let (wcos, wsin): (Vec<f32>, Vec<f32>) = (0..n / 2)
            .map(|k| {
                let a = -2.0 * std::f32::consts::PI * k as f32 / n as f32;
                (a.cos(), a.sin())
            })
            .unzip();
        Self {
            n,
            circ: vec![0.0; n],
            wpos: 0,
            filled: 0,
            hann,
            win_gain,
            wcos,
            wsin,
            rev,
            re: vec![0.0; n],
            im: vec![0.0; n],
            db: vec![-240.0; n / 2],
            peak_db: vec![-240.0; n / 2],
            sr: sr.max(1) as f32,
        }
    }

    /// Feed interleaved stereo; keeps (L+R)/2 (Ableton-Spectrum "mono"
    /// input) as the analyzer signal.
    pub fn push(&mut self, data: &[f32]) {
        for pair in data.as_chunks::<2>().0 {
            let m = (pair[0] + pair[1]) * 0.5;
            self.circ[self.wpos] = m;
            self.wpos = (self.wpos + 1) % self.n;
            self.filled = (self.filled + 1).min(self.n);
        }
    }

    /// Refresh the magnitude display from the current history. Peaks
    /// decay `decay_db` per call (once per UI frame).
    pub fn refresh(&mut self, decay_db: f32) {
        if self.filled < self.n {
            return;
        }
        let n = self.n;
        // Oldest sample sits at wpos (the buffer is full).
        for i in 0..n {
            let s = self.circ[(self.wpos + i) % n];
            self.re[i] = s * self.hann[i];
            self.im[i] = 0.0;
        }
        self.fft();
        let norm = 2.0 / self.win_gain;
        for i in 0..n / 2 {
            let (re, im) = (self.re[i], self.im[i]);
            let mag = (re * re + im * im).sqrt() * norm;
            let db = if mag > 0.0 {
                (20.0 * mag.max(1e-12).log10()).max(-240.0)
            } else {
                -240.0
            };
            self.db[i] = db;
            let held = (self.peak_db[i] - decay_db).max(-240.0);
            self.peak_db[i] = db.max(held);
        }
    }

    fn fft(&mut self) {
        let n = self.n;
        for i in 0..n {
            let j = self.rev[i] as usize;
            if j > i {
                self.re.swap(i, j);
                self.im.swap(i, j);
            }
        }
        let mut size = 2usize;
        while size <= n {
            let half = size / 2;
            let step = n / size;
            let mut g = 0usize;
            while g < n {
                for k in 0..half {
                    let (cre, cim) = (self.wcos[k * step], self.wsin[k * step]);
                    let a = g + k;
                    let b = a + half;
                    let tr = self.re[b] * cre - self.im[b] * cim;
                    let ti = self.re[b] * cim + self.im[b] * cre;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
                g += size;
            }
            size <<= 1;
        }
    }

    /// Bin frequency in Hz (bin 0 is skipped by the painter).
    pub fn bin_hz(&self, bin: usize) -> f32 {
        bin as f32 * self.sr / self.n as f32
    }
    pub fn n_bins(&self) -> usize {
        self.db.len()
    }
}

/// Lock-free single-producer / single-consumer ring of interleaved
/// stereo master samples: the RT thread writes every block after master
/// gain, the GUI drains it once per frame. Overflow drops NEW samples
/// (the RT thread never blocks, waits, or re-reads a mutex); at ~1.4 s
/// capacity a stalled UI only ever loses meter resolution.
///
/// Storage is one `AtomicU32` per sample (f32 bit patterns), so there is
/// no unsafe aliasing anywhere.
pub struct MeterRing {
    buf: Vec<AtomicU32>,
    /// Frame capacity (power of two); positions below count FRAMES.
    cap: usize,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl Default for MeterRing {
    fn default() -> Self {
        Self::new()
    }
}

impl MeterRing {
    pub fn new() -> Self {
        let cap = 1usize << 16; // frames (65536 ≈ 1.37 s @48 kHz)
        let mut buf = Vec::with_capacity(cap * 2);
        for _ in 0..cap * 2 {
            buf.push(AtomicU32::new(0.0f32.to_bits()));
        }
        Self {
            buf,
            cap,
            mask: cap - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Producer (RT thread): write an interleaved stereo block. Returns
    /// the frame count actually stored (may drop the tail on overflow).
    pub fn write(&self, data: &[f32]) -> usize {
        let want = data.len() / 2;
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        let n = want.min(self.cap - (h - t));
        for f in 0..n {
            let slot = (h + f) & self.mask;
            self.buf[slot * 2].store(data[f * 2].to_bits(), Ordering::Relaxed);
            self.buf[slot * 2 + 1].store(data[f * 2 + 1].to_bits(), Ordering::Relaxed);
        }
        self.head.store(h + n, Ordering::Release);
        n
    }

    /// Consumer (GUI thread): append up to `max_frames` interleaved
    /// frames to `out` (cleared first). Returns the frame count.
    pub fn drain(&self, out: &mut Vec<f32>, max_frames: usize) -> usize {
        let h = self.head.load(Ordering::Acquire);
        let mut t = self.tail.load(Ordering::Relaxed);
        let avail = h - t;
        let n = avail.min(max_frames).min(self.cap);
        out.clear();
        out.reserve(n * 2);
        for _ in 0..n {
            let slot = t & self.mask;
            out.push(f32::from_bits(self.buf[slot * 2].load(Ordering::Relaxed)));
            out.push(f32::from_bits(
                self.buf[slot * 2 + 1].load(Ordering::Relaxed),
            ));
            t += 1;
        }
        self.tail.store(t, Ordering::Release);
        n
    }

    pub fn frames_available(&self) -> usize {
        self.head.load(Ordering::Acquire) - self.tail.load(Ordering::Relaxed)
    }
}

/// GUI-side aggregate: one owner for the LUFS meter, spectrum and the
/// peak/hold/clip ballistics fed from the same drained samples.
pub struct MasterMeter {
    pub lufs: LufsMeter,
    pub spectrum: Spectrum,
    /// Instantaneous per-channel peak (amplitude, 0..~1) since last feed.
    pub l_peak: f32,
    pub r_peak: f32,
    /// Peak holds in dB (fast decay, meter-ballistics style).
    pub l_hold: f32,
    pub r_hold: f32,
    /// A sample reached full scale since the last clip reset.
    pub clip: bool,
    /// Total frames fed (telemetry / "meter alive" test).
    pub frames_fed: u64,
}

impl MasterMeter {
    pub fn new(sr: u32) -> Self {
        Self {
            lufs: LufsMeter::new(sr),
            spectrum: Spectrum::new(sr),
            l_peak: 0.0,
            r_peak: 0.0,
            l_hold: -120.0,
            r_hold: -120.0,
            clip: false,
            frames_fed: 0,
        }
    }

    /// Feed a drained interleaved stereo chunk (any length).
    pub fn feed(&mut self, data: &[f32]) {
        let (mut lp, mut rp) = (0.0f32, 0.0f32);
        let mut clip = false;
        for pair in data.as_chunks::<2>().0 {
            let (l, r) = (pair[0].abs(), pair[1].abs());
            if l > lp {
                lp = l;
            }
            if r > rp {
                rp = r;
            }
            if l >= 1.0 || r >= 1.0 {
                clip = true;
            }
        }
        // Peak display is a max over the CURRENT frame's ballistics: the
        // slow decay happens in `tick`, so a loud frame followed by a
        // quiet UI frame never blanks the meter.
        self.l_peak = self.l_peak.max(lp);
        self.r_peak = self.r_peak.max(rp);
        self.clip |= clip;
        self.frames_fed += (data.len() / 2) as u64;
        self.lufs.push_stereo(data);
        self.spectrum.push(data);
    }

    /// Per UI frame: decay the instantaneous peaks and the holds.
    pub fn tick(&mut self) {
        // Ballistics order matters: the hold compares against THIS
        // frame's peak BEFORE the decay (otherwise a sustained signal
        // reads 2.5 dB low forever), then the instantaneous peak decays
        // for the next frame. Hold falls 0.35 dB/frame toward the floor.
        let ld = 20.0 * (self.l_peak.max(1e-9)).log10();
        let rd = 20.0 * (self.r_peak.max(1e-9)).log10();
        self.l_peak *= 0.75;
        self.r_peak *= 0.75;
        self.l_hold = if ld > self.l_hold {
            ld
        } else {
            (self.l_hold - 0.35).max(-120.0)
        };
        self.r_hold = if rd > self.r_hold {
            rd
        } else {
            (self.r_hold - 0.35).max(-120.0)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_sine(out: &mut Vec<f32>, sr: u32, freq: f32, amp: f32, frames: usize) {
        out.clear();
        for i in 0..frames {
            let s = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin();
            out.push(s);
            out.push(s);
        }
    }

    #[test]
    fn k_weight_stage1_matches_bs1770_spec_at_48k() {
        // ITU-R BS.1770-4 Table 1 (48 kHz) constants.
        let f = k_stage1(48_000.0);
        assert!((f.b0 - 1.535_124_9).abs() < 1e-6, "b0 {}", f.b0);
        assert!((f.b1 - (-2.691_696_2)).abs() < 1e-6, "b1 {}", f.b1);
        assert!((f.b2 - 1.198_392_9).abs() < 1e-6, "b2 {}", f.b2);
        assert!((f.a1 - (-1.690_659_3)).abs() < 1e-6, "a1 {}", f.a1);
        assert!((f.a2 - 0.732_480_76).abs() < 1e-6, "a2 {}", f.a2);
        let g = k_stage2(48_000.0);
        assert!((g.b0 - 1.0).abs() < 1e-6);
        assert!((g.b1 - (-2.0)).abs() < 1e-6);
        assert!((g.b2 - 1.0).abs() < 1e-6);
        assert!((g.a1 - (-1.990_047_5)).abs() < 1e-6, "hp a1 {}", g.a1);
        assert!((g.a2 - 0.990_072_25).abs() < 1e-6, "hp a2 {}", g.a2);
    }

    #[test]
    fn sine_reads_expected_lufs_all_windows() {
        // Canonical calibration: a stereo 1 kHz sine at amplitude 0.1
        // (−20 dBFS) reads −20.0 LUFS — the K-weighting gain at 1 kHz
        // (+0.69 dB) exactly cancels the BS.1770 −0.691 offset.
        let sr = 48_000;
        let mut m = LufsMeter::new(sr);
        let mut buf = Vec::new();
        write_sine(&mut buf, sr, 1000.0, 0.1, 3 * 48_000); // 3 s (S needs 30×100ms)
        m.push_stereo(&buf);
        let want = -20.0;
        let mty = m.momentary().expect("momentary");
        let st = m.short_term().expect("short-term");
        let integ = m.integrated().expect("integrated");
        assert!((mty - want).abs() < 0.05, "momentary {mty}");
        assert!((st - want).abs() < 0.05, "short-term {st}");
        assert!((integ - want).abs() < 0.05, "integrated {integ}");
    }

    #[test]
    fn integrated_gates_silence_and_needs_four_windows() {
        let sr = 48_000;
        let mut m = LufsMeter::new(sr);
        let mut buf = Vec::new();
        // 1 s loud, then 3 s of pure silence.
        write_sine(&mut buf, sr, 1000.0, 0.1, 48_000);
        m.push_stereo(&buf);
        let loud = m.integrated().expect("integrated while loud");
        let silence = vec![0.0f32; 3 * 48_000 * 2];
        m.push_stereo(&silence);
        let after = m.integrated().expect("integrated after silence");
        // Silence blocks fail the absolute gate: I only moves by the few
        // mixed loud/silent 400 ms gating blocks straddling the boundary
        // (standard behaviour — measured: −0.71 dB, not the −6.02 dB an
        // ungated mean would give).
        assert!(
            (loud - after).abs() < 0.9,
            "{loud} vs {after} after silence"
        );
        assert!(after > -21.0, "gated I drifted down to {after}");
        // A meter shorter than one 400 ms gating block reports nothing.
        let mut m2 = LufsMeter::new(sr);
        write_sine(&mut buf, sr, 1000.0, 0.1, 2_400); // 50 ms
        m2.push_stereo(&buf);
        assert_eq!(m2.windows_done(), 0);
        assert!(m2.integrated().is_none());
        assert!(m2.momentary().is_none());
    }

    #[test]
    fn momentary_short_term_window_sizes() {
        // Feed 10 s of loud sine; momentary answers from window 4, S
        // only from window 30.
        let sr = 48_000;
        let mut m = LufsMeter::new(sr);
        let mut buf = Vec::new();
        write_sine(&mut buf, sr, 1000.0, 0.1, 4 * 4_800); // exactly 4 windows
        m.push_stereo(&buf);
        assert_eq!(m.windows_done(), 4);
        assert!(m.momentary().is_some());
        assert!(m.short_term().is_none());
        write_sine(&mut buf, sr, 1000.0, 0.1, 26 * 4_800); // +26 = 30
        m.push_stereo(&buf);
        assert!(m.short_term().is_some());
    }

    #[test]
    fn lra_of_a_steady_tone_is_near_zero() {
        let sr = 48_000;
        let mut m = LufsMeter::new(sr);
        let mut buf = Vec::new();
        write_sine(&mut buf, sr, 1000.0, 0.1, 6 * 48_000);
        m.push_stereo(&buf);
        let lra = m.lra().expect("lra");
        assert!(lra < 0.5, "steady-tone LRA {lra}");
    }

    #[test]
    fn reset_integrated_loses_history_but_not_ballistics() {
        let sr = 48_000;
        let mut m = LufsMeter::new(sr);
        let mut buf = Vec::new();
        write_sine(&mut buf, sr, 1000.0, 0.1, 48_000);
        m.push_stereo(&buf);
        m.reset_integrated();
        assert!(m.integrated().is_none());
        assert_eq!(m.windows_done(), 0);
        // Still measuring: 0.5 s more re-fills momentary.
        write_sine(&mut buf, sr, 1000.0, 0.1, 24_000);
        m.push_stereo(&buf);
        let mty = m.momentary().expect("momentary after reset");
        assert!((mty + 20.0).abs() < 0.1, "post-reset momentary {mty}");
    }

    #[test]
    fn spectrum_peaks_at_the_sine_bin() {
        let sr = 48_000;
        let mut s = Spectrum::new(sr);
        let mut buf = Vec::new();
        // v49: full 8192-sample window at EXACTLY bin 800 (4687.5 Hz),
        // amplitude 0.1 (≈ −20 dB). Hann-normalised coherent reading.
        write_sine(&mut buf, sr, 4687.5, 0.1, 8192);
        s.push(&buf);
        s.refresh(0.0);
        let want_bin = (4687.5 * 8192.0 / sr as f32).round() as usize; // 800
        let (mut best, mut best_i) = (-999.0f32, 0usize);
        for i in 1..s.n_bins() {
            if s.db[i] > best {
                best = s.db[i];
                best_i = i;
            }
        }
        assert!(
            best_i.abs_diff(want_bin) <= 1,
            "peak bin {best_i} want {want_bin}"
        );
        assert!((best + 20.0).abs() < 1.0, "peak level {best} dB want ≈ -20");
        // Bins far from the tone sit 30+ dB down.
        assert!(s.db[1500] < best - 30.0);
    }

    #[test]
    fn spectrum_resolution_is_8192_deep() {
        // v49 R3 pin: bin spacing ≈5.86 Hz @48 kHz (was 23.4 Hz) with a
        // 4096-bin display — fine enough that narrow EQ moves and low
        // tones are distinct features, not smeared blobs.
        let s = Spectrum::new(48_000);
        assert!((s.bin_hz(1) - 5.859375).abs() < 0.01, "{}", s.bin_hz(1));
        assert!(s.n_bins() >= 4096, "bins {}", s.n_bins());
    }

    #[test]
    fn meter_ring_wrap_and_overflow_drop() {
        let r = MeterRing::new();
        let cap = r.cap;
        // Wrap: write just under capacity, drain, write again.
        let block = vec![0.5f32; (cap - 10) * 2];
        assert_eq!(r.write(&block), cap - 10);
        let mut out = Vec::new();
        assert_eq!(r.drain(&mut out, usize::MAX), cap - 10);
        assert_eq!(out.len(), (cap - 10) * 2);
        assert!((out[4] - 0.5).abs() < 1e-6);
        assert_eq!(r.write(&block), cap - 10);
        assert_eq!(r.drain(&mut out, 100), 100); // capped read works
        assert_eq!(r.drain(&mut out, usize::MAX), cap - 110);
        // Overflow: a full ring drops the excess and never blocks.
        let big = vec![1.0f32; (cap + 1000) * 2];
        assert_eq!(r.write(&big), cap);
        assert_eq!(r.frames_available(), cap);
        assert_eq!(r.write(&big), 0);
        let mut out = Vec::new();
        assert_eq!(r.drain(&mut out, usize::MAX), cap);
    }

    #[test]
    fn master_meter_feed_tracks_peak_clip_and_lufs() {
        let sr = 48_000;
        let mut m = MasterMeter::new(sr);
        let mut buf = Vec::new();
        write_sine(&mut buf, sr, 1000.0, 1.4, 48_000); // clipping-level loud
        m.feed(&buf);
        assert!(m.clip);
        assert!(m.l_peak > 1.3 && m.r_peak > 1.3);
        assert_eq!(m.frames_fed, 48_000);
        assert!(m.lufs.momentary().expect("m") > -1.5);
        // tick decays ballistics.
        let before = m.l_peak;
        m.tick();
        assert!(m.l_peak < before);
        assert!(m.l_hold > -1.0); // held value near 0 dB
    }

    #[test]
    fn live_path_ring_to_master_meter_reads_calibration() {
        // The exact UI wiring: RT writes interleaved blocks, the frame
        // loop drains and feeds. A 1 kHz sine at -20 dBFS must read
        // I = -20.0 LUFS through the ring (see the calibration pin).
        let sr = 48_000u32;
        let ring = MeterRing::new();
        let mut mm = MasterMeter::new(sr);
        let mut full = Vec::new();
        write_sine(&mut full, sr, 1000.0, 0.1, 3 * 48_000);
        let frames = full.len() / 2;
        let mut i = 0usize;
        while i < frames {
            let n = (frames - i).min(256); // RT callback size
            let blk = &full[i * 2..(i + n) * 2];
            assert_eq!(ring.write(blk), n, "ring keeps up with 256-frame blocks");
            // "UI frame": drain what's there, feed, tick.
            let mut buf = Vec::new();
            let got = ring.drain(&mut buf, 4_800);
            assert!(got > 0);
            mm.feed(&buf);
            mm.tick();
            i += n;
        }
        let integ = mm.lufs.integrated().expect("integrated after 3 s");
        assert!(
            (integ - (-20.0)).abs() < 0.1,
            "live path I = {integ} for -20 dBFS tone"
        );
        assert!(!mm.clip, "quiet tone must not clip");
        assert!((mm.l_hold - (-20.0)).abs() < 1.0, "hold {}", mm.l_hold);
        assert_eq!(mm.frames_fed, frames as u64);
        // Drained ring is empty again (UI never re-feeds stale audio).
        let mut buf = Vec::new();
        assert_eq!(ring.drain(&mut buf, 4_800), 0);
    }
}
