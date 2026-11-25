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
    if let Some(v) = pre_bend {
        ev.push((0, [0xE0, (v & 0x7F) as u8, (v >> 7) as u8]));
    }
    ev.push((8, [0x90, note, 110]));
    let n_blocks = 48;
    for b in 0..n_blocks {
        ev.clear();
        if b == 0 {
            if let Some(v) = pre_bend {
                ev.push((0, [0xE0, (v & 0x7F) as u8, (v >> 7) as u8]));
            }
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
    // Measure L channel.
    let l: Vec<f32> = all.chunks(2).map(|c| c[0]).collect();
    let hz = zc_hz(&l, 0.25);
    let peak = l.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    match hz {
        Some(h) => println!("DIAG {name}: hz={h:.2} peak_dbfs={:.1}", 20.0 * peak.log10()),
        None => println!("DIAG {name}: SILENT peak_dbfs={:.1}", 20.0 * (peak.max(1e-12)).log10()),
    }
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
    // raw for +cents at the announced range: full wheel = ±range st.
    let raw = (8192.0 + cents / range as f64 * 8191.0).round() as u16;
    let raw_neg = (8192.0 - cents / range as f64 * 8191.0).round() as u16;
    println!("SPIKE {path:?} cents={cents} range={range} note={note} expect_ratio={:.5}", 2f64.powf(cents / 1200.0));
    run_case(entry, "control", note, None, None);
    run_case(entry, "pre_bend", note, Some(raw), None);
    run_case(entry, "pre_bend_neg", note, Some(raw_neg), None);
    run_case(entry, "post_bend", note, None, Some((raw, 4)));
    entry.all_notes_off();
    std::mem::forget(entry_arc.clone());
    println!("SPIKE DONE");
    std::process::exit(0);
}
