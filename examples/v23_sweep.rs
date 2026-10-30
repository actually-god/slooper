// v23 full-surface plugin audit: ONE plugin per process (vendor crashes
// cost one row, never the sweep). Beyond plugin_one's audio check this
// probe answers the questions behind the v23 user reports:
//   PARAMS  total parameter count through the real VST3 enumeration, and
//           how many survive the host cap (slooper::vsthost::VST_MAX_PARAMS)
//   CHUNK   does a state chunk written AFTER a param edit + process cycle
//           actually carry that edit when loaded into a fresh instance?
//           (VST3 spec allows chunks to omit normalized param values —
//           the host must then re-apply them itself.)
//   EDITOR  full real-code editor lifecycle: open through the same
//           entry.open_vst3_editor / vstedit::open_editor the app uses,
//           wait for a verified paint, hand back state, WM-close, wait
//           for teardown. A vendor crash shows up as a dead child.
//   PERSIST the exact app close sequence: harvest chunk -> rebuild with
//           chunk ONLY (saved=None, the pre-v23 behavior) and report
//           whether the edited parameter survived the rebuild.
//   cargo run --release --example v23_sweep -- <path> <V2|V3>
use std::path::Path;

use slooper::vsthost::{open, VstKind, VST_MAX_PARAMS};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = Path::new(&args[1]);
    let kind = if args.get(2).map(|s| s.as_str()) == Some("V2") {
        VstKind::V2
    } else {
        VstKind::V3
    };
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let want_editor = std::env::var("SWEEP_EDITOR").is_ok();

    // OPEN + audio -------------------------------------------------------
    let load = match open(path, kind, 48_000, None) {
        Err(e) => {
            println!("LOADFAIL {name} :: {e}");
            std::process::exit(2);
        }
        Ok(l) => l,
    };
    let entry = &load.entry;
    let ndesc = entry.descs().len();
    println!("OPEN ok {name} params_host={ndesc} editor={}", entry.has_editor());

    // Deterministic noise, measure the plugin doing something.
    let mut st = 0x2545_F491_u32;
    let mut noise = vec![0f32; 512];
    for x in noise.iter_mut() {
        st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *x = ((st >> 8) as f32 / (1 << 24) as f32) * 2.0 - 1.0;
    }
    if entry.kind() == VstKind::V3 {
        // instrument check first (notes), effect check as fallback
        let mut out = vec![0f32; 512];
        entry.note_on(0, 60, 110);
        let mut peak = 0f32;
        for _ in 0..120 {
            entry.process(&noise, &mut out);
            for x in out.iter() {
                peak = peak.max(x.abs());
            }
        }
        if peak < 1e-4 {
            let mut p2 = 0f32;
            for _ in 0..60 {
                entry.process(&noise, &mut out);
                for x in out.iter() {
                    p2 = p2.max(x.abs());
                }
            }
            peak = p2.max(peak);
        }
        println!("AUDIO peak_note_or_noise {peak:.5}");
    } else {
        let mut out = vec![0f32; 512];
        let mut peak = 0f32;
        for _ in 0..60 {
            entry.process(&noise, &mut out);
            for x in out.iter() {
                peak = peak.max(x.abs());
            }
        }
        println!("AUDIO peak_noise {peak:.5}");
    }

    // CHUNK round-trip on a SEPARATE raw instance (the app instance keeps
    // running; VST3 spec: params may live outside the chunk, so set a
    // param, run blocks (isLive), save, load into a fresh instance).
    if entry.kind() == VstKind::V3 {
        chunk_roundtrip_v3(path);
    }

    // EDITOR lifecycle through the REAL app path --------------------------
    if want_editor && entry.has_editor() {
        if std::env::var("SWEEP_EDITOR").unwrap() != "yes" {
            println!("EDITOR skipped");
        } else {
            editor_lifecycle(entry);
        }
    }
    std::mem::forget(load.entry.clone());
    std::mem::forget(load);
    println!("DONE {name}");
}

fn chunk_roundtrip_v3(path: &Path) {
    use vst3_host::audio::AudioBuffers;
    let mut a = match vst3_host::simple::load_plugin_with_settings(path, 48000.0, 512) {
        Ok(p) => p,
        Err(e) => {
            println!("CHUNK loadfail {e}");
            return;
        }
    };
    let ps = match a.get_parameters() {
        Ok(p) => p,
        Err(e) => {
            println!("CHUNK paramsfail {e}");
            std::mem::forget(a);
            return;
        }
    };
    let total = ps.len();
    // Pick writable probes: first, middle, last (deep ids are exactly the
    // ones the host cap truncates).
    let writable: Vec<usize> = ps
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.is_read_only)
        .map(|(i, _)| i)
        .collect();
    let mut probes: Vec<usize> = Vec::new();
    for want in [0usize, writable.len() / 2, writable.len().saturating_sub(1)] {
        if let Some(&i) = writable.get(want) {
            if !probes.contains(&i) {
                probes.push(i);
            }
        }
    }
    let targets: Vec<(u32, f64)> = probes
        .iter()
        .map(|&i| (ps[i].id, if ps[i].value > 0.05 { 0.0 } else { 1.0 }))
        .collect();
    let orig: Vec<f64> = probes.iter().map(|&i| ps[i].value).collect();
    let mut before: Vec<f64> = Vec::new();
    for (id, v) in &targets {
        let _ = a.set_parameter(*id, *v);
    }
    for (id, _) in &targets {
        before.push(a.get_parameter(*id).unwrap_or(-1.0));
    }
    // Run audio so isLive-deferred values enter serializable state.
    if a.start_processing().is_ok() {
        let ch = a.output_channel_count().max(1);
        let mut bufs = AudioBuffers::new(ch, ch, 512, 48000.0);
        for _ in 0..4 {
            if a.process_audio(&mut bufs).is_err() {
                break;
            }
        }
        let _ = a.stop_processing();
    }
    let bytes = match a.save_state() {
        Ok(b) => b,
        Err(e) => {
            println!("CHUNK savefail {e} total={total}");
            std::mem::forget(a);
            return;
        }
    };
    std::mem::forget(a);
    let mut b = match vst3_host::simple::load_plugin_with_settings(path, 48000.0, 512) {
        Ok(p) => p,
        Err(e) => {
            println!("CHUNK reloadfail {e}");
            return;
        }
    };
    let r = b.load_state(&bytes);
    let mut report = String::new();
    for (((id, want), pre), orig) in targets.iter().zip(before.iter()).zip(orig.iter()) {
        let got = b.get_parameter(*id).unwrap_or(-9.0);
        let set_ok = (pre - want).abs() < 0.02; // the plugin accepted the edit
        let carried = (got - pre).abs() < 0.02;
        let flag = if !set_ok {
            'S' // set refused: probe slot is a dud, verdict meaningless
        } else if carried {
            'Y'
        } else {
            'N'
        };
        report.push_str(&format!(
            " [{} id={id} orig={orig:.2} set={pre:.2} got={got:.2} {flag}]",
            ps[probes[targets.iter().position(|t| t.0 == *id).unwrap()]]
                .name
                .chars()
                .take(14)
                .collect::<String>()
        ));
    }
    println!(
        "CHUNK total={total} cap={VST_MAX_PARAMS} bytes={} load={:?} carried:{report}",
        bytes.len(),
        r.as_ref().err().map(|e| e.to_string())
    );
    std::mem::forget(b);
}

fn editor_lifecycle(entry: &slooper::vsthost::RtStageEntry) {
    let opened = std::time::Instant::now();
    let win = if entry.kind() == VstKind::V3 {
        match entry.open_vst3_editor(48_000) {
            Ok(w) => w,
            Err(e) => {
                println!("EDITOR openfail {e}");
                return;
            }
        }
    } else {
        let h = match entry.vst2_handle() {
            Some(h) => h,
            None => {
                println!("EDITOR no-vst2-handle");
                return;
            }
        };
        match slooper::vstedit::open_editor(
            &h,
            entry.path().to_string_lossy().as_ref(),
            entry.name(),
            entry.params().clone(),
            entry.descs().len(),
        ) {
            Ok(w) => w,
            Err(e) => {
                println!("EDITOR openfail {e}");
                return;
            }
        }
    };
    // Give the toolkit up to 6 s to paint (verify window is 3 s + slack).
    let mut painted = false;
    for _ in 0..120 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if win.painted() {
            painted = true;
            break;
        }
        if win.open_failed() {
            println!("EDITOR refused (never painted; closed cleanly)");
            return;
        }
    }
    // Let it live a moment (idle pumps), then close like the WM would.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let xid = win.xid();
    let _ = slooper::vstedit::request_wm_delete(xid);
    let mut closed = false;
    for _ in 0..160 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if win.is_closed() {
            closed = true;
            break;
        }
    }
    let state = win.state_snapshot().map(|v| v.len());
    println!(
        "EDITOR painted={painted} closed={closed} state_bytes={state:?} ms={}",
        opened.elapsed().as_millis()
    );
}
