// v20 probe: dump descs (with ids) AND drive cell writes end-to-end.
use std::path::Path;
use slooper::vsthost::{open, VstKind};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let kind = if a.get(2).map(|s| s.as_str()) == Some("V2") {
        VstKind::V2
    } else {
        VstKind::V3
    };
    let load = open(Path::new(&a[1]), kind, 48_000, None).expect("open");
    let e = &load.entry;
    for (k, d) in e.descs().iter().enumerate() {
        println!("{k}\tid={} def={} wr={} {}", d.id, d.default, d.writable, d.name);
    }
    // Deterministic noise.
    let mut st = 0x2545_F491_u32;
    let mut noise = vec![0f32; 512];
    for x in noise.iter_mut() {
        st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *x = ((st >> 8) as f32 / (1 << 24) as f32) * 2.0 - 1.0;
    }
    let measure = |e: &slooper::vsthost::RtStageEntry, blocks: usize| -> f32 {
        let mut peak = 0f32;
        let mut out = [0f32; 512];
        for _ in 0..blocks {
            e.process(&noise, &mut out);
            peak = peak.max(out.iter().fold(0f32, |m, x| m.max(x.abs())));
        }
        peak
    };
    let _ = measure(e, 24);
    for (k, d) in e.descs().iter().enumerate() {
        if !d.writable {
            continue;
        }
        e.set_param(k, 0.0);
        let p0 = measure(e, 12);
        e.set_param(k, 1.0);
        let p1 = measure(e, 12);
        println!("EFFECT idx {k:<2} {:<24} v0={p0:.4} v1={p1:.4}", d.name);
    }
    // Cell-only write on the LAST writable param, then re-measure (the fix path).
    let last = e
        .descs()
        .iter()
        .rposition(|d| d.writable)
        .unwrap();
    // simulate the editor drain: generation bump through the public mirror is
    // not pub — use set_param for the probe, then measure.
    e.set_param(last, 0.5);
    println!("EFFECT last idx {last}: {}", measure(e, 12));
}
