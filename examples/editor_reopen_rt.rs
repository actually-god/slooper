// Close/reopen cycle repro WITH live audio processing on the instance:
// a background thread runs `process()` continuously (what the JACK RT
// callback does for a chain slot), while the main thread cycles
// open -> paint -> WM-close -> reopen. Crashes only under this
// interleave reproduce the app-level failure.
//
//   cargo run --example editor_reopen_rt [path.so ...]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use slooper::vsthost::{open, VstKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths: Vec<String> = if args.is_empty() {
        vec![
            "/home/admin/.vst/wolf-shaper-vst.so".into(),
            "/home/admin/.vst/DragonflyHallReverb-vst.so".into(),
            "/home/admin/.vst/amsynth_vst.so".into(),
        ]
    } else {
        args
    };
    let stop = Arc::new(AtomicBool::new(false));
    for path in paths {
        let load = match open(Path::new(&path), VstKind::V2, 48_000, None) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("SKIP {path}: {e}");
                continue;
            }
        };
        let entry = load.entry;
        if !entry.has_editor() {
            continue;
        }
        let handle = entry.vst2_handle().expect("vst2 handle");
        // RT stand-in: process() in a tight loop like a 64-frame block.
        let e2 = entry.clone();
        let s2 = stop.clone();
        let _rt = std::thread::spawn(move || {
            let inp = vec![0.001f32; 480];
            let mut out = vec![0.0f32; 480];
            while !s2.load(Ordering::Relaxed) {
                e2.process(&inp, &mut out);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        for cycle in 1..=4u32 {
            let win = slooper::vstedit::open_editor(&handle, &path, entry.name())
                .unwrap_or_else(|e| panic!("open_editor cycle {cycle}: {e}"));
            let mut waited = 0u32;
            while !win.painted() && !win.is_closed() && !win.open_failed() && waited < 9000 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                waited += 100;
            }
            let painted = win.painted();
            println!(
                "{path} cycle {cycle}: painted={painted} refused={}",
                win.open_failed()
            );
            if painted {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let xid = win.xid();
                assert!(xid != 0 && slooper::vstedit::request_wm_delete(xid), "wm delete");
                let mut w2 = 0u32;
                while !win.is_closed() && w2 < 4000 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    w2 += 100;
                }
                println!("  closed={}", win.is_closed());
            }
            drop(win);
            // Aggressive variant: re-open while a fresh editor for the
            // same instance may still be settling (cycle 4).
            if cycle == 4 {
                let a = slooper::vstedit::open_editor(&handle, &path, entry.name()).unwrap();
                let b = slooper::vstedit::open_editor(&handle, &path, entry.name()).unwrap();
                // Both alive, then the map-replacement pattern: drop one
                // handle while the other keeps pumping (App::insert does
                // exactly this when a second open replaces the map entry).
                std::thread::sleep(std::time::Duration::from_millis(400));
                drop(a);
                std::thread::sleep(std::time::Duration::from_millis(600));
                drop(b);
                println!("  double-open cycle done");
                break;
            }
        }
        println!("{path}: survived");
    }
    stop.store(true, Ordering::Relaxed);
    println!("ALL OK");
    std::process::exit(0); // exit before RT join races plugin teardown
}
