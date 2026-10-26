// Editor-pump cadence probe (temporary dev tool; see SLOOPER_VST_STATS).
//   SLOOPER_VST_STATS=1 DISPLAY=:0 cargo run --release --example vst_pump_probe [seconds]
//   PUMP_ONLY=v2|v3   restrict to one generation
//   PUMP_NAME=wolf    substring of the plugin name to open (default: first match)
use std::time::{Duration, Instant};

use slooper::vsthost::{default_vst_dirs, open, scan, VstKind};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let only = std::env::var("PUMP_ONLY").unwrap_or_default();
    let name = std::env::var("PUMP_NAME").unwrap_or_default().to_lowercase();
    let found = scan(&default_vst_dirs());
    let pick =
        |k: VstKind| found.iter().find(|e| e.kind == k && (name.is_empty() || e.name.to_lowercase().contains(&name)));

    if !only.contains("v3") {
        if let Some(e) = pick(VstKind::V2) {
            let l = open(&e.path, e.kind, 48_000, None).expect("open v2");
            let h = l.entry.vst2_handle().unwrap();
            if let Ok(w) = slooper::vstedit::open_editor(&h, &e.path.to_string_lossy(), "p2") {
                let t = Instant::now();
                while !w.painted() && t.elapsed().as_secs() < 6 {
                    std::thread::sleep(Duration::from_millis(20));
                }
                println!(
                    "VST2 {} painted={} in {:.2}s",
                    e.name,
                    w.painted(),
                    t.elapsed().as_secs_f32()
                );
                std::mem::forget(w);
            }
            std::mem::forget(l);
        }
    }
    if !only.contains("v2") {
        if let Some(e) = pick(VstKind::V3) {
            let l = open(&e.path, e.kind, 48_000, None).expect("open v3");
            if let Ok(w) = l.entry.open_vst3_editor(48_000) {
                let t = Instant::now();
                while !w.painted() && t.elapsed().as_secs() < 6 {
                    std::thread::sleep(Duration::from_millis(20));
                }
                println!(
                    "VST3 {} painted={} in {:.2}s",
                    e.name,
                    w.painted(),
                    t.elapsed().as_secs_f32()
                );
                std::mem::forget(w);
            }
            std::mem::forget(l);
        }
    }
    println!("idling {secs}s — watch [vst-stats]");
    std::thread::sleep(Duration::from_secs(secs));
    std::process::exit(0);
}
