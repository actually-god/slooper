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
        out.notes.sort_by_key(|n| (n.start, n.note));
        out.ccs.sort_by_key(|c| (c.at, c.num));
        out.press.sort_by_key(|p| (p.at, p.ch));
        out.bends.sort_by_key(|b| (b.at, b.ch));
        out.polys.sort_by_key(|p| (p.at, p.note));
        out.len_ticks = out.len_ticks.max(other.len_ticks);
        out
    }

    /// True when nothing sounds.
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
            && self.ccs.is_empty()
            && self.press.is_empty()
            && self.bends.is_empty()
            && self.polys.is_empty()
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
                            if let Some(rest) =
                                String::from_utf8_lossy(data).strip_prefix("slooper-len ")
                            {
                                if let Ok(v) = rest.trim().parse::<u32>() {
                                    pat.len_ticks = v;
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
}
