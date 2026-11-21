//! v108/MCP-02 pins — `settings.list/get/set` (card t_377dcec9).
//!
//! Pins drive the REAL dispatcher (`rpc::dispatch`), never the tool fns
//! (v107 contract), and read back through the same tools (the observed
//! surface the AC speaks of: "the pin observes the running value, not just
//! the file"). Requirement -> pin mapping lives in the card's
//! REQUIREMENTS.md (scratch/slooper-v108/).

use serde_json::{json, Value};

use crate::config::Config;
use crate::mcp::rpc;
use crate::mcp_settings;
use crate::ui::{headless_app, App};

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
    r["result"]["structuredContent"].clone()
}

fn settings_list(app: &mut App) -> Vec<Value> {
    let s = structured(app, "settings.list", json!({}));
    assert!(s["count"].as_u64().unwrap() as usize >= 70, "config has ~79 persisted fields");
    s["settings"].as_array().unwrap().clone()
}

fn get_value(app: &mut App, name: &str) -> Value {
    structured(app, "settings.get", json!({"name": name}))["value"].clone()
}

fn cfg_text() -> String {
    std::fs::read_to_string(Config::path()).expect("config.json in the headless sandbox")
}

// ── R1: parity — the surface IS the serde schema ──────────────────────────

#[test]
fn v108_settings_list_parity() {
    let mut app = headless_app("v108par");
    let entries = settings_list(&mut app);
    // THE auto-derivation proof: keys of to_value(Config::default()) —
    // the serde schema itself — equal the exposed names, both ways.
    let schema: Vec<String> = serde_json::to_value(Config::default())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let exposed: Vec<String> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    let mut s2 = exposed.clone();
    s2.sort();
    let missing: Vec<&String> = schema.iter().filter(|k| !exposed.contains(k)).collect();
    let extra: Vec<&String> = exposed.iter().filter(|k| !schema.contains(k)).collect();
    assert!(missing.is_empty(), "serde fields missing from settings.list: {missing:?}");
    assert!(extra.is_empty(), "settings.list exposes non-schema names: {extra:?}");
    assert_eq!(entries.len(), schema.len(), "exact count parity");
    assert!(s2.windows(2).all(|w| w[0] < w[1]), "names unique");
    // Every entry carries the schema fields the card demands.
    for e in &entries {
        assert!(e["json_type"].is_string(), "{e}");
        assert!(e["requires_restart"].is_boolean(), "{e}");
        assert!(e.get("value").is_some(), "{e}");
    }
    // Reverse completeness of the CLASSIFICATION tables (typo guard: a
    // LIVE/RESTART name that no longer exists in Config must go red).
    let all: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    for n in mcp_settings::LIVE.iter().chain(mcp_settings::RESTART) {
        assert!(all.contains(n), "classified name {n} is not a serialized Config field");
    }
}

// ── R2: reads report the RUNNING app, not the file ─────────────────────────

#[test]
fn v108_settings_get_and_list_live_values() {
    let mut app = headless_app("v108live");
    assert_eq!(get_value(&mut app, "snap_on"), json!(true), "default");
    // Live-write through the tool, then DELETE the file: a file-reading
    // surface would now fall back or fail — the live surface keeps the
    // running value untouched.
    let ack = structured(&mut app, "settings.set", json!({"name": "snap_on", "value": false}));
    assert_eq!(ack["applied"], "live");
    std::fs::remove_file(Config::path()).unwrap();
    assert_eq!(get_value(&mut app, "snap_on"), json!(false), "get reads RUNNING state");
    let found = settings_list(&mut app)
        .into_iter()
        .find(|e| e["name"] == "snap_on")
        .unwrap();
    assert_eq!(found["value"], json!(false), "list reads RUNNING state");
}

// ── R3: live tier applies through the real seams + echoes + persists ──────

#[test]
fn v108_settings_set_live_roundtrip() {
    let mut app = headless_app("v108set");
    // bool
    let ack = structured(&mut app, "settings.set", json!({"name": "snap_on", "value": false}));
    assert_eq!(ack["applied"], "live");
    assert_eq!(ack["requires_restart"], false);
    assert_eq!(ack["value"], json!(false), "observed-then-ack");
    assert_eq!(get_value(&mut app, "snap_on"), json!(false));
    assert!(cfg_text().contains("\"snap_on\": false"), "persists");
    assert!(app.status.contains("MCP: snap_on = false"), "status echo: {}", app.status);
    // int + string-field seam (metro_bpm lives in a String TextEdit)
    structured(&mut app, "settings.set", json!({"name": "roll_vel_step", "value": 9}));
    assert_eq!(get_value(&mut app, "roll_vel_step"), json!(9));
    structured(&mut app, "settings.set", json!({"name": "metro_bpm", "value": 97}));
    assert_eq!(get_value(&mut app, "metro_bpm"), json!(97));
    assert!(cfg_text().contains("\"metro_bpm\": 97"));
    // string enum -> theme mirror (visuals follow next frame)
    structured(&mut app, "settings.set", json!({"name": "theme", "value": "light"}));
    assert_eq!(get_value(&mut app, "theme"), json!("light"));
    // Array seam: the SHORT live list pads to the full padded table — the
    // readback is the LIVE state (132-entry table), not the submitted echo.
    structured(&mut app, "settings.set", json!({"name": "trk_vol", "value": [64, 32]}));
    let vol = get_value(&mut app, "trk_vol");
    let vol = vol.as_array().unwrap();
    assert_eq!((&vol[0], &vol[1]), (&json!(64), &json!(32)));
    assert!(vol[2..].iter().all(|v| *v == json!(crate::ui::VOL_UNITY_MIDI)), "rest padded");
    // clamped numeric + selection-invariant seam (n_tracks runs clamp_track_sel)
    structured(&mut app, "settings.set", json!({"name": "n_tracks", "value": 8}));
    assert_eq!(get_value(&mut app, "n_tracks"), json!(8));
    assert!(cfg_text().contains("\"n_tracks\": 8"));
}

#[test]
fn v108_settings_shortcuts_live_fill() {
    let mut app = headless_app("v108sc");
    // migrate_shortcuts only TRUSTS full-width tables (partial widths are
    // legacy remaps or defaults) — so the MCP contract is read-modify-write
    // of the full table. Read the current one, modify three slots:
    let cur = get_value(&mut app, "shortcuts");
    let mut table: Vec<Value> = cur.as_array().unwrap().clone();
    assert_eq!(table.len(), crate::ui::N_ACTIONS, "snapshot is full width");
    // The v35 go-to pair — the v108 product-bug fix: Home/End were bound in
    // the defaults but key_from_name dropped them on every load. They must
    // survive an MCP round trip now.
    let home_i = table.iter().position(|v| *v == json!("Home")).expect("default Home bind");
    let end_i = table.iter().position(|v| *v == json!("End")).expect("default End bind");
    table[0] = json!("Q"); // real rename
    table[1] = json!(null); // explicit unbind
    table[3] = json!("ZogZogNotAKey"); // unparseable -> unbound (trusted table)
    structured(&mut app, "settings.set", json!({"name": "shortcuts", "value": table}));
    let got = get_value(&mut app, "shortcuts");
    let got = got.as_array().unwrap();
    assert_eq!(got[0], json!("Q"), "parsed shortcut applied live");
    assert_eq!(got[1], json!(null), "explicit unbind honored");
    assert_eq!(got[3], json!(null), "unparseable name unbinds (trusted table)");
    assert_eq!(got[home_i], json!("Home"), "Home survives the round trip (bug fix)");
    assert_eq!(got[end_i], json!("End"), "End survives the round trip (bug fix)");
    // And a persisted file carrying Home/End reaches a fresh Config load.
    assert!(cfg_text().contains("\"Home\"") && cfg_text().contains("\"End\""), "persisted");
    let cfg = Config::load();
    let names: Vec<Option<&str>> = cfg.shortcuts.iter().map(|s| s.as_deref()).collect();
    assert!(names.iter().any(|n| *n == Some("Home")), "restart read keeps Home");
}

// ── R3 gold: the live apply path is OBSERVABLY real — the MCP server
// itself hot-starts from a settings.set ────────────────────────────────────

#[test]
fn v108_settings_mcp_enable_hotstarts() {
    let dir = std::env::temp_dir().join("slooper-mcp-v108hot");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(crate::paths::DATA_DIR_ENV, &dir);
    let mut app = headless_app("v108hot");
    let ctx = egui::Context::default();
    app.mcp_port = 0; // ephemeral, never a fixed port in tests (D6)
    assert!(!app.mcp.as_ref().unwrap().is_running());
    let ack = structured(&mut app, "settings.set", json!({"name": "mcp_enabled", "value": true}));
    assert_eq!(ack["applied"], "live");
    // The frame-loop sync is what starts the server — same as the Settings
    // checkbox path (v107), so the tool write reached the REAL seam.
    app.mcp_frame(&ctx);
    assert!(app.mcp.as_ref().unwrap().is_running(), "settings.set hot-started the server");
    // Port change hot-restarts (live, not restart-tier):
    let old = app.mcp.as_ref().unwrap().port().unwrap();
    structured(&mut app, "settings.set", json!({"name": "mcp_port", "value": 0}));
    app.mcp_frame(&ctx);
    let fresh = app.mcp.as_ref().unwrap().port().unwrap();
    assert!(fresh > 0 && fresh != old, "port change re-binds the listener");
    structured(&mut app, "settings.set", json!({"name": "mcp_enabled", "value": false}));
    app.mcp_frame(&ctx);
    assert!(!app.mcp.as_ref().unwrap().is_running(), "off applies live too");
    std::env::remove_var(crate::paths::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── R4: refusals — structured errors, NOTHING half-applied ────────────────

#[test]
fn v108_settings_set_rejects_types() {
    let mut app = headless_app("v108type");
    let before = cfg_text();
    for (name, value) in [
        ("snap_on", json!("yes")),         // string -> bool
        ("loop_mode", json!("area")),      // label is docs, not the value
        ("trk_vol", json!([300])),         // u8 range is serde's job
        ("metro_bpm", json!("fast")),      // not a number
        ("midi", json!({"action": 1})),    // object -> table
    ] {
        let e = is_error(&mut app, "settings.set", json!({"name": name, "value": value}));
        assert_eq!(e["error"], "invalid_params", "{name}={value}");
        let msg = call(&mut app, "settings.set", json!({"name": name, "value": value}))["result"]
            ["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(msg.contains(name), "message names the field: {msg}");
    }
    assert_eq!(get_value(&mut app, "snap_on"), json!(true), "unchanged");
    assert_eq!(cfg_text(), before, "file untouched on refusal");
}

#[test]
fn v108_settings_set_rejects_domain() {
    let mut app = headless_app("v108dom");
    let before = cfg_text();
    for (name, value) in [
        ("loop_mode", json!(7)),      // > 2
        ("roll_vel_step", json!(0)),  // < 1
        ("metro_bpm", json!(400)),    // > 300
        ("n_tracks", json!(99)),      // > 16
        ("theme", json!("neon")),     // not dark/light
        ("snap_sub", json!(5)),       // not in the SUBDIVISIONS table
        ("gui_scale", json!(9.0)),    // outside the product zoom clamp
    ] {
        let e = is_error(&mut app, "settings.set", json!({"name": name, "value": value}));
        assert_eq!(e["error"], "out_of_range", "{name}={value}");
    }
    assert_eq!(get_value(&mut app, "loop_mode"), json!(1), "loop_mode unchanged");
    assert_eq!(get_value(&mut app, "gui_scale"), json!(1.0), "zoom unchanged");
    assert_eq!(cfg_text(), before, "file untouched on refusal");
}

#[test]
fn v108_settings_unknown_field_lists_valid() {
    let mut app = headless_app("v108unk");
    for tool in ["settings.get", "settings.set"] {
        let args = json!({"name": "snapp_on", "value": false});
        let e = is_error(&mut app, tool, args.clone());
        assert_eq!(e["error"], "invalid_params");
        let msg = call(&mut app, tool, args)["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(msg.contains("unknown setting"), "{tool}: {msg}");
        assert!(msg.contains("snap_on"), "{tool}: refusal lists the valid names");
        assert!(msg.len() < 12_000, "bounded message");
    }
}

// ── R5 + R6: restart tier — surfaced, NOT half-applied, persists, clobber-
// resistant, and dropped when the user edits the field ────────────────────

#[test]
fn v108_settings_requires_restart() {
    let mut app = headless_app("v108rs");
    // Surfaced in the list schema:
    let e = settings_list(&mut app)
        .into_iter()
        .find(|e| e["name"] == "win_audio_backend")
        .unwrap();
    assert_eq!(e["requires_restart"], true);
    assert_eq!(e["value"], json!("auto"));
    assert_eq!(e["choices"], json!(["auto", "wasapi", "jack"]));
    // Set: persisted, LIVE state untouched (no half-apply):
    let ack = structured(
        &mut app,
        "settings.set",
        json!({"name": "win_audio_backend", "value": "wasapi"}),
    );
    assert_eq!(ack["applied"], "restart");
    assert_eq!(ack["requires_restart"], true);
    assert_eq!(ack["value"], json!("auto"), "running value unchanged");
    assert_eq!(ack["pending"], json!("wasapi"));
    assert!(app.status.contains("applies on next start"), "echo says so: {}", app.status);
    assert_eq!(get_value(&mut app, "win_audio_backend"), json!("auto"), "get too");
    assert!(cfg_text().contains("\"win_audio_backend\": \"wasapi\""), "persisted now");
}

#[test]
fn v108_settings_restart_persists_and_yields_to_user() {
    let mut app = headless_app("v108ovl");
    structured(
        &mut app,
        "settings.set",
        json!({"name": "win_audio_backend", "value": "wasapi"}),
    );
    // A later GUI-driven save (any live-tier set runs save_config) must
    // NOT resurrect the stale live value over the pending one:
    structured(&mut app, "settings.set", json!({"name": "snap_on", "value": false}));
    assert!(cfg_text().contains("\"win_audio_backend\": \"wasapi\""), "overlay survives saves");
    // Restart itself: the file is what App::new reads (Config::load IS the
    // startup read path), so the value arrives.
    assert_eq!(Config::load().win_audio_backend, "wasapi", "restart delivers it");
    // USER EDIT beats the overlay: change the live mirror the way the
    // backend picker does (test seam), then let any save run.
    app.test_edit_win_backend("jack");
    assert!(cfg_text().contains("\"win_audio_backend\": \"jack\""), "user owns the field");
    assert!(!cfg_text().contains("wasapi"), "stale pending fully dropped");
    structured(&mut app, "settings.set", json!({"name": "snap_on", "value": true}));
    assert!(cfg_text().contains("\"win_audio_backend\": \"jack\""), "entry stayed dropped");
}

// ── R7: metadata overlay + tier flags in the list schema ─────────────────

#[test]
fn v108_settings_list_schema_meta() {
    let mut app = headless_app("v108meta");
    let mut find = |n: &str| {
        settings_list(&mut app)
            .into_iter()
            .find(|e| e["name"] == n)
            .unwrap()
    };
    let m = find("theme");
    assert_eq!(m["choices"], json!(["dark", "light"]));
    let m = find("metro_bpm");
    assert_eq!(m["unit"], "BPM");
    assert_eq!(m["min"], 20);
    assert_eq!(m["max"], 300);
    let m = find("n_tracks");
    assert_eq!((m["min"].as_i64(), m["max"].as_i64()), (Some(1), Some(16)));
    let m = find("snap_sub");
    assert_eq!(m["choices"], json!([1, 2, 3, 4, 6, 8]), "derived from SUBDIVISIONS");
    let m = find("loop_mode");
    assert_eq!((m["min"].as_i64(), m["max"].as_i64()), (Some(0), Some(2)));
    // Tier flags: representative samples of both sides.
    for n in ["ports_in", "track_chain", "sampler_maps", "side_wins", "win_midi_inputs"] {
        assert_eq!(find(n)["requires_restart"], true, "{n} must be restart-tier");
    }
    for n in ["snap_on", "metro_bpm", "theme", "mcp_enabled", "trk_vol"] {
        assert_eq!(find(n)["requires_restart"], false, "{n} must be live-tier");
    }
}

// ── R9: read-only latch refuses the writer only ───────────────────────────

#[test]
fn v108_settings_read_only_latch() {
    let mut app = headless_app("v108latch");
    app.mcp_read_only = true;
    let r = call(&mut app, "settings.set", json!({"name": "snap_on", "value": false}));
    assert_eq!(r["error"]["code"], -32003, "latch refuses the writer");
    assert_eq!(get_value(&mut app, "snap_on"), json!(true), "nothing applied");
    // Reads live under the latch (goal: the agent may still observe).
    assert_eq!(get_value(&mut app, "snap_on"), json!(true));
    assert!(!settings_list(&mut app).is_empty());
}

// ── Classification COMPLETENESS: every live field actually has an apply
// arm (writing its current value back must succeed), and every restart
// field must answer "restart" — the total-match pin of the whole design. ──

#[test]
fn v108_settings_tier_coverage_all_fields() {
    let mut app = headless_app("v108cov");
    let entries = settings_list(&mut app);
    let mut live = 0usize;
    let mut restart = 0usize;
    for e in &entries {
        let name = e["name"].as_str().unwrap();
        let cur = e["value"].clone();
        let ack = structured(&mut app, "settings.set", json!({"name": name, "value": cur.clone()}));
        if e["requires_restart"] == true {
            restart += 1;
            assert_eq!(ack["applied"], "restart", "{name}");
        } else {
            live += 1;
            assert_eq!(ack["applied"], "live", "{name}");
            // rack_collapsed serializes from a HashSet — set-compare arrays
            // of strings so element order cannot flake.
            let same = match (&ack["value"], &cur) {
                (Value::Array(a), Value::Array(b))
                    if a.iter().all(|v| v.is_string()) && a.len() == b.len() =>
                {
                    let mut a: Vec<&Value> = a.iter().collect();
                    let mut b: Vec<&Value> = b.iter().collect();
                    a.sort_by_key(|v| v.to_string());
                    b.sort_by_key(|v| v.to_string());
                    a == b
                }
                (a, b) => a == b,
            };
            assert!(same, "{name}: live field echoes its value ({} vs {cur})", ack["value"]);
        }
    }
    assert!(live >= 55 && restart >= 20, "sanity: {live} live + {restart} restart");
}

/// The module docs promise this name: classification tables and the serde
/// schema agree in BOTH directions (typo in a table goes red even before
/// settings.list would surface it via entry()'s runtime refusal).
#[test]
fn v108_settings_classify_total() {
    let keys: Vec<String> = serde_json::to_value(Config::default())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let classified: Vec<&str> =
        mcp_settings::LIVE.iter().chain(mcp_settings::RESTART).copied().collect();
    for k in &keys {
        assert!(classified.contains(&k.as_str()), "unclassified Config field {k}");
    }
    for c in &classified {
        assert!(keys.iter().any(|k| k == c), "classification names no field: {c}");
    }
    assert_eq!(classified.len(), keys.len(), "no duplicates in the tables");
    assert_eq!(mcp_settings::classify("snap_on"), Some(mcp_settings::Apply::Live));
    assert_eq!(mcp_settings::classify("ports_in"), Some(mcp_settings::Apply::Restart));
    assert_eq!(mcp_settings::classify("nope"), None);
}

// ── R8 slow tier: REAL client over a REAL socket, restart persistence ─────

#[test]
#[ignore = "slow-tier: real socket"]
fn v108_mcp_settings_socket_roundtrip_e2e() {
    // The card's first AC end to end: a real MCP-over-HTTP client against
    // a live lazily-started server (port 0 + endpoint file, MCP-01's
    // harness shape), driving the settings surface; then the restart read.
    let dir = std::env::temp_dir().join("slooper-mcp-v108sock");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(crate::paths::DATA_DIR_ENV, &dir);
    let mut app = headless_app("v108sock");
    let ctx = egui::Context::default();
    app.mcp_port = 0;
    app.mcp_enabled = true;
    app.mcp_frame(&ctx);
    let (port, token) = crate::mcp::bridge::read_endpoint().expect("endpoint file");

    let mut call = |body: &str| -> (u16, Option<String>) {
        use std::io::{Read, Write};
        let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(8)))
            .unwrap();
        // Header built from SEPARATE format args (the auth line must never
        // sit as a literal interpolation pair — proven CI hazard).
        let auth = format!("Bearer {}", token);
        let head = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            auth,
            body.len()
        );
        sock.write_all(head.as_bytes()).unwrap();
        sock.write_all(body.as_bytes()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
        {
            let mut srv = app.mcp.take().unwrap();
            srv.drain(&mut app);
            app.mcp = Some(srv);
        }
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let status: u16 = String::from_utf8_lossy(&raw[..split])
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let body = &raw[split + 4..];
        (
            status,
            if body.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(body).into_owned())
            },
        )
    };
    let mut rpc_over = |id: i64, method: &str, params: Value| -> Value {
        let (st, body) = call(
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
        );
        assert_eq!(st, 200);
        serde_json::from_str(body.as_ref().unwrap()).unwrap()
    };

    // Real client sees the settings surface in tools/list.
    let v = rpc_over(1, "tools/list", json!({}));
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for n in ["settings.list", "settings.get", "settings.set"] {
        assert!(names.contains(&n), "{n} advertised over the socket");
    }

    // settings.list over the socket.
    let v = rpc_over(
        2,
        "tools/call",
        json!({"name": "settings.list", "arguments": {}}),
    );
    let sc = &v["result"]["structuredContent"];
    assert!(sc["count"].as_u64().unwrap() >= 70, "full surface on the wire");

    // Live write over the socket.
    let v = rpc_over(
        3,
        "tools/call",
        json!({"name": "settings.set", "arguments": {"name": "metro_bpm", "value": 97}}),
    );
    assert_eq!(v["result"]["structuredContent"]["value"], 97);

    // Restart-tier write over the socket: surfaced + persisted.
    let v = rpc_over(
        4,
        "tools/call",
        json!({"name": "settings.set",
               "arguments": {"name": "win_audio_backend", "value": "wasapi"}}),
    );
    assert_eq!(v["result"]["structuredContent"]["requires_restart"], true);

    // A refusal answers as a structured domain error, not a transport one.
    let v = rpc_over(
        5,
        "tools/call",
        json!({"name": "settings.set", "arguments": {"name": "metro_bpm", "value": 4000}}),
    );
    assert_eq!(v["result"]["isError"], true);
    assert_eq!(v["result"]["structuredContent"]["error"], "out_of_range");

    // THE restart: Config::load() is exactly what the next app start reads.
    let cfg = Config::load();
    assert_eq!(cfg.metro_bpm, 97, "live-tier write persisted");
    assert_eq!(cfg.win_audio_backend, "wasapi", "restart-tier write persisted");

    std::env::remove_var(crate::paths::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}
