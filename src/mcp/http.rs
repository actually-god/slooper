//! Minimal HTTP/1.1 for the MCP endpoint (docs/MCP-DESIGN.md §2/D1).
//!
//! Request line + headers + Content-Length body; `Connection: close`, no
//! keep-alive state, `read_timeout` 10 s. The gate is a PURE function of
//! (method, path, headers, token) in the fixed order the design froze:
//! `method != POST → 405` → `Host → 403` → `Origin → 403` → `token → 401`
//! → `path → 404` (`parse → -32700` happens after the body read, in
//! serve_conn) — so quick-tier pins can drive the real gate with zero
//! sockets, while serve_conn is one thin mechanical wrapper around it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

use serde_json::Value;

use super::{rpc, Cmd};

/// Hard cap on a request body (a JSON-RPC call is a few KB; anything
/// bigger is a bug or an attack).
pub const MAX_BODY: usize = 1_000_000;

pub(crate) fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Host header → bare host name (bracketed IPv6 or host:port forms).
pub(crate) fn host_name(host: &str) -> String {
    let h = host.trim();
    if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        h.rsplit_once(':').map(|(n, _)| n).unwrap_or(h).to_string()
    }
}

/// DNS-rebinding defense #1: only loopback Host values are ever served.
pub fn host_ok(host: Option<&str>) -> bool {
    host.is_some_and(|h| matches!(host_name(h).as_str(), "127.0.0.1" | "localhost" | "::1"))
}

/// DNS-rebinding defense #2: a browser must come from the same loopback
/// origin (native clients send no Origin at all — that passes).
pub fn origin_ok(origin: Option<&str>) -> bool {
    match origin {
        None => true,
        Some("null") => true,
        Some(o) => {
            let o = o.trim_end_matches('/');
            o == "http://127.0.0.1"
                || o == "http://localhost"
                || o.starts_with("http://127.0.0.1:")
                || o.starts_with("http://localhost:")
                || o.starts_with("http://[::1]:")
        }
    }
}

pub fn token_ok(auth: Option<&str>, token: &str) -> bool {
    auth == Some(&format!("Bearer {token}"))
}

/// Fixed gate order (design §3). The Err arm is (HTTP status, text body);
/// 200-vs-202-vs-JSON handling lives in serve_conn after the body read.
pub fn gate(
    method: &str,
    path: &str,
    host: Option<&str>,
    origin: Option<&str>,
    auth: Option<&str>,
    token: &str,
) -> Result<(), (u16, &'static str)> {
    if method != "POST" {
        // Spec-legal (2025-11-25): a server offering no SSE stream answers
        // GET with 405 instead of hosting a stream.
        return Err((405, "405 - method not allowed (POST only; JSON responses, no SSE)"));
    }
    if !host_ok(host) {
        return Err((403, "403 - Host rejected (DNS-rebinding defense)"));
    }
    if !origin_ok(origin) {
        return Err((403, "403 - Origin rejected"));
    }
    if !token_ok(auth, token) {
        return Err((401, "401 - bearer token required"));
    }
    if path != "/mcp" {
        return Err((404, "404 - unknown path"));
    }
    Ok(())
}

/// JSON-RPC error bodies for the two bridge-path failures, as shared
/// builders so pins exercise the exact bytes the worker writes (D6).
pub fn busy_body(id: &Value) -> Value {
    rpc::err_resp(id, -32001, "server busy: UI command queue full — retry")
}

pub fn timeout_body(id: &Value) -> Value {
    rpc::err_resp(id, -32002, "UI-thread timeout (frame loop frozen?)")
}

/// Outcome of handing one request to the bounded UI queue — the worker's
/// only path into the app (D3). Extracted (and pinned) so the -32001/-32000
/// mapping is testable without threads.
pub enum Submit {
    Ok,
    Busy,
    Stopped,
}

pub fn queue_submit(tx: &SyncSender<Cmd>, cmd: Cmd) -> Submit {
    match tx.try_send(cmd) {
        Ok(()) => Submit::Ok,
        Err(TrySendError::Full(_)) => Submit::Busy,
        Err(TrySendError::Disconnected(_)) => Submit::Stopped,
    }
}

/// Await the UI-thread result of a submitted command. Extracted from
/// serve_conn so the timeout taxonomy is fast-tier testable with an
/// injected short wait (the production wait lives on the server).
pub enum Awaited {
    Done(Option<Value>),
    Timeout,
}

pub fn await_result(rx: &std::sync::mpsc::Receiver<Option<Value>>, wait: Duration) -> Awaited {
    match rx.recv_timeout(wait) {
        Ok(resp) => Awaited::Done(resp),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Awaited::Timeout,
        // The executor dropped the oneshot (server stopped mid-call).
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Awaited::Done(None),
    }
}

fn write_http(sock: &mut TcpStream, status: u16, reason: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes());
    let _ = sock.write_all(body);
    let _ = sock.flush();
}

/// One request per connection (Connection: close). Runs on a per-conn
/// worker thread — touches NO app state except pushing to the queue.
pub(crate) fn serve_conn(
    mut sock: TcpStream,
    token: &str,
    tx: &SyncSender<Cmd>,
    wait: Duration,
    ctx: egui::Context,
) {
    sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut br = BufReader::new(sock.try_clone().unwrap());

    let mut reqline = String::new();
    if br.read_line(&mut reqline).unwrap_or(0) == 0 {
        return;
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut l = String::new();
        if br.read_line(&mut l).unwrap_or(0) == 0 {
            break;
        }
        let l = l.trim_end().to_string();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    let mut parts = reqline.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        write_http(&mut sock, 400, "Bad Request", "text/plain", b"400");
        return;
    };
    if let Err((code, msg)) = gate(
        method,
        path,
        header(&headers, "host"),
        header(&headers, "origin"),
        header(&headers, "authorization"),
        token,
    ) {
        let reason: &str = match code {
            405 => "Method Not Allowed",
            403 => "Forbidden",
            401 => "Unauthorized",
            _ => "Not Found",
        };
        write_http(&mut sock, code, reason, "text/plain", msg.as_bytes());
        return;
    }
    let len: usize = header(&headers, "content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if len == 0 || len > MAX_BODY {
        write_http(&mut sock, 400, "Bad Request", "text/plain", b"400 - bad or missing Content-Length");
        return;
    }
    let mut body = vec![0u8; len];
    if br.read_exact(&mut body).is_err() {
        return;
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            write_http(
                &mut sock,
                400,
                "Bad Request",
                "application/json",
                rpc::err_resp(&Value::Null, -32700, "parse error").to_string().as_bytes(),
            );
            return;
        }
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);

    let (rtx, rrx) = std::sync::mpsc::channel::<Option<Value>>();
    match queue_submit(tx, Cmd { req, tx: rtx }) {
        Submit::Busy => {
            write_http(&mut sock, 200, "OK", "application/json", busy_body(&id).to_string().as_bytes());
            return;
        }
        Submit::Stopped => {
            write_http(&mut sock, 503, "Service Unavailable", "text/plain", b"503 - app shutting down");
            return;
        }
        Submit::Ok => {}
    }
    // Wake the frame loop (production-proven from foreign threads; the
    // idle request_repaint_after(100 ms) heartbeat bounds worst-case
    // latency even if a wake were ever missed).
    ctx.request_repaint();
    match await_result(&rrx, wait) {
        Awaited::Timeout => {
            write_http(&mut sock, 200, "OK", "application/json", timeout_body(&id).to_string().as_bytes())
        }
        // Notification: dispatch returned None → 202 Accepted, empty body.
        Awaited::Done(None) => write_http(&mut sock, 202, "Accepted", "text/plain", b""),
        Awaited::Done(Some(v)) => {
            write_http(&mut sock, 200, "OK", "application/json", v.to_string().as_bytes())
        }
    }
}
