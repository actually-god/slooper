//! MIDI pattern assets.
//!
//! A *pattern* is a named bundle of musical events — notes (with per-note
//! pressure), control changes and channel pressure — stored in ticks
//! (pulses-per-quarter, PPQ) with an embedded tempo, so it is sample-rate
//! and frame-rate independent. Patterns are saved as Standard MIDI Files
//! (format 0) so they round-trip with any external editor; the clip
//! timeline still addresses them in samples via
//! [`Pattern::ticks_to_samples`].
//!
//! The clip model treats a pattern exactly like an audio asset: asset id →
//! pattern, clip `pos`/`cut`/`play_len` in samples. The engine bounces or
//! renders them live through the track's synth.

use std::path::Path;

/// Ticks per quarter note every pattern uses (SMF's universal default).
pub const PPQ: u16 = 480;

/// One note: start/length in ticks, MIDI pitch/velocity/channel, and a
/// per-note pressure byte (channel aftertouch captured while the note was
/// held; 0 = untouched).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Note {
    pub ch: u8,
    pub note: u8,
    pub vel: u8,
    pub press: u8,
    pub start: u32,
    pub len: u32,
    /// v101 (pb-2): index into [`Pattern::note_bends`] — the in-roll
    /// pitch curve drawn ON this note (spec docs/SAMPLER-PITCH.md § 5).
    /// Field-owned handle (NOT positional): moves/clones with the note;
    /// `merged_with` must offset the other pattern's handles.
    pub bend: Option<u32>,
}

/// A timed controller change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cc {
    pub ch: u8,
    pub num: u8,
    pub val: u8,
    pub at: u32,
}

/// A timed channel-pressure (0xD) message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Press {
    pub ch: u8,
    pub val: u8,
    pub at: u32,
}

/// A timed pitch-bend (0xE) message, v42/MPE. `val` is the raw 14-bit
/// unsigned value (0..=16383, center 8192) — exactly what the VST3 host
/// shim hands plugins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bend {
    pub ch: u8,
    pub val: u16,
    pub at: u32,
}

/// v52 R6: ONE editable pitch-bend segment — a glide from (t0,v0) to
/// (t1,v1), velocities in SEMITONES (x100 fixed point in the file),
/// `curve` −100..100 warps the value ramp: value = lerp(v0,v1,s'), where
/// s' = s + c·s(1−s), c = curve/100 (positive bulges EARLY — ease-out,
/// negative sags — ease-in; the warp is monotonic for |c| ≤ 1).
/// Segments are the EDITABLE source of truth; `Pattern::bake_bends`
/// samples them into the standard 14-bit `bends` lane the engine, the
/// VST shim and every external editor already understand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BendSeg {
    pub t0: u32,
    pub v0: f32,
    pub t1: u32,
    pub v1: f32,
    pub curve: f32,
}

/// Full-scale semitone span of a baked bend (standard receivers map the
/// 14-bit range to ±2 st; the raw value is scale-only, 12 st is our
/// editor's full deflection so glides of a whole octave reach full scale).
pub const BEND_FULL_ST: f32 = 12.0;

/// Bake one segment into raw 14-bit bends (dense: every `step` ticks).
pub fn bake_seg(seg: &BendSeg, step: u32, out: &mut Vec<Bend>) {
    let t1 = seg.t1.max(seg.t0 + 1);
    let steps = ((t1 - seg.t0) / step.max(1)).max(1);
    let c = (seg.curve / 100.0).clamp(-1.0, 1.0);
    for i in 0..=steps {
        let s = i as f32 / steps as f32;
        let sp = (s + c * s * (1.0 - s)).clamp(0.0, 1.0);
        let v = seg.v0 + (seg.v1 - seg.v0) * sp;
        let d = (v / BEND_FULL_ST * 8191.0).round().clamp(-8192.0, 8191.0) as i32;
        out.push(Bend {
            ch: 0,
            val: (8192 + d) as u16,
            at: seg.t0 + ((t1 - seg.t0) as f64 * s as f64).round() as u32,
        });
    }
}

/// Raw 14-bit value → semitones (editor space).
pub fn bend_to_st(val: u16) -> f32 {
    (val as f32 - 8192.0) / 8191.0 * BEND_FULL_ST
}

/// Semitones → raw 14-bit value.
pub fn st_to_bend(v: f32) -> u16 {
    let d = (v / BEND_FULL_ST * 8191.0).round().clamp(-8192.0, 8191.0) as i32;
    (8192 + d) as u16
}

/// v101 (pb-2, spec docs/SAMPLER-PITCH.md § 5): ONE in-roll pitch curve,
/// drawn ON a note — the table entry `Note::bend` indexes. Control points
/// `(t, cents)`: `t` is the fraction of the NOTE LENGTH (0..=1, so the
/// curve stretches with the note body), `cents` is the pitch offset from
/// the note's own row (±2400). Evaluated with monotone piecewise cubic
/// Hermite (PCHIP): 2 points degrade to EXACTLY the old linear sweep, and
/// strokes never overshoot their control values (vibrato-safe). Past the
/// last point the value HOLDS (808 slides stay put — no return-to-0).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct NoteBend {
    /// Control points, sorted by t, strictly increasing t (2..=16; the
    /// engine clamps, sorts and decimates on [`NoteBend::sanitize`]).
    pub pts: Vec<(f32, f32)>,
}

/// Point cap the engine enforces on top of the UI's (RDP simplification
/// is pb-3's; the engine still refuses to evaluate runaway tables).
pub const NB_MAX_PTS: usize = 16;
/// Full deflection of a note curve in cents (spec range decision).
pub const NB_MAX_CENTS: f32 = 2400.0;

/// Cents → playback frequency multiplier. Composes MULTIPLICATIVELY
/// after the note's base frequency and any detune (B_T13 lesson: bends
/// must compose with detune, never replace it).
pub fn cents_to_freq_mult(cents: f32) -> f32 {
    (cents * (1.0 / 1200.0)).exp2()
}

impl NoteBend {
    /// Sanitize + wrap a control-point list (what the UI/API hands in).
    pub fn new(pts: Vec<(f32, f32)>) -> Self {
        let mut nb = NoteBend { pts };
        nb.sanitize();
        nb
    }

    /// Clamp ranges, sort by t, drop duplicate/decreasing t (keeps the
    /// FIRST of a tie), decimate to [`NB_MAX_PTS`] keeping endpoints.
    pub fn sanitize(&mut self) {
        for p in self.pts.iter_mut() {
            p.0 = p.0.clamp(0.0, 1.0);
            p.1 = p.1.clamp(-NB_MAX_CENTS, NB_MAX_CENTS);
        }
        self.pts
            .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        self.pts.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-5);
        while self.pts.len() > NB_MAX_PTS {
            // Uniform-ish decimation: drop the middle point each pass.
            self.pts.remove(self.pts.len() / 2);
        }
    }

    /// Curve value in CENTS at note-fraction `t` (PCHIP; hold-first /
    /// hold-last outside the point span; empty = 0 = no bend).
    pub fn cents_at(&self, t: f32) -> f32 {
        let p = &self.pts;
        match p.len() {
            0 => 0.0,
            1 => p[0].1,
            // 2 points == the legacy linear sweep, bit-for-bit the lerp.
            2 => {
                if t <= p[0].0 {
                    return p[0].1;
                }
                if t >= p[1].0 {
                    return p[1].1;
                }
                p[0].1 + (p[1].1 - p[0].1) * ((t - p[0].0) / (p[1].0 - p[0].0))
            }
            _ => {
                let n = p.len().min(NB_MAX_PTS);
                if t <= p[0].0 {
                    return p[0].1;
                }
                if t >= p[n - 1].0 {
                    return p[n - 1].1;
                }
                // Secant slopes d[i] over [i, i+1].
                let mut d = [0f32; NB_MAX_PTS];
                for i in 0..n - 1 {
                    d[i] = (p[i + 1].1 - p[i].1) / (p[i + 1].0 - p[i].0);
                }
                // Segment k with p[k].0 < t <= p[k+1].0.
                let mut k = 0usize;
                while k + 1 < n - 1 && t > p[k + 1].0 {
                    k += 1;
                }
                // Fritsch–Carlson monotone tangents: simple ends, interior
                // zero at sign changes else weighted harmonic mean of the
                // adjacent secants — guarantees no overshoot.
                let mut m = [0f32; NB_MAX_PTS];
                m[0] = d[0];
                m[n - 1] = d[n - 2];
                for i in 1..n - 1 {
                    m[i] = if d[i - 1] * d[i] <= 0.0 {
                        0.0
                    } else {
                        let h0 = p[i].0 - p[i - 1].0;
                        let h1 = p[i + 1].0 - p[i].0;
                        let w1 = 2.0 * h1 + h0;
                        let w2 = h1 + 2.0 * h0;
                        (w1 + w2) / (w1 / d[i - 1] + w2 / d[i])
                    };
                }
                let h = p[k + 1].0 - p[k].0;
                let s = (t - p[k].0) / h;
                let s2 = s * s;
                let s3 = s2 * s;
                ((2.0 * s3 - 3.0 * s2 + 1.0) * p[k].1
                    + (s3 - 2.0 * s2 + s) * h * m[k]
                    + (-2.0 * s3 + 3.0 * s2) * p[k + 1].1
                    + (s3 - s2) * h * m[k + 1])
                    .clamp(-NB_MAX_CENTS, NB_MAX_CENTS)
            }
        }
    }

    /// True when a curve would actually sound (≥1 usable point).
    pub fn active(&self) -> bool {
        !self.pts.is_empty()
    }
}

/// A timed polyphonic aftertouch (0xA) message, v42/MPE: pressure for ONE
/// held note while other voices on the same channel stay unaffected — the
/// core per-voice expression message of MPE performance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Poly {
    pub ch: u8,
    pub note: u8,
    pub val: u8,
    pub at: u32,
}

/// A musical pattern in ticks. `bpm` is the embedded tempo (BPM × 1); the
/// UI always shows/edits it through the project tempo when recording.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Pattern {
    pub bpm: u16,
    /// Pattern length in ticks (drives clip span; ≥ one bar).
    pub len_ticks: u32,
    pub notes: Vec<Note>,
    pub ccs: Vec<Cc>,
    pub press: Vec<Press>,
    /// v42/MPE lanes: 14-bit bends and per-note aftertouch. Round-trip
    /// through the SMF file like every other lane.
    pub bends: Vec<Bend>,
    pub polys: Vec<Poly>,
    /// v52 R6: editable pitch-bend glides (the SOURCE; `bends` is their
    /// baked playback form — hand-written `bends` from an import survive
    /// until the first segment edit re-bakes the lane).
    /// v101 (pb-2): after load-time migration [`migrate_bend_segs`] these
    /// hold ONLY ORPHAN segments (no owning note — glides drawn into
    /// gaps); owned glides live on their note as [`Pattern::note_bends`].
    pub bend_segs: Vec<BendSeg>,
    /// v101 (pb-2): in-roll per-note pitch curves (spec § 5). Table is
    /// INDEXED BY `Note::bend`; entries unowned after a note removal are
    /// harmless (bake iterates notes) and compact away at save/load
    /// (see [`Pattern::compact_note_bends`]).
    pub note_bends: Vec<NoteBend>,
}

impl Pattern {
    pub fn new(bpm: u16) -> Self {
        Pattern {
            bpm: bpm.max(1),
            len_ticks: 4 * PPQ as u32,
            notes: Vec::new(),
            ccs: Vec::new(),
            press: Vec::new(),
            bends: Vec::new(),
            polys: Vec::new(),
            bend_segs: Vec::new(),
            note_bends: Vec::new(),
        }
    }

    /// v42: additive merge — every note / CC / press / bend / poly event of
    /// BOTH patterns lands in the result (nothing is replaced); the merged
    /// length is the longer pattern's, and the tempo stays `self`'s.
    pub fn merged_with(&self, other: &Pattern) -> Pattern {
        let mut out = self.clone();
        let self_notes = out.notes.len();
        // v101 (pb-2): the curve table is INDEXED BY Note::bend — two
        // patterns cannot share one table without offsetting. Append
        // other's curves and shift the APPENDED notes' handles by the
        // pre-extend base (the asset-ids-are-indices trap, spec § 5).
        let base = out.note_bends.len() as u32;
        out.note_bends.extend(other.note_bends.iter().cloned());
        out.notes.extend(other.notes.iter().copied());
        if base > 0 {
            for n in out.notes[self_notes..].iter_mut() {
                if let Some(i) = n.bend {
                    n.bend = Some(i + base);
                }
            }
        }
        out.ccs.extend(other.ccs.iter().copied());
        out.press.extend(other.press.iter().copied());
        out.bends.extend(other.bends.iter().copied());
        out.polys.extend(other.polys.iter().copied());
        out.bend_segs.extend(other.bend_segs.iter().copied());
        out.notes.sort_by_key(|n| (n.start, n.note));
        out.ccs.sort_by_key(|c| (c.at, c.num));
        out.press.sort_by_key(|p| (p.at, p.ch));
        out.bends.sort_by_key(|b| (b.at, b.ch));
        out.polys.sort_by_key(|p| (p.at, p.note));
        out.bend_segs.sort_by_key(|s| s.t0);
        out.len_ticks = out.len_ticks.max(other.len_ticks);
        out
    }

    /// v101: any note carries a usable in-roll pitch curve?
    pub fn has_note_bends(&self) -> bool {
        self.notes.iter().any(|n| {
            matches!(n.bend, Some(i) if self.note_bends.get(i as usize).is_some_and(|nb| nb.active()))
        })
    }

    /// v101 (pb-2, spec § 6): one-time load migration — every legacy
    /// glide-lane [`BendSeg`] whose body a note CONTAINS (1-tick
    /// tolerance) becomes that note's [`NoteBend`]; the legacy `curve`
    /// warp contributes one extra midpoint so 3 pts reproduce the warped
    /// shape; values beyond the note end CLAMP (per-note curves have no
    /// empty-time — documented edge). ORPHAN segs (no owning note, glides
    /// into gaps) stay in `bend_segs` untouched — the legacy strip keeps
    /// them and they keep sounding. NEVER touches the `bends` lane, so a
    /// migrated load renders EXACTLY the old audio until the first edit.
    /// Idempotent: run again after a migration it is a no-op.
    pub fn migrate_bend_segs(&mut self) {
        if self.bend_segs.is_empty() {
            return;
        }
        let mut keep: Vec<BendSeg> = Vec::new();
        let mut owned = 0usize;
        for seg in self.bend_segs.iter().copied() {
            // Owner = LAST note (max start) whose body contains t0.
            let owner = self
                .notes
                .iter()
                .filter(|n| (n.start as i64 - 1..=(n.start + n.len) as i64 + 1).contains(&(seg.t0 as i64)))
                .max_by_key(|n| n.start)
                .copied();
            let Some(on) = owner else {
                keep.push(seg); // orphan — legacy strip territory
                continue;
            };
            let oi = self
                .notes
                .iter()
                .position(|n| n.start == on.start && n.note == on.note && n.ch == on.ch && n.len == on.len)
                .unwrap();
            if self.notes[oi].bend.is_some() {
                // Note already carries a curve (file had both meta layers
                // — only hand-edited files do); curve wins, seg retires.
                owned += 1;
                continue;
            }
            let len = on.len.max(1) as f64;
            let end = (on.start + on.len) as f64;
            let t1c = (seg.t1 as f64).min(end); // CLAMP at note end (spec)
            let span = (seg.t1 - seg.t0).max(1) as f64;
            let c = (seg.curve / 100.0).clamp(-1.0, 1.0);
            let val_at = |t: f64| -> f32 {
                // f32 math EXACTLY like bake_seg (bit-identity of the
                // migrated re-bake against the legacy lane).
                let s = (((t - seg.t0 as f64) / span) as f32).clamp(0.0, 1.0);
                let sp = (s + c * s * (1.0 - s)).clamp(0.0, 1.0);
                seg.v0 + (seg.v1 - seg.v0) * sp
            };
            let norm = |t: f64| -> f32 {
                ((t - on.start as f64) / len).clamp(0.0, 1.0) as f32
            };
            let mut pts = vec![
                (norm(seg.t0 as f64), seg.v0 * 100.0),
                (norm(t1c), val_at(t1c) * 100.0),
            ];
            if c.abs() >= 0.005 {
                // Warp midpoint: at the UNWARPED middle tick, warped value.
                let mid = seg.t0 as f64 + span * 0.5;
                if mid > seg.t0 as f64 && mid < t1c {
                    pts.insert(1, (norm(mid), val_at(mid) * 100.0));
                }
            }
            let nb = NoteBend::new(pts);
            if nb.pts.len() < 2 {
                keep.push(seg); // degenerate overlap — leave legacy intact
                continue;
            }
            self.note_bends.push(nb);
            self.notes[oi].bend = Some((self.note_bends.len() - 1) as u32);
            owned += 1;
        }
        if owned > 0 {
            self.bend_segs = keep;
        }
    }

    /// v101: drop curve entries no live note owns, remapping the
    /// remaining handles (compaction; shared handles stay shared).
    pub fn compact_note_bends(&mut self) {
        let mut map = vec![u32::MAX; self.note_bends.len()];
        let mut live: Vec<NoteBend> = Vec::new();
        for n in &mut self.notes {
            let Some(i) = n.bend else { continue };
            let i = i as usize;
            if i >= self.note_bends.len() || self.note_bends[i].pts.is_empty() {
                n.bend = None;
                continue;
            }
            if map[i] == u32::MAX {
                map[i] = live.len() as u32;
                live.push(self.note_bends[i].clone());
            }
            n.bend = Some(map[i]);
        }
        self.note_bends = live;
    }

    /// v52 R6: rebuild the raw `bends` lane from the editable segments
    /// (segments are then authoritative). Call after ANY seg edit.
    /// v101 (pb-2): per-note [`NoteBend`] curves join the same lane
    /// (single truth for VST feed, MIDI export, scripted ABI v2 and the
    /// roll audition — everything downstream already understands bends).
    /// Curves sample dense every 8 ticks across their own point span,
    /// exactly like [`bake_seg`]; on overlap the LAST-ONSET note wins
    /// each tick (spec § 5); nothing returns to 0 after a note ends
    /// (808 slides stay put, same as the old lane). A pattern with NO
    /// curves takes the old code path UNCHANGED (back-compat pins).
    pub fn bake_bends(&mut self) {
        if !self.has_note_bends() {
            if self.bend_segs.is_empty() {
                // v102 (pb-3): no source left = the audible lane must go
                // silent too — a bare `return` kept the LAST baked curve
                // sounding after its owner note/curve was deleted.
                self.bends.clear();
                return;
            }
            let mut out = Vec::new();
            for seg in self.bend_segs.iter() {
                bake_seg(seg, 8, &mut out);
            }
            out.sort_by_key(|b| b.at);
            self.bends = out;
            return;
        }
        // tick -> (onset of the winning source, raw 14-bit value).
        let mut lane: std::collections::BTreeMap<u32, (u32, u16)> = Default::default();
        let put = |lane: &mut std::collections::BTreeMap<u32, (u32, u16)>,
                       at: u32,
                       onset: u32,
                       val: u16| {
            match lane.get(&at) {
                Some(&(o, _)) if o > onset => {} // earlier onset never trumps later
                _ => {
                    lane.insert(at, (onset, val));
                }
            }
        };
        // Legacy (orphan) segments first — same math as the old path.
        for seg in self.bend_segs.iter() {
            let t1 = seg.t1.max(seg.t0 + 1);
            let steps = ((t1 - seg.t0) / 8).max(1);
            let c = (seg.curve / 100.0).clamp(-1.0, 1.0);
            for i in 0..=steps {
                let s = i as f32 / steps as f32;
                let sp = (s + c * s * (1.0 - s)).clamp(0.0, 1.0);
                let v = seg.v0 + (seg.v1 - seg.v0) * sp;
                let at = seg.t0 + ((t1 - seg.t0) as f64 * s as f64).round() as u32;
                put(&mut lane, at, seg.t0, st_to_bend(v));
            }
        }
        // Note curves in onset order (later onsets overwrite on conflict).
        let mut items: Vec<(u32, usize, u32, u32)> = Vec::new(); // (onset, curve idx, note start, note len)
        for n in self.notes.iter() {
            if let Some(i) = n.bend {
                if self.note_bends.get(i as usize).is_some_and(|nb| nb.active()) {
                    items.push((n.start, i as usize, n.start, n.len));
                }
            }
        }
        items.sort();
        for (_, ci, ns, nl) in items {
            let nb = &self.note_bends[ci];
            let len = nl.max(1) as f32;
            let ts = nb.pts[0].0;
            let te = nb.pts[nb.pts.len() - 1].0;
            let t0 = (ns as f32 + ts * len).round() as u32;
            let t1 = ((ns as f32 + te * len).round() as u32).max(t0 + 1);
            let steps = ((t1 - t0) / 8).max(1);
            for i in 0..=steps {
                let s = i as f32 / steps as f32;
                let at = t0 + ((t1 - t0) as f64 * s as f64).round() as u32;
                let tn = ((at as f32 - ns as f32) / len).clamp(0.0, 1.0);
                // pts are CENTS; the lane speaks semitones.
                put(&mut lane, at, ns, st_to_bend(nb.cents_at(tn) * 0.01));
            }
        }
        self.bends = lane
            .iter()
            .map(|(at, (_, v))| Bend { ch: 0, val: *v, at: *at })
            .collect();
    }

    /// v52 R6: an imported file's raw bends (no `slooper-bnd` meta)
    /// become a polyline of curve-0 segments so the lane can display and
    /// edit them. Bends are left untouched until the first edit re-bakes.
    pub fn segs_from_bends(&mut self) {
        let mut bs = self.bends.clone();
        bs.sort_by_key(|b| b.at);
        let mut segs = Vec::new();
        for w in bs.windows(2) {
            if w[0].at == w[1].at {
                continue;
            }
            segs.push(BendSeg {
                t0: w[0].at,
                v0: bend_to_st(w[0].val),
                t1: w[1].at,
                v1: bend_to_st(w[1].val),
                curve: 0.0,
            });
        }
        self.bend_segs = segs;
    }

    /// True when nothing sounds.
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
            && self.ccs.is_empty()
            && self.press.is_empty()
            && self.bends.is_empty()
            && self.polys.is_empty()
            && self.bend_segs.is_empty()
    }

    /// Samples per tick at `bpm` and `rate` (f64 to survive long patterns).
    pub fn spb(&self, rate: u32) -> f64 {
        60.0 / self.bpm.max(1) as f64 * rate as f64
    }

    pub fn ticks_to_samples(&self, ticks: u32, rate: u32) -> u64 {
        (self.spb(rate) * ticks as f64 / PPQ as f64).round() as u64
    }

    pub fn samples_to_ticks(&self, samples: u64, rate: u32) -> u32 {
        (samples as f64 * PPQ as f64 / self.spb(rate)).round() as u32
    }

    /// Clip length of the whole pattern, in samples.
    pub fn len_samples(&self, rate: u32) -> u64 {
        self.ticks_to_samples(self.len_ticks, rate)
    }

    /// Round the length up to the next whole bar (4/4 at PPQ) and drop
    /// events beyond it.
    pub fn quantize_grid(&mut self, step_ticks: u32, snap: bool) {
        let step = step_ticks.max(1);
        if snap {
            for n in &mut self.notes {
                n.start = n.start.div_euclid(step) * step;
                n.len = (n.len.div_euclid(step).max(1)) * step;
            }
        }
        let end = self
            .notes
            .iter()
            .map(|n| n.start + n.len)
            .chain(self.ccs.iter().map(|c| c.at))
            .chain(self.press.iter().map(|p| p.at))
            .max()
            .unwrap_or(0);
        let bar = 4 * PPQ as u32;
        self.len_ticks = end.div_euclid(bar).max(1) * bar + (end % bar != 0) as u32 * bar;
        self.len_ticks = self.len_ticks.max(bar);
    }

    /// Sort by tick (stable order inside a tick is insertion order).
    pub fn sort(&mut self) {
        self.notes.sort_by_key(|n| (n.start, n.note));
        self.ccs.sort_by_key(|c| c.at);
        self.press.sort_by_key(|p| p.at);
    }

    // ------------------------------------------------------------------
    // Standard MIDI File (format 0) I/O
    // ------------------------------------------------------------------

    /// Write a format-0 SMF: tempo meta, notes, CCs, channel pressure,
    /// plus a private text meta carrying the pattern length (SMF has no
    /// length concept of its own; without it a re-load would trim a
    /// deliberate tail of silence).
    pub fn save_mid(&self, path: &Path) -> Result<(), String> {
        let mut tr: Vec<u8> = Vec::new();
        // Tempo: microseconds per quarter note.
        let us = (60_000_000u32 / self.bpm.max(1) as u32)
            .max(1)
            .to_be_bytes();
        tr.extend_from_slice(&vlq(0));
        // SMF tempo = 3 big-endian bytes (drop the top byte, keep [1..4]).
        tr.extend_from_slice(&[0xFF, 0x51, 0x03, us[1], us[2], us[3]]);
        // Private length meta.
        let txt = format!("slooper-len {}", self.len_ticks);
        tr.extend_from_slice(&vlq(0));
        tr.push(0xFF);
        tr.push(0x01);
        tr.extend_from_slice(&vlq(txt.len() as u32));
        tr.extend_from_slice(txt.as_bytes());
        // v52 R6: private bend-segment meta — the EDITABLE curve layer
        // (the baked 0xE events below stay the portable playback form).
        if !self.bend_segs.is_empty() {
            let mut s = String::from("slooper-bnd");
            for g in &self.bend_segs {
                s.push_str(&format!(
                    " {}:{}:{}:{}:{}",
                    g.t0,
                    (g.v0 * 100.0).round() as i32,
                    g.t1,
                    (g.v1 * 100.0).round() as i32,
                    g.curve.round() as i32,
                ));
            }
            tr.extend_from_slice(&vlq(0));
            tr.push(0xFF);
            tr.push(0x01);
            tr.extend_from_slice(&vlq(s.len() as u32));
            tr.extend_from_slice(s.as_bytes());
        }
        // v101 (pb-2): private note-curve meta — in-roll pitch curves.
        // One whitespace-separated entry per OWNING note:
        //   <ch>:<pitch>:<start>:<t>x1000:<cents>x100(@...)*
        // t in 1/1000 note-length units, cents in 1/100 cent (exact
        // round-trip at UI-representable resolutions). Entries unowned
        // in RAM simply never get written → the table self-compacts.
        {
            let mut s = String::new();
            for n in &self.notes {
                let Some(i) = n.bend else { continue };
                let Some(nb) = self.note_bends.get(i as usize) else { continue };
                if !nb.active() {
                    continue;
                }
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&format!(
                    "{}:{}:{}:",
                    n.ch & 0xF,
                    n.note & 0x7F,
                    n.start
                ));
                for (k, p) in nb.pts.iter().enumerate() {
                    if k > 0 {
                        s.push('@');
                    }
                    s.push_str(&format!(
                        "{}:{}",
                        (p.0 * 1000.0).round() as u32,
                        (p.1 * 100.0).round() as i32
                    ));
                }
            }
            if !s.is_empty() {
                let s = format!("slooper-npb {s}");
                tr.extend_from_slice(&vlq(0));
                tr.push(0xFF);
                tr.push(0x01);
                tr.extend_from_slice(&vlq(s.len() as u32));
                tr.extend_from_slice(s.as_bytes());
            }
        }

        // Flatten to (tick, event-bytes) and diff-sort.
        let mut ev: Vec<(u32, Vec<u8>)> = Vec::new();
        for n in &self.notes {
            ev.push((
                n.start,
                vec![0x90 | n.ch & 0xF, n.note & 0x7F, n.vel.clamp(1, 127)],
            ));
            ev.push((
                (n.start + n.len.max(1)).min(u32::MAX - 1),
                vec![0x80 | n.ch & 0xF, n.note & 0x7F, n.vel / 2],
            ));
        }
        for c in &self.ccs {
            ev.push((c.at, vec![0xB0 | c.ch & 0xF, c.num & 0x7F, c.val & 0x7F]));
        }
        for p in &self.press {
            ev.push((p.at, vec![0xD0 | p.ch & 0xF, p.val & 0x7F]));
        }
        // v42/MPE lanes: 0xE carries LSB then MSB (7-bit halves), 0xA is
        // (note, pressure) — standard SMF voice encodings.
        for b in &self.bends {
            ev.push((
                b.at,
                vec![
                    0xE0 | b.ch & 0xF,
                    (b.val & 0x7F) as u8,
                    ((b.val >> 7) & 0x7F) as u8,
                ],
            ));
        }
        for p in &self.polys {
            ev.push((p.at, vec![0xA0 | p.ch & 0xF, p.note & 0x7F, p.val & 0x7F]));
        }
        ev.sort_by_key(|(t, b)| (*t, b[0] >> 4, b[1]));
        let mut last = 0u32;
        for (t, b) in ev {
            let t = t.max(last);
            tr.extend_from_slice(&vlq(t - last));
            tr.extend_from_slice(&b);
            last = t;
        }
        tr.extend_from_slice(&vlq(0));
        tr.extend_from_slice(&[0xFF, 0x2F, 0x00]);

        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"MThd");
        out.extend_from_slice(&6u32.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 1]); // format 0, 1 track
        out.extend_from_slice(&PPQ.to_be_bytes());
        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(tr.len() as u32).to_be_bytes());
        out.extend_from_slice(&tr);
        std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parse a format-0/1 SMF. Notes are matched on→off (a retrigger of the
    /// same pitch ends the previous voice). Tempo comes from the first set
    /// tempo meta (default 120). Channel pressure landing inside a held
    /// note also stamps that note's pressure byte. Text meta
    /// `slooper-len <ticks>` restores a saved length.
    pub fn load_mid(path: &Path) -> Result<Pattern, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let b = bytes.as_slice();
        if b.len() < 14 || &b[0..4] != b"MThd" {
            return Err("not a SMF file (missing MThd)".into());
        }
        let ntrk = u16::from_be_bytes([b[10], b[11]]) as usize;
        let mut pat = Pattern::new(120);
        // v101: slooper-npb parses before notes exist — park links here,
        // resolve after the track loop once every note has its final
        // (ch, pitch, start) key.
        let mut nb_links: Vec<(u8, u8, u32, u32)> = Vec::new();
        let mut pos = 14usize;
        for tk in 0..ntrk {
            if pos + 8 > b.len() || &b[pos..pos + 4] != b"MTrk" {
                if tk == 0 {
                    return Err("missing MTrk chunk".into());
                }
                break;
            }
            let len = u32::from_be_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
            let mut p = pos + 8;
            let end = (p + len).min(b.len());
            let mut tick = 0u32;
            let mut running = 0u8;
            // note → index of currently-held voice
            let mut held: std::collections::HashMap<(u8, u8), usize> = Default::default();
            while p < end {
                let (dt, np) = match read_vlq(b, p) {
                    Some(v) => v,
                    None => break,
                };
                p = np;
                tick = tick.wrapping_add(dt);
                if p >= end {
                    break;
                }
                let mut st = b[p];
                if st < 0x80 {
                    st = running; // running status
                } else {
                    p += 1;
                }
                if p >= end {
                    // Truncated chunk: status with no room for its payload.
                    break;
                }
                if st == 0xFF {
                    // Meta: type + length + data.
                    let mt = b[p];
                    p += 1;
                    let (ml, np) = read_vlq(b, p).ok_or("truncated meta")?;
                    p = np;
                    let data = &b[p.min(end)..(p + ml as usize).min(end)];
                    p += ml as usize;
                    match mt {
                        0x51 if data.len() == 3 => {
                            let us = u32::from_be_bytes([0, data[0], data[1], data[2]]);
                            pat.bpm = (60_000_000 / us.max(1)).clamp(1, u16::MAX as u32) as u16;
                        }
                        0x01 => {
                            let txt = String::from_utf8_lossy(data).into_owned();
                            if let Some(rest) = txt.strip_prefix("slooper-len ") {
                                if let Ok(v) = rest.trim().parse::<u32>() {
                                    pat.len_ticks = v;
                                }
                            } else if let Some(rest) = txt.strip_prefix("slooper-bnd") {
                                // v52 R6: editable bend segments.
                                for g in rest.split_whitespace() {
                                    let f: Vec<&str> = g.split(':').collect();
                                    if f.len() == 5 {
                                        if let (Ok(t0), Ok(v0), Ok(t1), Ok(v1), Ok(cv)) = (
                                            f[0].parse::<u32>(),
                                            f[1].parse::<i32>(),
                                            f[2].parse::<u32>(),
                                            f[3].parse::<i32>(),
                                            f[4].parse::<i32>(),
                                        ) {
                                            pat.bend_segs.push(BendSeg {
                                                t0,
                                                v0: v0 as f32 / 100.0,
                                                t1,
                                                v1: v1 as f32 / 100.0,
                                                curve: cv as f32,
                                            });
                                        }
                                    }
                                }
                            } else if let Some(rest) = txt.strip_prefix("slooper-npb") {
                                // v101 (pb-2): in-roll per-note pitch curves.
                                for g in rest.split_whitespace() {
                                    let f: Vec<&str> = g.splitn(4, ':').collect();
                                    if f.len() != 4 {
                                        continue;
                                    }
                                    let (Ok(ch), Ok(pitch), Ok(start)) = (
                                        f[0].parse::<u8>(),
                                        f[1].parse::<u8>(),
                                        f[2].parse::<u32>(),
                                    ) else {
                                        continue;
                                    };
                                    let mut pts = Vec::new();
                                    for q in f[3].split('@') {
                                        let t: Vec<&str> = q.split(':').collect();
                                        if t.len() != 2 {
                                            continue;
                                        }
                                        if let (Ok(tt), Ok(cc)) =
                                            (t[0].parse::<u32>(), t[1].parse::<i32>())
                                        {
                                            pts.push((tt as f32 / 1000.0, cc as f32 / 100.0));
                                        }
                                    }
                                    let nb = NoteBend::new(pts);
                                    if !nb.active() {
                                        continue;
                                    }
                                    pat.note_bends.push(nb);
                                    nb_links.push((
                                        ch & 0xF,
                                        pitch & 0x7F,
                                        start,
                                        (pat.note_bends.len() - 1) as u32,
                                    ));
                                }
                            }
                        }
                        0x2F => break, // end of track
                        _ => {}
                    }
                    continue;
                }
                if st == 0xF0 || st == 0xF7 {
                    // SysEx: skip payload.
                    let (ml, np) = read_vlq(b, p).ok_or("truncated sysex")?;
                    p = np + ml as usize;
                    continue;
                }
                let hi = st & 0xF0;
                let ch = st & 0x0F;
                match hi {
                    0x90 | 0x80 => {
                        if p + 2 > end {
                            break;
                        }
                        let (note, vel) = (b[p] & 0x7F, b[p + 1]);
                        p += 2;
                        if hi == 0x90 && vel > 0 {
                            running = st;
                            if let Some(prev) = held.remove(&(ch, note)) {
                                pat.notes[prev].len = tick - pat.notes[prev].start;
                            }
                            pat.notes.push(Note {
                                ch,
                                note,
                                vel,
                                press: 0,
                                start: tick,
                                len: 1,
                            bend: None,});
                            held.insert((ch, note), pat.notes.len() - 1);
                        } else if let Some(prev) = held.remove(&(ch, note)) {
                            running = st;
                            pat.notes[prev].len = (tick - pat.notes[prev].start).max(1);
                        }
                    }
                    0xB0 => {
                        if p + 2 > end {
                            break;
                        }
                        pat.ccs.push(Cc {
                            ch,
                            num: b[p],
                            val: b[p + 1],
                            at: tick,
                        });
                        p += 2;
                        running = st;
                    }
                    0xD0 => {
                        if p >= end {
                            break;
                        }
                        pat.press.push(Press {
                            ch,
                            val: b[p],
                            at: tick,
                        });
                        p += 1;
                        running = st;
                    }
                    0xC0 => {
                        // Program change: one data byte.
                        if p + 1 > end {
                            break;
                        }
                        p += 1;
                        running = st;
                    }
                    0xE0 => {
                        // Pitch bend: TWO data bytes (LSB, MSB). Consuming
                        // one desynced the stream — every event after the
                        // first bend in a file parsed as garbage.
                        // v42/MPE: captured into the bend lane (14-bit).
                        if p + 2 > end {
                            break;
                        }
                        pat.bends.push(Bend {
                            ch,
                            val: ((b[p + 1] as u16) << 7) | (b[p] as u16),
                            at: tick,
                        });
                        p += 2;
                        running = st;
                    }
                    0xA0 => {
                        // Poly aftertouch: two data bytes. v42/MPE: kept as
                        // a per-note pressure lane.
                        if p + 2 > end {
                            break;
                        }
                        pat.polys.push(Poly {
                            ch,
                            note: b[p] & 0x7F,
                            val: b[p + 1] & 0x7F,
                            at: tick,
                        });
                        p += 2;
                        running = st;
                    }
                    0xF0 => {
                        // System common/realtime: nothing here belongs to
                        // the pattern model; skip the right number of
                        // payload bytes so the stream stays aligned.
                        let extra: usize = match st {
                            0xF2 => 2,
                            0xF3 => 1,
                            _ => 0,
                        };
                        if p + extra > end {
                            break;
                        }
                        p += extra;
                    }
                    _ => break,
                }
            }
            // Any note still held at end-of-track rings to the pattern end.
            for idx in held.values() {
                let n = &mut pat.notes[*idx];
                n.len = pat.len_ticks.saturating_sub(n.start).max(1);
            }
            pos = end;
        }
        // Stamp each note with the LAST channel pressure at/under its start
        // (that is the pressure the finger was holding it with).
        for n in &mut pat.notes {
            if let Some(p) = pat
                .press
                .iter()
                .rev()
                .find(|p| p.ch == n.ch && p.at <= n.start)
            {
                n.press = p.val;
            }
        }
        // v101 (pb-2): attach parsed curves to their owning notes by the
        // exact (ch, pitch, start) key (the meta is written per OWNING
        // note, so the key is the author identity; first match wins).
        for (ch, pitch, start, idx) in nb_links.iter().copied() {
            if let Some(n) = pat
                .notes
                .iter_mut()
                .find(|n| n.ch == ch && n.note == pitch && n.start == start && n.bend.is_none())
            {
                n.bend = Some(idx);
            }
        }
        // v52 R6: file finalization — an editable meta layer is
        // authoritative (re-baked from segments); a foreign file's raw
        // bends get polyline segments so the lane can display/edit them.
        // v101 (pb-2, spec § 6): slooper-native segment layers migrate
        // ONCE into per-note curves here (orphans stay); the bake then
        // reproduces the SAME 14-bit lane (2-pt PCHIP == the old lerp on
        // the same 8-tick grid), so old projects load audio-identical.
        // A native CURVE file (slooper-npb) is also NOT a foreign
        // import: its raw 0xE lane is its own baked form — no polyline.
        if !pat.bend_segs.is_empty() {
            pat.migrate_bend_segs();
            pat.bake_bends();
        } else if !pat.bends.is_empty() && pat.note_bends.is_empty() {
            pat.segs_from_bends();
        }
        pat.sort();
        if pat.len_ticks == 0
            || pat.len_ticks < pat.notes.iter().map(|n| n.start + n.len).max().unwrap_or(0)
        {
            pat.quantize_grid(1, false);
        }
        Ok(pat)
    }
}

fn vlq(mut v: u32) -> Vec<u8> {
    let mut out = vec![(v & 0x7F) as u8];
    v >>= 7;
    while v > 0 {
        out.push(((v & 0x7F) as u8) | 0x80);
        v >>= 7;
    }
    out.reverse();
    out
}

fn read_vlq(b: &[u8], mut p: usize) -> Option<(u32, usize)> {
    let mut v = 0u32;
    loop {
        let x = *b.get(p)?;
        v = (v << 7) | (x & 0x7F) as u32;
        p += 1;
        if x & 0x80 == 0 {
            return Some((v, p));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v42: merged_with layers BOTH patterns additively — every event
    /// survives, lanes stay sorted, the longer length wins.
    #[test]
    fn v42_merged_with_layers_every_lane() {
        let mut a = Pattern::new(120);
        a.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 90,
            press: 0,
            start: 0,
            len: 96,
        bend: None,});
        a.bends.push(Bend {
            ch: 0,
            val: 8192,
            at: 10,
        });
        let mut b = Pattern::new(120);
        b.len_ticks = 8 * 4 * PPQ as u32;
        b.notes.push(Note {
            ch: 1,
            note: 64,
            vel: 70,
            press: 0,
            start: 24,
            len: 48,
        bend: None,});
        b.polys.push(Poly {
            ch: 0,
            note: 60,
            val: 33,
            at: 5,
        });
        let m = a.merged_with(&b);
        assert_eq!(m.notes.len(), 2, "both notes keep sounding");
        assert_eq!(m.notes[0].note, 60);
        assert_eq!(m.notes[1].note, 64);
        assert_eq!(m.bends.len(), 1);
        assert_eq!(m.polys.len(), 1);
        assert_eq!(m.len_ticks, b.len_ticks, "the longer pattern sets length");
    }

    #[test]
    fn pattern_roundtrips_through_smf() {
        let mut p = Pattern::new(96);
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 40,
            start: 0,
            len: 240,
        bend: None,});
        p.notes.push(Note {
            ch: 1,
            note: 67,
            vel: 80,
            press: 0,
            start: 240,
            len: 240,
        bend: None,});
        p.ccs.push(Cc {
            ch: 0,
            num: 7,
            val: 90,
            at: 120,
        });
        p.press.push(Press {
            ch: 1,
            val: 40,
            at: 200,
        });
        p.len_ticks = 960;
        let dir = std::env::temp_dir().join("sp-midipat-rt");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pat.mid");
        p.save_mid(&f).unwrap();
        let q = Pattern::load_mid(&f).unwrap();
        assert_eq!(q.bpm, 96);
        assert_eq!(q.len_ticks, 960);
        assert_eq!(q.notes.len(), 2);
        let n0 = q.notes.iter().find(|n| n.note == 60).unwrap();
        assert_eq!((n0.vel, n0.start, n0.len), (100, 0, 240));
        // Channel pressure (ch 1 @ 200) stamps the note held on that channel.
        let n1 = q.notes.iter().find(|n| n.note == 67).unwrap();
        assert_eq!(n1.press, 40, "channel pressure stamped onto the held note");
        assert!(q.ccs.iter().any(|c| c.num == 7 && c.val == 90));
    }

    #[test]
    fn tick_sample_conversion_tracks_tempo() {
        let p = Pattern::new(120);
        // One quarter note at 120 BPM = 0.5 s = 24000 samples @48k.
        assert_eq!(p.ticks_to_samples(PPQ as u32, 48_000), 24_000);
        assert_eq!(p.samples_to_ticks(24_000, 48_000), PPQ as u32);
    }

    #[test]
    fn quantize_snaps_and_extends_to_whole_bars() {
        let mut p = Pattern::new(120);
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 90,
            press: 0,
            start: 100,
            len: 300,
        bend: None,});
        p.quantize_grid(PPQ as u32 / 4, true);
        let n = &p.notes[0];
        assert_eq!(n.start % (PPQ as u32 / 4), 0);
        assert_eq!(n.len % (PPQ as u32 / 4), 0);
        assert_eq!(p.len_ticks % (4 * PPQ as u32), 0);
    }

    #[test]
    fn smf_pitch_bend_and_syscommon_keep_the_stream_aligned() {
        // Track: C-3 on; PITCH BEND (2 bytes, LSB/MSB); C#3 on via
        // RUNNING status / off; poly-aftertouch; a system-realtime byte.
        let mut trk: Vec<u8> = Vec::new();
        trk.extend_from_slice(&[0x00, 0x90, 60, 100]); // C-3 on
        trk.extend_from_slice(&[0x00, 0xE0, 0x40, 0x20]); // pitch bend (2 bytes!)
                                                          // explicit note-on at 16 (a mis-sized pitch-bend skip would
                                                          // consume THIS status byte), then running-status note-off
        trk.extend_from_slice(&[0x10, 0x90, 61, 100]);
        trk.extend_from_slice(&[0x00, 61, 0x00]);
        trk.extend_from_slice(&[0x00, 0xA0, 60, 50]); // poly aftertouch (2 bytes)
        trk.extend_from_slice(&[0x00, 0xFA]); // timing clock (realtime, 0 bytes)
        trk.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]); // end of track
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(b"MThd");
        b.extend_from_slice(&6u32.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 1, 0x01, 0xE0]); // fmt 0, 1 track, 480 PPQ
        b.extend_from_slice(b"MTrk");
        b.extend_from_slice(&(trk.len() as u32).to_be_bytes());
        b.extend_from_slice(&trk);
        let dir = std::env::temp_dir().join("sp-smfalign.mid");
        std::fs::write(&dir, &b).unwrap();
        let pat = Pattern::load_mid(&dir).expect("parse");
        let notes: Vec<(u8, u32, u32)> =
            pat.notes.iter().map(|n| (n.note, n.start, n.len)).collect();
        assert_eq!(
            notes,
            vec![(60, 0, 4 * PPQ as u32), (61, 16, 1)],
            "pitch bend must consume exactly its two payload bytes \
             (one-byte consumption desynced every following event)"
        );
        // v42/MPE: the bend and poly-AT messages in this file are kept.
        assert_eq!(
            pat.bends,
            vec![Bend {
                ch: 0,
                val: 0x20 << 7 | 0x40,
                at: 0
            }],
            "pitch bend captured into the bend lane (14-bit)"
        );
        assert_eq!(
            pat.polys,
            vec![Poly {
                ch: 0,
                note: 60,
                val: 50,
                at: 0x10
            }],
            "poly aftertouch captured into the poly lane"
        );
    }

    /// v42/MPE: both new lanes must survive a save/load round trip.
    #[test]
    fn v42_mpe_lanes_roundtrip_through_smf() {
        let mut p = Pattern::new(120);
        p.bends.push(Bend {
            ch: 3,
            val: 8392,
            at: 100,
        });
        p.bends.push(Bend {
            ch: 3,
            val: 0,
            at: 200,
        });
        p.polys.push(Poly {
            ch: 5,
            note: 64,
            val: 77,
            at: 150,
        });
        let f = std::env::temp_dir().join(format!("sp-mpe-rt-{}.mid", std::process::id()));
        p.save_mid(&f).unwrap();
        let q = Pattern::load_mid(&f).unwrap();
        assert_eq!(q.bends, p.bends, "bend lane round trip");
        assert_eq!(q.polys, p.polys, "poly lane round trip");
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn v52_bend_seg_bake_curve_and_linearity() {
        // A straight (curve 0) glide bakes its midpoint exactly; a +100
        // curve bulges EARLY (ease-out), a -100 curve SAGS (ease-in),
        // and the bake is monotonic (no wiggle) at full curvature.
        let mut pat = Pattern::new(120);
        pat.bend_segs.push(BendSeg {
            t0: 0,
            v0: 0.0,
            t1: 480,
            v1: 6.0,
            curve: 0.0,
        });
        pat.bake_bends();
        assert!(!pat.bends.is_empty());
        assert_eq!(pat.bends.first().unwrap().val, st_to_bend(0.0));
        assert_eq!(pat.bends.last().unwrap().val, st_to_bend(6.0));
        let at = |p: &Pattern, t: u32| {
            p.bends
                .iter()
                .min_by_key(|b| (b.at as i64 - t as i64).abs())
                .unwrap()
                .val
        };
        assert!((bend_to_st(at(&pat, 240)) - 3.0).abs() < 0.1, "linear mid");
        let mut early = pat.clone();
        early.bend_segs[0].curve = 100.0;
        early.bake_bends();
        assert!(
            bend_to_st(at(&early, 120)) > 2.0,
            "curve +100 must bulge early, got {:.2} st",
            bend_to_st(at(&early, 120))
        );
        let mut late = pat.clone();
        late.bend_segs[0].curve = -100.0;
        late.bake_bends();
        assert!(
            bend_to_st(at(&late, 120)) < 1.2,
            "curve -100 must sag, got {:.2} st",
            bend_to_st(at(&late, 120))
        );
        for w in early.bends.windows(2) {
            assert!(w[1].val >= w[0].val, "bake must stay monotonic");
        }
    }

    #[test]
    fn v52_bend_segs_roundtrip_through_mid() {
        // v101 CONTRACT CHANGE (spec § 6): an OWNED segment now MIGRATES
        // to its note's pitch-curve on load — the file format is
        // unchanged (slooper-bnd still written for un-migrated/legacy
        // readers, the baked 0xE lane carries the sound for everyone).
        // Orphan segs (no containing note) keep the OLD contract: the
        // editable layer survives verbatim and re-bakes equal.
        let mut p = Pattern::new(120);
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
        bend: None,});
        p.bend_segs.push(BendSeg {
            t0: 300,
            v0: 0.0,
            t1: 400,
            v1: -3.5,
            curve: 65.0,
        });
        p.bake_bends();
        let f = std::env::temp_dir().join(format!("sp-v52bnd-{}.mid", std::process::id()));
        p.save_mid(&f).unwrap();
        let q = Pattern::load_mid(&f).unwrap();
        assert_eq!(q.bend_segs.len(), 1, "orphan editable layer survives the file");
        let g = &q.bend_segs[0];
        assert_eq!((g.t0, g.t1), (300, 400));
        assert!(g.v0.abs() < 0.011 && (g.v1 + 3.5).abs() < 0.011);
        assert!((g.curve - 65.0).abs() < 0.51);
        // The meta is authoritative: the playback lane re-bakes equal.
        assert_eq!(q.bends.len(), p.bends.len(), "rebaked lane matches");
        // And a seg that LIVES INSIDE a note now arrives as a curve.
        let mut p2 = Pattern::new(120);
        p2.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
            bend: None,
        });
        p2.bend_segs.push(BendSeg {
            t0: 20,
            v0: 0.0,
            t1: 200,
            v1: -2.0,
            curve: 0.0,
        });
        let f2 = std::env::temp_dir().join(format!("sp-v52bnd2-{}.mid", std::process::id()));
        p2.save_mid(&f2).unwrap();
        let q2 = Pattern::load_mid(&f2).unwrap();
        std::fs::remove_file(&f).ok();
        std::fs::remove_file(&f2).ok();
        assert!(q2.bend_segs.is_empty(), "owned seg migrated away");
        assert!(q2.notes[0].bend.is_some(), "…into the owning note's curve");
        assert!(!q2.bends.is_empty(), "lane re-derived for old-app playback");
    }

    #[test]
    fn v52_bend_import_raw_bends_become_polyline_segs() {
        // A foreign file's raw 0xE events (no slooper-bnd meta) load as
        // curve-0 polyline segments — editable immediately, and the
        // PLAYBACK lane keeps the ORIGINAL events until an edit re-bakes.
        let mut p = Pattern::new(120);
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 90,
            press: 0,
            start: 0,
            len: 480,
        bend: None,});
        p.bends.push(Bend {
            ch: 0,
            val: st_to_bend(2.0),
            at: 96,
        });
        p.bends.push(Bend {
            ch: 0,
            val: st_to_bend(-1.0),
            at: 192,
        });
        let f = std::env::temp_dir().join(format!("sp-v52bndi-{}.mid", std::process::id()));
        p.save_mid(&f).unwrap();
        let q = Pattern::load_mid(&f).unwrap();
        assert_eq!(q.bends.len(), 2, "raw bends untouched");
        assert_eq!(q.bend_segs.len(), 1, "raw bends became one polyline seg");
        let g = &q.bend_segs[0];
        assert_eq!((g.t0, g.t1), (96, 192));
        assert!((bend_to_st(st_to_bend(2.0)) - g.v0).abs() < 0.01);
        assert!((g.v1 + 1.0).abs() < 0.011);
        assert_eq!(g.curve, 0.0);
        std::fs::remove_file(&f).ok();
    }

    // ------------------------------------------------------------------
    // v101 (pb-2): per-note pitch curves — model, PCHIP, derived bake,
    // migration, remap, SMF round-trip. Spec: docs/SAMPLER-PITCH.md §5-6.
    // Every pin is sabotage-verified: the assertion FAILS when the
    // behavior under test is broken (both polarities where meaningful).
    // ------------------------------------------------------------------

    #[test]
    fn v101_pchip_two_points_are_exactly_linear_and_hold() {
        let c = NoteBend::new(vec![(0.0, 0.0), (1.0, 1200.0)]);
        // Exact lerp everywhere between, hold-first/hold-last outside.
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            assert!((c.cents_at(t) - t * 1200.0).abs() < 1e-3, "t {t}");
        }
        assert_eq!(c.cents_at(-0.5), 0.0, "hold first");
        assert_eq!(c.cents_at(3.0), 1200.0, "hold last — 808 slides stay put");
        // The engine's freq mult composes multiplicatively (B_T13).
        assert!((cents_to_freq_mult(1200.0) - 2.0).abs() < 1e-6);
        assert!((cents_to_freq_mult(-1200.0) - 0.5).abs() < 1e-6);
        assert!((cents_to_freq_mult(0.0) - 1.0).abs() < 1e-9);
        assert!((cents_to_freq_mult(100.0) - 2f32.powf(1.0 / 12.0)).abs() < 1e-5);
    }

    #[test]
    fn v101_pchip_never_overshoots_its_control_values() {
        // Vibrato-ish zig-zag: every segment is monotone, so a curve
        // that overshoots between points is the classic Catmull-Rom bug.
        let c = NoteBend::new(vec![
            (0.0, 0.0),
            (0.2, 500.0),
            (0.4, -500.0),
            (0.6, 1200.0),
            (0.8, -1200.0),
            (1.0, 0.0),
        ]);
        // Piecewise bounds: between control points the value must stay
        // within that segment's [min(y), max(y)].
        let mut i = 0;
        while i + 1 < c.pts.len() {
            let (a, b) = (c.pts[i], c.pts[i + 1]);
            let lo = a.1.min(b.1);
            let hi = a.1.max(b.1);
            for k in 0..=40 {
                let t = a.0 + (b.0 - a.0) * k as f32 / 40.0;
                let v = c.cents_at(t);
                assert!(
                    v >= lo - 1e-3 && v <= hi + 1e-3,
                    "overshoot at t {t}: {v} outside [{lo},{hi}]"
                );
            }
            i += 1;
        }
        // Control points are hit EXACTLY (a curve must render as drawn).
        for p in &c.pts {
            assert!((c.cents_at(p.0) - p.1).abs() < 1e-3);
        }
    }

    #[test]
    fn v101_sanitize_clamps_sorts_dedupes_and_caps() {
        // Unsorted, out-of-range, duplicated t, over-cap count.
        let mut pts = vec![(0.5, 9999.0), (0.0, 0.0), (0.5, -50.0)];
        for i in 1..40 {
            pts.push((i as f32 / 60.0, 10.0));
        }
        let c = NoteBend::new(pts);
        assert!(c.pts.len() <= NB_MAX_PTS, "cap enforced");
        for w in c.pts.windows(2) {
            assert!(w[0].0 < w[1].0, "strictly increasing t: {:?}", c.pts);
        }
        for p in &c.pts {
            assert!(p.0 >= 0.0 && p.0 <= 1.0, "t clamped");
            assert!(p.1 >= -NB_MAX_CENTS && p.1 <= NB_MAX_CENTS, "cents clamped");
        }
        // Empty curve = no bend, no panic.
        assert_eq!(NoteBend::default().cents_at(0.5), 0.0);
    }

    #[test]
    fn v101_curve_bakes_dense_on_the_lane_and_holds_last() {
        // A -12 st -> +12 st slide over a 480-tick note: dense 8-tick
        // grid, first sample -full-scale, last +full-scale, and NO
        // return-to-0 sample past the note end (hold-last semantics).
        let mut p = Pattern::new(120);
        let mut n = Note {
            ch: 0,
            note: 36,
            vel: 100,
            press: 0,
            start: 0,
            len: 480,
            bend: None,
        };
        p.note_bends.push(NoteBend::new(vec![(0.0, -1200.0), (1.0, 1200.0)]));
        n.bend = Some(0);
        p.notes.push(n);
        p.bake_bends();
        assert!(!p.bends.is_empty(), "curve must populate the lane");
        assert_eq!(p.bends.first().unwrap().at, 0);
        assert_eq!(p.bends.first().unwrap().val, st_to_bend(-12.0), "starts at -full");
        assert_eq!(p.bends.last().unwrap().at, 480);
        assert_eq!(p.bends.last().unwrap().val, st_to_bend(12.0), "ends at +full");
        let mid = p.bends.iter().find(|b| b.at == 240).unwrap();
        assert_eq!(mid.val, st_to_bend(0.0), "midpoint crosses zero");
        assert_eq!(p.bends.len(), 61, "dense 8-tick grid, same density as bake_seg");
        // Monotonic — no wobble, no post-note return samples.
        for w in p.bends.windows(2) {
            assert!(w[0].val < w[1].val, "monotonic sweep, ticks {}->{}", w[0].at, w[1].at);
            assert_eq!(w[1].at - w[0].at, 8);
        }
    }

    #[test]
    fn v101_overlapping_curves_last_onset_wins() {
        // Note A (onset 0) sweeps DOWN, note B (onset 128) sweeps UP.
        // Where both cover a tick, the LATER onset (B) owns the lane.
        let mut p = Pattern::new(120);
        p.note_bends.push(NoteBend::new(vec![(0.0, -1200.0), (1.0, -1200.0)]));
        p.note_bends.push(NoteBend::new(vec![(0.0, 1200.0), (1.0, 1200.0)]));
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 480,
            bend: Some(0),
        });
        p.notes.push(Note {
            ch: 0,
            note: 64,
            vel: 100,
            press: 0,
            start: 128,
            len: 480,
            bend: Some(1),
        });
        p.bake_bends();
        // In the overlap (ticks 128..480) the lane must sit at B's +full.
        for b in p.bends.iter().filter(|b| b.at >= 136 && b.at <= 472) {
            assert_eq!(b.val, st_to_bend(12.0), "tick {}: last-onset B must own", b.at);
        }
        // Before B's onset A owns it (sabotage polarity: would also be
        // true if the merge dropped everything — pin both sides).
        let a_own = p.bends.iter().find(|b| b.at == 64).unwrap();
        assert_eq!(a_own.val, st_to_bend(-12.0), "A owns pre-B territory");
    }

    #[test]
    fn v101_migrate_moves_owned_segs_and_keeps_orphans_lane_untouched() {
        // note 0..240 owns seg 20..220; orphan seg 300..400 sits in a gap.
        let mut p = Pattern::new(120);
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
            bend: None,
        });
        p.bend_segs.push(BendSeg {
            t0: 20,
            v0: 0.0,
            t1: 220,
            v1: 5.0,
            curve: 0.0,
        });
        p.bend_segs.push(BendSeg {
            t0: 300,
            v0: -2.0,
            t1: 400,
            v1: 0.0,
            curve: 0.0,
        });
        // A second note carries a WARPED owned seg (curve +100): the
        // legacy warp must land as an extra midpoint (2->3 pts).
        p.notes.push(Note {
            ch: 0,
            note: 65,
            vel: 100,
            press: 0,
            start: 480,
            len: 240,
            bend: None,
        });
        p.bend_segs.push(BendSeg {
            t0: 500,
            v0: 0.0,
            t1: 700,
            v1: 10.0,
            curve: 100.0,
        });
        p.bake_bends();
        let lane_before = p.bends.clone();
        p.migrate_bend_segs();
        assert_eq!(p.bends, lane_before, "migration NEVER touches the audio lane");
        assert_eq!(p.bend_segs.len(), 1, "orphan seg survives in the legacy strip");
        assert_eq!(p.bend_segs[0].t0, 300);
        assert!(p.notes[0].bend.is_some(), "owning note gained the curve");
        let c = &p.note_bends[p.notes[0].bend.unwrap() as usize];
        assert_eq!(c.pts.len(), 2, "curve-0 seg -> linear 2-pt curve");
        assert!((c.pts[0].0 - 20.0 / 240.0).abs() < 1e-4);
        assert!((c.pts[0].1 - 0.0).abs() < 0.5);
        assert!((c.pts[1].0 - 220.0 / 240.0).abs() < 1e-4);
        assert!((c.pts[1].1 - 500.0).abs() < 0.5, "cents = st x100");
        // Warped owned seg -> 3 pts carrying the warp's extra midpoint.
        assert!(p.notes[1].bend.is_some(), "warped seg migrated too");
        let cw = &p.note_bends[p.notes[1].bend.unwrap() as usize];
        assert_eq!(cw.pts.len(), 3, "curve-100 warp adds a midpoint");
        assert!((cw.pts[1].0 - 0.5).abs() < 1e-4, "midpoint at mid tick");
        assert!((cw.pts[1].1 - 750.0).abs() < 1.0, "warped value at midpoint (s=.5 -> sp=.75)");
        // The WARP really bulges: linear would sit at 500 cents there.
        assert!((cw.cents_at(0.5) - 500.0).abs() > 50.0, "warp shape survives, not flattened");
        // Idempotent: second run is a no-op (no owner-less segs match).
        let (segs2, curves2) = (p.bend_segs.clone(), p.note_bends.clone());
        p.migrate_bend_segs();
        assert_eq!(p.bend_segs, segs2);
        assert_eq!(p.note_bends, curves2);
    }

    #[test]
    fn v101_migrated_linear_seg_rebakes_bit_identical_lane() {
        // THE back-compat pin (spec § 6 + acceptance "bit-identical"): a
        // contained integer-semitone sweep, migrated then re-baked, must
        // produce the EXACT same 14-bit lane as the legacy bake_seg.
        let mut p = Pattern::new(120);
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 480,
            bend: None,
        });
        p.bend_segs.push(BendSeg {
            t0: 0,
            v0: 0.0,
            t1: 480,
            v1: -3.5,
            curve: 0.0,
        });
        p.bake_bends();
        let legacy_lane = p.bends.clone();
        assert_eq!(legacy_lane.len(), 61);
        p.migrate_bend_segs();
        p.bake_bends();
        assert_eq!(p.bends, legacy_lane, "migrated re-bake must be bit-identical");
    }

    #[test]
    fn v101_note_bends_roundtrip_through_mid() {
        // Curves survive save → load: pts exact at file resolution, the
        // (ch,pitch,start) link re-attaches, and the re-baked lane is
        // STABLE across the reload (idempotent bake).
        let mut p = Pattern::new(120);
        p.note_bends.push(NoteBend::new(vec![
            (0.0, 0.0),
            (0.25, -300.0),
            (0.75, 600.0),
            (1.0, 1200.0),
        ]));
        p.notes.push(Note {
            ch: 1,
            note: 55,
            vel: 90,
            press: 0,
            start: 96,
            len: 320,
            bend: Some(0),
        });
        p.notes.push(Note {
            ch: 1,
            note: 57,
            vel: 90,
            press: 0,
            start: 480,
            len: 96,
            bend: None,
        });
        p.bake_bends();
        let before = p.bends.clone();
        assert!(!before.is_empty());
        let f = std::env::temp_dir().join(format!("sp-v101rt-{}.mid", std::process::id()));
        p.save_mid(&f).unwrap();
        let q = Pattern::load_mid(&f).unwrap();
        std::fs::remove_file(&f).ok();
        assert_eq!(q.note_bends.len(), 1, "one curve survives");
        let c = &q.note_bends[0];
        let orig = &p.note_bends[0];
        assert_eq!(c.pts.len(), orig.pts.len());
        for (a, b) in c.pts.iter().zip(orig.pts.iter()) {
            assert!((a.0 - b.0).abs() <= 0.0005, "t resolution 1/1000");
            assert!((a.1 - b.1).abs() <= 0.011, "cents resolution 1/100");
        }
        let n55 = q.notes.iter().find(|n| n.note == 55).unwrap();
        let n57 = q.notes.iter().find(|n| n.note == 57).unwrap();
        assert_eq!(n55.bend, Some(0), "curve re-linked to its author note");
        assert_eq!(n57.bend, None, "plain note stays plain");
        assert!(q.bend_segs.is_empty(), "native curve file is NOT a foreign import (no polyline bloat)");
        assert_eq!(q.bends, before, "lane is stable across the round-trip");
        // And the meta is INVISIBLE to bend-less patterns (back-compat).
        let mut r = Pattern::new(120);
        r.notes.push(Note {
            ch: 0,
            note: 48,
            vel: 80,
            press: 0,
            start: 0,
            len: 120,
            bend: None,
        });
        let f2 = std::env::temp_dir().join(format!("sp-v101pl-{}.mid", std::process::id()));
        r.save_mid(&f2).unwrap();
        let raw = std::fs::read(&f2).unwrap();
        std::fs::remove_file(&f2).ok();
        assert!(
            !raw.windows(11).any(|w| w == b"slooper-npb"),
            "no curve meta written for curve-less patterns"
        );
    }

    #[test]
    fn v101_merge_offsets_curve_handles() {
        // The asset-ids-are-indices trap: merging must shift other's
        // handles by self's table length, else curves cross-wire.
        let mut a = Pattern::new(120);
        a.note_bends.push(NoteBend::new(vec![(0.0, -600.0), (1.0, -600.0)]));
        a.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 90,
            press: 0,
            start: 0,
            len: 240,
            bend: Some(0),
        });
        let mut b = Pattern::new(120);
        b.note_bends.push(NoteBend::new(vec![(0.0, 600.0), (1.0, 600.0)]));
        b.notes.push(Note {
            ch: 0,
            note: 64,
            vel: 90,
            press: 0,
            start: 0,
            len: 240,
            bend: Some(0),
        });
        let mut m = a.merged_with(&b);
        assert_eq!(m.note_bends.len(), 2, "both tables merged");
        let n60 = m.notes.iter().find(|n| n.note == 60).unwrap();
        let n64 = m.notes.iter().find(|n| n.note == 64).unwrap();
        assert_eq!(n60.bend, Some(0));
        assert_eq!(n64.bend, Some(1), "other's handle was OFFSET, not copied");
        assert!((m.note_bends[n64.bend.unwrap() as usize].cents_at(0.5) - 600.0).abs() < 0.5,
            "handle resolves to ITS OWN curve (cross-wire sabotage check)");
        m.bake_bends();
        // Last-onset-wins on total overlap: equal onsets, sorted-stable
        // later entry (n64) takes the lane — just prove BOTH curves'
        // values appear and the lane is sound.
        assert!(!m.bends.is_empty());
    }

    #[test]
    fn v101_remove_note_and_compact_close_the_handle() {
        // Deleting the bent note must (a) leave its curve OUT of the
        // audible lane, (b) let compaction drop the entry and leave the
        // OTHER note's handle pointing at ITS curve.
        let mut p = Pattern::new(120);
        p.note_bends.push(NoteBend::new(vec![(0.0, -600.0), (1.0, -600.0)]));
        p.note_bends.push(NoteBend::new(vec![(0.0, 900.0), (1.0, 900.0)]));
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 90,
            press: 0,
            start: 0,
            len: 240,
            bend: Some(0),
        });
        p.notes.push(Note {
            ch: 0,
            note: 64,
            vel: 90,
            press: 0,
            start: 480,
            len: 240,
            bend: Some(1),
        });
        p.bake_bends();
        let lane_both = p.bends.len();
        // Remove the first note (what the UI delete sites do).
        p.notes.remove(0);
        if p.has_note_bends() {
            p.bake_bends();
        }
        p.compact_note_bends();
        assert_eq!(p.note_bends.len(), 1, "orphaned entry compacted away");
        assert_eq!(p.notes[0].bend, Some(0), "survivor's handle REMAPPED");
        assert!(
            (p.note_bends[0].cents_at(0.5) - 900.0).abs() < 0.5,
            "remapped handle still resolves to the SURVIVOR'S curve"
        );
        p.bake_bends();
        // Lane lost the -600 region entirely: every lane sample is the
        // +900 st level now (sabotage: forgetting bake leaves -600 tails).
        assert!(p.bends.len() < lane_both, "lane shrank with the deleted curve");
        for b in p.bends.iter().filter(|b| b.at >= 480) {
            assert_eq!(b.val, st_to_bend(9.0), "post-survival lane is pure survivor");
        }
        assert!(
            !p.bends.iter().take_while(|b| b.at < 480).any(|b| (b.val as i32 - st_to_bend(-6.0) as i32).abs() < 4),
            "deleted note's curve fully GONE from the lane"
        );
    }

    #[test]
    fn v101_legacy_segs_only_bake_path_untouched() {
        // No curves anywhere -> bake_bends takes the UNCHANGED legacy
        // path: byte-identical to calling bake_seg directly.
        let mut p = Pattern::new(120);
        p.bend_segs.push(BendSeg {
            t0: 48,
            v0: 1.25,
            t1: 300,
            v1: -2.0,
            curve: -40.0,
        });
        p.bake_bends();
        let mut direct = Vec::new();
        bake_seg(&p.bend_segs[0], 8, &mut direct);
        direct.sort_by_key(|b| b.at);
        assert_eq!(p.bends, direct, "legacy path must not change AT ALL");
    }
}
