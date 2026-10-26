// VST pipeline performance probe.
//
// Measures the four surfaces the user experiences as "laggy":
//   1. scan() wall time            (UI thread blocks this long on first
//                                   chain-window open / Rescan)
//   2. open() wall time per plugin (UI thread blocks this long per add/bind)
//   3. process() realtime factor   (audio pipeline headroom)
//   4. vst2-gui thread job latency while editors are open — every plugin
//      input gesture is serviced by that thread; a noop round-trip while
//      editors pump is the honest "input lag" upper bound.
//
//   cargo run --release --example vst_perf_probe
use std::path::Path;
use std::time::{Duration, Instant};

use slooper::vsthost::{default_vst_dirs, on_gui_thread, open, scan, VstKind};

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = (((v.len() - 1) as f64) * p).round() as usize;
    v[i]
}

/// Send `n` noop jobs through the GUI-thread proxy with `gap` between them
/// and report the latency distribution in ms.
fn gui_latency(label: &str, n: usize, gap: Duration) {
    // Warm-up: let any queued work settle.
    let _ = on_gui_thread(|| ());
    std::thread::sleep(Duration::from_millis(100));
    let mut lats = Vec::new();
    for _ in 0..n {
        let t0 = Instant::now();
        let _ = on_gui_thread(|| ());
        lats.push(t0.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(gap);
    }
    println!(
        "gui-latency {label:<28} p50={:6.2}ms p95={:7.2}ms max={:7.2}ms",
        pct(&mut lats, 0.50),
        pct(&mut lats, 0.95),
        pct(&mut lats, 1.00),
    );
}

fn main() {
    let dirs = default_vst_dirs();
    println!("scan dirs: {}", dirs.len());

    // ---- 1. scan ---------------------------------------------------------
    let t0 = Instant::now();
    let found = scan(&dirs);
    println!(
        "scan: {} entries in {:.2} s ({} ok)",
        found.len(),
        t0.elapsed().as_secs_f32(),
        found.iter().filter(|e| e.error.is_none()).count()
    );

    // ---- 2. open() per plugin -------------------------------------------
    let mut loads = Vec::new();
    for e in found.iter().filter(|e| e.error.is_none()) {
        let t0 = Instant::now();
        match open(&e.path, e.kind, 48_000, None) {
            Ok(l) => {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                println!(
                    "open {:>4} {:<28} {:8.1} ms  params={}",
                    e.kind.tag(),
                    e.name,
                    ms,
                    l.entry.descs().len()
                );
                loads.push(l);
            }
            Err(err) => println!("open {:>4} {:<28} FAIL {err}", e.kind.tag(), e.name),
        }
    }

    // ---- 3. process realtime factor --------------------------------------
    for l in loads.iter().filter(|l| !l.entry.descs().is_empty()).take(4) {
        let n = 2000usize; // ~21 s at 48k in 512-blocks
        let t0 = Instant::now();
        for i in 0..n {
            let mut out = [0f32; 512];
            let input = if i % 2 == 0 {
                [0.01f32; 512]
            } else {
                [-0.01f32; 512]
            };
            l.entry.process(&input, &mut out);
        }
        let secs = t0.elapsed().as_secs_f64();
        println!(
            "process {:<28} {:6.1}x realtime ({:.2} ms per 512-blk)",
            l.entry.name(),
            (n as f64 * 512.0 / 48_000.0) / secs,
            secs / n as f64 * 1000.0
        );
    }

    // ---- 4. GUI-thread responsiveness ------------------------------------
    let v2 = loads
        .iter()
        .find(|l| l.entry.kind() == VstKind::V2 && l.entry.has_editor());
    if let Some(l) = v2 {
        let h = l.entry.vst2_handle().unwrap();
        match slooper::vstedit::open_editor(&h, &l.entry.path().to_string_lossy(), "probe-v2") {
            Ok(mut w) => {
                let mut waited = Duration::ZERO;
                while !w.painted() && waited.as_secs() < 6 {
                    std::thread::sleep(Duration::from_millis(20));
                    waited += Duration::from_millis(20);
                }
                println!(
                    "editor VST2 {} painted={} in {:.2}s (rect {:?})",
                    l.entry.name(),
                    w.painted(),
                    waited.as_secs_f32(),
                    w.editor_size()
                );
                gui_latency("VST2 editor open", 60, Duration::from_millis(16));
                // A UI-like burst: 50 Hz tiny jobs (what egui round-trips do).
                gui_latency("VST2 + burst", 60, Duration::from_millis(4));
                let t0 = Instant::now();
                w.close();
                println!(
                    "editor VST2 close: {:.1} ms",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
            Err(e) => println!("editor VST2 open failed: {e}"),
        }
    }

    let v3 = loads
        .iter()
        .find(|l| l.entry.kind() == VstKind::V3 && l.entry.has_editor());
    if let Some(l) = v3 {
        match l.entry.open_vst3_editor(48_000) {
            Ok(mut w) => {
                let mut waited = Duration::ZERO;
                while !w.painted() && waited.as_secs() < 6 {
                    std::thread::sleep(Duration::from_millis(20));
                    waited += Duration::from_millis(20);
                }
                println!(
                    "editor VST3 {} painted={} in {:.2}s",
                    l.entry.name(),
                    w.painted(),
                    waited.as_secs_f32()
                );
                gui_latency("VST3 editor open", 60, Duration::from_millis(16));
                let t0 = Instant::now();
                w.close();
                println!(
                    "editor VST3 close: {:.1} ms",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
            Err(e) => println!("editor VST3 open failed: {e}"),
        }
    }

    // Both editors at once (rotation cost).
    let mut keep = Vec::new();
    for l in loads.iter().filter(|l| l.entry.has_editor()).take(2) {
        let w = if l.entry.kind() == VstKind::V2 {
            l.entry.vst2_handle().and_then(|h| {
                slooper::vstedit::open_editor(&h, &l.entry.path().to_string_lossy(), "probe-both")
                    .ok()
            })
        } else {
            l.entry.open_vst3_editor(48_000).ok()
        };
        if let Some(w) = w {
            keep.push(w);
        }
    }
    if !keep.is_empty() {
        std::thread::sleep(Duration::from_millis(1500));
        gui_latency(
            &format!("{} editors open", keep.len()),
            60,
            Duration::from_millis(16),
        );
        for mut w in keep {
            w.close();
        }
    }

    // Never drop plugin instances while probing (teardown rule: leak).
    for l in loads {
        std::mem::forget(l);
    }
    let _ = Path::new("");
}
