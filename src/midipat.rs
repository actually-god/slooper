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
    pub bend_segs: Vec<BendSeg>,
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
        }
    }

    /// v42: additive merge — every note / CC / press / bend / poly event of
    /// BOTH patterns lands in the result (nothing is replaced); the merged
    /// length is the longer pattern's, and the tempo stays `self`'s.
    pub fn merged_with(&self, other: &Pattern) -> Pattern {
        let mut out = self.clone();
        out.notes.extend(other.notes.iter().copied());
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

    /// v52 R6: rebuild the raw `bends` lane from the editable segments
    /// (segments are then authoritative). Call after ANY seg edit.
    pub fn bake_bends(&mut self) {
        if self.bend_segs.is_empty() {
            return;
        }
        let mut out = Vec::new();
        for seg in self.bend_segs.iter() {
            bake_seg(seg, 8, &mut out);
        }
        out.sort_by_key(|b| b.at);
        self.bends = out;
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
                            });
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
        // v52 R6: file finalization — an editable meta layer is
        // authoritative (re-baked from segments); a foreign file's raw
        // bends get polyline segments so the lane can display/edit them.
        if !pat.bend_segs.is_empty() {
            pat.bake_bends();
        } else if !pat.bends.is_empty() {
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
        });
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
        });
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
        });
        p.notes.push(Note {
            ch: 1,
            note: 67,
            vel: 80,
            press: 0,
            start: 240,
            len: 240,
        });
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
        });
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
        let mut p = Pattern::new(120);
        p.notes.push(Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
        });
        p.bend_segs.push(BendSeg {
            t0: 100,
            v0: 0.0,
            t1: 400,
            v1: -3.5,
            curve: 65.0,
        });
        p.bake_bends();
        let f = std::env::temp_dir().join(format!("sp-v52bnd-{}.mid", std::process::id()));
        p.save_mid(&f).unwrap();
        let q = Pattern::load_mid(&f).unwrap();
        assert_eq!(q.bend_segs.len(), 1, "editable layer survives the file");
        let g = &q.bend_segs[0];
        assert_eq!((g.t0, g.t1), (100, 400));
        assert!(g.v0.abs() < 0.011 && (g.v1 + 3.5).abs() < 0.011);
        assert!((g.curve - 65.0).abs() < 0.51);
        // The meta is authoritative: the playback lane re-bakes equal.
        assert_eq!(q.bends.len(), p.bends.len(), "rebaked lane matches");
        std::fs::remove_file(&f).ok();
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
        });
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
}
