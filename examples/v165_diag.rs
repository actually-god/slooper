// v165 DIAGNOSTIC (temporary): js80p receives pitch bend ONLY as a mapped
// PARAMETER (getMidiControllerAssignment -> (channel<<8)|129). This probe
// drives that parameter DIRECTLY through slooper's normal param wire
// (set_param -> runner ring -> inputParameterChanges) and measures zc Hz.
// audible = the js80p side works and the crate's MAPPING DISCOVERY is the
// remaining suspect. Silent = js80p's param plumbing itself ignores it.
use std::path::Path;

use slooper::vsthost::{open, VstKind};

const SR: f64 = 48_000.0;
const BLK: usize = 512;

fn zc_hz(x: &[f32], skip_s: f64) -> Option<f64> {
    let skip = (skip_s * SR) as usize;
    if x.len() <= skip + 4 {
        return None;
    }
    let seg = &x[skip..];
    let peak = seg.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak < 0.005 {
        return None;
    }
    let thr = peak * 0.15;
    let mut state = 0i8;
    let mut cross = 0u64;
    for &v in seg {
        if v > thr && state != 1 {
            if state == -1 {
                cross += 1;
            }
            state = 1;
        } else if v < -thr && state != -1 {
            if state == 1 {
                cross += 1;
            }
            state = -1;
        }
    }
    Some((cross as f64 / 2.0) / (seg.len() as f64 / SR))
}

fn main() {
    let arg1 = std::env::args().nth(1).unwrap();
    let path = Path::new(&arg1);
    let load = match open(path, VstKind::V3, SR as u32, None) {
        Ok(l) => l,
        Err(e) => {
            println!("LOADFAIL {e}");
            std::process::exit(2);
        }
    };
    let e = &load.entry;
    let rt = e.as_any().downcast_ref::<slooper::vsthost::RtStageEntry>().expect("rt");
    // Descriptor hunt: js80p midi-ctl param ids are (channel<<8)|controller.
    let want = 129u32; // kPitchBend, channel 0
    let mut hit: Option<(usize, u32, String)> = None;
    for (i, d) in e.descs().iter().enumerate() {
        if d.id == want {
            hit = Some((i, d.id, d.name.clone()));
            break;
        }
    }
    println!("DIAG descs={} pitchwheel_ch0_hit={hit:?}", e.descs().len());
    for (i, d) in e.descs().iter().enumerate().take(40) {
        println!("DIAG desc[{i}] id={} name={:?}", d.id, d.name);
    }
    let Some((idx, id, name)) = hit else {
        println!("DIAG no direct id 129 — scanning name matches");
        for (i, d) in e.descs().iter().enumerate() {
            if d.name.to_lowercase().contains("wheel") || d.name.to_lowercase().contains("bend") {
                println!("DIAG candidate desc[{i}] id={} name={:?}", d.id, d.name);
            }
        }
        return;
    };
    let _ = name;
    let sil = vec![0f32; BLK];
    let mut out = vec![0f32; BLK];
    let mut all: Vec<f32> = Vec::new();
    for _ in 0..8 {
        rt.process_with_midi(&sil, &mut out, &[]);
        all.extend_from_slice(&out);
    }
    rt.process_with_midi(&sil, &mut out, &[(4, [0x90, 60, 110])]); // hold note ch0
    all.extend_from_slice(&out);
    // 8 blocks center-ish, then push the pitch-wheel param up, hold, then down.
    for _ in 0..8 {
        rt.process_with_midi(&sil, &mut out, &[]);
        all.extend_from_slice(&out);
    }
    rt.set_param(idx, 1.0); // FULL UP from center 0.5
    for _ in 0..16 {
        rt.process_with_midi(&sil, &mut out, &[]);
        all.extend_from_slice(&out);
    }
    rt.set_param(idx, 0.0); // FULL DOWN
    for _ in 0..16 {
        rt.process_with_midi(&sil, &mut out, &[]);
        all.extend_from_slice(&out);
    }
    rt.set_param(idx, 0.5);
    let l: Vec<f32> = all.chunks(2).map(|c| c[0]).collect();
    // Times are in L-channel seconds: l covers l.len()/SR.
    let d = l.len() as f64 / SR;
    println!("DIAG l_seconds={d:.3}");
    let win = |a: f64, b: f64| -> Option<f64> {
        let s = (a * SR) as usize;
        let t = ((b * SR) as usize).min(l.len());
        if t <= s {
            return None;
        }
        zc_hz(&l[s..t], 0.0)
    };
    let base0 = d * 0.10;
    println!("DIAG id={id} idx={idx}");
    println!("DIAG base: {:?}", win(base0, d * 0.17).map(|h| format!("{h:.2}")));
    println!("DIAG fullup: {:?}", win(d * 0.22, d * 0.50).map(|h| format!("{h:.2}")));
    println!("DIAG fulldown: {:?}", win(d * 0.56, d * 0.92).map(|h| format!("{h:.2}")));
    println!("DIAG drops={}", rt.midi_drops());
}
