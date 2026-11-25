// v164 SPIKE (card t_0c73850c, edo-1): hosted-plugin pitch-bend physics.
//
// Question the EDO plugin path depends on: does a plugin LATCH a pitch bend
// that arrives in the same block immediately BEFORE its note-on (sounding
// the note detuned from its very first sample), and does it still glide a
// held note when the bend arrives AFTER? Measures onset-inclusive
// zero-crossing Hz for four cases and prints DIAG lines.
//
//   cargo run --example edo_bend_spike -- <path> <V2|V3>
//   PROBE_CENTS=50 PROBE_RANGE=2 (bend range announced, default ±2)
use std::path::Path;

use slooper::vsthost::{open, StageEntry, VstKind};

const SR: f64 = 48_000.0;
const BLK: usize = 512;

/// Absolute-window zero-crossing Hz with a small hysteresis; the window
/// starts AFTER `skip_s` so attack transients and plugin latency are out.
fn zc_hz(x: &[f32], skip_s: f64) -> Option<f64> {
    let skip = (skip_s * SR) as usize;
    if x.len() <= skip + 4 {
        return None;
    }
    let seg = &x[skip..];
    let peak = seg.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak < 0.005 {
        return None;
    }
    let thr = peak * 0.15;
    let mut state = 0i8; // -1 below, +1 above
    let mut cross = 0u64;
    for &v in seg {
        if v > thr && state != 1 {
            if state == -1 {
                cross += 1;
            }
            state = 1;
        } else if v < -thr && state != -1 {
            if state == 1 {
                cross += 1;
            }
            state = -1;
        }
    }
    // A full period = 2 crossings.
    Some((cross as f64 / 2.0) / (seg.len() as f64 / SR))
}

fn run_case(
    entry: &slooper::vsthost::RtStageEntry,
    name: &str,
    note: u8,
    pre_bend: Option<u16>, // raw 14-bit arriving BEFORE the note-on
    post_bend: Option<(u16, usize)>, // raw + block index AFTER onset
) {
    entry.all_notes_off();
    entry.flush();
    let sil = vec![0f32; BLK];
    let mut out = vec![0f32; BLK];
    let mut all: Vec<f32> = Vec::new();
    // Silence tail so flush/latency pushes settle.
    for _ in 0..8 {
        entry.process_with_midi(&sil, &mut out, &[]);
        all.extend_from_slice(&out);
    }
    let mut ev: Vec<(usize, [u8; 3])> = Vec::new();
    // Normalize the wheel state a PREVIOUS case may have left parked:
    // send center (8192) when this case has no pre-bend, so `control` and
    // `post_bend` start from the wheel's zero, not the last case's value.
    let pre = pre_bend.unwrap_or(8192);
    ev.push((0, [0xE0, (pre & 0x7F) as u8, (pre >> 7) as u8]));
    ev.push((8, [0x90, note, 110]));
    let n_blocks = 48;
    for b in 0..n_blocks {
        ev.clear();
        if b == 0 {
            ev.push((0, [0xE0, (pre & 0x7F) as u8, (pre >> 7) as u8]));
            ev.push((8, [0x90, note, 110]));
        }
        if let Some((v, at)) = post_bend {
            if b == at {
                ev.push((8, [0xE0, (v & 0x7F) as u8, (v >> 7) as u8]));
            }
        }
        entry.process_with_midi(&sil, &mut out, &ev);
        all.extend_from_slice(&out);
    }
    // Measure L channel. TWO windows: early (right after onset+latency —
    // catches PRE-note latching) and tail (last third — catches glide that
    // arrives after onset). A pre-latched case reads equal early+tail.
    let l: Vec<f32> = all.chunks(2).map(|c| c[0]).collect();
    let early = zc_hz(&l, 0.25);
    let tstart = 0.25 + (l.len() as f64 / SR - 0.25) * 0.66;
    let l2: Vec<f32> = l[(tstart * SR) as usize..].to_vec();
    let tail = zc_hz(&l2, 0.0);
    let peak = l.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let f = |h: Option<f64>| match h {
        Some(h) => format!("{h:.2}"),
        None => "SILENT".to_string(),
    };
    println!("DIAG {name}: early_hz={} tail_hz={} peak_dbfs={:.1}", f(early), f(tail), 20.0 * peak.max(1e-12).log10());
    std::thread::sleep(std::time::Duration::from_millis(120));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = Path::new(&args[1]);
    let kind = if args.get(2).map(|s| s.as_str()) == Some("V2") {
        VstKind::V2
    } else {
        VstKind::V3
    };
    let cents: f64 = std::env::var("PROBE_CENTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50.0);
    let range: u8 = std::env::var("PROBE_RANGE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let note: u8 = std::env::var("PROBE_NOTE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let load = match open(path, kind, SR as u32, None) {
        Ok(l) => l,
        Err(e) => {
            println!("LOADFAIL {e}");
            std::process::exit(2);
        }
    };
    let entry_arc = load.entry;
    let Some(entry) = entry_arc
        .as_any()
        .downcast_ref::<slooper::vsthost::RtStageEntry>()
    else {
        println!("DIAG no RtStageEntry");
        std::process::exit(2);
    };
    entry.set_bend_range(range);
    // ROLL-SPACE law (midipat ±12 st full scale): the vsthost queue()
    // choke point rescales this into the announced ±range channel scale,
    // exactly like every real engine feed. (The first spike version
    // pre-divided by `range` here — double scaling + u16 wrap = garbage.)
    let st = cents / 100.0;
    let raw = slooper::midipat::st_to_bend(st as f32);
    let raw_neg = slooper::midipat::st_to_bend(-st as f32);
    println!(
        "SPIKE {path:?} cents={cents} range={range} note={note} raw={raw} raw_neg={raw_neg} expect_ratio={:.5}",
        2f64.powf(cents / 1200.0)
    );
    println!("DIAG entry bend_range={} drops_before={}", entry.bend_range(), entry.midi_drops());
    run_case(entry, "control", note, None, None);
    println!("DIAG drops_after_control={}", entry.midi_drops());
    run_case(entry, "pre_bend", note, Some(raw), None);
    println!("DIAG drops_after_pre_bend={}", entry.midi_drops());
    run_case(entry, "pre_bend_neg", note, Some(raw_neg), None);
    run_case(entry, "post_bend", note, None, Some((raw, 4)));
    println!("DIAG drops_final={}", entry.midi_drops());
    entry.all_notes_off();
    std::mem::forget(entry_arc.clone());
    println!("SPIKE DONE");
    std::process::exit(0);
}
