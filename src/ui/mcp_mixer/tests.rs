//! v110/MCP-04 pins — `mixer.*` / `chain.*` / `assets.*` (card t_e0c0ad5e).
//!
//! Pins drive the REAL dispatcher (`rpc::dispatch`) — the v107 contract —
//! and read back through the same tools the agent uses (observed-then-ack
//! proof, not mirror poking). Requirement -> pin mapping:
//! ~/.hermes/profiles/ui-designer/cache/scratch/slooper-v110/REQUIREMENTS.md.

use serde_json::{Value, json};

use crate::config::Config;
use crate::mcp::rpc;
use crate::ui::{App, headless_app};

fn call(app: &mut App, name: &str, args: Value) -> Value {
    rpc::dispatch(
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":args}}),
        app,
    )
    .expect("tools/call answers")
}

/// structuredContent of a tools/call response (panics loudly on -32xxx so
/// a latch/refusal never masquerades as a result in the pins).
fn structured(app: &mut App, name: &str, args: Value) -> Value {
    let r = call(app, name, args);
    assert!(r.get("error").is_none(), "unexpected JSON-RPC error: {r}");
    r["result"]["structuredContent"].clone()
}

fn is_error(app: &mut App, name: &str, args: Value) -> Value {
    let r = call(app, name, args);
    assert_eq!(
        r["result"]["isError"].as_bool(),
        Some(true),
        "expected a structured domain failure, got {r}"
    );
    let mut sc = r["result"]["structuredContent"].clone();
    // The human text lives in content[0].text (rpc::tool_err) — surface it
    // under "message" so pins can grep refusals for self-documentation.
    if let Some(t) = r["result"]["content"][0]["text"].as_str() {
        sc["message"] = json!(t);
    }
    sc
}

fn cfg_text() -> String {
    std::fs::read_to_string(Config::path()).expect("config.json in the headless sandbox")
}

/// The samples folder headless_app builds (contains kick.wav, and IS the
/// one configured rack root).
fn rack_root(app: &App) -> std::path::PathBuf {
    std::path::PathBuf::from(&app.rack[0].path)
}

fn kick_path(app: &App) -> String {
    rack_root(app).join("kick.wav").to_string_lossy().into_owned()
}

// ── R6: registration + declared flags ─────────────────────────────────────

#[test]
fn v110_mcp_table_registered() {
    let names: Vec<(&str, bool, bool)> = crate::mcp::tools::table()
        .iter()
        .map(|t| (t.name, t.read, t.destructive))
        .collect();
    for (name, read, destr) in [
        ("mixer.list", true, false),
        ("mixer.get", true, false),
        ("mixer.set", false, false),
        ("chain.catalog", true, false),
        ("chain.list", true, false),
        ("chain.pick", false, false),
        ("chain.remove", false, true),
        ("assets.list", true, false),
        ("assets.import", false, false),
        ("assets.remove", false, true),
        ("assets.assign", false, false),
    ] {
        let hit = names.iter().find(|(n, ..)| *n == name);
        assert!(hit.is_some(), "{name} must be registered (v110)");
        let (_, r, d) = hit.unwrap();
        assert_eq!(*r, read, "{name}: readOnlyHint flag");
        assert_eq!(*d, destr, "{name}: destructiveHint flag");
    }
}

// ── R1: mixer round-trips on the live app ─────────────────────────────────

#[test]
fn v110_mcp_mixer_roundtrip() {
    let mut app = headless_app("v110mix");
    let s = structured(&mut app, "mixer.set", json!({"track":0,"control":"vol","value":90}));
    assert_eq!(s["row"]["vol"], json!(90), "ack carries the OBSERVED row");
    assert!(s["status"].as_str().unwrap().contains("MCP"), "status echo: {s}");
    let g = structured(&mut app, "mixer.get", json!({"track":0}));
    assert_eq!(g["vol"], json!(90), "get reads the live mirror");
    // The engine echo must be visible to the settings surface too (same
    // underlying seam: config_snapshot).
    let st = structured(&mut app, "settings.get", json!({"name":"trk_vol"}));
    assert_eq!(st["value"][0], json!(90), "snapshot follows the mixer seam");
    // gain + pan over the same path; list shows all three per track.
    structured(&mut app, "mixer.set", json!({"track":2,"control":"gain","value":64}));
    structured(&mut app, "mixer.set", json!({"track":2,"control":"pan","value":100}));
    let l = structured(&mut app, "mixer.list", json!({}));
    assert_eq!(l["tracks"][2]["gain"], json!(64));
    assert_eq!(l["tracks"][2]["pan"], json!(100));
    // Persistence: a discrete agent call lands NOW (unlike a live drag).
    let cfg: Config = serde_json::from_str(&cfg_text()).unwrap();
    assert_eq!(cfg.trk_vol[0], 90, "config file carries the live value");
    assert_eq!(cfg.trk_gain[2], 64, "gain persisted through the seam");
    assert_eq!(cfg.trk_pan[2], 100, "pan persisted through the seam");
}

#[test]
fn v110_mcp_mixer_bad_args() {
    let mut app = headless_app("v110mixbad");
    let before = app.trk_vol.to_vec();
    let before_status = app.status.clone();
    // Unknown track / control / value are all refused with ZERO change.
    for args in [
        json!({"track":99,"control":"vol","value":1}),
        json!({"track":0,"control":"wobble","value":1}),
        json!({"track":0,"control":"vol","value":128}),
        json!({"track":0,"control":"vol","value":"loud"}),
        json!({"control":"vol","value":1}), // track missing
    ] {
        let e = is_error(&mut app, "mixer.set", args.clone());
        assert!(
            e["error"].as_str().is_some_and(|k| k == "out_of_range" || k == "invalid_params"),
            "{args} must refuse structured, got {e}"
        );
    }
    assert_eq!(app.trk_vol.to_vec(), before, "refusals change nothing");
    assert_eq!(app.status, before_status, "refusals do not even echo");
}

#[test]
fn v110_mcp_mixer_color() {
    let mut app = headless_app("v110col");
    let s = structured(&mut app, "mixer.set", json!({"track":1,"control":"color","value":3}));
    assert_eq!(s["row"]["color"], json!(3));
    assert_eq!(app.trk_color[1], Some(3));
    structured(&mut app, "mixer.set", json!({"track":1,"control":"color","value":null}));
    assert_eq!(app.trk_color[1], None, "null resets to the kind colors");
    let e = is_error(&mut app, "mixer.set", json!({"track":1,"control":"color","value":99}));
    assert_eq!(e["error"], "out_of_range");
    assert_eq!(app.trk_color[1], None, "bad index changed nothing");
}

#[test]
fn v110_mcp_mixer_sc_send() {
    let mut app = headless_app("v110sc");
    let s = structured(&mut app, "mixer.set", json!({"track":0,"control":"sc_send","value":[2,5]}));
    assert_eq!(s["row"]["sc_send"], json!([2, 5]));
    assert_eq!(app.sc_send[0], (1u128 << 2) | (1u128 << 5));
    // A track cannot key itself (the GUI chips enforce the same rule).
    let e = is_error(&mut app, "mixer.set", json!({"track":3,"control":"sc_send","value":[3]}));
    assert_eq!(e["error"], "out_of_range");
    assert_eq!(app.sc_send[3], 0, "self-send refused before any write");
    // [] clears (the strip's "none" button); snapshot agrees.
    structured(&mut app, "mixer.set", json!({"track":0,"control":"sc_send","value":[]}));
    assert_eq!(app.sc_send[0], 0);
    let st = structured(&mut app, "settings.get", json!({"name":"sc_send"}));
    assert_eq!(st["value"][0], json!(0));
}

#[test]
fn v110_mcp_mixer_master() {
    let mut app = headless_app("v110mas");
    let s = structured(&mut app, "mixer.set", json!({"control":"master_vol","value":100}));
    assert_eq!(s["master"]["vol"], json!(100));
    assert_eq!(app.master_midi, 100);
    assert!(app.master_dirty, "the dirty flag drives the engine push");
    let l = structured(&mut app, "mixer.list", json!({}));
    assert_eq!(l["master"]["vol"], json!(100));
    let e = is_error(&mut app, "mixer.set", json!({"control":"master_vol","value":128}));
    assert_eq!(e["error"], "out_of_range");
    assert_eq!(app.master_midi, 100, "refusal left the mirror");
    // Same seam the VolMaster binding / settings-apply arm use (extracted
    // v110 home): settings.set must still reach the identical state.
    structured(&mut app, "settings.set", json!({"name":"master_vol","value":115}));
    assert_eq!(app.master_midi, 115, "both surfaces share one home");
}

#[test]
fn v110_mcp_read_only_latch() {
    let mut app = headless_app("v110latch");
    app.mcp_read_only = true;
    let before = app.trk_vol.to_vec();
    for (name, args) in [
        ("mixer.set", json!({"track":0,"control":"vol","value":1})),
        ("chain.pick", json!({"track":0,"plugin":"chorus"})),
        ("chain.remove", json!({"track":0,"slot":0})),
        ("assets.remove", json!({"asset":0})),
        ("assets.import", json!({"path":"nope.wav"})),
        ("assets.assign", json!({"target":"channel","track":0,"asset":0})),
    ] {
        // The latch is TRANSPORT-level (rpc gate, not the tool table):
        // a JSON-RPC -32003 error, the request never reaches execute().
        let r = call(&mut app, name, args);
        assert_eq!(
            r["error"]["code"].as_i64(),
            Some(-32003),
            "{name} under the latch must hit the rpc gate"
        );
    }
    assert_eq!(app.trk_vol.to_vec(), before, "the latch means NOTHING moves");
    app.mcp_read_only = false;
    // Reads survive the (now-off) latch untouched — sanity that the write
    // refusal above was the latch, not a broken dispatcher.
    let l = structured(&mut app, "mixer.list", json!({}));
    assert!(l["tracks"].is_array());
}

// ── R2: chain tools (script tier; plugin tier = slow) ─────────────────────

#[test]
fn v110_mcp_chain_script_roundtrip() {
    let mut app = headless_app("v110chain");
    let cat = structured(&mut app, "chain.catalog", json!({}));
    let stems: Vec<&str> = cat["scripts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert!(stems.contains(&"chorus"), "the combo catalog must list chorus: {stems:?}");
    let empty = structured(&mut app, "chain.list", json!({"track":0}));
    assert_eq!(empty["count"], json!(0));

    let pick = structured(&mut app, "chain.pick", json!({"track":0,"plugin":"chorus"}));
    assert_eq!(pick["status"], "added");
    assert_eq!(pick["rows"].as_array().unwrap().len(), 1);
    assert_eq!(pick["rows"][0]["kind"], "script");
    assert_eq!(pick["rows"][0]["id"], "chorus");
    assert_eq!(pick["rows"][0]["status"], "ready");
    assert_eq!(app.fx_rows[0].len(), 1, "the add_fx_script seam ran");

    let l = structured(&mut app, "chain.list", json!({"track":0}));
    assert_eq!(l["count"], json!(1));

    // Bad args first (still zero change).
    let e = is_error(&mut app, "chain.pick", json!({"track":0,"plugin":"no-such-thing"}));
    assert_eq!(e["error"], "not_found");
    let e = is_error(&mut app, "chain.remove", json!({"track":0,"slot":7}));
    assert_eq!(e["error"], "out_of_range");
    assert_eq!(app.fx_rows[0].len(), 1);

    let rm = structured(&mut app, "chain.remove", json!({"track":0,"slot":0}));
    assert_eq!(rm["removed"], "chorus", "the seam's own label echoes");
    assert_eq!(rm["rows"].as_array().unwrap().len(), 0);
    assert_eq!(app.fx_rows[0].len(), 0);
}

#[test]
fn v110_mcp_chain_pick_instrument_refused() {
    let mut app = headless_app("v110instr");
    // A scanned INSTRUMENT sits in the catalog (chain hosts effects only).
    app.vst_scan.push(crate::vsthost::VstEntry {
        path: std::path::PathBuf::from("/fake/synth.vst3"),
        kind: crate::vsthost::VstKind::V3,
        name: "FakeSynth".into(),
        vendor: "test".into(),
        instrument: true,
        plugin_id: None,
        error: None,
    });
    let before = app.fx_rows[0].len();
    let e = is_error(&mut app, "chain.pick", json!({"track":0,"plugin":"/fake/synth.vst3"}));
    assert_eq!(e["error"], "invalid_params");
    assert!(
        e["message"].as_str().is_some_and(|m| m.contains("instrument")),
        "the refusal explains the ♪ route: {e}"
    );
    assert_eq!(app.fx_rows[0].len(), before, "no row may enter for an instrument");
    // Same refusal via the name alias.
    let e2 = is_error(&mut app, "chain.pick", json!({"track":0,"plugin":"FakeSynth"}));
    assert_eq!(e2["error"], "invalid_params");
}

#[test]
fn v110_mcp_chain_slot_insert_shifts_pending() {
    use crate::audio::MAX_LOOPS;
    let mut app = headless_app("v110slot");
    // A hosted slot with a load still in flight (row index registered in
    // vst_pending_gui — the async contract add_fx_plugin establishes).
    let (_tx, rx) = std::sync::mpsc::channel();
    app.fx_rows[0].push(crate::ui::ChainRow::Vst {
        state: None,
        path: "/fake/held.vst3".into(),
        kind: crate::vsthost::VstKind::V3,
        plugin_id: None,
        name: "Held".into(),
        params: Vec::new(),
        entry: None,
        open_rx: Some(rx),
        program: None,
        bypass: false,
    });
    app.vst_pending_gui.push((0, 0));
    // Script into slot 0: the loading row must follow it (shift to 1),
    // else poll_vst_opens would land its entry into the WRONG row.
    let pick = structured(&mut app, "chain.pick", json!({"track":0,"plugin":"chorus","slot":0}));
    assert_eq!(pick["rows"][0]["id"], "chorus");
    assert_eq!(pick["rows"][1]["name"], "Held");
    assert_eq!(app.vst_pending_gui, vec![(0usize, 1usize)], "pending index follows its row");
    // Bad slot (past the post-pick length) refuses with nothing added.
    let e = is_error(
        &mut app,
        "chain.pick",
        json!({"track":0,"plugin":"chorus","slot":MAX_LOOPS as u64 + 9}),
    );
    assert_eq!(e["error"], "out_of_range");
    assert_eq!(app.fx_rows[0].len(), 2);
}

/// Slow tier: a REAL scanned plugin must ride pick -> loading -> ready.
/// dlopen ⇒ excluded from the quick tier (slooper-dev test tiers).
#[test]
#[ignore = "slow-tier: dlopen plugin load"]
fn v110_mcp_chain_pick_plugin_async() {
    let mut app = headless_app("v110plug");
    if !app.vst_scanned && app.vst_scan_rx.is_none() {
        app.vst_rescan();
    }
    // Pump the drain the production frame loop runs (poll_vst_opens).
    for _ in 0..1200 {
        app.poll_vst_opens();
        if app.vst_scanned {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let pickable: Vec<_> = app
        .vst_scan
        .iter()
        .filter(|e| e.error.is_none() && !e.instrument)
        .cloned()
        .collect();
    let Some(entry) = pickable.into_iter().next() else {
        eprintln!(
            "no scannable effect plugin in this environment (scan note: {:?}) — \
             catalog-only verdict",
            app.vst_note
        );
        let cat = structured(&mut app, "chain.catalog", json!({}));
        assert!(cat["scanned"].as_bool().unwrap(), "the scan itself must complete");
        return;
    };
    let pick = structured(
        &mut app,
        "chain.pick",
        json!({"track":0, "plugin": entry.path.to_string_lossy()}),
    );
    assert_eq!(pick["status"], "loading", "async ack (the UI thread never blocks)");
    let l = structured(&mut app, "chain.list", json!({"track":0}));
    assert_eq!(l["rows"][0]["status"], "loading");
    // Wait for the plugin-thread landing (same drain production pumps).
    let mut ready = false;
    for _ in 0..800 {
        app.poll_vst_opens();
        let l = structured(&mut app, "chain.list", json!({"track":0}));
        if l["rows"][0]["status"] == "ready" {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(ready, "loading -> ready must land (note: {:?})", app.vst_note);
    let rm = structured(&mut app, "chain.remove", json!({"track":0,"slot":0}));
    assert_eq!(rm["removed"], entry.name, "removal echoes the plugin label");
}

// ── R3/R4: assets rack round-trip, guard, assign ──────────────────────────

#[test]
fn v110_mcp_assets_roundtrip() {
    let mut app = headless_app("v110rack");
    let kick = kick_path(&app);
    let imp = structured(&mut app, "assets.import", json!({"path": kick}));
    assert_eq!(imp["kind"], "asset");
    assert_eq!(imp["name"], "kick");
    assert_eq!(imp["asset_kind"], "audio");
    let id = imp["asset"].as_u64().unwrap();
    let l = structured(&mut app, "assets.list", json!({}));
    assert!(
        l["assets"].as_array().unwrap().iter().any(|a| a["asset"] == json!(id)),
        "imported asset is listed"
    );
    // The rack folder lists the file itself (kind + path are agent-facing;
    // the rack paints STEMS — the list follows the same rule).
    assert!(
        l["racks"][0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "kick" && f["kind"] == "audio"),
        "rack scan lists the file: {}",
        l["racks"][0]["files"]
    );
    // Import is DEDUP (ensure_asset), the source file untouched.
    let again = structured(&mut app, "assets.import", json!({"path": kick}));
    assert_eq!(again["asset"], json!(id), "re-import dedups to the same table id");
    // Bad args.
    let e = is_error(&mut app, "assets.import", json!({"path": "/nope/nada.wav"}));
    assert_eq!(e["error"], "invalid_params");
    let txt = rack_root(&app).join("notes.txt");
    std::fs::write(&txt, "hi").unwrap();
    let e = is_error(&mut app, "assets.import", json!({"path": txt}));
    assert_eq!(e["error"], "invalid_params");
    // Table removal (asset ids remap — the tool says so).
    let rm = structured(&mut app, "assets.remove", json!({"asset": id}));
    assert_eq!(rm["table_removal"], json!(true));
    let l = structured(&mut app, "assets.list", json!({}));
    assert!(
        !l["assets"].as_array().unwrap().iter().any(|a| a["name"] == "kick"),
        "removed asset is gone"
    );
    // Removal by PATH still works INSIDE the rack (file deleted there).
    let wav2 = rack_root(&app).join("two.wav");
    std::fs::copy(rack_root(&app).join("kick.wav"), &wav2).unwrap();
    structured(&mut app, "assets.import", json!({"path": wav2}));
    let rm = structured(&mut app, "assets.remove", json!({"path": wav2}));
    assert_eq!(rm["deleted_file"], json!(wav2));
    assert!(!wav2.exists(), "rack-internal file really left the disk");
    assert!(rack_root(&app).join("kick.wav").exists(), "the fixture survives");
}

#[test]
fn v110_mcp_assets_remove_guard() {
    let mut app = headless_app("v110guard");
    // A foreign WAV in temp (NOT under any rack root): removal must REFUSE
    // and the file must SURVIVE byte-identical (foreign files never mutate).
    let foreign = std::env::temp_dir().join("v110-foreign.wav");
    std::fs::copy(rack_root(&app).join("kick.wav"), &foreign).unwrap();
    let before = std::fs::read(&foreign).unwrap();
    let e = is_error(&mut app, "assets.remove", json!({"path": foreign}));
    assert_eq!(e["error"], "invalid_params");
    assert!(
        e["message"].as_str().unwrap().contains("rack"),
        "the refusal names the rack rule: {e}"
    );
    assert_eq!(std::fs::read(&foreign).unwrap(), before, "foreign file untouched");
    let _ = std::fs::remove_file(&foreign);
    // Unknown asset id: structured out_of_range.
    let e = is_error(&mut app, "assets.remove", json!({"asset": 4242}));
    assert_eq!(e["error"], "out_of_range");
}

#[test]
fn v110_mcp_assets_assign_channel() {
    let mut app = headless_app("v110chan");
    let kick = kick_path(&app);
    let imp = structured(&mut app, "assets.import", json!({"path": kick}));
    let id = imp["asset"].as_u64().unwrap();
    let a = structured(
        &mut app,
        "assets.assign",
        json!({"target":"channel","track":0,"asset":id,"pos":1000,"lane":0}),
    );
    assert_eq!(a["lane"], json!(0));
    let clips = &app.arr.tracks[0].lanes[0].clips;
    assert_eq!(clips.len(), 1, "the lane_row drop contract placed a clip");
    assert_eq!(clips[0].asset, id as u32);
    assert_eq!(clips[0].pos, 1000);
    assert_eq!(app.sel_clips, vec![(0usize, 0usize, 0usize)], "placed clip is selected");
    assert!(a["status"].as_str().unwrap().contains("clip placed"));
    // Bad target / bad asset: refusal with no clip added.
    let e = is_error(&mut app, "assets.assign", json!({"target":"wobble","track":0,"asset":id}));
    assert_eq!(e["error"], "invalid_params");
    let e = is_error(&mut app, "assets.assign", json!({"target":"channel","track":0,"asset":999}));
    assert_eq!(e["error"], "out_of_range");
    assert_eq!(app.arr.tracks[0].lanes[0].clips.len(), 1);
    // Exactly-one-source rule.
    let e = is_error(
        &mut app,
        "assets.assign",
        json!({"target":"channel","track":0,"asset":id,"path":kick}),
    );
    assert_eq!(e["error"], "invalid_params");
}

/// Bind a sampler script synth as a fixture (the same handle type the
/// production bind lands) — then the sampler/keymap/pad assign surfaces.
fn bind_sampler(app: &mut App, t: usize) -> bool {
    if let Some(inst) = app.synth_registry.create_instance("sampler", 48_000.0, &[]) {
        app.track_synth[t] = Some("sampler".into());
        app.track_synth_inst[t] = Some(std::sync::Arc::new(crate::audio::SynthHandle::Script(
            inst,
        )));
        return true;
    }
    false
}

#[test]
fn v110_mcp_assets_assign_sampler_keymap_pad() {
    let mut app = headless_app("v110asg");
    let kick = kick_path(&app);
    let id = structured(&mut app, "assets.import", json!({"path": kick}))["asset"]
        .as_u64()
        .unwrap();
    assert!(bind_sampler(&mut app, 0), "the bundled sampler script must instantiate");

    // target=sampler: the sampler window's drop-zone seam. The LIVE synth
    // param0 is the source of truth (a key-map exists only once one was
    // opened — the legacy single-asset route writes the param + slot 0
    // only if a map exists).
    let a = structured(
        &mut app,
        "assets.assign",
        json!({"target":"sampler","track":0,"asset":id}),
    );
    assert!(a["status"].as_str().unwrap().contains("sampler"));
    assert_eq!(
        a["synth_param0"],
        json!(id as f64),
        "the live sampler param adopted the asset: {a}"
    );

    // target=keymap: the C3 first-layer drop-zone seam.
    let a = structured(
        &mut app,
        "assets.assign",
        json!({"target":"keymap","track":1,"asset":id}),
    );
    // Track 1 has NO synth bound — the seam seeds the map regardless of a
    // bound synth (the drop handler path is identical); the SLOT must exist.
    assert_eq!(a["slots"], json!(1), "first key-map layer created at C3");
    let map = app.sampler_maps[1].clone().expect("map seeded");
    assert_eq!(map.slots[0].root, 48, "the Empty zone creates the C3 layer");

    // target=pad: the per-pad "wav" button seam (kit take-put flow).
    let pads = app.kit(0).len();
    assert!(pads > 0, "the default kit has pads");
    let a = structured(
        &mut app,
        "assets.assign",
        json!({"target":"pad","track":0,"pad":0,"asset":id}),
    );
    assert!(a["pad_asset_path"].as_str().unwrap().contains("kick"));
    assert_eq!(app.kit(0)[0].asset_path, kick);
    // Out-of-range pad + unbound-sampler refusal (zero change).
    let e = is_error(&mut app, "assets.assign", json!({"target":"pad","track":0,"pad":pads as u64 + 5,"asset":id}));
    assert_eq!(e["error"], "out_of_range");
    let e = is_error(&mut app, "assets.assign", json!({"target":"sampler","track":3,"asset":id}));
    assert_eq!(e["error"], "wrong_kind", "track 3 has no script synth bound");
    assert!(app.sampler_maps.get(3).map(|m| m.is_none()).unwrap_or(true));
}

#[test]
fn v113_mcp_reloc_remaps_targets() {
    let mut app = headless_app("v113reloc");
    // A loading row sits at 0 with routing already aimed at it.
    let (_tx, rx) = std::sync::mpsc::channel();
    app.fx_rows[0].push(crate::ui::ChainRow::Vst {
        state: None,
        path: "/fake/held.vst3".into(),
        kind: crate::vsthost::VstKind::V3,
        plugin_id: None,
        name: "Held".into(),
        params: Vec::new(),
        entry: None,
        open_rx: Some(rx),
        program: None,
        bypass: false,
    });
    app.vst_pending_gui.push((0, 0));
    let dir = std::env::temp_dir().join("sp-uity-v113reloc").join("auto");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("c.slauto");
    let a = crate::automation::Automation {
        target: "fx:0:1".into(),
        len_samples: 4096,
        nodes: vec![
            crate::automation::Node { t: 0, v: 0.5, c: 0.0 },
            crate::automation::Node { t: 4096, v: 0.5, c: 0.0 },
        ],
    };
    std::fs::write(&path, a.to_json().unwrap()).unwrap();
    let id = app.assets.len() as u32;
    app.assets.push(crate::assets::Asset::automation_asset(
        id,
        "c".into(),
        path,
        std::sync::Arc::new(a),
        false,
    ));
    app.arr.insert(0, 0, crate::arrange::Clip::new(id, 0));
    app.value_binds.push(crate::config::ValueBind {
        target: "fx:0:1".into(),
        track: 0,
        bind: crate::midi::MidiBind { ty: 1, ch: 0, d1: 7 },
        kind: 0,
        lo: 0,
        hi: 127,
    });
    // Insert a script at slot 0: the loading row AND its routing shift to 1.
    let pick = structured(&mut app, "chain.pick", json!({"track": 0, "plugin": "chorus", "slot": 0}));
    assert_eq!(pick["rows"][1]["name"], "Held");
    assert_eq!(app.vst_pending_gui, vec![(0usize, 1usize)], "v110 contract preserved via the shared seam");
    assert_eq!(app.assets[id as usize].automation.as_ref().unwrap().target, "fx:1:1", "curve follows");
    assert_eq!(app.value_binds[0].target, "fx:1:1", "binding follows");
    // And chain.remove on this track rides the same seam end-to-end.
    let rm = structured(&mut app, "chain.remove", json!({"track": 0, "slot": 0}));
    assert_eq!(rm["removed"], "chorus");
    assert_eq!(app.assets[id as usize].automation.as_ref().unwrap().target, "fx:0:1", "shift back down");
    assert_eq!(app.value_binds[0].target, "fx:0:1");
    assert_eq!(app.vst_pending_gui, vec![(0usize, 0usize)]);
}
