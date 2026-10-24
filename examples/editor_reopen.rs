// Close/reopen cycle repro (v0.11 "closing and re-opening the plugin GUI
// crashes the whole program"): open an editor, let it paint, inject the
// WM close, wait for the teardown, and RE-OPEN on the same live instance.
// Repeat N cycles. A crash here reproduces the app-level crash exactly —
// the app drives the same vstedit/vsthost machinery.
//
//   cargo run --example editor_reopen [path.so ...]
use std::path::Path;

use slooper::vsthost::{open, VstKind};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths: Vec<String> = if args.is_empty() {
        vec![
            "/home/admin/.vst/wolf-shaper-vst.so".into(),
            "/home/admin/.vst/DragonflyHallReverb-vst.so".into(),
        ]
    } else {
        args
    };
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
            eprintln!("SKIP {path}: no editor");
            continue;
        }
        let handle = entry.vst2_handle().expect("vst2 handle");
        for cycle in 1..=3u32 {
            let win = slooper::vstedit::open_editor(&handle, &path, entry.name())
                .unwrap_or_else(|e| panic!("open_editor cycle {cycle}: {e}"));
            // Wait for the verdict (painted / refused / closed).
            let mut waited = 0u32;
            while !win.painted() && !win.is_closed() && !win.open_failed() && waited < 9000 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                waited += 100;
            }
            let painted = win.painted();
            println!(
                "{path} cycle {cycle}: painted={painted} refused={} closed={}",
                win.open_failed(),
                win.is_closed()
            );
            if painted {
                // Live a moment like a user poking knobs would, then close.
                std::thread::sleep(std::time::Duration::from_millis(300));
                let xid = win.xid();
                assert!(xid != 0 && slooper::vstedit::request_wm_delete(xid), "wm delete");
                let mut w2 = 0u32;
                while !win.is_closed() && w2 < 4000 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    w2 += 100;
                }
                println!("  after WM close: closed={} (waited {w2} ms)", win.is_closed());
            }
            // Drop the handle like the app's reaper does, THEN re-open.
            drop(win);
        }
        println!("{path}: 3 close/reopen cycles survived");
    }
    println!("ALL OK");
}
