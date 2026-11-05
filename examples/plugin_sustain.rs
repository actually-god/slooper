// v37 R4 repro: feed one long note through the EXACT host path the audio
// thread uses (process_with_midi, 512-sample internal blocks) and measure
// whether the plugin sustains between note-on and note-off.
use slooper::vsthost::{open, StageEntry, VstKind};

fn run(path: &str, hold_s: f64) -> (f32, f32, f32) {
    let p = std::path::Path::new(path);
    let load = open(p, VstKind::V3, 48_000, None)
        .or_else(|_| open(p, VstKind::V2, 48_000, None))
        .expect("open");
    let e: &std::sync::Arc<dyn StageEntry> = &load.entry;
    let sr = 48_000usize;
    let total = (2.5 * sr as f64) as usize;
    let on_at = 0usize;
    let off_at = (hold_s * sr as f64) as usize;
    let sil = vec![0.0f32; 512];
    let mut out = vec![0.0f32; 512];
    let mut mid = 0.0f32; // energy in the LAST 30% of the hold (sustain proof)
    let mut post = 0.0f32; // energy 0.9 s after the off (leak proof)
    let mut n = 0usize;
    while n < total {
        let mut evq: Vec<(usize, [u8; 3])> = Vec::new();
        if n == on_at {
            evq.push((0, [0x90, 60, 110]));
        }
        if n / 512 == off_at / 512 && n <= off_at && n + 512 > off_at {
            evq.push((off_at - n, [0x80, 60, 0]));
        }
        e.process_with_midi(&sil[..512], &mut out[..512], &evq);
        let peak = out[..512].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let t = n as f64 / sr as f64;
        if t > hold_s * 0.6 && t < hold_s * 0.95 {
            mid = mid.max(peak);
        }
        if t > hold_s + 0.9 {
            post = post.max(peak);
        }
        n += 512;
    }
    e.all_notes_off();
    // give the release a moment
    for _ in 0..200 {
        e.process_with_midi(&sil[..512], &mut out[..512], &[]);
    }
    (mid, post, 0.0)
}

fn main() {
    let path = std::env::args().nth(1).expect("plugin path");
    let (short_mid, short_post, _) = run(&path, 0.12);
    let (long_mid, long_post, _) = run(&path, 2.0);
    println!(
        "short note (120ms): sustain-window peak {short_mid:.5}, 0.9s-after-off peak {short_post:.5}"
    );
    println!(
        "long note (2s):   sustain-window peak {long_mid:.5}, 0.9s-after-off peak {long_post:.5}"
    );
    if long_mid <= 1e-4 {
        println!("VERDICT: long note SILENT mid-hold — host is dropping/shortening holds");
    } else if long_mid <= short_mid * 1.05 && long_mid > 1e-4 {
        println!(
            "VERDICT: plugin sustains (loud at 1.2s into the hold) — HOST IS OK at this layer"
        );
    } else {
        println!("VERDICT: sustains (long-mid {long_mid:.4} vs short-mid {short_mid:.4})");
    }
}
