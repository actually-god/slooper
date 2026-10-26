// v17 crash repro: replay the app's synth-binding sequence for a VST3
// instrument outside the app — open() (instance #1), hold a note through
// a few blocks, then open the plugin's own editor (v0.12 SHADOW instance =
// instance #2 of the same binary) exactly like `auto_open_synth_gui` does,
// keep processing blocks while the editor lives, then close everything.
// Prints a step trace so a crash's LAST line localizes the fault.
//
//   cargo run --release --example v17_synth_probe [path.vst3]
use std::path::Path;

use slooper::vsthost::{default_vst_dirs, open, scan, VstKind};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| format!("{}/.vst3/vitalium.vst3", std::env::var("HOME").unwrap()));
    // The app scans its folders (chain / synth windows) WHILE a bound
    // instrument is already live: every scan probe is an EXTRA instance of
    // an already-loaded binary. Replay that first.
    let already_bound = std::env::var("V17_SCAN_FIRST").is_ok();
    if already_bound {
        let load = open(Path::new(&path), VstKind::V3, 48_000, None).expect("pre-open");
        eprintln!("[probe] bound {} before scan", load.entry.name());
        std::mem::forget(load.entry);
    }
    eprintln!("[probe] scanning default dirs (instance churn)");
    let found = scan(&default_vst_dirs());
    eprintln!("[probe] scan done: {} entries", found.len());
    eprintln!("[probe] opening {path} (instance #1)");
    let load = open(Path::new(&path), VstKind::V3, 48_000, None).expect("open vst3");
    let entry = load.entry;
    eprintln!("[probe] opened: {} instrument", entry.name());
    entry.note_on(0, 60, 110);
    let mut out = vec![0f32; 512];
    let silence = [0f32; 512];
    for _ in 0..100 {
        entry.process(&silence, &mut out);
    }
    let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    eprintln!("[probe] instance #1 processed, last-block peak {peak:.4}");
    eprintln!("[probe] opening editor (shadow instance #2)");
    let mut win = entry.open_vst3_editor(48_000).expect("open editor");
    eprintln!("[probe] editor requested; pumping audio + editor");
    let mut painted = false;
    for i in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        for _ in 0..5 {
            entry.process(&silence, &mut out);
        }
        if win.painted() {
            painted = true;
            eprintln!("[probe] editor painted at ~{}00 ms", i);
            break;
        }
        if win.open_failed() {
            eprintln!("[probe] editor refused at ~{}00 ms", i);
            break;
        }
    }
    eprintln!(
        "[probe] painted = {painted}, children = {}, closed = {}",
        win.child_count(),
        win.is_closed()
    );
    eprintln!("[probe] closing editor");
    win.close();
    std::thread::sleep(std::time::Duration::from_millis(500));
    eprintln!("[probe] editor closed; dropping instance #1");
    drop(entry);
    eprintln!("[probe] clean exit");
}
