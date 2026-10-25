// v0.12 plugin audit: instantiates EVERY plugin on this machine and proves
// it is audibly alive in the role the UI would use it for:
//   * instruments: a held middle C must produce output
//   * effects: fed noise through, the output must not be a black hole
//     (silence AND exact-passthrough both fail an effect's job only when
//     the plugin mutes; passthrough still reports as `dry` for triage).
//
//   cargo run --release --example plugin_audit
use std::path::Path;

use slooper::vsthost::{default_vst_dirs, open, scan, VstKind};

fn instrument_peak(path: &Path, kind: VstKind) -> Result<f32, String> {
    let load = open(path, kind, 48_000, None)?;
    let entry = load.entry;
    entry.note_on(0, 60, 110);
    let mut peak = 0.0f32;
    let mut out = vec![0f32; 512];
    let silence = [0f32; 512];
    for _ in 0..120 {
        entry.process(&silence, &mut out);
        for x in out.iter() {
            peak = peak.max(x.abs());
        }
    }
    entry.all_notes_off();
    // Probe rule: never drop a plugin instance while others' toolkits live.
    std::mem::forget(entry);
    Ok(peak)
}

fn effect_peak(path: &Path, kind: VstKind) -> Result<(f32, f32), String> {
    // open() rejects nothing for effects; drive pink-ish noise in, listen.
    let load = open(path, kind, 48_000, None)?;
    let entry = load.entry;
    let mut rng = 12345u32;
    let mut out_peak = 0.0f32;
    let mut diff = 0.0f32;
    let mut out = vec![0f32; 512];
    for _ in 0..60 {
        let mut input = vec![0f32; 512];
        for x in input.iter_mut() {
            rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *x = (((rng >> 16) as f32) / 32768.0 - 1.0) * 0.25;
        }
        entry.process(&input, &mut out);
        for i in 0..512 {
            out_peak = out_peak.max(out[i].abs());
            diff = diff.max((out[i] - input[i]).abs());
        }
    }
    std::mem::forget(entry);
    Ok((out_peak, diff))
}

fn main() {
    let dirs = default_vst_dirs();
    let found = scan(&dirs);
    let mut fails = 0;
    for e in found.iter().filter(|e| e.error.is_none()) {
        let kind = e.kind;
        let role = if e.instrument { "instr" } else { "effect" };
        let (verdict, detail) = if e.instrument {
            match instrument_peak(&e.path, kind) {
                Ok(p) if p > 1e-4 => ("ALIVE".to_string(), format!("peak {p:.4}")),
                Ok(p) => {
                    fails += 1;
                    ("SILENT".to_string(), format!("peak {p:.6}"))
                }
                Err(err) => {
                    fails += 1;
                    ("LOADFAIL".to_string(), err)
                }
            }
        } else {
            match effect_peak(&e.path, kind) {
                Ok((p, d)) if p > 1e-4 => {
                    let tag = if d < 1e-6 { "ALIVE-DRY" } else { "ALIVE" };
                    (tag.to_string(), format!("peak {p:.4} diff {d:.4}"))
                }
                Ok((p, _)) => {
                    fails += 1;
                    ("SILENT".to_string(), format!("peak {p:.6}"))
                }
                Err(err) => {
                    fails += 1;
                    ("LOADFAIL".to_string(), err)
                }
            }
        };
        println!("{verdict:9} {role:6} {:10} {:50} {detail}", format!("{kind:?}"), e.name);
    }
    println!("--- {} plugins audited, {fails} failure(s)", found.iter().filter(|e| e.error.is_none()).count());
    std::process::exit(if fails > 0 { 1 } else { 0 });
}
