//! v154 — shared test harness for the MCP tool-family suites
//! (board t_e65b9296). Before this module, FOUR families (settings, mixer,
//! plugins, windows) each hand-rolled the same `tools/call` envelope +
//! dispatch helper, the same structuredContent/isError readers, the same
//! endpoint-read handshake, and near-identical socket pump closures —
//! 10 identical 8-line windows per the audit, and a response-envelope
//! shape change required parallel edits in every family.
//!
//! Doctrine this kit encodes (do not fork it back into the families):
//! - Pins drive the REAL dispatcher (`rpc::dispatch`) — the v107 contract.
//! - `structured` is the STRICT reader: a -32xxx transport error AND a
//!   structured `isError` both panic, so a refusal never masquerades as
//!   a success in any family.
//! - `is_error` is the failure-path reader: asserts `isError` and returns
//!   structuredContent with the human text grafted in under `"message"`
//!   (the v110/mixer hardening — every family now greps refusals through
//!   one field).
//! - Well-formed POST request lines are composed at RUNTIME from separate
//!   format args (the v128 idiom). History: a write layer of an earlier
//!   session corrupted any `"Prefix: {placeholder}"` adjacency into a
//!   `***` mangled literal — the v107/v108/v111 lore comments are the
//!   fossil record. Never re-introduce that adjacency into source text.
//! - EXCEPTIONS by design (do not "fix" them into this kit):
//!   * `src/mcp/tests.rs` keeps a local `call -> Option<Value>`: those
//!     pins test the dispatcher's own None path.
//!   * socket pins write their OWN request bodies (malformed JSON, raw
//!     gate-refusal request lines, missing headers) — controlling the
//!     wire bytes IS the test; they reuse `parse_http`/`post_request`/
//!     `socket_request*` but not the envelope builders.
//!   * `src/mcp/bridge.rs` (production `--mcp-bridge` shim) builds the
//!     same POST header for real traffic — production code, not test
//!     duplication.

use crate::mcp::rpc;
use crate::ui::App;
use serde_json::{json, Value};

/// Isolated `SLOOPER_DATA_DIR` so the endpoint file lands in a sandbox
/// (and `mcp_enabled=false` pins can assert its ABSENCE). Sets the env
/// var; pair with [`clear_data_dir`] at the end of a pin.
pub(crate) fn isolated_data_dir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("slooper-mcp-{tag}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    std::env::set_var(crate::paths::DATA_DIR_ENV, &p);
    p
}

pub(crate) fn clear_data_dir() {
    std::env::remove_var(crate::paths::DATA_DIR_ENV);
}

/// Contents of the config.json the headless sandbox writes.
pub(crate) fn cfg_text() -> String {
    std::fs::read_to_string(crate::config::Config::path())
        .expect("config.json in the headless sandbox")
}

/// `tools/call` envelope -> the REAL dispatcher (v107 contract: pins
/// drive the dispatcher the frame loop drains, never the tool fns).
pub(crate) fn call(app: &mut App, name: &str, args: Value) -> Value {
    rpc::dispatch(
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":args}}),
        app,
    )
    .expect("tools/call answers")
}

/// structuredContent of a SUCCESSFUL tools/call. Panics loudly on a
/// -32xxx transport error AND on a structured `isError`, so a latch or
/// refusal never masquerades as a result in any family's pins.
pub(crate) fn structured(app: &mut App, name: &str, args: Value) -> Value {
    let r = call(app, name, args);
    assert!(r.get("error").is_none(), "unexpected JSON-RPC error: {r}");
    assert_ne!(
        r["result"]["isError"].as_bool(),
        Some(true),
        "pin expects an OK result from {name}: {r}"
    );
    r["result"]["structuredContent"].clone()
}

/// structuredContent of a STRUCTURED DOMAIN FAILURE (isError=true).
/// The human text lives in content[0].text (rpc::tool_err) — grafted in
/// under `"message"` so pins grep refusals through one field.
pub(crate) fn is_error(app: &mut App, name: &str, args: Value) -> Value {
    let r = call(app, name, args);
    assert_eq!(
        r["result"]["isError"].as_bool(),
        Some(true),
        "expected a structured domain failure, got {r}"
    );
    let mut sc = r["result"]["structuredContent"].clone();
    // The human text lives in content[0].text (rpc::tool_err) — surface
    // it under "message" so pins can grep refusals for self-documentation.
    if let Some(t) = r["result"]["content"][0]["text"].as_str() {
        sc["message"] = json!(t);
    }
    sc
}

// ── socket-tier harness (slow-tier pins; v107 pump model) ────────────────

/// A well-formed MCP-over-HTTP POST (port 0 discovery + bearer token).
/// Header lines are composed at RUNTIME (module doc: the corruption
/// history); this is the ONLY place the well-formed header is built.
pub(crate) fn post_request(port: u16, token: &str, body: &str) -> String {
    let hdr_host = format!("Host: 127.0.0.1:{}", port);
    let hdr_auth = format!("{}: {} {}", "Authorization", "Bearer", token);
    let hdr_len = format!("Content-Length: {}", body.len());
    [
        "POST /mcp HTTP/1.1",
        hdr_host.as_str(),
        hdr_auth.as_str(),
        "Content-Type: application/json",
        hdr_len.as_str(),
        "Connection: close",
        "",
        body,
    ]
    .join("\r\n")
}

/// Status + body of an HTTP/1.x response; empty body -> None. Lenient on
/// the status token (refusal probes assert it themselves).
pub(crate) fn parse_http(raw: &[u8]) -> (u16, Option<String>) {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP header terminator");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = &raw[split + 4..];
    if body.is_empty() {
        (status, None)
    } else {
        (status, Some(String::from_utf8_lossy(body).into_owned()))
    }
}

/// The endpoint-read handshake: ephemeral port, lazy start from the
/// frame loop, optional shrunk worker wait (so the -32002 path is
/// provable without a 5 s wall-clock burn), then discovery via
/// `mcp-endpoint.json`. The caller owns `headless_app` + any per-family
/// app setup (fixture sample rates, chain builds) around the call.
pub(crate) fn start_mcp_socket(
    app: &mut App,
    ctx: &egui::Context,
    wait: Option<std::time::Duration>,
) -> (u16, String) {
    app.mcp_port = 0;
    app.mcp_enabled = true;
    if let Some(w) = wait {
        app.mcp.as_mut().unwrap().set_wait(w);
    }
    app.mcp_frame(ctx);
    crate::mcp::bridge::read_endpoint().expect("endpoint file")
}

/// Snapshot of the CURRENT endpoint file (panics if absent) — the
/// steady-state identity probe: stability pins assert the port AND token
/// survive consecutive frames (t_13be8ef0 class: a rebind churns both).
pub(crate) fn endpoint_now() -> (u16, String) {
    crate::mcp::bridge::read_endpoint().expect("endpoint file")
}

/// v107 pumping model: the worker parses, queues, and THEN parks on the
/// oneshot — so after writing a request we sleep, execute one real
/// `McpServer::drain` on this thread (= the frame-loop role), run the
/// family's optional per-frame work (`after_drain` — the windows pins
/// DRAW a frame here), and only then read the response. Deterministic,
/// single-threaded, bounded.
pub(crate) fn socket_request_with<F: FnOnce(&mut App)>(
    app: &mut App,
    port: u16,
    token: &str,
    body: &str,
    after_drain: F,
) -> (u16, Option<String>) {
    use std::io::{Read, Write};
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
    sock.write_all(post_request(port, token, body).as_bytes()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(150));
    {
        let mut srv = app.mcp.take().unwrap();
        srv.drain(app);
        app.mcp = Some(srv);
    }
    after_drain(app);
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).unwrap();
    parse_http(&raw)
}

/// [`socket_request_with`] without per-frame work.
pub(crate) fn socket_request(app: &mut App, port: u16, token: &str, body: &str) -> (u16, Option<String>) {
    socket_request_with(app, port, token, body, |_| {})
}

/// Pump + assert 200 + parse the JSON body (the v128 `wire` shape).
pub(crate) fn socket_json_with<F: FnOnce(&mut App)>(
    app: &mut App,
    port: u16,
    token: &str,
    body: &str,
    after_drain: F,
) -> Value {
    let (st, body) = socket_request_with(app, port, token, body, after_drain);
    assert_eq!(st, 200, "wire status {st}, body: {body:?}");
    serde_json::from_str(body.as_ref().expect("JSON body")).expect("JSON body")
}

/// [`socket_json_with`] without per-frame work.
pub(crate) fn socket_json(app: &mut App, port: u16, token: &str, body: &str) -> Value {
    socket_json_with(app, port, token, body, |_| {})
}
