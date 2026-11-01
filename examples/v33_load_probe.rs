// v33: load-time profiler for the plugin open path. Usage:
//   cargo run --release --example v33_load_probe -- <path> [<path> ...]
// Prints one `T <kind> <ms> <path>` line per plugin (plus LOADFAIL rows).
// Run each hostile vendor in its own child (see the v19 audit driver note).
use std::path::PathBuf;
use std::time::Instant;

use slooper::vsthost::{open, VstKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    for a in args {
        let p = PathBuf::from(&a);
        let kind = if a.ends_with(".vst3") {
            VstKind::V3
        } else {
            VstKind::V2
        };
        let t0 = Instant::now();
        match open(&p, kind, 48_000, None) {
            Ok(_keep) => {
                let ms = t0.elapsed().as_millis();
                println!("T {} {:>6} {}", if matches!(kind, VstKind::V3) { "V3" } else { "V2" }, ms, a);
                // Never drop plugin instances in probes (skill rule).
                std::mem::forget(_keep);
            }
            Err(e) => println!("LOADFAIL {} :: {e}", a),
        }
    }
}
