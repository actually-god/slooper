//! JSON-RPC 2.0 dispatcher (docs/MCP-DESIGN.md D1 dual-shape protocol,
//! §6 error taxonomy). Pure w.r.t. threads: [`dispatch`] executes inline
//! and is called ONLY from the UI frame loop (`McpServer::drain`) or from
//! quick-tier pins — the contract is pins drive THIS function, never the
//! tool fns, so gate/taxonomy/latch/firewall/ack behavior is real.
//!
//! Returns `None` for notifications (HTTP layer maps that to 202).

use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Value};

use crate::ui::App;

use super::tools::{ToolDef, ToolError};

/// Spec revisions accepted on one dispatcher (handshake era + 2026 stateless).
pub const PROTOCOLS: [&str; 5] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"];
/// Handshake-era "latest we speak" (2026-07-28 needs no initialize).
pub const LATEST: &str = "2025-11-25";

pub fn ok_resp(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn err_resp(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

/// Successful tools/call envelope for a structured result (observed-then-ack:
/// `structured` IS the post-change state re-read through the getters).
fn tool_ok(structured: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": structured.to_string()}],
        "structuredContent": structured,
    })
}

/// Domain failure (isError, design §6): human text + machine-readable kind.
fn tool_err(e: &ToolError) -> Value {
    json!({
        "content": [{"type": "text", "text": e.message}],
        "structuredContent": {"error": e.kind},
        "isError": true,
    })
}

fn server_info() -> Value {
    json!({"name": "slooper", "version": env!("CARGO_PKG_VERSION")})
}

/// One request in, one response out (None = notification → 202).
pub fn dispatch(req: &Value, app: &mut App) -> Option<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let is_notif = req.get("id").is_none();
    let Some(method) = req.get("method").and_then(|m| m.as_str()) else {
        return if is_notif {
            None
        } else {
            Some(err_resp(&id, -32600, "invalid request: no string \"method\""))
        };
    };
    let params = req.get("params").cloned().unwrap_or(json!({}));
    match method {
        // Handshake era. Never REQUIRED (2026 clients skip it).
        "initialize" => {
            let want = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("");
            let chosen = if PROTOCOLS.contains(&want) { want } else { LATEST };
            Some(ok_resp(
                &id,
                json!({
                    "protocolVersion": chosen,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": server_info(),
                    "instructions": "slooper DAW control. Read tools list canonical ids; \
                                     write tools echo the post-change state (observed-then-ack).",
                }),
            ))
        }
        // Dropped in 2026, still sent by today's SDKs — keep answering.
        "ping" => Some(ok_resp(&id, json!({}))),
        // 2026-07-28 stateless discovery (MUST-have).
        "server/discover" => Some(ok_resp(
            &id,
            json!({
                "supportedVersions": PROTOCOLS,
                "capabilities": {"tools": {}},
                "serverInfo": server_info(),
            }),
        )),
        "tools/list" => Some(ok_resp(
            &id,
            json!({
                "tools": app.mcp_tools.iter().map(ToolDef::render).collect::<Vec<_>>(),
                "ttlMs": 60_000,
                "cacheScope": "private",
            }),
        )),
        "tools/call" => Some(tools_call(app, &id, &params)),
        m if m.starts_with("notifications/") => None,
        _ => {
            if is_notif {
                None // unknown notification: accepted silently (202), per spec
            } else {
                Some(err_resp(&id, -32601, &format!("method not found: {method}")))
            }
        }
    }
}

fn tools_call(app: &mut App, id: &Value, params: &Value) -> Value {
    let Some(name) = params.get("name").and_then(|n| n.as_str()) else {
        return err_resp(id, -32602, "tools/call needs params.name (string)");
    };
    // Copy the fn pointer + flags out of the borrow: the executor needs
    // &mut App next, so no borrow of app.mcp_tools may stay alive.
    let (execute, is_read) = match app.mcp_tools.iter().find(|t| t.name == name) {
        Some(t) => (t.execute, t.read),
        None => {
            return err_resp(
                id,
                -32601,
                &format!("unknown tool: {name} — call tools/list for the live registry"),
            )
        }
    };
    // The read-only latch (§3): user yanks the lever, agent connection lives.
    if !is_read && app.mcp_read_only {
        return err_resp(
            id,
            -32003,
            &format!("read-only mode refuses \"{name}\" — untick it in Settings \u{25b8} AUTOMATION"),
        );
    }
    let args = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return err_resp(id, -32602, "tools/call params.arguments must be an object"),
    };
    // Panic firewall (v23 doctrine): an MCP bug must never take the DAW down.
    let r = catch_unwind(AssertUnwindSafe(|| execute(app, &args)));
    match r {
        Ok(Ok(structured)) => ok_resp(id, tool_ok(structured)),
        Ok(Err(te)) => ok_resp(id, tool_err(&te)),
        Err(_) => err_resp(id, -32000, "internal error (caught panic); server still alive"),
    }
}

/// Kept so the taxonomy's error-kind set has one source of truth
/// (pins + domain cards build ToolErrors through the constructors).
pub const ERROR_KINDS: [&str; 6] =
    ["invalid_params", "not_found", "out_of_range", "wrong_kind", "read_only", "busy"];
