// v17 crash repro #2 (the user's exact app sequence):
//   1. bind vitalium (full open) and KEEP PROCESSING it on an audio-like
//      thread (the RT callback),
//   2. run scan() on the GUI thread while it is live (this is what opening
//      the synth picker / chain window does),
//   3. keep processing and open the plugin's editor.
// A clean run prints every step; a host bug shows up as a JUCE assert
// storm, a hang, or a SIGSEGV.
//
//   cargo run --release --example v17_appflow_probe
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use slooper::vsthost::{default_vst_dirs, open, scan, VstKind};

fn main() {
    let path = format!("{}/.vst3/vitalium.vst3", std::env::var("HOME").unwrap());
    let skip_scan_live = std::env::var("V17_SKIP_SCAN_LIVE").is_ok();
    eprintln!("[flow] opening vitalium (the bound instrument)");
    let load = open(Path::new(&path), VstKind::V3, 48_000, None).expect("open vitalium");
    let entry = load.entry;
    entry.note_on(0, 60, 110);
    let running = Arc::new(AtomicBool::new(true));
    let rt = {
        let running = running.clone();
        let entry = entry.clone();
        std::thread::spawn(move || {
            let mut out = vec![0f32; 512];
            let silence = [0f32; 512];
            let mut peak = 0f32;
            while running.load(Ordering::Relaxed) {
                entry.process(&silence, &mut out);
                for x in out.iter() {
                    peak = peak.max(x.abs());
                }
                std::thread::sleep(std::time::Duration::from_micros(10_700)); // ~48 kHz @ 512
            }
            eprintln!("[flow] RT thread done, peak {peak:.4}");
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(400));
    if skip_scan_live {
        eprintln!("[flow] (scan skipped: fix-mode run)");
    } else {
        eprintln!("[flow] scanning plugin dirs WHILE vitalium processes");
        let found = scan(&default_vst_dirs());
        eprintln!("[flow] scan done: {} entries", found.len());
    }
    std::thread::sleep(std::time::Duration::from_millis(600));
    eprintln!("[flow] opening plugin editor");
    let mut win = entry.open_vst3_editor(48_000).expect("open editor");
    let mut painted = false;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if win.painted() {
            painted = true;
            break;
        }
        if win.open_failed() {
            break;
        }
    }
    eprintln!("[flow] painted = {painted}; holding editor 2 s while RT runs");
    std::thread::sleep(std::time::Duration::from_millis(2_000));
    win.close();
    std::thread::sleep(std::time::Duration::from_millis(400));
    running.store(false, Ordering::Relaxed);
    let _ = rt.join();
    eprintln!("[flow] surviving to drop is the point of this probe");
    std::mem::forget(entry);
    eprintln!("[flow] clean exit (instance leaked per probe rule)");
}
