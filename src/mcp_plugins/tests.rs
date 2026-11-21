//! v109/MCP-05 pins (board t_3f374341): the plugins/plugin/sampler/drum/midi
//! tool family. Acceptance pass = scratch/slooper-v109/REQUIREMENTS.md.
//! Tier-1 doctrine (v32): pins drive the TOOL layer (call), never the
//! implementers; writes are asserted through the read tools / live state
//! the GUI itself reads.
use super::*;
use crate::audio::SynthHandle;
use crate::mcp::rpc;
use crate::ui::{headless_app, App};
use std::path::PathBuf;
use std::sync::{Arc, Once};

/// Isolated data dir for the v65/v69/v56 preset storages + the MCP bridge
/// endpoint file (same recipe as the mcp02 pins).
fn isolated_data_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sp-v109-{}-{:x}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(crate::paths::DATA_DIR_ENV, &dir);
    dir
}

/// Pins drive the REAL dispatcher (v107/v108 contract), never the tool fns.
fn call(app: &mut App, name: &str, args: Value) -> Value {
    rpc::dispatch(
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":args}}),
        app,
    )
    .expect("tools/call answers")
}

fn structured(app: &mut App, name: &str, args: Value) -> Value {
    let r = call(app, name, args);
    assert!(r.get("error").is_none(), "unexpected JSON-RPC error: {r}");
    assert_eq!(
        r["result"]["isError"].as_bool(),
        Some(false),
        "{name}: {}",
        r["result"]
    );
    r["result"]["structuredContent"].clone()
}

fn is_error(app: &mut App, name: &str, args: Value, code: &str) -> String {
    let shown = args.to_string();
    let r = call(app, name, args);
    assert_eq!(
        r["result"]["isError"].as_bool(),
        Some(true),
        "{name} must fail on {shown}: {}",
        r["result"]
    );
    assert_eq!(
        r["result"]["error"]["code"].as_str().unwrap(),
        code,
        "{name} error code on {shown}: {}",
        r["result"]
    );
    r["result"]["error"]["message"].as_str().unwrap().to_string()
}

/// Headless app on an isolated data dir with a real sample rate.
fn fixture(tag: &str) -> (App, PathBuf) {
    let dir = isolated_data_dir(tag);
    let mut app = headless_app(&format!("mcp05-{tag}"));
    app.mcp05_test_set_sr(48000);
    (app, dir)
}

/// DFT bin power of `x[a..b]` at `f` Hz (dsp-audio-probe Goertzel).
fn tone_power(x: &[f32], f: f64, rate: f64, a: usize, b: usize) -> f64 {
    let w = std::f64::consts::TAU * f / rate;
    let (mut s0, mut s1) = (0.0f64, 0.0f64);
    for &v in &x[a..b] {
        s0 = v as f64 + 2.0 * w.cos() * s1 - s0;
        std::mem::swap(&mut s1, &mut s0);
        std::mem::swap(&mut s1, &mut s0);
    }
    let real = s1 - w.cos() * s0;
    let imag = w.sin() * s0;
    (real * real + imag * imag) / (b - a) as f64
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len() as f64).sqrt()
}

fn render(inst: &Arc<crate::synth::Instance>, note: u8, vel: u8) -> Vec<f32> {
    let mut ev = vec![(0u64, 3u8, 0u8, note, vel), (40_000, 0, 0, note, 0)];
    let mut out = vec![0f32; 8192];
    crate::audio::render_midi_events(inst, &mut ev, 0, out.len(), &mut out);
    out
}

// ── R1/R2: catalog parity + instances + schemas ─────────────────────────

#[test]
fn v109_plugins_list_catalog_parity() {
    let _d = isolated_data_dir("list");
    let mut app = headless_app("mcp05-list");
    let out = structured(&mut app, "plugins.list", json!({"kind": "effect"}));
    let listed: Vec<String> = out["effects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["stem"].as_str().unwrap().to_string())
        .collect();
    // PARITY: every EXAMPLES entry must be listed — no hand-written list
    // anywhere on this path.
    for (name, _src) in crate::fx::EXAMPLES {
        let stem = name.trim_end_matches(".rs");
        assert!(listed.iter().any(|s| s == stem), "catalog missing {stem}");
    }
    // every entry carries a full param schema
    for e in out["effects"].as_array().unwrap() {
        for p in e["params"].as_array().unwrap() {
            for k in ["index", "name", "min", "max", "default", "unit", "x-slooper-unit"] {
                assert!(p.get(k).is_some(), "param schema missing {k} on {e}");
            }
        }
    }
    // vfilter's enum param exposes its labels (pickers over MCP)
    let vf = out["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["stem"] == "vfilter")
        .unwrap();
    let mode = vf["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "mode")
        .unwrap();
    assert_eq!(mode["labels"].as_array().unwrap().len(), 4);
    // synths: BUNDLED parity
    let out = structured(&mut app, "plugins.list", json!({"kind": "synth"}));
    let listed: Vec<String> = out["synths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["stem"].as_str().unwrap().to_string())
        .collect();
    for (name, _src) in crate::synth::BUNDLED {
        let stem = name.trim_end_matches(".rs");
        assert!(listed.iter().any(|s| s == stem), "synth catalog missing {stem}");
    }
    assert!(is_error(&mut app, "plugins.list", json!({"kind": "vst"}), "invalid")
        .contains("all"));
}

#[test]
fn v109_plugins_instances_addresses() {
    let (mut app, _dir) = fixture("inst");
    app.mcp05_set_track_synth(0, "drum");
    app.mcp05_add_fx_script(0, "delay");
    app.mcp05_add_fx_script(0, "chorus");
    app.mcp05_set_track_synth(1, "piano");
    let out = structured(&mut app, "plugins.instances", json!({}));
    let has = |q: Value| {
        out["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| q.as_object().unwrap().iter().all(|(k, v)| &i[k] == v))
    };
    assert!(has(json!({"kind": "synth", "track": 0, "stem": "drum"})), "{out}");
    assert!(has(json!({"kind": "synth", "track": 1, "stem": "piano"})), "{out}");
    // FX rows addressable {track,row} in dense order
    let fx0: Vec<&Value> = out["instances"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == "fx" && i["track"] == 0)
        .collect();
    assert_eq!(fx0.len(), 2);
    assert_eq!(fx0[0]["row"], 0);
    assert_eq!(fx0[0]["stem"], "delay");
    assert_eq!(fx0[1]["row"], 1);
    assert_eq!(fx0[1]["stem"], "chorus");
    assert_eq!(out["count"], 4);
    // the {track,row} address resolves through describe
    let d = structured(&mut app, "plugin.describe", json!({"track": 0, "row": 1}));
    assert_eq!(d["stem"], "chorus");
}

#[test]
fn v109_plugin_params_roundtrip_and_status_echo() {
    let (mut app, _dir) = fixture("param");
    app.mcp05_add_fx_script(0, "delay");
    let p = structured(&mut app, "plugin.params", json!({"track": 0, "row": 0}));
    assert_eq!(p["stem"], "delay");
    let n = p["count"].as_u64().unwrap() as usize;
    assert!(n >= 3, "delay has params: {p}");
    // pick the LAST param (output level) and prove set -> observed -> file.
    let last = n - 1;
    let lo = p["params"][last]["min"].as_f64().unwrap();
    let hi = p["params"][last]["max"].as_f64().unwrap();
    let target = lo + (hi - lo) * 0.25;
    let ack = structured(
        &mut app,
        "plugin.param.set",
        json!({"track": 0, "row": 0, "param": last, "value": target}),
    );
    assert!((ack["value"].as_f64().unwrap() - target).abs() < 1e-5, "{ack}");
    assert!(app.status.starts_with("MCP:"), "status echo: {}", app.status);
    // the read tool agrees (observed through the getter, not the arg)
    let got = structured(
        &mut app,
        "plugin.param.get",
        json!({"track": 0, "row": 0, "param": last}),
    );
    assert!((got["value"].as_f64().unwrap() - target).abs() < 1e-5);
    // the live chain instance (what the RT block and the editor read) moved
    let live = app.mcp05_fx_live(0, 0).expect("live fx");
    assert!((live.param(last) - target as f32).abs() < 1e-5);
    // persisted row value matches too (fx_save_pending -> config)
    let (_stem, params, _b, _v) = app.mcp05_fx_row(0, 0).unwrap();
    assert!((params[last] - target as f32).abs() < 1e-5);
}

#[test]
fn v109_plugin_param_validation_errors() {
    let (mut app, _dir) = fixture("val");
    app.mcp05_add_fx_script(0, "vfilter");
    // param index beyond the table
    let m = is_error(
        &mut app,
        "plugin.param.set",
        json!({"track": 0, "row": 0, "param": 99, "value": 1.0}),
        "out_of_range",
    );
    assert!(m.contains("vfilter") && m.contains("params"), "{m}");
    // value outside the declared range
    let m = is_error(
        &mut app,
        "plugin.param.set",
        json!({"track": 0, "row": 0, "param": 1, "value": 90_000.0}),
        "out_of_range",
    );
    assert!(m.contains("20") && m.contains("18000"), "{m}");
    // enum param must land on a slot (vfilter mode: LP/BP/HP/notch = 0..3)
    let m = is_error(
        &mut app,
        "plugin.param.set",
        json!({"track": 0, "row": 0, "param": 0, "value": 1.5}),
        "out_of_range",
    );
    assert!(m.contains("enum"), "{m}");
    // an enum slot value IS accepted (positive control against sabotage)
    let ok = call(
        &mut app,
        "plugin.param.set",
        json!({"track": 0, "row": 0, "param": 0, "value": 2.0}),
    );
    assert_eq!(ok["isError"].as_bool(), Some(false), "{ok}");
    // missing args -> invalid
    is_error(&mut app, "plugin.param.set", json!({"track": 0}), "invalid");
    // track beyond the project -> out_of_range
    is_error(
        &mut app,
        "plugin.params",
        json!({"track": 99, "row": 0}),
        "out_of_range",
    );
    // row beyond the chain -> not_found
    is_error(
        &mut app,
        "plugin.params",
        json!({"track": 0, "row": 5}),
        "not_found",
    );
    // HOSTED plugin target -> wrong_kind (native scripts only)
    app.mcp05_test_add_fx_vst_row(0, "GhostVerb");
    let m = is_error(
        &mut app,
        "plugin.params",
        json!({"track": 0, "row": 1}),
        "wrong_kind",
    );
    assert!(m.contains("GhostVerb"), "{m}");
}

// ── R3: the set is AUDIBLE (slow tier) ───────────────────────────────────

#[test]
#[ignore = "slow-tier: plugin script spawn + DSP render"]
fn v109_plugin_param_set_fx_audible() {
    let (mut app, _dir) = fixture("fxaud");
    app.mcp05_add_fx_script(0, "vfilter");
    let inst = app.mcp05_fx_live(0, 0).expect("live vfilter");
    // Deterministic white noise (LCG, dsp-audio-probe transient doctrine).
    let mut noise = vec![0f32; 4096];
    let mut s = 0x1234_5678u32;
    for v in noise.iter_mut() {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        *v = ((s >> 8) as f32 / 8388608.0) - 1.0;
    }
    // mode 0 = LP, mode 2 = HP (vfilter P_MODE). Same cut for both.
    fn proc_render(app: &mut App, inst: &fx::Instance, noise: &[f32], mode: f64) -> Vec<f32> {
        structured(
            app,
            "plugin.param.set",
            json!({"track": 0, "row": 0, "param": 0, "value": mode}),
        );
        noise.iter().map(|&x| inst.process_private(x)).collect()
    }
    structured(
        &mut app,
        "plugin.param.set",
        json!({"track": 0, "row": 0, "param": 1, "value": 2000.0}),
    );
    let lp = proc_render(&mut app, &inst, &noise, 0.0);
    let hp = proc_render(&mut app, &inst, &noise, 2.0);
    let low = tone_power(&lp, 300.0, 48000.0, 1024, 4096);
    let hi = tone_power(&lp, 6000.0, 48000.0, 1024, 4096);
    assert!(low > 25.0 * hi, "LP pass should kill the top: {low} vs {hi}");
    let low2 = tone_power(&hp, 300.0, 48000.0, 1024, 4096);
    let hi2 = tone_power(&hp, 6000.0, 48000.0, 1024, 4096);
    assert!(hi2 > 25.0 * low2, "HP pass should kill the bottom: {hi2} vs {low2}");
    // NEGATIVE control: a no-op set (mode 2 again) leaves bytes identical.
    let hp2 = proc_render(&mut app, &inst, &noise, 2.0);
    assert_eq!(hp, hp2, "same params must render identically (determinism)");
}

#[test]
#[ignore = "slow-tier: piano script spawn + DSP render"]
fn v109_plugin_param_set_synth_audible() {
    let (mut app, _dir) = fixture("sndaud");
    app.mcp05_set_track_synth(0, "piano");
    let inst = match app.mcp05_synth_handle(0).expect("bound piano").as_ref() {
        SynthHandle::Script(i) => i.clone(),
        _ => panic!("script"),
    };
    let p = structured(&mut app, "plugin.params", json!({"track": 0}));
    let lvl = p["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["name"] == "level")
        .expect("piano has a level param");
    let (k, lo, hi) = (
        lvl["index"].as_u64().unwrap(),
        lvl["min"].as_f64().unwrap(),
        lvl["max"].as_f64().unwrap(),
    );
    // (plain sequence: closures capturing &mut app would hold the borrow)
    let quiet = {
        let ack = structured(
            &mut app,
            "plugin.param.set",
            json!({"track": 0, "param": k, "value": lo}),
        );
        assert!((ack["value"].as_f64().unwrap() - lo).abs() < 1e-5, "{ack}");
        rms(&render(&inst, 60, 110))
    };
    let loud = {
        let ack = structured(
            &mut app,
            "plugin.param.set",
            json!({"track": 0, "param": k, "value": hi}),
        );
        assert!((ack["value"].as_f64().unwrap() - hi).abs() < 1e-5, "{ack}");
        rms(&render(&inst, 60, 110))
    };
    assert!(
        loud > 8.0 * quiet.max(1e-9),
        "level {lo}->{hi} must change the render: rms {quiet} -> {loud}"
    );
    assert!(app.status.starts_with("MCP:"));
}

// ── R4: preset storage round-trips (real bar storage, sanitize honored) ──

#[test]
fn v109_fx_preset_roundtrip_and_sanitize() {
    let (mut app, _dir) = fixture("fxpre");
    app.mcp05_add_fx_script(0, "delay");
    // change two params through the tool
    let p = structured(&mut app, "plugin.params", json!({"track": 0, "row": 0}));
    let (i0, i1) = (0usize, p["count"].as_u64().unwrap() as usize - 1);
    let v0 = p["params"][i0]["min"].as_f64().unwrap() + 0.0;
    let v1 = p["params"][i1]["max"].as_f64().unwrap();
    for (k, v) in [(i0, v0), (i1, v1)] {
        structured(
            &mut app,
            "plugin.param.set",
            json!({"track": 0, "row": 0, "param": k, "value": v}),
        );
    }
    // hostile name: the storage's sanitizer wins; ack reports the real name
    let saved = structured(
        &mut app,
        "plugin.preset.save",
        json!({"track": 0, "row": 0, "name": "../../evil name"}),
    );
    assert_eq!(saved["name"], "evil name", "{saved}");
    let list = structured(&mut app, "plugin.preset.list", json!({"track": 0, "row": 0}));
    assert!(
        list["presets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "evil name"),
        "{list}"
    );
    assert!(!list["builtin"].as_array().unwrap().is_empty(), "builtins listed");
    // move away, load back, observe
    for k in [i0, i1] {
        let mid = (p["params"][k]["min"].as_f64().unwrap()
            + p["params"][k]["max"].as_f64().unwrap())
            / 2.0;
        structured(
            &mut app,
            "plugin.param.set",
            json!({"track": 0, "row": 0, "param": k, "value": mid}),
        );
    }
    let loaded = structured(
        &mut app,
        "plugin.preset.load",
        json!({"track": 0, "row": 0, "name": "evil name"}),
    );
    assert_eq!(loaded["applied"], 2, "{loaded}");
    let live = app.mcp05_fx_live(0, 0).unwrap();
    assert!((live.param(i0) - v0 as f32).abs() < 1e-5);
    assert!((live.param(i1) - v1 as f32).abs() < 1e-5);
    // missing name -> not_found carrying the available list
    let m = is_error(
        &mut app,
        "plugin.preset.load",
        json!({"track": 0, "row": 0, "name": "nope"}),
        "not_found",
    );
    assert!(m.contains("evil name"), "{m}");
}

#[test]
fn v109_synth_preset_roundtrip() {
    let (mut app, dir) = fixture("synpre");
    app.mcp05_set_track_synth(0, "piano");
    let p = structured(&mut app, "plugin.params", json!({"track": 0}));
    let k = p["count"].as_u64().unwrap() as usize - 1;
    let hi = p["params"][k]["max"].as_f64().unwrap();
    structured(
        &mut app,
        "plugin.param.set",
        json!({"track": 0, "param": k, "value": hi}),
    );
    let saved = structured(
        &mut app,
        "plugin.preset.save",
        json!({"track": 0, "name": "bright!!"}),
    );
    assert_eq!(saved["name"], "bright", "{saved}"); // sanitize honored
    assert!(dir.join("synth-presets").join("piano").join("bright.json").exists());
    let mid = hi / 2.0;
    structured(
        &mut app,
        "plugin.param.set",
        json!({"track": 0, "param": k, "value": mid}),
    );
    let loaded = structured(
        &mut app,
        "plugin.preset.load",
        json!({"track": 0, "name": "bright"}),
    );
    let obs = &loaded["observed"];
    assert!((obs["params"][k]["value"].as_f64().unwrap() - hi).abs() < 1e-5, "{obs}");
    is_error(
        &mut app,
        "plugin.preset.load",
        json!({"track": 0, "name": "ghost"}),
        "not_found",
    );
}

#[test]
fn v109_sampler_map_preset_roundtrip() {
    let (mut app, dir) = fixture("mappre");
    app.mcp05_set_track_synth(0, "sampler");
    let a0 = app.mcp05_test_add_audio_asset("a0.wav", PathBuf::from("/tmp/v109-a0.wav"));
    app.mcp05_sampler_map_commit(0, crate::config::SamplerMapSave { slots: vec![], mode: 0 });
    structured(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "asset_id": a0, "root": 60, "vel": 100}),
    );
    let saved = structured(
        &mut app,
        "plugin.preset.save",
        json!({"track": 0, "surface": "map", "name": "my keys"}),
    );
    assert_eq!(saved["name"], "my keys");
    assert!(dir.join("sampler-presets").join("my keys.json").exists());
    // wipe the live map, load the preset back
    app.mcp05_sampler_map_commit(0, crate::config::SamplerMapSave { slots: vec![], mode: 0 });
    let empty = structured(&mut app, "sampler.map.get", json!({"track": 0}));
    assert_eq!(empty["count"], 0);
    let loaded = structured(
        &mut app,
        "plugin.preset.load",
        json!({"track": 0, "surface": "map", "name": "my keys"}),
    );
    assert_eq!(loaded["observed"]["count"], 1, "{loaded}");
    assert_eq!(loaded["observed"]["slots"][0]["root"], 60);
    // surface=map on a NON-sampler track -> wrong_kind
    app.mcp05_set_track_synth(1, "piano");
    is_error(
        &mut app,
        "plugin.preset.load",
        json!({"track": 1, "surface": "map", "name": "my keys"}),
        "wrong_kind",
    );
}

// ── R5: sampler map / layers / drum pads / midi notes ────────────────────

#[test]
fn v109_sampler_map_set_get_layers() {
    let (mut app, _dir) = fixture("smap");
    app.mcp05_set_track_synth(0, "sampler");
    let a0 = app.mcp05_test_add_audio_asset("kick.wav", PathBuf::from("/tmp/v109-kick.wav"));
    let a1 = app.mcp05_test_add_audio_asset("snare.wav", PathBuf::from("/tmp/v109-snare.wav"));
    let _ = a1;
    let out = structured(&mut app, "sampler.map.get", json!({"track": 0}));
    assert_eq!(out["has_map"], false, "fresh track has no map: {out}");
    // set slot 0 by TABLE INDEX — decomposed read-back
    let ack = structured(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "asset_id": a0, "root": 48, "vel": 100}),
    );
    assert_eq!(ack["slots"][0]["asset_id"], a0);
    assert_eq!(ack["slots"][0]["root"], 48);
    assert_eq!(ack["slots"][0]["vel"], 100);
    assert!(app.status.starts_with("MCP:"));
    // by exact rack path
    structured(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 1, "path": "/tmp/v109-snare.wav", "root": 54, "vel": 90}),
    );
    let out = structured(&mut app, "sampler.map.get", json!({"track": 0}));
    assert_eq!(out["slots"][1]["asset_id"], 1);
    // layer add on the SAME root (velocity layer per v52)
    let ack = structured(
        &mut app,
        "sampler.layers.add",
        json!({"track": 0, "root": 48, "asset_id": 1, "vel": 40}),
    );
    assert_eq!(ack["added_slot"], 2);
    assert_eq!(ack["layers_by_root"][0]["root"], 48);
    assert_eq!(ack["layers_by_root"][0]["layers"].as_array().unwrap().len(), 2);
    // mode
    let ack = structured(&mut app, "sampler.map.set", json!({"track": 0, "mode": 1}));
    assert_eq!(ack["mode_name"], "crossfade");
    // remove dense-indexes and shifts
    let ack = structured(&mut app, "sampler.layers.remove", json!({"track": 0, "slot": 1}));
    assert_eq!(ack["count"], 2);
    assert_eq!(ack["slots"][1]["root"], 48, "slots stay dense after removal");
    // editing the live map reaches the live instance (v51 publish seam)
    let inst = match app.mcp05_synth_handle(0).unwrap().as_ref() {
        SynthHandle::Script(i) => i.clone(),
        _ => panic!(),
    };
    let (map, mode, live) = inst.map_snapshot();
    assert!(live && !map.is_empty() && mode == 1, "map published live: {map:?} {mode}");
    // non-sampler track -> wrong_kind
    app.mcp05_set_track_synth(1, "piano");
    is_error(&mut app, "sampler.map.get", json!({"track": 1}), "wrong_kind");
}

#[test]
fn v109_sampler_validation_errors() {
    let (mut app, _dir) = fixture("sval");
    app.mcp05_set_track_synth(0, "sampler");
    app.mcp05_test_add_audio_asset("k.wav", PathBuf::from("/tmp/v109-k.wav"));
    // asset ids are rack table indices (R6 example error verbatim)
    let m = is_error(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "asset_id": 7}),
        "out_of_range",
    );
    assert!(m.contains("rack") && m.contains("MCP-04"), "{m}");
    is_error(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "path": "/nope/ghost.wav"}),
        "not_found",
    );
    is_error(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "asset_id": 0, "vel": 0}),
        "out_of_range",
    );
    is_error(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "asset_id": 0, "root": 128}),
        "out_of_range",
    );
    let m = is_error(
        &mut app,
        "sampler.layers.remove",
        json!({"track": 0, "slot": 9}),
        "out_of_range",
    );
    assert!(m.contains("dense"), "{m}");
    // slot beyond count+1 is a GAP — refused, count is dense
    let m = is_error(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 4, "asset_id": 0}),
        "out_of_range",
    );
    assert!(m.contains("append") || m.contains("dense"), "{m}");
    // asset edit without a slot target
    is_error(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "asset_id": 0}),
        "invalid",
    );
    // MIDI assets are not audio-slot material
    let pid = app.mcp05_test_add_pattern_asset("p.mid", PathBuf::from("/tmp/v109-p.mid"));
    is_error(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "asset_id": pid}),
        "wrong_kind",
    );
}

#[test]
#[ignore = "slow-tier: sampler script spawn + DSP render"]
fn v109_sampler_map_asset_and_velocity_audio() {
    // R3 sampler half: via MCP tools ONLY, pick slots/roots/layers, then
    // render and PROVE which asset (root select) and which layer (velocity
    // cutoff) the engine played — the v51 tone pair as the sound source.
    static TONES: Once = Once::new();
    TONES.call_once(|| {
        let mk = |f: f64| -> Arc<[f32]> {
            (0..4800)
                .map(|i| (i as f64 * std::f64::consts::TAU * f / 48000.0).sin() as f32 * 0.5)
                .collect()
        };
        crate::synth::set_host_assets(Arc::new(vec![mk(200.0), mk(800.0)]), 48_000);
    });
    let (mut app, _dir) = fixture("smaud");
    app.mcp05_set_track_synth(0, "sampler");
    let a0 = app.mcp05_test_add_audio_asset("low200.wav", PathBuf::from("/tmp/v109-lo.wav"));
    let a1 = app.mcp05_test_add_audio_asset("hi800.wav", PathBuf::from("/tmp/v109-hi.wav"));
    structured(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "asset_id": a0, "root": 48, "vel": 127}),
    );
    structured(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 1, "asset_id": a1, "root": 54, "vel": 127}),
    );
    let inst = match app.mcp05_synth_handle(0).unwrap().as_ref() {
        SynthHandle::Script(i) => i.clone(),
        _ => panic!(),
    };
    let dom = |note: u8, vel: u8, want: f64, off: f64| {
        let x = render(&inst, note, vel);
        let a = tone_power(&x, want, 48000.0, 512, 7680);
        let b = tone_power(&x, off, 48000.0, 512, 7680);
        assert!(a > 25.0 * b, "note {note} v{vel}: wanted {want}Hz, got {a} vs {b}");
    };
    // ROOT selection: note 48 plays asset 0 (200 Hz), note 54 asset 1.
    dom(48, 100, 200.0, 800.0);
    dom(54, 100, 800.0, 200.0);
    // VELOCITY layering on root 48 (mode 0 cutoff): retune slot 0 to a SOFT
    // 200 Hz layer (vel 40) and move slot 2 onto root 48 as the HARD 800 Hz
    // layer (vel 110). Layers on root 48: [asset0@40, asset1@110].
    structured(
        &mut app,
        "sampler.layers.add",
        json!({"track": 0, "root": 48, "asset_id": a1, "vel": 110}),
    );
    structured(
        &mut app,
        "sampler.map.set",
        json!({"track": 0, "slot": 0, "vel": 40}),
    );
    // cutoff: note vel 60 -> highest layer at-or-below = asset0 (200 Hz)
    dom(48, 60, 200.0, 800.0);
    // note vel 120 -> asset1 (800 Hz) — the VELOCITY LAYER flipped the sample
    dom(48, 120, 800.0, 200.0);
}

#[test]
fn v109_drum_kit_pad_and_preset_roundtrip() {
    let (mut app, _dir) = fixture("drum");
    app.mcp05_set_track_synth(0, "drum");
    app.mcp05_set_track_synth(1, "drum");
    let kick = app.mcp05_test_add_audio_asset("808kick.wav", PathBuf::from("/tmp/v109-808.wav"));
    let out = structured(&mut app, "drum.kit.get", json!({"track": 0}));
    let npads = out["count"].as_u64().unwrap() as usize;
    assert!(npads >= 8, "stock kit has pads: {out}");
    // assign a rack asset + velocity + mute through the tool
    let ack = structured(
        &mut app,
        "drum.pad.set",
        json!({"track": 0, "pad": 0, "asset_id": kick, "vel": 95, "muted": true, "name": "BOOM"}),
    );
    assert_eq!(ack["pads"][0]["asset_id"], kick);
    assert_eq!(ack["pads"][0]["vel"], 95);
    assert_eq!(ack["pads"][0]["muted"], true);
    assert_eq!(ack["pads"][0]["name"], "BOOM");
    // errors
    let m = is_error(
        &mut app,
        "drum.pad.set",
        json!({"track": 0, "pad": npads + 5, "vel": 9}),
        "out_of_range",
    );
    assert!(m.contains("pads"), "{m}");
    is_error(
        &mut app,
        "drum.pad.set",
        json!({"track": 0, "pad": 0, "vel": 0}),
        "out_of_range",
    );
    // save as a named user kit (v69 storage), load onto another track
    let saved = structured(
        &mut app,
        "drum.preset.save",
        json!({"track": 0, "name": "v109 kit"}),
    );
    assert_eq!(saved["name"], "v109 kit");
    let list = structured(&mut app, "drum.preset.list", json!({}));
    assert!(list["presets"].as_array().unwrap().iter().any(|x| x == "v109 kit"));
    let loaded = structured(
        &mut app,
        "drum.preset.load",
        json!({"track": 1, "name": "v109 kit"}),
    );
    assert_eq!(loaded["pads"][0]["name"], "BOOM", "{loaded}");
    assert_eq!(loaded["pads"][0]["vel"], 95);
    is_error(
        &mut app,
        "drum.preset.load",
        json!({"track": 1, "name": "ghost kit"}),
        "not_found",
    );
}

#[test]
fn v109_midi_note_set_commit_and_errors() {
    let (mut app, dir) = fixture("midi");
    let mid = dir.join("seq.mid");
    let pid = app.mcp05_test_add_pattern_asset("seq.mid", mid.clone());
    let audio = app.mcp05_test_add_audio_asset("hit.wav", dir.join("hit.wav"));
    let out = structured(&mut app, "midi.get", json!({"asset": pid}));
    assert_eq!(out["note_count"], 0);
    // add + overwrite (same pitch+start replaces — idempotent)
    structured(
        &mut app,
        "midi.note.set",
        json!({"asset": pid, "note": 60, "start": 0, "vel": 100}),
    );
    structured(
        &mut app,
        "midi.note.set",
        json!({"asset": pid, "note": 60, "start": 0, "len": 960, "vel": 111}),
    );
    structured(
        &mut app,
        "midi.note.set",
        json!({"asset": pid, "note": 64, "start": 480, "vel": 90}),
    );
    let out = structured(&mut app, "midi.get", json!({"asset": pid}));
    assert_eq!(out["note_count"], 2, "overwrite did not duplicate: {out}");
    assert_eq!(out["notes"][0]["len"], 960);
    assert_eq!(out["notes"][0]["vel"], 111);
    // commit is REAL: the roll's seam saved the .mid and re-pushed the
    // engine table — the asset's pattern file now parses back the notes.
    assert!(mid.exists(), "in-place commit must save the .mid");
    let back = crate::midipat::Pattern::load_mid(&mid).expect("saved mid parses");
    assert_eq!(back.notes.len(), 2);
    // remove by pitch row / by hit; missing -> not_found
    let ack = structured(&mut app, "midi.note.remove", json!({"asset": pid, "note": 60}));
    assert_eq!(ack["removed"], 1);
    is_error(
        &mut app,
        "midi.note.remove",
        json!({"asset": pid, "note": 60}),
        "not_found",
    );
    // errors
    is_error(&mut app, "midi.get", json!({"asset": 99}), "not_found");
    is_error(&mut app, "midi.get", json!({"asset": audio}), "wrong_kind");
    is_error(
        &mut app,
        "midi.note.set",
        json!({"asset": pid, "note": 128}),
        "out_of_range",
    );
    is_error(
        &mut app,
        "midi.note.set",
        json!({"asset": pid, "note": 60, "vel": 0}),
        "out_of_range",
    );
}

// ── R7: the whole surface over a REAL MCP socket exchange ────────────────

#[test]
#[ignore = "slow-tier: real socket + worker thread + frame pump"]
fn v109_mcp_socket_plugins_surface_e2e() {
    // v107 pumping model: the worker parses + queues + parks on the oneshot;
    // we sleep briefly, drain on THIS thread (= frame-loop role), read.
    let _dir = isolated_data_dir("sock");
    let mut app = headless_app("mcp05sock");
    app.mcp05_test_set_sr(48000);
    let ctx = egui::Context::default();
    app.mcp_port = 0;
    app.mcp_enabled = true;
    app.mcp.as_mut().unwrap().set_wait(std::time::Duration::from_millis(600));
    app.mcp_frame(&ctx);
    let (port, token) = crate::mcp::bridge::read_endpoint().expect("endpoint file");
    app.mcp05_add_fx_script(0, "delay"); // UI-thread chain build (before wire)

    fn wire(app: &mut App, port: u16, token: &str, body: &str) -> Value {
        use std::io::{Read, Write};
        let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        // Header lines composed at runtime (a prior write layer of this
        // session mangled "Prefix: {ph}" adjacency; see v107 lore).
        let hdr_auth = format!("{}: {} {}", "Authorization", "Bearer", token);
        let hdr_host = format!("Host: 127.0.0.1:{}", port);
        let hdr_len = format!("Content-Length: {}", body.len());
        let req = [
            "POST /mcp HTTP/1.1",
            hdr_host.as_str(),
            hdr_auth.as_str(),
            "Content-Type: application/json",
            hdr_len.as_str(),
            "Connection: close",
            "",
            body,
        ]
        .join("\r\n");
        sock.write_all(req.as_bytes()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
        {
            let mut srv = app.mcp.take().unwrap();
            srv.drain(app);
            app.mcp = Some(srv);
        }
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("HTTP head");
        let status: u16 = String::from_utf8_lossy(&raw[..split])
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        assert_eq!(status, 200, "wire status: {}", String::from_utf8_lossy(&raw[..split]));
        serde_json::from_slice(&raw[split + 4..]).expect("JSON body")
    }

    // tools/list carries the whole v109 family
    let v = wire(&mut app, port, &token, &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string());
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for n in [
        "plugins.list",
        "plugins.instances",
        "plugin.describe",
        "plugin.params",
        "plugin.param.get",
        "plugin.param.set",
        "plugin.preset.list",
        "plugin.preset.save",
        "plugin.preset.load",
        "sampler.map.get",
        "sampler.map.set",
        "sampler.layers.add",
        "sampler.layers.remove",
        "drum.kit.get",
        "drum.pad.set",
        "drum.preset.list",
        "drum.preset.save",
        "drum.preset.load",
        "midi.get",
        "midi.note.set",
        "midi.note.remove",
    ] {
        assert!(names.contains(&n), "tools/list missing {n}");
    }
    assert!(!names.contains(&"core.status"), "reserved names not registered");

    // tools/call plugin.param.set over the wire -> live state moved
    let p = structured(&mut app, "plugin.params", json!({"track": 0, "row": 0}));
    let last = (p["count"].as_u64().unwrap() as usize) - 1;
    let hi = p["params"][last]["max"].as_f64().unwrap();
    let v = wire(
        &mut app,
        port,
        &token,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"plugin.param.set",
              "arguments":{"track":0,"row":0,"param":last,"value":hi}}})
            .to_string(),
    );
    assert_eq!(v["result"]["isError"], false, "{v}");
    assert_eq!(v["result"]["structuredContent"]["value"], hi);
    let live = app.mcp05_fx_live(0, 0).unwrap();
    assert!(
        (live.param(last) - hi as f32).abs() < 1e-5,
        "wire write must move the live instance"
    );

    // structured error over the wire: bad sampler asset id keeps its code
    let v = wire(
        &mut app,
        port,
        &token,
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sampler.map.set",
              "arguments":{"track":0,"slot":0,"asset_id":4242}}})
            .to_string(),
    );
    assert_eq!(v["result"]["isError"], true);
    assert_eq!(v["result"]["error"]["code"], "out_of_range");
}
