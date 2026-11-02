// v31 probe: reproduce the Analog Lab V editor crash context — an editor
// opening while the RT path streams (the exact state of a bound synth in
// the running app when the user clicks the strip button). The RT thread
// starts BEFORE open_editor and keeps streaming through its lifetime.
//
//   cargo run --release --example alv_editor_live [path.so]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use slooper::vsthost::{open, StageEntry, VstKind};

fn peak_of(entry: &slooper::vsthost::RtStageEntry, blocks: usize) -> f32 {
    let mut peak = 0f32;
    let mut out = vec![0f32; 512];
    let silence = [0f32; 512];
    for _ in 0..blocks {
        entry.process(&silence, &mut out);
        for x in out.iter() {
            peak = peak.max(x.abs());
        }
    }
    peak
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/home/admin/.vst/yabridge/Analog Lab V.so".to_string());
    let load = open(Path::new(&path), VstKind::V2, 48_000, None)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    let entry_arc = load.entry;
    let entry = entry_arc
        .as_any()
        .downcast_ref::<slooper::vsthost::RtStageEntry>()
        .unwrap();
    entry.note_on(0, 60, 110);
    let pre = peak_of(&entry, 120); // ~1.3 s of notes, editor NOT yet open
    println!("PRE-EDITOR peak {pre:.5}");

    let handle = match entry.vst2_handle() {
        Some(h) => h,
        None => {
            println!("no vst2 handle");
            std::process::exit(3);
        }
    };

    // RT thread streams BEFORE and DURING editor bring-up — the app's
    // real topology (JACK callback thread vs gui thread).
    let entry_rt = Arc::clone(&entry_arc);
    let live = Arc::new(AtomicBool::new(true));
    let live2 = live.clone();
    let rt = std::thread::spawn(move || {
        let mut out = vec![0f32; 512];
        let silence = [0f32; 512];
        while live2.load(Ordering::Relaxed) {
            entry_rt.process(&silence, &mut out);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let mut win = slooper::vstedit::open_editor(
        &handle,
        &path,
        entry.name(),
        entry.params().clone(),
        entry.descs().len(),
    )
    .unwrap_or_else(|e| panic!("open_editor: {e}"));

    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let painted = win.painted();
    println!("EDITOR painted={painted} (rt thread streaming)");
    std::thread::sleep(std::time::Duration::from_millis(2000));
    live.store(false, Ordering::Relaxed);
    win.close();
    println!("SURVIVED editor open/close under audio load");
    let _ = rt.join();
    std::mem::forget(entry_arc.clone());
}
