// v19: audit ONE plugin path in a dedicated process (vendor crashes can
// take down a whole-process audit; a per-plugin child isolates them).
//   cargo run --release --example plugin_one -- <path> <V2|V3>
use std::path::Path;

use slooper::vsthost::{open, scan, VstKind};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // SCAN mode: enumerate one directory in an isolated child so a hostile
    // vendor module cannot abort the whole registry scan.
    if args.get(1).map(|s| s.as_str()) == Some("--scan") {
        let found = scan(&[Path::new(&args[2]).to_path_buf()]);
        for e in found {
            println!(
                "ENTRY {:?} inst={} err={:?} :: {}",
                e.kind, e.instrument, e.error, e.path.display()
            );
        }
        std::process::exit(0);
    }
    let path = Path::new(&args[1]);
    let kind = if args.get(2).map(|s| s.as_str()) == Some("V2") {
        VstKind::V2
    } else {
        VstKind::V3
    };
    let name = std::env::var("PROBE_NAME").unwrap_or_else(|_| {
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let role = std::env::var("PROBE_ROLE").unwrap_or_default();
    match open(path, kind, 48_000, None) {
        Err(e) => {
            println!("LOADFAIL {name} :: {e}");
            std::process::exit(2);
        }
        Ok(load) => {
            let entry = load.entry;
            if role == "instr" {
                entry.note_on(0, 60, 110);
                let mut peak = 0.0f32;
                let mut out = vec![0f32; 512];
                let silence = [0f32; 512];
                for _ in 0..120 {
                    entry.process(&silence, &mut out);
                    for x in out.iter() {
                        peak = peak.max(x.abs());
                    }
                }
                entry.all_notes_off();
                std::mem::forget(entry);
                let v = if peak > 1e-4 { "ALIVE" } else { "SILENT" };
                println!("{v} instr {name} peak {peak:.4}");
            } else {
                let mut rng = 12345u32;
                let mut out_peak = 0.0f32;
                let mut diff = 0.0f32;
                let mut out = vec![0f32; 512];
                for _ in 0..60 {
                    let mut input = vec![0f32; 512];
                    for x in input.iter_mut() {
                        rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
                        *x = (((rng >> 16) as f32) / 32768.0 - 1.0) * 0.25;
                    }
                    entry.process(&input, &mut out);
                    for i in 0..512 {
                        out_peak = out_peak.max(out[i].abs());
                        diff = diff.max((out[i] - input[i]).abs());
                    }
                }
                std::mem::forget(entry);
                let v = if out_peak < 1e-5 {
                    "BLACKHOLE"
                } else if diff < 1e-6 {
                    "ALIVE-DRY"
                } else {
                    "ALIVE"
                };
                println!("{v} effect {name} peak {out_peak:.4} diff {diff:.4}");
            }
            std::process::exit(0);
        }
    }
}
