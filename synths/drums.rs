// ============================================================ slooper ==
// slooper-synth-version: 8
// drums.rs — an analog-style drum kit driven straight from MIDI notes.
//
// Every note is one drum, synthesized from oscillators + filtered noise
// (no samples, no assets): the lower octave is the standard GM-style kit
// map, and notes outside the kit range play a pitched tom so the whole
// keyboard is playable. Choke the cymbals and open hats by writing the
// same note again short (or just play the closed hat).
//
//   35,36  kick        41,43,45    tom (low..mid)
//   38,40  snare       47,48,50    tom (hi)
//   37,39  clap/stick  42,44       closed hat
//   46     open hat    49,55,57    crash
//   51     ride        others      pitched tom
//
// Recipe per drum (all decays scale with the "decay" slider):
//   kick   : sine, pitch sweep 150->45 Hz over 25 ms, 340 ms body decay,
//            plus 4 ms noise click for the beater transient
//   snare  : 188 Hz sine body + RBJ bandpassed noise, split envelopes
//   clap   : three 12 ms noise bursts into one bandpass + tail
//   hats   : six square oscillators at the classic metallic non-integer
//            ratios, 2-pole DC-block highpass; closed decays in 45 ms,
//            open rings and chokes on note-off
//   toms   : pitch-swept sine, 300 ms decay, pitch follows the note
//   crash  : inharmonic metal bank + noise, 6 kHz highpass, 1.5 s decay
//   ride   : same bank tuned up, shorter decay with a ping partial
//
// 12 voices, oldest-voice stealing. process() runs on the audio thread:
// allocation-free fixed arrays, deterministic noise (LCG per voice),
// everything clamped so NaN cannot escape into the loop buffer.
//
// v136 (card t_d28458f1) contract lanes — the kit honours every expression
// input like its melodic siblings (script ABI v3: bend + poly_at):
//   * the piano-roll GLIDE lane (bend, ±12 st) rides the same pitch
//     multiplier as the tune knob, so held kicks/snares/toms — and the hat
//     fundamentals, exactly where tune goes — bend with the ramp; crash and
//     ride stay pitch-immune, like they are for tune.
//   * poly-AT (MPE note pressure) is per-voice and class-based: pressed
//     open hats / crash / ride CHOKES (decay up to 4x faster), pressed
//     closed hats brighten (HF pre-emphasis), pressed
//     snares/claps brighten (noise bandpass climbs up to 1.8x), pressed
//     kicks/toms raise their head TENSION (pitch x1.25) — and a val of 0
//     reads as pressure released, never silence.
//
// v137 (card t_3bd30160) tail discipline — every voice retires only after
//   SEVEN time constants of its kind's slowest envelope component
//   (e^-7 ≈ -61 dBFS, provably inaudible), fading linearly to EXACT zero
//   over the last FADE_S seconds so `alive = false` lands on a silent
//   sample. The old budgets (1.5-3.2 tau) hard-silenced a long-decay crash
//   at ~-13 dB — an audible chop + DC step in the middle of the ring.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 12;
const TAU: f64 = core::f64::consts::TAU;

/// v137: linear fade-out length before the retire point — the envelope
/// multiplier walks 1 -> 0 over this window and the kill lands on exact
/// zero, so retirement is continuous by construction (no gate step).
const FADE_S: f64 = 0.008;

// Drum kinds.
const KICK: u8 = 0;
const SNARE: u8 = 1;
const CLAP: u8 = 2;
const CHAT: u8 = 3;
const OHAT: u8 = 4;
const TOM: u8 = 5;
const CRASH: u8 = 6;
const RIDE: u8 = 7;

/// RBJ biquad (bandpass used for snare/clap noise).
#[derive(Clone, Copy)]
struct Biquad {
    a0: f64, a1: f64, a2: f64, b1: f64, b2: f64,
    x1: f64, x2: f64, y1: f64, y2: f64,
}

impl Biquad {
    fn bandpass(sr: f64, f: f64, q: f64) -> Biquad {
        let mut b = Biquad { a0: 0.0, a1: 0.0, a2: 0.0, b1: 0.0, b2: 0.0,
                             x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 };
        b.set_bandpass(sr, f, q);
        b
    }
    /// v136: retune IN PLACE — coefficients only, delay state kept, so a
    /// pressure move glides the snare/clap noise band without restarting
    /// its filter (and val 0 recomputes the exact base coefficients, so a
    /// released voice is bit-for-bit its pressure-less self).
    fn set_bandpass(&mut self, sr: f64, f: f64, q: f64) {
        let w = TAU * f / sr;
        let (sn, cs) = w.sin_cos();
        let al = sn / (2.0 * q);
        let a0 = al;
        let a1 = 0.0;
        let a2 = -al;
        let b0 = 1.0 + al;
        let b1 = -2.0 * cs;
        let b2 = 1.0 - al;
        self.a0 = a0 / b0;
        self.a1 = a1 / b0;
        self.a2 = a2 / b0;
        self.b1 = b1 / b0;
        self.b2 = b2 / b0;
    }
    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.a0 * x + self.a1 * self.x1 + self.a2 * self.x2
            - self.b1 * self.y1 - self.b2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        if y.is_finite() { y } else { 0.0 }
    }
}

#[derive(Clone, Copy)]
struct Voice {
    alive: bool,
    kind: u8,
    note: u8,
    /// Trigger time (seconds), age: time since trigger.
    t: f64,
    age: f64,
    vel: f64,
    /// v136: poly-AT pressure of this note, 0..1 (0 = released).
    at: f64,
    /// v136: closed-hat brightness pre-emphasis history (previous sample).
    hf_prev: f64,
    choked: bool,
    t_choke: f64,
    // Oscillator phases / incremental states.
    p1: f64, p2: f64, p3: f64, p4: f64, p5: f64, p6: f64,
    // Base frequency (tuned), and the metallic bank's fundamental.
    f0: f64,
    // DC-block highpass (two one-poles) state, per voice.
    hp_y1: f64, hp_x1: f64, hp_y2: f64, hp_x2: f64,
    /// Deterministic noise source.
    seed: u32,
    /// Bandpass for snare/clap noise.
    bq: Biquad,
}

struct Drums {
    sr: f64,
    voices: [Voice; VOICES],
    next: usize,
    /// v136: pitch bend (roll GLIDE lane), semitones (±12 at full).
    bend_st: f64,
}

impl Voice {
    const DEAD: Voice = Voice {
        alive: false, kind: 0, note: 0, t: 0.0, age: 0.0, vel: 0.0, at: 0.0,
        hf_prev: 0.0, choked: false, t_choke: 0.0,
        p1: 0.0, p2: 0.0, p3: 0.0, p4: 0.0, p5: 0.0, p6: 0.0,
        f0: 0.0, hp_y1: 0.0, hp_x1: 0.0, hp_y2: 0.0, hp_x2: 0.0,
        seed: 1, bq: Biquad { a0: 0.0, a1: 0.0, a2: 0.0, b1: 0.0, b2: 0.0,
                              x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 },
    };
    #[inline]
    fn noise(&mut self) -> f64 {
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.seed >> 8) as f64) / 8_388_608.0 - 1.0
    }
    #[inline]
    fn hp2(&mut self, x: f64, a: f64) -> f64 {
        let y1 = a * (self.hp_y1 + x - self.hp_x1);
        let y2 = a * (self.hp_y2 + y1 - self.hp_x2);
        self.hp_x1 = x; self.hp_y1 = y1;
        self.hp_x2 = y1; self.hp_y2 = y2;
        y2
    }
}

/// MIDI note -> (kind, base frequency).
fn kit_map(note: u8) -> (u8, f64) {
    match note {
        35 | 36 => (KICK, 46.0),
        38 | 40 => (SNARE, 188.0),
        37 | 39 => (CLAP, 1250.0),
        42 | 44 => (CHAT, 40.0),
        46 => (OHAT, 40.0),
        49 | 55 | 57 => (CRASH, 31.0),
        51 => (RIDE, 36.0),
        // Pitched tom fallback: f0 follows the note, quarter-register.
        _ => {
            let f = (440.0 * (2.0f64).powf((note as f64 - 60.0 - 24.0) / 12.0))
                .clamp(55.0, 400.0);
            (TOM, f)
        }
    }
}

extern "C" fn create(sr: f64, _host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let s = Box::new(Drums {
        sr: if sr > 1.0 { sr } else { 48000.0 },
        voices: [Voice::DEAD; VOICES],
        next: 0,
        bend_st: 0.0,
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Drums) });
    }
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let d = unsafe { &mut *(s as *mut Drums) };
    let sr = d.sr;
    let (kind, base) = kit_map(note);

    // Choke: an open hat / cymbal re-struck is damped, not stacked.
    for v in &mut d.voices {
        if v.alive && (v.kind == OHAT || v.kind == CRASH || v.kind == RIDE) {
            if kind == CHAT || v.note == note {
                v.choked = true;
                v.t_choke = v.t;
            }
        }
    }

    // Free voice, else steal the oldest.
    let mut idx = None;
    for k in 0..VOICES {
        if !d.voices[k].alive {
            idx = Some(k);
            break;
        }
    }
    let idx = idx.unwrap_or_else(|| {
        let mut best = d.next;
        let mut best_age = -1.0;
        for k in 0..VOICES {
            if d.voices[k].age > best_age {
                best_age = d.voices[k].age;
                best = k;
            }
        }
        d.next = (best + 1) % VOICES;
        best
    });

    let mut v = Voice::DEAD;
    v.alive = true;
    v.kind = kind;
    v.note = note;
    v.vel = ((vel.max(1) as f64) / 127.0).powf(1.3);
    v.seed = (note as u32).wrapping_mul(2654435761) ^ 0x9E3779B1;
    v.f0 = base;
    match kind {
        SNARE => v.bq = Biquad::bandpass(sr, 1750.0, 1.1),
        CLAP => v.bq = Biquad::bandpass(sr, 1250.0, 1.4),
        _ => {}
    }
    d.voices[idx] = v;
}

extern "C" fn note_off(s: *mut core::ffi::c_void, _ch: u8, note: u8) {
    if s.is_null() {
        return;
    }
    let d = unsafe { &mut *(s as *mut Drums) };
    // Choke open hats / cymbals on key release (kick/snare/toms ring out).
    for v in &mut d.voices {
        if v.alive && v.note == note
            && (v.kind == OHAT || v.kind == CRASH || v.kind == RIDE)
        {
            v.choked = true;
            v.t_choke = v.t;
        }
    }
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let d = unsafe { &mut *(s as *mut Drums) };
    for v in &mut d.voices {
        v.alive = false;
    }
}

extern "C" fn cc(_s: *mut core::ffi::c_void, _ch: u8, _num: u8, _val: u8) {}
extern "C" fn press(_s: *mut core::ffi::c_void, _ch: u8, _val: u8) {}

// --- v136 (card t_d28458f1): GLIDE lane (ABI v2/v3 bend). The host decodes
// the 14-bit bend / roll GLIDE lane to SEMITONES (full deflection = ±12) and
// calls this immediately before the next process() block; it rides the tune
// multiplier there, so held pitched voices bend exactly like the tune knob
// turns — 0 st is the untouched pre-v136 arithmetic.
#[no_mangle]
pub extern "C" fn bend(s: *mut core::ffi::c_void, _ch: u8, st: f32) {
    if s.is_null() {
        return;
    }
    unsafe { &mut *(s as *mut Drums) }.bend_st = st as f64;
}

// v136: MPE note pressure (raw MIDI byte 0..=127) per held note. Class law:
// ringing cymbals choke, hats/clap/snares brighten, kicks/toms take head
// tension. Voices that match nothing are untouched; 0 = pressure released
// (every law is exactly its pressure-less value there, never silence).
#[no_mangle]
pub extern "C" fn poly_at(s: *mut core::ffi::c_void, _ch: u8, note: u8, val: u8) {
    if s.is_null() {
        return;
    }
    let d = unsafe { &mut *(s as *mut Drums) };
    let sr = d.sr;
    let at = val as f64 / 127.0;
    for v in &mut d.voices {
        if v.alive && v.note == note {
            v.at = at;
            // Bright-law voices carry the pressure in their noise band.
            match v.kind {
                SNARE => v.bq.set_bandpass(sr, 1750.0 * (1.0 + 0.8 * at), 1.1),
                CLAP => v.bq.set_bandpass(sr, 1250.0 * (1.0 + 0.8 * at), 1.4),
                _ => {}
            }
        }
    }
}

/// Per-voice soft saturation (tanh-ish): compresses each hit's OWN
/// peaks toward |y| < 1/0.6 ≈ 1.67 before the sample enters the mix, so
/// one hot voice can never dominate. It does NOT limit the SUM — up to
/// VOICES shaped voices still stack past unity, and the engine's output
/// clipper is what keeps the final mix in range.
#[inline]
fn soft(x: f64) -> f64 {
    x / (1.0 + x.abs() * 0.6)
}

extern "C" fn process(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32) {
    if s.is_null() {
        return;
    }
    let d = unsafe { &mut *(s as *mut Drums) };
    let sr = d.sr;

    let gain = unsafe { *params.add(0) }.clamp(0.0, 1.5) as f64;
    let tune = unsafe { *params.add(1) }.clamp(-12.0, 12.0) as f64;
    let dec = unsafe { *params.add(2) }.clamp(0.25, 2.5) as f64;
    let l_snare = unsafe { *params.add(3) }.clamp(0.0, 1.5) as f64;
    let l_hat = unsafe { *params.add(4) }.clamp(0.0, 1.5) as f64;
    let l_tom = unsafe { *params.add(5) }.clamp(0.0, 1.5) as f64;
    let l_cymb = unsafe { *params.add(6) }.clamp(0.0, 1.5) as f64;

    // Pitch multiplier for the melodic voices AND the hats (a hat is a
    // tuned head too — the v136 header states this same law); only the
    // crash/ride inharmonic banks below are pitch-immune.
    // v136: the GLIDE lane rides this multiplier exactly like the tune
    // knob (0 st keeps the pre-v136 arithmetic bit-for-bit).
    let tmul = (2.0f64).powf((tune + d.bend_st) / 12.0);
    // One-pole DC-block coefficient (highpass ~6 kHz region of hats).
    let hpa = (-(TAU * 7200.0 / sr)).exp();
    let hpa = if hpa.is_finite() { hpa } else { 0.0 };

    for k in 0..n {
        for v in &mut d.voices {
            if !v.alive {
                continue;
            }
            let t = v.t;
            let choke = if v.choked {
                (-(t - v.t_choke).max(0.0) / 0.02).exp()
            } else {
                1.0
            };
            let mut y = 0.0f64;
            let mut lv = 1.0;
            match v.kind {
                KICK => {
                    let fs = v.f0 * tmul * (1.0 + 0.25 * v.at)
                        * (1.0 + 2.4 * (-t / 0.026).exp());
                    v.p1 += TAU * fs / sr;
                    let body = v.p1.sin();
                    let env = (-t / (0.34 * dec)).exp();
                    let click = v.noise() * (-t / 0.0035).exp();
                    y = (body + click * 0.5) * env;
                }
                SNARE => {
                    v.p1 += TAU * v.f0 * tmul * (1.0 + 0.25 * v.at) / sr;
                    let tone = v.p1.sin();
                    let tenv = (-t / (0.085 * dec)).exp();
                    let nenv = (-t / (0.16 * dec)).exp();
                    let nz_in = v.noise();
                    let nz = v.bq.run(nz_in);
                    y = (tone * 0.55 * tenv + nz * nenv) * 0.9;
                    lv = l_snare;
                }
                CLAP => {
                    // Three quick bursts then a tail, one shared bandpass.
                    let b = |t0: f64| {
                        if t > t0 { (-((t - t0)) / 0.012).exp() } else { 0.0 }
                    };
                    let env = b(0.0) + b(0.011) + b(0.023)
                        + (-t / (0.14 * dec)).exp() * 0.9;
                    let c_in = v.noise();
                    y = v.bq.run(c_in) * env;
                    lv = l_snare * 0.9;
                }
                CHAT | OHAT => {
                    // Six square partials at metallic inharmonic ratios.
                    let x = v.f0 * tmul;
                    v.p1 += TAU * x * 1.0000 / sr;
                    v.p2 += TAU * x * 1.2715 / sr;
                    v.p3 += TAU * x * 1.3576 / sr;
                    v.p4 += TAU * x * 1.6134 / sr;
                    v.p5 += TAU * x * 1.8892 / sr;
                    v.p6 += TAU * x * 2.1405 / sr;
                    let m = ((v.p1.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p2.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p3.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p4.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p5.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p6.sin() > 0.0) as i8 as f64 - 0.5);
                    let h = v.hp2(m / 3.0, hpa);
                    // v136: pressure brightens the CLOSED hat by pre-
                    // emphasis (the proven v134 brightness law — the DC-
                    // block corner is provably tilt-blind, measured); at
                    // 0 the path stays bit-identical (adds 0.0), and the
                    // open hat keeps its untouched metallic body.
                    let h = if v.kind == CHAT && v.at > 0.0 {
                        let e = h + 2.25 * v.at * (h - v.hf_prev);
                        v.hf_prev = h;
                        if e.is_finite() { e } else { h }
                    } else {
                        h
                    };
                    let tau = if v.kind == CHAT {
                        0.045 * dec
                    } else {
                        0.5 * dec / (1.0 + 3.0 * v.at) // v136: pressure chokes
                    };
                    let env = (-t / tau).exp();
                    y = h * env;
                    lv = l_hat;
                }
                TOM => {
                    let fs = v.f0 * tmul * (1.0 + 0.25 * v.at)
                        * (0.55 + 0.45 * (-t / 0.06).exp());
                    v.p1 += TAU * fs / sr;
                    let env = (-t / (0.32 * dec)).exp();
                    y = v.p1.sin() * env;
                    lv = l_tom;
                }
                CRASH | RIDE => {
                    // Metallic bank + noise; crash long, ride shorter/pingier.
                    let x = v.f0 * (1.0 + if v.kind == RIDE { 0.15 } else { 0.0 });
                    v.p1 += TAU * x * 1.0 / sr;
                    v.p2 += TAU * x * 1.41 / sr;
                    v.p3 += TAU * x * 1.89 / sr;
                    v.p4 += TAU * x * 2.56 / sr;
                    v.p5 += TAU * x * 3.83 / sr;
                    let m = ((v.p1.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p2.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p3.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p4.sin() > 0.0) as i8 as f64 - 0.5)
                        + ((v.p5.sin() > 0.0) as i8 as f64 - 0.5);
                    let nse = v.noise();
                    let h = v.hp2(m * 0.2 + nse * 0.5, hpa);
                    let tau = if v.kind == CRASH { 1.5 * dec } else { 0.7 * dec }
                        / (1.0 + 3.0 * v.at); // v136: pressure chokes
                    let env = (-t / tau).exp();
                    y = h * env * 0.8;
                    lv = l_cymb;
                }
                _ => v.alive = false,
            }

            v.t += 1.0 / sr;
            v.age += 1.0 / sr;

            // v137: retire at 7 time constants of the kind's SLOWEST
            // envelope component (e^-7 ≈ -61 dBFS) + FADE_S, with a linear
            // fade to exact zero inside that last FADE_S — so `alive =
            // false` lands on a silent sample and never chops the ring
            // (the old 1.5-3.2·tau gates cut a long crash at ~-13 dB).
            // Coefficients are 7·(tau/dec): tau = the *pressure-free*
            // constant; poly-AT only ever divides tau, so a pressure-blind
            // budget is never early, and a 20 ms choke always dies first.
            let budget = match v.kind {
                CRASH => 10.5 * dec + FADE_S, // tau = 1.5*dec
                RIDE => 4.9 * dec + FADE_S,   // tau = 0.7*dec
                OHAT => 3.5 * dec + FADE_S,   // tau = 0.5*dec
                KICK => 2.38 * dec + FADE_S,  // tau = 0.34*dec
                TOM => 2.24 * dec + FADE_S,   // tau = 0.32*dec
                SNARE => 1.12 * dec + FADE_S, // tau = 0.16*dec (noise tail)
                CLAP => 0.98 * dec + FADE_S,  // tau = 0.14*dec
                CHAT => 0.315 * dec + FADE_S, // tau = 0.045*dec
                _ => FADE_S,
            };
            let fade = if v.t > budget - FADE_S {
                ((budget - v.t) / FADE_S).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let smp = y * v.vel * lv * gain * choke * fade;
            if smp.is_finite() {
                // Append (the buffer arrives zeroed — the engine mixes in).
                unsafe { *out.add(k) += soft(smp) as f32 };
            }
            if v.t >= budget {
                v.alive = false;
            }
        }
    }
}

static PARAMS: [SynthParamV1; 7] = [
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.5, default: 0.9 },
    SynthParamV1 { label: c"tune".as_ptr(), min: -12.0, max: 12.0, default: 0.0 },
    SynthParamV1 { label: c"decay".as_ptr(), min: 0.25, max: 2.5, default: 1.0 },
    SynthParamV1 { label: c"snare".as_ptr(), min: 0.0, max: 1.5, default: 1.0 },
    SynthParamV1 { label: c"hat".as_ptr(), min: 0.0, max: 1.5, default: 0.75 },
    SynthParamV1 { label: c"tom".as_ptr(), min: 0.0, max: 1.5, default: 1.0 },
    SynthParamV1 { label: c"cymbal".as_ptr(), min: 0.0, max: 1.5, default: 0.6 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"drums".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Analog drum kit: kick/snare/hats/toms/cymbals from MIDI notes 35-57, pitched toms above.".as_ptr(),
    n_params: 7,
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

// v136: cumulative ABI v3 table (bend + poly_at) — the same add-only ladder
// as the sibling scripts; older hosts keep loading the frozen v1 DESC above.
static DESC_V3: SynthInfoV3 = SynthInfoV3 { abi: SLOOPER_SYNTH_ABI_V3, bend, poly_at };

#[no_mangle]
pub extern "C" fn slooper_synth_info_v3() -> *const SynthInfoV3 {
    &DESC_V3
}
