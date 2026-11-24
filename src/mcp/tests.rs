//! v107 MCP quick-tier pins (docs/MCP-DESIGN.md D6): the REAL dispatcher
//! and the REAL pure gate, zero sockets. Slow tier (real socket / bridge
//! child) lives at the bottom, `#[ignore = "slow-tier: …"]` — CI runs it
//! via the windows-native full suite (`--include-ignored`).
//!
//! Contract: pins drive `rpc::dispatch` / `http::gate` / `McpServer` —
//! never the tool fns — so gate order, taxonomy, latch, firewall and the
//! observed-ack are exercised exactly as production reaches them.

use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use crate::mcp::http;
use crate::mcp::rpc;
use crate::mcp::testkit::{self, clear_data_dir, isolated_data_dir, parse_http};
use crate::mcp::tools::{ToolDef, ToolError};
use crate::ui::headless_app;
use crate::ui::App;

// ── probe tools (registered per-App: test isolation, never global) ──────

static PROBE_THREAD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn probe_read(_app: &mut App, args: &Value) -> Result<Value, ToolError> {
    Ok(json!({"got": args.get("text").cloned().unwrap_or(Value::Null)}))
}

fn probe_write(_app: &mut App, _args: &Value) -> Result<Value, ToolError> {
    Ok(json!({"wrote": true}))
}

fn probe_panic(_app: &mut App, _args: &Value) -> Result<Value, ToolError> {
    panic!("v107 probe firewall trip (expected by v107_mcp_rpc_panic_firewall)")
}

fn probe_domain_err(_app: &mut App, _args: &Value) -> Result<Value, ToolError> {
    Err(ToolError::not_found("no such pad"))
}

fn probe_thread(_app: &mut App, _args: &Value) -> Result<Value, ToolError> {
    let id = thread_serial();
    Ok(json!({"thread": id}))
}

/// Stable per-process serial for the calling OS thread (main-test thread
/// and any spawn get distinct values; the VALUE identity does not matter,
/// only equality with the caller's own serial — see the thread pin).
fn thread_serial() -> usize {
    // Cheap identity hash of the thread id debug string.
    let s = format!("{:?}", std::thread::current().id());
    let mut h: usize = 0x100000001b3;
    for b in s.bytes() {
        h = (h ^ b as usize).wrapping_mul(0x100000001b3);
    }
    h
}

fn register_probes(app: &mut App) {
    app.mcp_tools.push(ToolDef {
        name: "probe.echo",
        description: "test probe: read-only echo",
        schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        read: true,
        destructive: false,
        idempotent: true,
        execute: probe_read,
    });
    app.mcp_tools.push(ToolDef {
        name: "probe.write",
        description: "test probe: declared writer",
        schema: json!({"type": "object", "properties": {}}),
        read: false,
        destructive: false,
        idempotent: true,
        execute: probe_write,
    });
    app.mcp_tools.push(ToolDef {
        name: "probe.panic",
        description: "test probe: panics (firewall must catch)",
        schema: json!({"type": "object", "properties": {}}),
        read: false,
        destructive: false,
        idempotent: false,
        execute: probe_panic,
    });
    app.mcp_tools.push(ToolDef {
        name: "probe.domainerr",
        description: "test probe: structured domain failure",
        schema: json!({"type": "object", "properties": {}}),
        read: true,
        destructive: false,
        idempotent: true,
        execute: probe_domain_err,
    });
    app.mcp_tools.push(ToolDef {
        name: "probe.thread",
        description: "test probe: reports a serial for the executing thread",
        schema: json!({"type": "object", "properties": {}}),
        read: true,
        destructive: false,
        idempotent: true,
        execute: probe_thread,
    });
    let _ = PROBE_THREAD.load(Ordering::Relaxed);
}

fn call(app: &mut App, name: &str, args: Value) -> Option<Value> {
    rpc::dispatch(
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":args}}),
        app,
    )
}

// ── protocol surface ─────────────────────────────────────────────────────

#[test]
fn v107_mcp_rpc_initialize() {
    let mut app = headless_app("v107init");
    let r = rpc::dispatch(
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-06-18","capabilities":{},
                          "clientInfo":{"name":"pin","version":"0"}}}),
        &mut app,
    )
    .expect("initialize answers");
    assert_eq!(r["result"]["protocolVersion"], "2025-06-18", "supported version echoed");
    assert_eq!(r["result"]["serverInfo"]["name"], "slooper");
    assert_eq!(r["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    // Unsupported/absent → our latest handshake-era revision.
    let r2 = rpc::dispatch(
        &json!({"jsonrpc":"2.0","id":2,"method":"initialize",
                "params":{"protocolVersion":"1999-01-01"}}),
        &mut app,
    )
    .expect("initialize answers (unknown version)");
    assert_eq!(r2["result"]["protocolVersion"], rpc::LATEST, "unknown version falls to LATEST");
}

#[test]
fn v107_mcp_rpc_ping() {
    let mut app = headless_app("v107ping");
    let r = rpc::dispatch(&json!({"jsonrpc":"2.0","id":7,"method":"ping"}), &mut app).unwrap();
    assert_eq!(r["result"], json!({}), "ping answers an empty result");
}

#[test]
fn v107_mcp_rpc_discover() {
    let mut app = headless_app("v107disc");
    let r = rpc::dispatch(&json!({"jsonrpc":"2.0","id":3,"method":"server/discover"}), &mut app)
        .unwrap();
    assert!(
        r["result"]["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28")),
        "2026 stateless revision advertised"
    );
    assert_eq!(r["result"]["serverInfo"]["name"], "slooper");
}

#[test]
fn v107_mcp_rpc_notification_no_response() {
    let mut app = headless_app("v107notif");
    assert!(
        rpc::dispatch(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}), &mut app)
            .is_none(),
        "initialized notification -> None (HTTP maps 202)"
    );
    assert!(
        rpc::dispatch(&json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}), &mut app)
            .is_none(),
        "any notifications/* -> None"
    );
    let r = rpc::dispatch(&json!({"jsonrpc":"2.0","id":9,"method":"bogus/method"}), &mut app);
    assert_eq!(r.unwrap()["error"]["code"], -32601, "unknown METHOD with id -> -32601");
    assert!(
        rpc::dispatch(&json!({"jsonrpc":"2.0","method":"bogus/notif"}), &mut app).is_none(),
        "unknown notification stays silent (202)"
    );
    let r = rpc::dispatch(&json!({"jsonrpc":"2.0","id":10}), &mut app).unwrap();
    assert_eq!(r["error"]["code"], -32600, "no method string -> -32600 invalid request");
}

#[test]
fn v107_mcp_tools_list_shape() {
    let mut app = headless_app("v107list");
    let r = rpc::dispatch(&json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}), &mut app).unwrap();
    let tools = r["result"]["tools"].as_array().unwrap().clone();
    let echo = tools
        .iter()
        .find(|t| t["name"] == "core.echo")
        .expect("core.echo in the demo registry");
    assert!(echo["description"].as_str().unwrap().len() > 20);
    assert_eq!(echo["inputSchema"]["type"], "object");
    assert_eq!(echo["inputSchema"]["required"][0], "text");
    assert_eq!(echo["annotations"]["readOnlyHint"], true);
    assert_eq!(echo["annotations"]["openWorldHint"], false);
    assert!(echo["annotations"].get("destructiveHint").is_some());
    assert!(echo["annotations"].get("idempotentHint").is_some());
    // 206-era CacheableResult fields ride along (older clients ignore).
    assert!(r["result"].get("ttlMs").is_some());
    assert_eq!(r["result"]["cacheScope"], "private");
}

#[test]
fn v107_mcp_core_echo_observed_ack() {
    let mut app = headless_app("v107echo");
    let r = call(&mut app, "core.echo", json!({"text": "hi"})).unwrap();
    assert_eq!(r["result"]["structuredContent"]["echo"], "hi");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("hi"), "text block mirrors");
    assert!(r["result"].get("isError").is_none(), "success has no isError");
    // Domain failure: missing required arg -> isError, NOT a JSON-RPC error.
    let e = call(&mut app, "core.echo", json!({})).unwrap();
    assert_eq!(e["result"]["isError"], true);
    assert_eq!(e["result"]["structuredContent"]["error"], "invalid_params");
    assert!(e.get("error").is_none(), "domain failures are successful results");
}

#[test]
fn v107_mcp_tool_registry_probe_tool() {
    // The MCP-02..05 contract: a registration (schema + description +
    // flags + fn) shows up in tools/list and executes via tools/call —
    // zero core edits.
    let mut app = headless_app("v107reg");
    register_probes(&mut app);
    let r = rpc::dispatch(&json!({"jsonrpc":"2.0","id":5,"method":"tools/list"}), &mut app).unwrap();
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"probe.echo") && names.contains(&"probe.write"), "registrations listed: {names:?}");
    let r = call(&mut app, "probe.echo", json!({"text": "pong"})).unwrap();
    assert_eq!(r["result"]["structuredContent"]["got"], "pong");
}

#[test]
fn v107_mcp_tool_call_executes_on_ui_thread() {
    let mut app = headless_app("v107thr");
    register_probes(&mut app);
    let mine = thread_serial();
    let r = call(&mut app, "probe.thread", json!({})).unwrap();
    assert_eq!(
        r["result"]["structuredContent"]["thread"].as_u64().map(|v| v as usize),
        Some(mine),
        "dispatch (the frame-loop executor) ran the tool on the caller's thread"
    );
}

#[test]
fn v107_mcp_read_only_latch() {
    let mut app = headless_app("v107ro");
    register_probes(&mut app);
    app.mcp_read_only = true;
    let w = call(&mut app, "probe.write", json!({})).unwrap();
    assert_eq!(w["error"]["code"], -32003, "declared writer refused while latched");
    let rd = call(&mut app, "probe.echo", json!({"text": "x"})).unwrap();
    assert_eq!(rd["result"]["structuredContent"]["got"], "x", "reads survive the latch");
    // Hot unlatch: next call already allowed (no restart anywhere).
    app.mcp_read_only = false;
    let w2 = call(&mut app, "probe.write", json!({})).unwrap();
    assert_eq!(w2["result"]["structuredContent"]["wrote"], true);
}

#[test]
fn v107_mcp_rpc_panic_firewall() {
    let mut app = headless_app("v107fire");
    register_probes(&mut app);
    let r = call(&mut app, "probe.panic", json!({})).unwrap();
    assert_eq!(r["error"]["code"], -32000, "caught panic -> -32000");
    // Server alive: the very next call works.
    let ok = call(&mut app, "probe.echo", json!({"text": "still here"})).unwrap();
    assert_eq!(ok["result"]["structuredContent"]["got"], "still here");
}

#[test]
fn v107_mcp_rpc_domain_error() {
    let mut app = headless_app("v107dom");
    register_probes(&mut app);
    let r = call(&mut app, "probe.domainerr", json!({})).unwrap();
    assert!(r.get("error").is_none());
    assert_eq!(r["result"]["isError"], true);
    assert_eq!(r["result"]["structuredContent"]["error"], "not_found");
    assert_eq!(r["result"]["content"][0]["text"], "no such pad");
    assert!(rpc::ERROR_KINDS.contains(&"not_found"));
}

#[test]
fn v107_mcp_rpc_unknown_tool_and_params() {
    let mut app = headless_app("v107unk");
    let r = call(&mut app, "no.such.tool", json!({})).unwrap();
    assert_eq!(r["error"]["code"], -32601, "unknown tool -> -32601");
    let r = rpc::dispatch(
        &json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"arguments":{}}}),
        &mut app,
    )
    .unwrap();
    assert_eq!(r["error"]["code"], -32602, "params without name -> -32602");
    let r = rpc::dispatch(
        &json!({"jsonrpc":"2.0","id":12,"method":"tools/call",
                "params":{"name":"core.echo","arguments":"nope"}}),
        &mut app,
    )
    .unwrap();
    assert_eq!(r["error"]["code"], -32602, "non-object arguments -> -32602");
}

// ── gate + bridge queue taxonomy (pure halves, real bytes) ───────────────

#[test]
fn v107_mcp_http_gate_order() {
    let tok = "t0k3n";
    let good = |m: &str, p: &str, h: Option<&str>, o: Option<&str>, a: Option<&str>| {
        http::gate(m, p, h, o, a, tok)
    };
    // 405 first, whatever else is broken.
    assert_eq!(good("GET", "/mcp", None, None, None).unwrap_err().0, 405);
    assert_eq!(good("GET", "/mcp", Some("evil.com"), Some("bad"), None).unwrap_err().0, 405);
    // Host beats origin/token/path (fixed order, design §3).
    assert_eq!(good("POST", "/mcp", Some("evil.com:47618"), None, None).unwrap_err().0, 403);
    assert_eq!(good("POST", "/x", Some("evil.com"), None, None).unwrap_err().0, 403);
    // Origin after host; missing Origin passes (native clients).
    assert_eq!(
        good("POST", "/mcp", Some("127.0.0.1:47618"), Some("http://evil.com"), None)
            .unwrap_err()
            .0,
        403
    );
    assert!(good("POST", "/mcp", Some("127.0.0.1:47618"), Some("null"), None).is_err());
    // Token after origin.
    assert_eq!(
        good("POST", "/mcp", Some("localhost:47618"), Some("http://localhost:47618"), Some("Bearer wrong"))
            .unwrap_err()
            .0,
        401
    );
    // Path last.
    assert_eq!(
        good("POST", "/other", Some("127.0.0.1:1"), None, Some(&format!("Bearer {tok}")))
            .unwrap_err()
            .0,
        404
    );
    // All good.
    assert!(good("POST", "/mcp", Some("127.0.0.1:47618"), None, Some(&format!("Bearer {tok}"))).is_ok());
    assert!(good("POST", "/mcp", Some("[::1]:47618"), Some("http://[::1]:47618"), Some(&format!("Bearer {tok}"))).is_ok());
}

#[test]
fn v107_mcp_bridge_busy_queue_busy_error() {
    // The real bounded queue + the real submit mapping + the exact -32001
    // body bytes the worker writes on overflow (shared builders, D6).
    let (tx, rx) = std::sync::mpsc::sync_channel::<crate::mcp::Cmd>(16);
    let id = json!(42);
    let mk = || crate::mcp::Cmd {
        req: json!({"jsonrpc":"2.0","id":42,"method":"ping"}),
        tx: std::sync::mpsc::channel().0,
    };
    let mut full = false;
    for _ in 0..64 {
        match http::queue_submit(&tx, mk()) {
            http::Submit::Ok => {}
            http::Submit::Busy => {
                full = true;
                break;
            }
            http::Submit::Stopped => panic!("queue must not be closed here"),
        }
    }
    assert!(full, "sync_channel(16) must refuse a 17th undrained command");
    drop(rx);
    assert!(matches!(http::queue_submit(&tx, mk()), http::Submit::Stopped));
    let body = http::busy_body(&id);
    assert_eq!(body["error"]["code"], -32001);
    assert_eq!(body["id"], 42);
}

#[test]
fn v107_mcp_bridge_timeout_short_wait() {
    // The shared await helper: no drained command -> Timeout -> the real
    // -32002 body. (The socket-layer 5 s default is exercised by the CI
    // slow tier; injecting the short wait keeps this fast-tier.)
    let (_tx, rx) = std::sync::mpsc::channel::<Option<Value>>();
    let t0 = std::time::Instant::now();
    let a = http::await_result(&rx, std::time::Duration::from_millis(80));
    assert!(matches!(a, http::Awaited::Timeout));
    assert!(t0.elapsed() >= std::time::Duration::from_millis(60), "waited, not looped");
    let body = http::timeout_body(&json!(5));
    assert_eq!(body["error"]["code"], -32002);
    // A drained notification maps through as Done(None) (202).
    let (tx2, rx2) = std::sync::mpsc::channel();
    tx2.send(None).unwrap();
    assert!(matches!(http::await_result(&rx2, std::time::Duration::from_secs(1)), http::Awaited::Done(None)));
}

// ── lifecycle (lazy start / hot stop / endpoint file / port busy) ────────

#[test]
fn v107_mcp_lifecycle_disabled_no_listener() {
    let dir = isolated_data_dir("v107off");
    let mut app = headless_app("v107off"); // Config default: mcp_enabled=false
    let ctx = egui::Context::default();
    assert!(!app.mcp_enabled, "MCP is OFF by default");
    app.mcp_port = 0; // ephemeral so a start-attempt could NEVER fail by luck
    // Assert after EVERY frame: a server that opens on frame N and
    // self-stops on N+1 would pass final-state-only asserts (a sab run
    // proved this pin vacuous against exactly that slip).
    app.mcp_frame(&ctx);
    assert!(!app.mcp.as_ref().unwrap().is_running(), "disabled boots no server (frame 1)");
    assert!(
        !crate::mcp::bridge::endpoint_path().exists(),
        "disabled boots ZERO sockets — no endpoint file at {} (frame 1)",
        crate::mcp::bridge::endpoint_path().display()
    );
    app.mcp_frame(&ctx);
    assert!(!app.mcp.as_ref().unwrap().is_running(), "disabled boots no server (frame 2)");
    assert!(
        !crate::mcp::bridge::endpoint_path().exists(),
        "disabled boots ZERO sockets — still no endpoint file (frame 2)"
    );
    assert!(app.mcp.as_ref().unwrap().status_line().contains("off"));
    clear_data_dir();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v107_mcp_lifecycle_hot_start_stop() {
    let dir = isolated_data_dir("v107hot");
    let mut app = headless_app("v107hot");
    let ctx = egui::Context::default();
    app.mcp_port = 0; // ephemeral — never a fixed port in tests (D6)
    app.mcp_enabled = true;
    app.mcp_frame(&ctx); // frame N: lazy start
    let srv = app.mcp.as_ref().unwrap();
    assert!(srv.is_running(), "enable starts the server with no restart");
    let port = srv.port().unwrap();
    assert!(port > 0, "port 0 resolved to an ephemeral port");
    // Endpoint file: the discovery contract (atomic, parseable, loopback).
    let body = std::fs::read_to_string(crate::mcp::bridge::endpoint_path())
        .expect("endpoint file after start");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["port"].as_u64().unwrap() as u16, port);
    assert_eq!(v["token"].as_str().unwrap().len(), 32, "token = 32 hex chars");
    assert_eq!(v["pid"].as_u64().unwrap() as u32, std::process::id());
    // Status echo on the transition (every action has feedback).
    assert!(app.status.contains("MCP server on"), "start echoes: {}", app.status);
    // Hot stop:
    app.mcp_enabled = false;
    app.mcp_frame(&ctx);
    assert!(!app.mcp.as_ref().unwrap().is_running(), "disable stops the server hot");
    assert!(!crate::mcp::bridge::endpoint_path().exists(), "stop removes our endpoint file");
    assert!(app.status.contains("stopped"), "stop echoes: {}", app.status);
    clear_data_dir();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v107_mcp_lifecycle_port_busy_keeps_boot() {
    let dir = isolated_data_dir("v107busy");
    let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let busy_port = blocker.local_addr().unwrap().port();
    let mut app = headless_app("v107busy");
    let ctx = egui::Context::default();
    app.mcp_port = busy_port;
    app.mcp_enabled = true;
    app.mcp_frame(&ctx);
    // The app keeps booting (we are mid-test = it booted), server down,
    // config untouched, visible echo with the port number in it.
    let srv = app.mcp.as_ref().unwrap();
    assert!(!srv.is_running(), "bind failure does NOT fake a running server");
    assert!(app.status.contains(&busy_port.to_string()), "echo names the port: {}", app.status);
    assert!(app.status.contains("could not start"), "echo explains: {}", app.status);
    assert!(app.mcp_enabled, "config survives the failure (user fixes, not loses, the setting)");
    // Backoff: another frame does not spam retries nor echo again.
    app.mcp_frame(&ctx);
    assert!(app.mcp.as_ref().unwrap().status_line().contains("stopped"), "status line keeps the why");
    // Release the port; a config change bypasses backoff immediately.
    drop(blocker);
    app.mcp_port = busy_port + 1;
    app.mcp_frame(&ctx);
    assert!(app.mcp.as_ref().unwrap().is_running(), "changing the port retries at once");
    assert!(app.status.contains("MCP server on"));
    clear_data_dir();
    let _ = std::fs::remove_dir_all(&dir);
}

// ── v156 (card t_13be8ef0): port-0 steady state — the rebind-storm class ──
//
// The shipped bug: `sync()` compared the CONFIGURED port against the
// listener's REAL port, so a configured 0 never matched its own ephemeral
// bind — every frame stopped and restarted the server on a new port with
// a re-rolled token, and no client could ever finish a request (field
// evidence: ~10 distinct ports in the endpoint file within 1.5 s). The
// D6 pins missed the class because every lifecycle pin called sync() at
// most once per asserted state — never twice in steady state.

#[test]
fn v156_mcp_port0_stable_across_frames() {
    // The storm guard: frames N..N+4 with the advertised "0 picks any
    // free port" flow must keep ONE bind — same port, same token,
    // endpoint file untouched — and that bind must be a live, serving
    // socket (the reported symptom was connection-refused on EVERY call).
    let dir = isolated_data_dir("v156p0");
    let mut app = headless_app("v156p0");
    let ctx = egui::Context::default();
    app.mcp_port = 0;
    app.mcp_enabled = true;
    app.mcp_frame(&ctx);
    let (port, token) = crate::mcp::bridge::read_endpoint().expect("endpoint after start");
    assert!(port > 0, "port 0 resolved to an ephemeral port");
    for frame in 2..=5 {
        app.mcp_frame(&ctx);
        let srv = app.mcp.as_ref().unwrap();
        assert!(srv.is_running(), "frame {frame}: server still running");
        assert_eq!(srv.port(), Some(port), "frame {frame}: port 0 keeps the current bind");
        let (ep_port, ep_token) =
            crate::mcp::bridge::read_endpoint().expect("endpoint file survives");
        assert_eq!(
            (ep_port, ep_token.as_str()),
            (port, token.as_str()),
            "frame {frame}: endpoint file must not churn (a rebind re-rolls port AND token)"
        );
        assert!(
            app.status.contains(&format!("127.0.0.1:{port} \u{2014}")),
            "frame {frame}: status names the LIVE port (a rebind echoes a different one): {}",
            app.status
        );
    }
    // Live round-trip on the surviving bind: full handshake + discovery
    // over the socket the endpoint file advertised (kit pump drains on
    // this thread exactly like the frame loop).
    let v = testkit::socket_json(
        &mut app,
        port,
        &token,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"pin","version":"1"}}}).to_string(),
    );
    assert_eq!(v["result"]["serverInfo"]["name"], "slooper");
    let v = testkit::socket_json(
        &mut app,
        port,
        &token,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}).to_string(),
    );
    assert!(
        v["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "core.echo"),
        "the surviving ephemeral bind serves tools/list"
    );
    assert_eq!(app.mcp.as_ref().unwrap().port(), Some(port), "round-trips did not churn the bind");
    // Ephemeral binds hot-stop cleanly.
    app.mcp_enabled = false;
    app.mcp_frame(&ctx);
    assert!(!app.mcp.as_ref().unwrap().is_running(), "disable hot-stops an ephemeral bind");
    assert!(!crate::mcp::bridge::endpoint_path().exists(), "stop removes our endpoint file");
    clear_data_dir();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v156_mcp_port_change_applies_live_without_storm() {
    // Config changes keep applying live, but ANY steady state must sit on
    // one bind — including port 0 reached FROM a fixed value (the user
    // typing 0 into the Settings box, not just 0-at-boot).
    let dir = isolated_data_dir("v156chg");
    let mut app = headless_app("v156chg");
    let ctx = egui::Context::default();
    // Fixed start on a grab-and-released ephemeral (pins never fight fixed ports).
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let f1 = probe.local_addr().unwrap().port();
    drop(probe);
    app.mcp_port = f1;
    app.mcp_enabled = true;
    app.mcp_frame(&ctx);
    assert_eq!(app.mcp.as_ref().unwrap().port(), Some(f1), "fixed port binds exactly");
    app.mcp_frame(&ctx);
    assert_eq!(app.mcp.as_ref().unwrap().port(), Some(f1), "fixed port is stable across frames");
    // The user types 0: the listener lands on a working bind and STAYS.
    app.mcp_port = 0;
    app.mcp_frame(&ctx);
    let p0 = app.mcp.as_ref().unwrap().port().expect("port 0 binds after a fixed value");
    assert!(p0 > 0);
    for frame in 2..=4 {
        app.mcp_frame(&ctx);
        assert_eq!(app.mcp.as_ref().unwrap().port(), Some(p0), "frame {frame}: 0-after-fixed must not storm");
        let (ep, _) = crate::mcp::bridge::read_endpoint().expect("endpoint tracks the live port");
        assert_eq!(ep, p0, "frame {frame}: endpoint file names the live port");
    }
    // Back to a fixed value: applies live, exactly, and is stable. (f2 is
    // kernel-guaranteed != p0: p0 is still bound while the probe binds.)
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let f2 = probe.local_addr().unwrap().port();
    drop(probe);
    app.mcp_port = f2;
    app.mcp_frame(&ctx);
    assert_eq!(app.mcp.as_ref().unwrap().port(), Some(f2), "0 -> fixed rebinds to the exact port");
    app.mcp_frame(&ctx);
    assert_eq!(app.mcp.as_ref().unwrap().port(), Some(f2), "fixed-after-0 is stable");
    clear_data_dir();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v156_mcp_sync_compares_configured_port() {
    // Drift guard for the exact comparison that stormed: the running-vs-
    // wanted check must compare the CONFIGURED port that produced the
    // bind (Running::cfg_port), never the listener's real port — a real
    // port can never equal 0, so the old form reborn would re-open the
    // storm. Needles split via concat! (source-scan doctrine; the scan
    // target is mod.rs, the pin lives in tests.rs — no self-match).
    let body = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp/mod.rs"),
    )
    .expect("src/mcp/mod.rs readable");
    let want = concat!("r.cfg_", "port != port");
    assert!(body.contains(want), "sync() must compare the CONFIGURED port: {want:?} missing");
    let storm = concat!("r.", "port != port");
    assert!(
        !body.contains(storm),
        "src/mcp/mod.rs must not compare the listener's REAL port against the config — \
         port 0 never equals its own ephemeral bind (t_13be8ef0 rebind storm)"
    );
    assert!(body.contains(concat!("cfg_", "port: port")), "the bind must record the configured port it was asked for");
}

// ── tool table invariants (source-scan family, design §6 pin) ────────────

#[test]
fn v107_mcp_tool_table_invariants() {
    // Contract over the COMPILED global table (probe tools live per-App,
    // never here, so the invariants are un-jammable by tests):
    // dotted lowercase names, real descriptions, real schemas, and every
    // tool declaring all four annotations (render() proves them).
    let tools = crate::mcp::tools::table();
    assert!(!tools.is_empty());
    for t in &tools {
        let (dom, rest) = t
            .name
            .split_once('.')
            .unwrap_or_else(|| panic!("tool {} needs a dotted domain.verb name", t.name));
        assert!(
            !dom.is_empty()
                && dom.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c == '.'),
            "name {} violates ^[a-z_]+\\.[a-z_.]+$",
            t.name
        );
        assert!(t.description.len() >= 20, "{}: thin description", t.name);
        assert!(t.schema.is_object() && t.schema.get("properties").is_some(), "{}: schema must be an object with properties", t.name);
        let a = t.render()["annotations"].clone();
        for k in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
            assert!(a.get(k).is_some_and(|v| v.is_boolean()), "{}: annotation {k}", t.name);
        }
        assert_eq!(a["openWorldHint"], false, "{}: never touches the outside world", t.name);
    }
}

#[test]
fn v107_mcp_config_defaults_roundtrip() {
    use crate::config::Config;
    let d = Config::default();
    assert!(!d.mcp_enabled, "OFF by default (D2)");
    assert_eq!(d.mcp_port, 47618);
    assert!(!d.mcp_read_only);
    // An old config file (no mcp_* keys at all) loads with the defaults.
    let old: Config = serde_json::from_str("{}").unwrap();
    assert!(!old.mcp_enabled && old.mcp_port == 47618 && !old.mcp_read_only);
    // Round-trip keeps a non-default combo.
    let c = crate::config::Config {
        mcp_enabled: true,
        mcp_port: 0,
        mcp_read_only: true,
        ..Default::default()
    };
    let back: Config = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert!(back.mcp_enabled && back.mcp_port == 0 && back.mcp_read_only);
}

// ── slow tier: real socket + real bridge child (CI --include-ignored) ────

/// One MCP-over-HTTP POST with arbitrary header lines (no Content-Length —
/// the helper appends it). Gate-refusal probes use this.
fn http_post_raw(sock_head_extra: &str, port: u16, body: &str) -> (u16, Option<String>) {
    use std::io::{Read, Write};
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
    let head = format!(
        "{sock_head_extra}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).unwrap();
    sock.write_all(body.as_bytes()).unwrap();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).unwrap();
    parse_http(&raw)
}

fn http_get(port: u16) -> (u16, Option<String>) {
    use std::io::{Read, Write};
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
    let req = format!("GET /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).unwrap();
    parse_http(&raw)
}

#[test]
#[ignore = "slow-tier: real socket"]
fn v107_mcp_socket_roundtrip_e2e() {
    // The card's headline AC: a REAL client over a REAL socket against a
    // live lazily-started server — port 0 + discovery file, full protocol
    // shape + gate refusals on the wire (design §7 slow tier).
    //
    // Pumping model: serve_conn parses, queues, and THEN parks on the
    // oneshot — so after writing a request we sleep, execute one real
    // `McpServer::drain` on this thread (= the frame-loop role), and only
    // then read the response. Deterministic, single-threaded, bounded.
    let dir = isolated_data_dir("v107sock");
    let mut app = headless_app("v107sock");
    let ctx = egui::Context::default();
    // Shrink the worker's oneshot wait (600 ms) so the frozen-frame-loop
    // -32002 path is provable on the wire without a 5 s wall-clock burn.
    // Every other call is drained well inside 600 ms by the kit pump below.
    let (port, token) = testkit::start_mcp_socket(
        &mut app, &ctx, Some(std::time::Duration::from_millis(600)));

    let mut call = |body: &str| -> (u16, Option<String>) {
        testkit::socket_request(&mut app, port, &token, body)
    };

    // initialize (handshake era)
    let (st, body) = call(
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"e2e","version":"1"}}}).to_string(),
    );
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(body.as_ref().unwrap()).unwrap();
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(v["result"]["serverInfo"]["name"], "slooper");

    // notification -> 202, empty body
    let (st, body) = call(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string());
    assert_eq!(st, 202, "notification -> 202");
    assert!(body.is_none(), "202 carries no body");

    // tools/list contains core.echo
    let (st, body) = call(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}).to_string());
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(body.as_ref().unwrap()).unwrap();
    assert!(
        v["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "core.echo"),
        "tools/list over the socket lists core.echo"
    );

    // tools/call core.echo -> structuredContent.echo
    let (st, body) = call(
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"core.echo","arguments":{"text":"hello slooper"}}})
            .to_string(),
    );
    assert_eq!(st, 200);
    let v: Value = serde_json::from_str(body.as_ref().unwrap()).unwrap();
    assert_eq!(v["result"]["structuredContent"]["echo"], "hello slooper");

    // parse error -> 400 with a -32700 JSON body (gate passed, parse failed)
    let (st, body) = call("{not json");
    assert_eq!(st, 400, "malformed JSON-RPC -> 400");
    let v: Value = serde_json::from_str(body.as_ref().unwrap()).unwrap();
    assert_eq!(v["error"]["code"], -32700);

    // ---- gate refusals at the socket layer ----
    let (st, _) = http_get(port);
    assert_eq!(st, 405, "GET -> 405 (no SSE offered)");
    let (st, _) = http_post_raw("", port, "{}"); // no usable request line
    assert_eq!(st, 400, "unparseable request line -> 400");
    let (st, _) = http_post_raw(
        &format!("POST /mcp HTTP/1.1\r\nHost: evil.com:{port}"),
        port,
        "{}",
    );
    assert_eq!(st, 403, "Host allow-list enforced on the wire");
    let (st, _) = http_post_raw(
        &format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}"),
        port,
        "{}",
    );
    assert_eq!(st, 401, "missing bearer token -> 401 on the wire");
    let (st, _) = http_post_raw(
        &format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://evil.com\r\nAuthorization: Bearer {token}"),
        port,
        "{}",
    );
    assert_eq!(st, 403, "foreign Origin -> 403 on the wire");

    // UI-thread timeout on the wire: push a command and deliberately do
    // NOT drain it; the worker's (test-shrunk) wait expires -> the client
    // gets the structured -32002 (the frozen-frame-loop path; AC 5 e2e).
    {
        use std::io::{Read, Write};
        // The request header is the kit's well-formed POST builder; the
        // deliberate NO-DRAIN between write and read is the probe itself
        // (and why this block stays hand-rolled).
        let body = "{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"ping\"}";
        let req = testkit::post_request(port, &token, body);
        let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
        sock.write_all(req.as_bytes()).unwrap();
        // Longer than the 600 ms worker wait, far shorter than the read timeout.
        std::thread::sleep(std::time::Duration::from_millis(1400));
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let (st, tbody) = parse_http(&raw);
        assert_eq!(st, 200, "a UI timeout is a protocol response, never a hang");
        let v: Value = serde_json::from_str(tbody.as_ref().unwrap()).unwrap();
        assert_eq!(v["error"]["code"], -32002, "undrained command -> -32002");
        assert_eq!(v["id"], 99);
    }

    // server still healthy afterwards; hot stop cleans up
    app.mcp_enabled = false;
    app.mcp_frame(&ctx);
    assert!(!app.mcp.as_ref().unwrap().is_running());
    assert!(!crate::mcp::bridge::endpoint_path().exists());
    clear_data_dir();
    let _ = std::fs::remove_dir_all(&dir);
}

/// v107: the app binary for the bridge-child test. PRIORITY: the
/// CI-staged `SLOOPER_PROBE_EXE` (Windows suite steps) then
/// `CARGO_BIN_EXE_slooper` (Linux slow tier, exported in ci.yml — cargo
/// injects it for INTEGRATION tests only, `--lib` never does); last
/// resort is the sibling-bin derive (`target/<profile>/deps/slooper-
/// <hash>` -> one up). Never the harness; the caller's wait is bounded.
fn app_binary() -> std::path::PathBuf {
    // W12e rule, learned the hard way on the Windows full-suite runner:
    // the pins that spawn the APP must ride the CI-STAGED exe —
    // SLOOPER_PROBE_EXE (both Windows suite steps export it; Linux slow
    // tier exports CARGO_BIN_EXE_slooper, ci.yml). The derive-below
    // fallback holds on Linux dev loops but NOT on the runner: there the
    // libtest harness can sit somewhere the sibling exe never was, and
    // execing a build-dir sibling that IS the harness would recurse.
    for var in ["SLOOPER_PROBE_EXE", "CARGO_BIN_EXE_slooper"] {
        if let Some(p) = std::env::var_os(var).filter(|p| !p.is_empty()) {
            return p.into();
        }
    }
    let exe = std::env::current_exe().expect("test exe path");
    let dir = exe.parent().and_then(|p| p.parent()).expect("deps parent");
    let name = if cfg!(windows) { "slooper.exe" } else { "slooper" };
    let p = dir.join(name);
    assert!(
        p.exists(),
        "no staged app exe (SLOOPER_PROBE_EXE / CARGO_BIN_EXE_slooper) \
         and no sibling bin at {} — export one before running this pin",
        p.display()
    );
    p
}

/// v107: resolver priority pin — the STAGED env exe always wins (both
/// names checked, empty values rejected), so the bridge-child pin can
/// never exec a libtest harness even when a sibling bin exists.
#[test]
fn v107_mcp_bridge_app_binary_prefers_staged_exe() {
    let real_probe = std::env::var_os("SLOOPER_PROBE_EXE");
    let real_bin = std::env::var_os("CARGO_BIN_EXE_slooper");
    std::env::set_var("SLOOPER_PROBE_EXE", "/probe/staged.exe");
    std::env::set_var("CARGO_BIN_EXE_slooper", "/linux/staged");
    assert_eq!(app_binary(), std::path::PathBuf::from("/probe/staged.exe"));
    std::env::set_var("SLOOPER_PROBE_EXE", "");
    assert_eq!(app_binary(), std::path::PathBuf::from("/linux/staged"));
    // Restore the real environment before the derive path is ever hit.
    match real_probe {
        Some(v) => std::env::set_var("SLOOPER_PROBE_EXE", v),
        None => std::env::remove_var("SLOOPER_PROBE_EXE"),
    }
    match real_bin {
        Some(v) => std::env::set_var("CARGO_BIN_EXE_slooper", v),
        None => std::env::remove_var("CARGO_BIN_EXE_slooper"),
    }
}

#[test]
#[ignore = "slow-tier: real socket"]
fn v107_mcp_bridge_stdio_child_forwards() {
    // Real `slooper --mcp-bridge` child (CARGO_BIN_EXE_slooper, NEVER the
    // build.rs-baked path — W12e rule): stdin line -> POST -> stdout line,
    // endpoint discovery via SLOOPER_DATA_DIR/mcp-endpoint.json. The test
    // thread plays the frame loop draining the queue.
    let dir = isolated_data_dir("v107br");
    let ctx = egui::Context::default();
    // Start the REAL server through the App — the one the pump loop below
    // drains via `app.mcp`. (A standalone McpServer here would own a queue
    // nobody drains: the shim would only ever collect the worker's -32002.)
    let mut app = headless_app("v107br");
    app.mcp_port = 0;
    app.mcp_enabled = true;
    app.mcp_frame(&ctx);
    assert!(app.mcp.as_ref().unwrap().is_running(), "server up");
    assert!(crate::mcp::bridge::endpoint_path().exists(), "child finds this");

    let mut child = std::process::Command::new(app_binary())
        .arg("--mcp-bridge")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("bridge child");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"ping"})
        )
        .unwrap();
        stdin.flush().unwrap();
    }
    // Pump the queue from this thread (= frame loop role), bounded.
    let t0 = std::time::Instant::now();
    loop {
        {
            let mut a = app.mcp.take().unwrap();
            a.drain(&mut app);
            app.mcp = Some(a);
        }
        if child.try_wait().ok().flatten().is_some() {
            break; // never — child waits for EOF
        }
        if t0.elapsed() > std::time::Duration::from_secs(4) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    // Close stdin -> child exits on EOF (bounded reap below).
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("child exits on stdin EOF");
    assert!(out.status.success(), "bridge exit status {:?}", out.status);
    let line = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
    let v: Value = serde_json::from_str(&line).expect("one JSON-RPC response line on stdout");
    assert_eq!(v["result"], json!({}), "ping round-tripped through the shim");
    assert_eq!(v["id"], 1);
    clear_data_dir();
    let _ = std::fs::remove_dir_all(&dir);
}

// ── docs drift guard (MCP-06 / t_4910331d) ──────────────────────────────

/// docs/MCP.md is the user-facing tool reference; this pin is the drift
/// guard that keeps it true: EVERY tool the live registry ships must have
/// its name in the doc, and the doc's tool-count line must equal the
/// registry. (The audit that opened this card had already miscounted a
/// hand-typed surface at 24 while main shipped 45 — prose drifts, a pin
/// does not. Regenerate the reference with `cargo run --example
/// mcp_tool_dump`.)
#[test]
fn mcp_docs_cover_live_tool_table() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs").join("MCP.md"),
    )
    .expect("docs/MCP.md exists (user-facing tool reference)");
    let table = super::tools::table();
    assert!(
        !table.is_empty(),
        "registry must be populated before asserting coverage"
    );
    assert!(
        doc.contains(&format!("**{} tools**", table.len())),
        "docs/MCP.md tool-count line is stale: registry ships {} tools \
         (run `cargo run --example mcp_tool_dump` and re-fold the doc)",
        table.len()
    );
    for t in &table {
        assert!(
            doc.contains(&format!("`{}`", t.name)),
            "docs/MCP.md is missing the live tool `{}`",
            t.name
        );
    }
}

// ── v154: testkit single-source tripwires (board t_e65b9296) ────────────
//
// These pins guard the REPO, not the app: MCP tool-family suites must keep
// driving the harness through the shared kit instead of re-cloning it. The
// needles are concat!-split so this pin file can never match itself
// (v150 lesson: a source pin whose needle lives in its own file passes
// vacuously).

/// Repo-relative src paths of every .rs file containing `needle`.
fn v154_src_files_containing(needle: &str) -> Vec<String> {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).expect("src tree readable") {
            let p = e.expect("dir entry").path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                let body = std::fs::read_to_string(&p).expect("rust source readable");
                if body.contains(needle) {
                    hits.push(
                        p.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
    }
    hits.sort();
    hits
}

#[test]
fn v154_mcp_testkit_helpers_single_sourced() {
    // The tools/call envelope + dispatch helper: ONLY the kit may own it.
    assert_eq!(
        v154_src_files_containing(concat!("expect(\"tools/call ", "answers\")")),
        vec!["src/mcp/testkit.rs".to_string()],
    );
    // The endpoint-read handshake: ONLY the kit may own it.
    assert_eq!(
        v154_src_files_containing(concat!("read_endpoint()", ".expect(")),
        vec!["src/mcp/testkit.rs".to_string()],
    );
    // A well-formed POST header may not be reassembled in any TEST file;
    // bridge.rs/http.rs build it for production traffic (the shim client
    // writes it, the server answers it) — testkit is the only TEST-side
    // builder.
    let allow = ["src/mcp/bridge.rs", "src/mcp/http.rs", "src/mcp/testkit.rs"];
    for f in v154_src_files_containing(concat!("Content-", "Type: application/json")) {
        assert!(
            allow.contains(&f.as_str()),
            "v154 regression: the MCP POST header was duplicated into {f}"
        );
    }
}

#[test]
fn v154_mcp_testkit_families_import_kit() {
    for f in [
        "src/mcp_settings/tests.rs",
        "src/ui/mcp_mixer/tests.rs",
        "src/mcp_plugins/tests.rs",
        "src/ui/mcp_windows.rs",
    ] {
        let body = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f),
        )
        .expect("family test file readable");
        assert!(
            body.contains("mcp::testkit"),
            "{f} must drive the harness through the shared testkit"
        );
    }
}

#[test]
fn v154_mcp_testkit_call_envelope_contract() {
    let mut app = headless_app("v154kit");
    let r = testkit::call(&mut app, "core.echo", json!({"text": "hi"}));
    assert_eq!(r["jsonrpc"], "2.0");
    assert_eq!(r["id"], 1);
    assert_eq!(r["result"]["structuredContent"]["echo"], "hi");
    assert_eq!(
        testkit::structured(&mut app, "core.echo", json!({"text": "hi"}))["echo"],
        "hi"
    );
    let e = testkit::is_error(
        &mut app,
        "settings.set",
        json!({"name": "metro_bpm", "value": 4000}),
    );
    assert_eq!(e["error"], "out_of_range");
    assert!(
        !e["message"].as_str().unwrap_or_default().is_empty(),
        "the human failure text must ride in 'message': {e}"
    );
}
