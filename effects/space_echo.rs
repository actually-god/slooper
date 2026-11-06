// ============================================================ slooper ==
// space_echo.rs — echo AND stereo reverb in one effect, custom child GUI.
//
// A STEREO (ABI v3) effects chain: a modulated feedback delay per side
// (tone-controlled damping in each loop, preserving the input image)
// running in parallel with an FDN reverb built from TWO decorrelated
// comb banks + all-pass diffusers fed from the echo loop state, cross-
// mixed by a tail-width knob, then dry/wet mix and output trim. Turning
// "echo" down and "verb" up turns the same circuit into a pure room
// simulator: the factory presets ship that way (practice hall ..
// cathedral). Mono input still WIDENS (the banks never share modes);
// width 0 collapses the tail to exact mono (bit-identical sides).
//
//   in_L ─ delay_L ─┬─ ×echo ──┐
//        └→ bank_L ──┐         │
//                   ├─ ×verb ──┤ mix → out_L
//   in_R ─ delay_R ─┬─ ×echo ──┤
//        └→ bank_R ──┘ ×verb ──┘
//   banks cross-mix: L' = a·(1+w)/2 + b·(1−w)/2  (w = tail width)
//
// Tier 5 (docs/FL-SUITE.md): this is Reeverb 2 / LuxeVerb made native —
// the FL rooms are stereo devices and the tier-1 script was mid-only.
// The v1 fallback is `process_st(x,x)` returning the LEFT channel: with
// the default width=1 the L bank carries the tier-1/2 tunings verbatim,
// so every mono forensic pin (v13 DC wash, v14 snap glitch, mode
// density, wet tail) measures the historical circuit unchanged.
//
// (The file stem stays `space_echo` because saved chains and persisted
// param rows reference it; only the display name says "Echo + Reverb".)
//
// Params are ordinary effect params, so the auto slider window works too;
// the child GUI below is a scripted canvas showing the echo pattern
// (tick spacing = time, fade = feedback, jitter = wobble) over a reverb
// tail wedge, plus an input level history trace (bottom-up: silence at
// the floor), a factory row of presets (Echo, Practice Hall, Auditorium,
// Opera House, Cathedral) and the knob row.
// ---------------------------------------------------------------------------

include!("effect_api.rs");

// ---------------------------------------------------------------------------
// Params — indices are STICKY per script stem (chain rows persist by index;
// appended params load as their descriptor default). 0..10 are the tier-1
// set; `width` (11) was APPENDED by tier 5.
// ---------------------------------------------------------------------------
const P_TIME: usize = 0; // delay seconds (log range via slider range)
const P_FEED: usize = 1; // delay feedback 0..0.95
const P_TONE: usize = 2; // wet-path brightness, Hz (log): echo-loop AND reverb-feed damping
const P_WOBBLE: usize = 3; // modulation depth 0..1 (= ±1 % of time)
const P_RATE: usize = 4; // modulation rate Hz
const P_DECAY: usize = 5; // reverb feedback 0.5..0.95
const P_DAMP: usize = 6; // reverb damping, Hz (log)
const P_MIX: usize = 7; // wet mix 0..1
const P_OUT: usize = 8; // output trim 0..2
const P_ECHO: usize = 9; // echo-path level 0..1.5
const P_VERB: usize = 10; // reverb-path level 0..1.5
const P_WIDTH: usize = 11; // tail width 0..2 (0 = exact-mono tail, 1 = banks split)
const NPARAMS: u32 = 12;

/// Factory presets — name + all params (same order as PARAMS).
/// "Echo" is the plain dub-style effect (the old default); the rooms run
/// echo LOW and reverb FULL so the FDN tail, not the slap, dominates —
/// bright tight hall → auditorium → opera house → vast dark cathedral.
/// (Time/feedbk shape the early slap, decay/damp the tail length/colour,
/// width the stereo image of the tail — rooms full, dub echo narrow.)
/// THE HOST MIRRORS THESE (`builtin_presets("space_echo")` in src/fx.rs).
const PRESETS: [(&str, [f32; NPARAMS as usize]); 5] = [
    ("Echo", [0.38, 0.62, 3600.0, 0.0, 0.6, 0.84, 2800.0, 0.4, 1.0, 1.0, 0.5, 0.6]),
    ("Practice Hall", [0.07, 0.3, 8000.0, 0.05, 0.5, 0.72, 7500.0, 0.5, 0.95, 0.3, 1.0, 1.0]),
    ("Auditorium", [0.14, 0.38, 5200.0, 0.1, 0.35, 0.84, 4500.0, 0.55, 0.95, 0.25, 1.0, 1.0]),
    ("Opera House", [0.2, 0.42, 3800.0, 0.15, 0.25, 0.88, 3200.0, 0.6, 0.95, 0.22, 1.0, 1.0]),
    ("Cathedral", [0.32, 0.5, 2200.0, 0.25, 0.14, 0.95, 1500.0, 0.7, 0.9, 0.18, 1.0, 1.0]),
];

// ---------------------------------------------------------------------------
// Delay line with cubic (Hermite) fractional read.
// ---------------------------------------------------------------------------
struct Delay {
    buf: Vec<f64>,
    wp: usize,
}

impl Delay {
    fn new(n: usize) -> Self {
        Self { buf: vec![0.0; n.max(4)], wp: 0 }
    }
    #[inline]
    fn write(&mut self, x: f64) {
        self.buf[self.wp] = x;
        self.wp = (self.wp + 1) % self.buf.len();
    }
    /// Read `d` samples in the past (d >= 1.0), cubic-interpolated.
    #[inline]
    fn read(&self, d: f64) -> f64 {
        let n = self.buf.len() as f64;
        let d = d.clamp(1.0, n - 2.0);
        let df = d.floor();
        let frac = d - df;
        let base = (self.wp as f64 - df) as i64;
        let idx = |k: i64| -> usize {
            let mut i = k % self.buf.len() as i64;
            if i < 0 {
                i += self.buf.len() as i64;
            }
            i as usize
        };
        let xm1 = self.buf[idx(base - 1)];
        let x0 = self.buf[idx(base)];
        let x1 = self.buf[idx(base + 1)];
        let x2 = self.buf[idx(base + 2)];
        // cubic Hermite (Catmull-Rom)
        let a = -0.5 * xm1 + 1.5 * x0 - 1.5 * x1 + 0.5 * x2;
        let b = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
        let c = -0.5 * xm1 + 0.5 * x1;
        ((a * frac + b) * frac + c) * frac + x0
    }
}

// ---------------------------------------------------------------------------
// Schroeder comb with damping (Freeverb topology) and all-pass diffusers.
// ---------------------------------------------------------------------------
/// Feedback comb with damped loop and a SLOWLY MODULATED delay time.
/// A fixed-length comb is a row of infinitely thin resonances; after a
/// transient they ring as individual high-pitched partials — the metallic
/// "ping" (worse with few, closely-spaced combs, and untreatable by the
/// damping knob). Sweeping the length a fraction of a sample at <1 Hz
/// (the EMT-250 / Bricasti trick) Doppler-spreads each mode into a dense
/// band, so late tails wash instead of ring. Read is linearly
/// interpolated at the fractional delay.
struct Comb {
    buf: Vec<f64>,
    idx: usize,
    lp: f64,
    /// second stage of the TWO-POLE loop damping (12 dB/oct rolloff —
    /// one-pole left an icy 4–8 kHz residue that read as "tinny")
    lp2: f64,
    /// fixed AIR-ABSORPTION pole in the loop (see [`AIR_FC`]): the user
    /// damping knob may legitimately pass full treble, and then a stored
    /// click edge recirculates bright and the bank re-rings it as a
    /// high-pitched tick train after every transient. Air always eats
    /// treble faster than the room decays — model it separately.
    lp3: f64,
    /// base delay in samples and modulation depth/phase offset
    base: f64,
    depth: f64,
    off: f64,
}

impl Comb {
    fn new(base: f64, off: f64) -> Self {
        // Headroom for the room stretch (decay drives delay up to ~2×)
        // plus modulation depth.
        let n = (base as usize * 9 / 4 + 8).max(8);
        // EMT plates modulate ~±0.5 % of the delay length. The old
        // ±0.18 %-max clamp left every comb mode pinned within ~±0.06 % —
        // the eight tuned banks then stacked into a sustained, high-pitched
        // WHINE (user report, 2026-10). At this depth the modes smear into
        // bands and the row of resonators becomes a wash.
        let depth = (base * 0.006).clamp(2.0, 12.0);
        Self { buf: vec![0.0; n], idx: 0, lp: 0.0, lp2: 0.0, lp3: 0.0, base, depth, off }
    }
    #[inline]
    fn process(&mut self, x: f64, fb: f64, damp_c: f64, air_c: f64, phase: f64, room: f64) -> f64 {
        // `room` stretches the delay with the decay knob: bigger room =
        // longer combs = lower, denser modes; a fixed bank heard as a boxy
        // midrange "cathedral-prison" ring no matter how long the decay.
        let d = self.base * room + self.depth * (phase + self.off).sin();
        let n = self.buf.len();
        let di = d as usize;
        let fr = d - di as f64;
        let i0 = (self.idx + n - 1 - (di % n)) % n;
        let i1 = (i0 + n - 1) % n;
        let out = self.buf[i0] + (self.buf[i1] - self.buf[i0]) * fr;
        self.lp = out * (1.0 - damp_c) + self.lp * damp_c;
        self.lp2 = self.lp * (1.0 - damp_c) + self.lp2 * damp_c;
        // Fixed air-absorption pole in SERIES with the user damping: even at
        // full-brightness damp, several kHz and up lose a step every loop
        // round, so a recirculated click EDGE (kick snap re-read from the
        // echo line at each echo period) decays out of the bank in a few
        // passes instead of ticking at the recirculation rate. The damp
        // knob still voices everything below the air band.
        self.lp3 = self.lp2 * (1.0 - air_c) + self.lp3 * air_c;
        if !self.lp3.is_finite() {
            self.lp = 0.0;
            self.lp2 = 0.0;
            self.lp3 = 0.0;
        }
        self.buf[self.idx] = x + self.lp3 * fb;
        self.idx = (self.idx + 1) % n;
        if !out.is_finite() {
            return 0.0;
        }
        out
    }
}

#[inline]
fn allpass(prev: &mut Allpass, x: f64, g: f64) -> f64 {
    let bufout = prev.buf[prev.idx];
    prev.buf[prev.idx] = x + g * bufout;
    prev.idx = (prev.idx + 1) % prev.buf.len();
    let out = bufout - g * prev.buf[prev.idx];
    if out.is_finite() {
        out
    } else {
        0.0
    }
}

struct Allpass {
    buf: Vec<f64>,
    idx: usize,
}

impl Allpass {
    fn new(n: usize) -> Self {
        Self { buf: vec![0.0; n.max(4)], idx: 0 }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// One side of the modulated echo: its own delay line, damped feedback
/// state, and in-loop DC blocker. Two instances = the input image survives
/// the echo (a hard-panned source echoes to the side it came from).
struct EchoPath {
    delay: Delay,
    /// one-pole damping state in the delay feedback loop
    tone_lp: f64,
    blk: DcBlock,
}

struct SpaceEcho {
    sr: f64,
    /// echo per side (L, R) — see [`EchoPath`]
    echo: [EchoPath; 2],
    phase: f64, // LFO phase (radians) — shared, both heads of one tape
    /// reverb comb banks: L keeps the tier-1 tunings verbatim (the v1
    /// fallback must stay bit-identical), R is spread 3.1 % with offset
    /// modulation phases so the banks never share modes — a MONO feed
    /// still decorrelates the tail.
    combs: [Comb; 8],
    combs_r: [Comb; 8],
    /// comb-length modulation clock (radians), one clock, per-comb offsets
    vphase: f64,
    /// The diffuser tunings are IDENTICAL between the banks on purpose:
    /// at width = 0 both sides receive the same cross-sum, so the R chain
    /// reproduces the L chain bit-for-bit once its buffers refresh — that
    /// is what makes "width 0" an EXACT mono tail (pinned by
    /// `tier5_space_echo_stereo_tail` in the host test suite).
    ap: [Allpass; 6],
    ap_r: [Allpass; 6],
    /// reverb-input pre-damp (one-pole at the TONE cutoff) per side: real
    /// rooms are fed AIR-ROLLED sound, and raw dry/echo fizz into the comb
    /// bank is where the "tinny" starts — filter before ringing, not after.
    verb_lp: [f64; 2],
    /// DC blockers on the two wet-path injections (see [`HP_FC`]): one on
    /// each reverb feed (dry + echo copies). The echo loops carry their own
    /// blockers in [`EchoPath`].
    verb_blk: [DcBlock; 2],
    /// DC-blocker pole (see [`HP_FC`]); constant per instance.
    r_hp: f64,
    /// comb lengths at sr=44.1 kHz; scaled by sr (R bank × the mode spread)
    mix: f64,
    out: f64,
    gui_open: AtomicU32,
    in_peak: AtomicU32,
}

/// Comb tunings (samples @ 44100) and their spread factors.
// Four combs bunched between 1422..1617 samples leave the mode spacing
// sparse enough for individual modes to ring out after a transient: that is
// the high-pitched metallic "ping" on snaps/claps, and no damping setting
// fixes it (low damp passes the ring, high damp only kills the tail). The
// classic Freeverb eight-comb bank from 1116..1617 samples multiplies the
// mode density and smears the resonance into a wash; the 0.5 sum below
// keeps the bank at the old level.
const COMB_TUNE: [f64; 8] = [1116.0, 1188.0, 1277.0, 1356.0, 1422.0, 1491.0, 1557.0, 1617.0];
/// Right-bank mode spread: every R comb is 3.1 % longer and its length-LFO
// offset is shifted 2.3 rad, so identical feeds exit the banks decorrelated
// within a few loop rounds (early reflections stay centred, the LATE tail
// opens — which is how real stereo rooms read).
const BANK_SPREAD: f64 = 1.031;
const BANK_PHASE: f64 = 2.3;
/// All-pass tunings (samples @ 44100). Six diffusers instead of the
/// original four: more smear per unit of ring, and the two added lengths
/// (672, 773) sit above the old top so the diffusion stays even up to
/// ~2 kHz where the ear reads "metal". IDENTICAL in both banks (see the
/// [`SpaceEcho::ap`] comment — it is what makes width 0 exactly mono).
const AP_TUNE: [f64; 6] = [225.0, 341.0, 441.0, 556.0, 672.0, 773.0];

impl SpaceEcho {
    fn new(sample_rate: f64) -> Self {
        let sr = if sample_rate.is_finite() && sample_rate > 1.0 { sample_rate } else { 48000.0 };
        let dl_n = (sr * 2.5) as usize + 16; // up to 2.5 s delay
        let scale = sr / 44100.0;
        let combs = COMB_TUNE.map(|t| Comb::new(t * scale, t)); // phase offset = tuning
        let combs_r = COMB_TUNE.map(|t| Comb::new(t * scale * BANK_SPREAD, t + BANK_PHASE));
        let ap = AP_TUNE.map(|t| Allpass::new((t * scale) as usize + 1));
        let ap_r = AP_TUNE.map(|t| Allpass::new((t * scale) as usize + 1));
        let path = || EchoPath { delay: Delay::new(dl_n), tone_lp: 0.0, blk: DcBlock::ZERO };
        Self {
            sr,
            echo: [path(), path()],
            phase: 0.0,
            combs,
            combs_r,
            vphase: 0.0,
            ap,
            ap_r,
            verb_lp: [0.0; 2],
            verb_blk: [DcBlock::ZERO; 2],
            r_hp: one_c(HP_FC, sr),
            mix: 0.4,
            out: 1.0,
            gui_open: AtomicU32::new(0),
            in_peak: AtomicU32::new(0),
        }
    }
}

// ---------------------------------------------------------------------------
// Audio entry points.
// ---------------------------------------------------------------------------
extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(SpaceEcho::new(sample_rate))) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut SpaceEcho) });
    }
}

#[inline]
fn one_c(fc_hz: f64, sr: f64) -> f64 {
    // one-pole lowpass coefficient
    (-(core::f64::consts::TAU * fc_hz.max(20.0) / sr)).exp()
}

/// One-pole DC blocker (high-pass) state: previous input and output.
#[derive(Clone, Copy)]
struct DcBlock {
    px: f64,
    py: f64,
}

impl DcBlock {
    const ZERO: DcBlock = DcBlock { px: 0.0, py: 0.0 };
    /// y[n] = r·y[n-1] + x[n] - x[n-1] — the classic DC blocker: flat above
    /// the corner, -20 dB/oct below it, so it removes DC and subsonic mud
    /// without coloring the audible band (corner ~28 Hz).
    #[inline]
    fn run(&mut self, x: f64, r: f64) -> f64 {
        let y = r * (self.py + x - self.px);
        self.px = x;
        self.py = y;
        if !y.is_finite() {
            *self = DcBlock::ZERO;
            return 0.0;
        }
        y
    }
}

/// DC-blocker corner in Hz. Drum impacts, finger snaps and fast staccato
/// carry real DC/sub-20 Hz energy; every comb in the FDN has a feedback of
/// up to 0.95, which is ~20 dB of gain at DC, and both wet feeds include a
/// copy of the dry signal — without this filter every abrupt change pumps a
/// DC thump through the room that washes for seconds (the "static" a dense
/// transient left behind: a click train measured at ~-43 dBFS of DC-heavy
/// residue 3 s after the sound ended). Real reverb units HP the wet path at
/// 30-80 Hz for exactly this reason.
const HP_FC: f64 = 28.0;

/// Fixed air-absorption corner in the comb loops (see [`Comb::lp3`]).
/// Measured root cause (2026-11 "high-pitched glitches around percussive
/// sounds"): a kick/snap edge is stored in the echo line and re-read at
/// every echo round-trip; that feed keeps the comb bank re-ringing the
/// bright edge, and with the damp knob at its bright end nothing in the
/// loop ate treble per pass — so the edge ticked at the recirculation rate
/// (HF burst −31 dBFS 145–225 ms after the hit vs −85 dB mid-tail,
/// Practice-Hall preset; near-zero for dark presets, matching the report
/// that only bright settings glitch). Real rooms always absorb treble
/// faster than they decay; one always-on pole models that floor.
const AIR_FC: f64 = 6500.0;

/// Room-feed brightness cap in Hz (see the feed comment in `process`).
const FEED_CAP_FC: f64 = 4200.0;

/// v1 entry / graceful fallback: run the stereo circuit with the pair
/// (x, x) and return the LEFT channel. At the default width=1 the L bank
/// and L diffuser carry the tier-1/2 tunings verbatim, so the mono path is
/// the historical circuit bit-for-bit (every pre-tier-5 forensic pin of
/// this effect measures the same DSP it was calibrated on).
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    let mut or = 0.0f32;
    let ol = process_st(state, input, input, params, 0.0, &mut or);
    ol
}

/// v2 entry: the vocoder-style sidechain key is not used here — ducking
/// is internal by design (echo loop state feeds the room).
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
    let st = unsafe { &mut *(state as *mut SpaceEcho) };
    let sr = st.sr;
    let g = |k: usize| unsafe { *params.add(k) };
    let xs = [if l.is_finite() { l as f64 } else { 0.0 }, if r.is_finite() { r as f64 } else { 0.0 }];

    // Input level for the GUI meter (free when the editor is closed).
    if st.gui_open.load(Relaxed) != 0 {
        let m = xs[0].max(xs[1]);
        let prev = f32::from_bits(st.in_peak.load(Relaxed)) as f64;
        let env = if m > prev { m } else { prev * 0.999 };
        st.in_peak.store((env as f32).to_bits(), Relaxed);
    }

    // ---- delay with LFO-modulated time and damped feedback ----
    let time_s = (g(P_TIME) as f64).clamp(0.02, 2.4);
    let fb = (g(P_FEED) as f64).clamp(0.0, 0.95);
    let wobble = (g(P_WOBBLE) as f64).clamp(0.0, 1.0);
    let rate = (g(P_RATE) as f64).clamp(0.01, 20.0);
    let decay = (g(P_DECAY) as f64).clamp(0.5, 0.95);
    let echo_lvl = (g(P_ECHO) as f64).clamp(0.0, 1.5);
    let verb_lvl = (g(P_VERB) as f64).clamp(0.0, 1.5);
    let width = (g(P_WIDTH) as f64).clamp(0.0, 2.0);
    st.mix += ((g(P_MIX) as f64).clamp(0.0, 1.0) - st.mix) * 0.01;
    st.out += ((g(P_OUT) as f64).clamp(0.0, 2.0) - st.out) * 0.01;

    let tone_c = one_c(g(P_TONE) as f64, sr);
    st.phase += core::f64::consts::TAU * rate / sr;
    if st.phase > core::f64::consts::TAU {
        st.phase -= core::f64::consts::TAU;
    }
    let w = 1.0 + wobble * 0.01 * st.phase.sin();
    let d = (time_s * sr * w).clamp(1.0, (st.echo[0].delay.buf.len() - 4) as f64);

    let mut echoed = [0.0f64; 2];
    let mut tone = [0.0f64; 2];
    for s in 0..2 {
        let e = &mut st.echo[s];
        let echoed_raw = e.delay.read(d);
        // The DC blocker (see [`HP_FC`]) belongs INSIDE the loop, on the
        // signal that re-enters feedback — never on the wet tap. On the
        // read side it acts as a differentiator across the stored click
        // step: every repeat of a transient re-sprouted a 2x edge spike at
        // full tap level no matter how damped the loop was (the "high-
        // pitched tick on each repeat after a drum hit"). Inside the loop
        // it still cannot accumulate: the filter's output feeds the damped
        // state, so subsonic content is drained once per round, and the
        // tap hears the plain delayed copy — DC and all, exactly like the
        // input it came from.
        let damped_in = e.blk.run(echoed_raw, st.r_hp);
        // damped feedback loop
        e.tone_lp = damped_in * (1.0 - tone_c) + e.tone_lp * tone_c;
        if !e.tone_lp.is_finite() {
            e.tone_lp = 0.0;
        }
        e.delay.write(xs[s] + e.tone_lp * fb);
        echoed[s] = echoed_raw;
        tone[s] = e.tone_lp;
    }

    // ---- FDN reverb in parallel with the echo (fed from echo + dry) ----
    // 0.45 feed: rooms need the reverb to read as the ROOM, not as a faint
    // halo around the slap — the preset row drives echo down / verb up.
    // The feed is damped at the TONE cutoff (user report: "the tone knob
    // does seemingly nothing" — at low feedback the echo loop alone made
    // tone inaudible; reverb brightness responds at any setting, so tone
    // now voices the entire wet path). Filtering before the comb bank is
    // what separates "air" from "tin can" — filters after the ring can
    // only dull everything together.
    // Feed the room DC-BLOCKED (see [`HP_FC`]): this path carries a copy of
    // the DRY signal, so without it every drum impact or snap injects its
    // DC/subsonic content into 8 combs running at up to 0.95 feedback and
    // the room answers with a mud wash that outlives the note by seconds.
    //
    // FEED SOURCE = the damped loop state `tone_lp`, never the raw delay
    // read. The delay line stores an input click as a hard step, and the
    // loop's tone pole filters on the WRITE side: every round-trip re-reads
    // the SAME stored discontinuity at full edge level, so a raw feed
    // re-injected full edge energy into the bank at each echo period — a
    // high-pitched tick train no loop or feed pole can chase, because each
    // tick arrives fresh, past every filter (measured −30 dBFS HF bursts at
    // 145/225 ms after a snap under the bright presets, immaterial to any
    // comb-loop treatment). tone_lp is the echo loop's own damped state —
    // repeats now enter the room exactly as bright (and no brighter) as
    // they audibly decay in the echo.
    //
    // The feed pre-damp is additionally CAPPED at FEED_CAP_FC (min with the
    // tone knob): real rooms are fed air-rolled sound, and even the dry
    // copy must not hand the bank the input's raw edge. Below the cap the
    // tone knob still voices the feed exactly as before.
    let feed_c = tone_c.max(one_c(FEED_CAP_FC, sr));
    let damp_c = 1.0 - one_c(g(P_DAMP) as f64, sr);
    let air_c = 1.0 - one_c(AIR_FC, sr);
    // Room stretch: decay 0.5 → 1.0× comb length, decay 0.95 → ~1.95×.
    // Long tails through SHORT combs are the boxy, metallic "cathedral
    // prison" ring — the modes have to actually descend for the room.
    let room = 1.0 + ((decay - 0.5) / 0.45).clamp(0.0, 1.0) * 0.95;
    // comb-length LFO: ~0.85 Hz, one clock, per-comb offsets from the tunings
    // (the R bank carries the extra BANK_PHASE shift — see [`BANK_SPREAD`]).
    st.vphase += core::f64::consts::TAU * 0.85 / sr;
    if st.vphase > core::f64::consts::TAU {
        st.vphase -= core::f64::consts::TAU;
    }
    let mut bank = [0.0f64; 2];
    for s in 0..2 {
        let feed = st.verb_blk[s].run(tone[s] * 0.6 + xs[s] * 0.4, st.r_hp) * 0.45 * (1.0 - feed_c)
            + st.verb_lp[s] * feed_c;
        if !feed.is_finite() {
            st.verb_lp[s] = 0.0;
            continue;
        }
        st.verb_lp[s] = feed;
        let combs = if s == 0 { st.combs.iter_mut() } else { st.combs_r.iter_mut() };
        for c in combs {
            bank[s] += c.process(feed, decay, damp_c, air_c, st.vphase, room);
        }
    }
    // Tail cross-mix (the width knob): w=1 → each side hears only its own
    // bank (full image), w=0 → both hear the same sum and, with identical
    // diffuser tunings, the tails become bit-identical — exact mono.
    let ca = 0.5 * (1.0 + width);
    let cb = 0.5 * (1.0 - width);
    let mut vl = (bank[0] * ca + bank[1] * cb) * 0.5;
    let mut vr = (bank[1] * ca + bank[0] * cb) * 0.5;
    for a in st.ap.iter_mut() {
        vl = allpass(a, vl, 0.5);
    }
    for a in st.ap_r.iter_mut() {
        vr = allpass(a, vr, 0.5);
    }

    let wet = [
        echoed[0] * echo_lvl + vl * verb_lvl,
        echoed[1] * echo_lvl + vr * verb_lvl,
    ];
    let ol = xs[0] * (1.0 - st.mix) + wet[0] * st.mix * st.out;
    let orv = xs[1] * (1.0 - st.mix) + wet[1] * st.mix * st.out;
    if !(ol.is_finite() && orv.is_finite()) {
        // Reset LOOP FILTERS only — never zero the delay lines / comb rings
        // on the audio thread (tier-4 rule: that is milliseconds of work).
        for e in st.echo.iter_mut() {
            e.tone_lp = 0.0;
        }
        st.verb_lp = [0.0; 2];
        unsafe { *out_r = 0.0 };
        return 0.0;
    }
    unsafe { *out_r = orv as f32 };
    ol as f32
}

// ---------------------------------------------------------------------------
// Custom child GUI.
// ---------------------------------------------------------------------------
const HIST: usize = 160;

struct Gs {
    /// input-level history, newest last.
    hist: Vec<f32>,
    /// animation clock (seconds of GUI time)
    clock: f64,
    /// level history from the audio side, cached across draws
    inst: Option<*mut core::ffi::c_void>,
    last_t: f64,
}

extern "C" fn gui_create(_sample_rate: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(Gs { hist: vec![0.0; HIST], clock: 0.0, inst: None, last_t: 0.0 })) as *mut core::ffi::c_void
}

extern "C" fn gui_destroy(gs: *mut core::ffi::c_void) {
    if gs.is_null() {
        return;
    }
    let gs = unsafe { Box::from_raw(gs as *mut Gs) };
    if let Some(p) = gs.inst {
        unsafe { (*(p as *mut SpaceEcho)).gui_open.store(0, Relaxed) };
    }
}

extern "C" fn gui_draw(
    gs: *mut core::ffi::c_void,
    state: *mut core::ffi::c_void,
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

    // animation clock from the app-wide time
    if gs.last_t == 0.0 {
        gs.last_t = fr.time;
    }
    gs.clock += (fr.time - gs.last_t).clamp(0.0, 0.1);
    gs.last_t = fr.time;

    if !state.is_null() {
        gs.inst = Some(state);
        unsafe { (*(state as *mut SpaceEcho)).gui_open.store(1, Relaxed) };
    }

    let gp = |k: u32| unsafe { (api.get_param)(c, k) };
    let sp = |k: u32, v: f32| unsafe { (api.set_param)(c, k, v) };

    let w = fr.width;
    let h = fr.height;
    let canvas_h = (h - 132.0).max(100.0);

    // ---- level history ----------------------------------------------------
    let lvl = if !state.is_null() {
        f32::from_bits(unsafe { (*(state as *const SpaceEcho)).in_peak.load(Relaxed) })
    } else {
        0.0
    };
    gs.hist.remove(0);
    gs.hist.push(lvl);

    unsafe {
        (api.rect_filled)(c, 8.0, 8.0, w - 16.0, canvas_h, 0.09, 0.10, 0.13, 1.0);
        // history trace — bottom-up like the EQ's analyser: silence sits at
        // the floor, louder input climbs toward the top (0 dBFS). The old
        // mapping was inverted (silence pinned the trace to the top edge).
        let cw = w - 16.0;
        let mut pts: Vec<f32> = Vec::with_capacity(HIST * 2);
        for (i, v) in gs.hist.iter().enumerate() {
            let db = 20.0f32 * (v.max(1e-6)).log10(); // -120..0
            let up = 1.0 - ((-db) / 60.0).clamp(0.0, 1.0);
            pts.push(8.0 + (i as f32 / (HIST - 1) as f32) * cw);
            pts.push(8.0 + canvas_h - up.max(0.0) * canvas_h * 0.98);
        }
        (api.polyline)(c, pts.as_ptr(), pts.len() as u32 / 2, 0, 1.4, 0.30, 0.85, 0.55, 0.85);

        // ---- echo pattern + reverb tail ---------------------------------
        let time = gp(P_TIME as u32);
        let feed = gp(P_FEED as u32);
        let wobble = gp(P_WOBBLE as u32);
        let rate = gp(P_RATE as u32);
        let decay = gp(P_DECAY as u32);
        let clk = gs.clock as f32;

        // scrolling playhead: 2 s across the canvas
        let scroll = 2.0f32;
        let px_per_s = cw / scroll;
        let midy = 8.0 + canvas_h * 0.62;
        // "now" is the right edge; echoes arrive every `time` seconds.
        let mut et = clk % time; // phase into the next echo
        let mut n = 0usize;
        while n < 24 {
            let x = cw + 8.0 - et * px_per_s;
            if x < 8.0 {
                break;
            }
            let amp = feed.powi(n as i32);
            // wobble jitters the arrival time
            let jitter = (clk * rate * core::f32::consts::TAU + n as f32 * 1.7).sin() * wobble * 0.01 * time;
            let x = x + jitter * px_per_s;
            let hh = (amp * canvas_h * 0.36).clamp(2.0, canvas_h * 0.48);
            (api.rect_filled)(c, x - 2.0, midy - hh / 2.0, 4.0, hh, 1.0, 0.75, 0.35, (0.25 + amp * 0.75).min(1.0));
            et += time;
            n += 1;
        }
        // reverb tail wedge behind each echo, sized by decay — drawn as an
        // L/R pair (tier 5: the tail is a stereo device) straddling the
        // echo line, spread by the width knob.
        let tail = (0.05_f32).max((decay.ln() / (0.95_f32).ln()).sqrt()) * time * 4.0;
        let wsp = gp(P_WIDTH as u32) * 3.0;
        et = clk % time;
        for n in 0..6 {
            let amp = feed.powi(n as i32);
            let x = cw + 8.0 - et * px_per_s;
            let tl = (tail * px_per_s * amp).clamp(0.0, cw * 2.0);
            (api.rect_filled)(c, x, midy - 4.0 - wsp, tl, 3.0, 0.42, 0.60, 0.95, 0.08 + amp * 0.16);
            (api.rect_filled)(c, x, midy + 1.0 + wsp, tl, 3.0, 0.95, 0.60, 0.42, 0.08 + amp * 0.16);
            et += time;
        }

        // input head marker on the left
        (api.text)(c, 12.0, 12.0, 10.0, 0, 0.7, 0.7, 0.75, 1.0, c"in".as_ptr());
        let lv = lvl.clamp(0.0, 1.0) * (canvas_h - 20.0);
        (api.rect_filled)(c, 12.0, 8.0 + canvas_h - 10.0 - lv, 6.0, lv, 0.30, 0.85, 0.55, 0.9);

        // ---- knob row ----------------------------------------------------
        // (ky starts high in the strip: the preset row lives at its bottom.)
        // Pitch 61 fits twelve knobs across the 760 px window.
        let ky = canvas_h + 6.0;
        let row = |i: usize, label: &str, idx: usize, min: f32, max: f32, curve: f32, fmt: &str| {
            let mut v = gp(idx as u32);
            let shown = match fmt {
                "ms" => format!("{label} {:.0}ms", v * 1000.0),
                "hz" => format!("{label} {:.1}Hz", v),
                "khz" => format!("{label} {:.1}kHz", v / 1000.0),
                "pct" => format!("{label} {:.0}%", v * 100.0),
                "db" => format!("{label} {:+.1}dB", 20.0 * (v.max(1e-4)).log10()),
                _ => format!("{label} {v:.2}"),
            };
            let s = std::ffi::CString::new(shown).unwrap();
            if (api.knob)(c, 12.0 + i as f32 * 61.0, ky + 18.0, 44.0, s.as_ptr(), &mut v, min, max, curve) != 0 {
                sp(idx as u32, v);
            }
        };
        row(0, "time", P_TIME, 0.02, 2.4, 0.0, "ms");
        row(1, "feedbk", P_FEED, 0.0, 0.95, 1.0, "pct");
        row(2, "tone", P_TONE, 200.0, 16000.0, 0.0, "khz");
        row(3, "wobble", P_WOBBLE, 0.0, 1.0, 1.0, "pct");
        row(4, "rate", P_RATE, 0.05, 12.0, 0.0, "hz");
        row(5, "echo", P_ECHO, 0.0, 1.5, 1.0, "pct");
        row(6, "verb", P_VERB, 0.0, 1.5, 1.0, "pct");
        row(7, "decay", P_DECAY, 0.5, 0.95, 1.0, "pct");
        row(8, "damp", P_DAMP, 200.0, 16000.0, 0.0, "khz");
        row(9, "mix", P_MIX, 0.0, 1.0, 1.0, "pct");
        row(10, "out", P_OUT, 0.0, 2.0, 1.0, "db");
        row(11, "width", P_WIDTH, 0.0, 2.0, 1.0, "pct");

        // ---- factory presets ------------------------------------------
        // One click applies all twelve params: plain echo, or a room with
        // the reverb carrying the tail (echo low, verb full).
        let py = canvas_h + 96.0;
        let s = std::ffi::CString::new("presets:").unwrap();
        (api.text)(c, 8.0, py + 5.0, 10.0, 0, 0.7, 0.7, 0.75, 1.0, s.as_ptr());
        for (pi, (name, vals)) in PRESETS.iter().enumerate() {
            let s = std::ffi::CString::new(*name).unwrap();
            if (api.button)(c, 62.0 + pi as f32 * 122.0, py, 118.0, s.as_ptr()) != 0 {
                for (k, v) in vals.iter().enumerate() {
                    sp(k as u32, *v);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Descriptor + GUI tables + exports.
// Tier 3/4 export quartet: a v3 script must export ALL prefix symbols
// (mirror spread.rs / echo3.rs exactly); the loader validates the prefix.
// ---------------------------------------------------------------------------
static PARAMS: [EffectParamV1; 12] = [
    EffectParamV1 { label: c"time".as_ptr(), min: 0.02, max: 2.4, default: 0.38 },
    EffectParamV1 { label: c"feedback".as_ptr(), min: 0.0, max: 0.95, default: 0.62 },
    EffectParamV1 { label: c"tone".as_ptr(), min: 200.0, max: 16000.0, default: 3600.0 },
    EffectParamV1 { label: c"wobble".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.05, max: 12.0, default: 0.6 },
    EffectParamV1 { label: c"decay".as_ptr(), min: 0.5, max: 0.95, default: 0.84 },
    EffectParamV1 { label: c"damp".as_ptr(), min: 200.0, max: 16000.0, default: 2800.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.4 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"echo".as_ptr(), min: 0.0, max: 1.5, default: 1.0 },
    EffectParamV1 { label: c"verb".as_ptr(), min: 0.0, max: 1.5, default: 1.0 },
    EffectParamV1 { label: c"width".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

const DESC_TEXT: &core::ffi::CStr =
    c"Modulated echo and STEREO FDN reverb (Fruity Reeverb 2 / LuxeVerb task): per-side echo, dual decorrelated comb banks, tail width (0 = exact mono). Damped loops, wobble LFO, animated echo-pattern window, factory presets (plain echo + practice hall .. cathedral).";

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Echo + Reverb".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TEXT.as_ptr(),
    n_params: NPARAMS,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"Echo + Reverb".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TEXT.as_ptr(),
    n_params: NPARAMS,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};

pub static DESC3: EffectInfoV3 = EffectInfoV3 {
    abi: SLOOPER_EFFECT_ABI_V3,
    name: c"Echo + Reverb".as_ptr(),
    author: c"slooper".as_ptr(),
    description: DESC_TEXT.as_ptr(),
    n_params: NPARAMS,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
    process_st,
};

pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: SLOOPER_EFFECT_GUI_ABI,
    want_width: 760.0,
    want_height: 424.0,
    create: gui_create,
    draw: gui_draw,
    destroy: gui_destroy,
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
pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 {
    &GUI
}
