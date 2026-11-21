//! THE tool table (docs/MCP-DESIGN.md §2/§6/D5).
//!
//! Every MCP domain tool is one [`ToolDef`] row. The `execute` function
//! runs ONLY on the UI frame-loop thread (inside `McpServer::drain`), so
//! handlers may use the same setter seams the UI uses — no locks, no RT
//! coupling. Read/write is a DECLARED flag (`read`), never a name
//! heuristic: `mcp_read_only` refuses every tool whose flag is false.
//!
//! Extension contract for MCP-02..05: add a module with a
//! `register(&mut Vec<ToolDef>)` fn and ONE line in [`install_all`].
//! The dispatcher, bridge and lifecycle never change for a new tool.
//! Names follow `^[a-z_]+\.[a-z_.]+$` (pinned by v107_mcp_tool_table_invariants);
//! units are the native currency of each setter seam (§6).

use serde_json::{json, Value};

use crate::ui::App;

/// Domain-level tool failure: a SUCCESSFUL JSON-RPC result carrying
/// `isError:true` + `structuredContent.error` (design §6) — the agent
/// gets a fixable message, not a transport error.
#[derive(Debug, Clone)]
pub struct ToolError {
    /// invalid_params | not_found | out_of_range | wrong_kind | read_only | busy
    pub kind: &'static str,
    pub message: String,
}

impl ToolError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        ToolError { kind: "invalid_params", message: msg.into() }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        ToolError { kind: "not_found", message: msg.into() }
    }
    pub fn out_of_range(msg: impl Into<String>) -> Self {
        ToolError { kind: "out_of_range", message: msg.into() }
    }
    pub fn wrong_kind(msg: impl Into<String>) -> Self {
        ToolError { kind: "wrong_kind", message: msg.into() }
    }
    pub fn busy(msg: impl Into<String>) -> Self {
        ToolError { kind: "busy", message: msg.into() }
    }
}

pub type ToolResult = Result<Value, ToolError>;

#[derive(Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// Static `json!` table (spike-verified shape).
    pub schema: Value,
    /// readOnlyHint — and the source of truth for the `mcp_read_only` latch.
    pub read: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub execute: fn(&mut App, &Value) -> ToolResult,
}

impl ToolDef {
    /// MCP tool descriptor as clients see it (all four annotations, always).
    pub fn render(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.schema,
            "annotations": {
                "readOnlyHint": self.read,
                "destructiveHint": self.destructive,
                "idempotentHint": self.idempotent,
                "openWorldHint": false,
            },
        })
    }
}

/// Global table: filled exactly once (OnceLock) by [`build`]. App::new
/// snapshots it into `App::mcp_tools`, so test registrations against one
/// App never leak into another (dispatch runs ONLY against the App copy).
static TOOLS: std::sync::OnceLock<Vec<ToolDef>> = std::sync::OnceLock::new();

/// Idempotent — safe to call from every App::new.
pub fn install_all() {
    TOOLS.get_or_init(build);
}

/// Snapshot of the global table (installs on first use).
pub fn table() -> Vec<ToolDef> {
    install_all();
    TOOLS.get().expect("OnceLock").clone()
}

fn build() -> Vec<ToolDef> {
    let mut v: Vec<ToolDef> = Vec::new();
    register_core(&mut v);
    // ── extension seam (one line per card, keep the order) ──────────────
    crate::mcp_settings::register(&mut v); // MCP-02 (settings.*)
    // crate::mcp_windows::register(&mut v);    // MCP-03 (windows.*)
    // crate::mcp_mixer::register(&mut v);      // MCP-04 (mixer.* chain.* assets.*)
    crate::mcp_plugins::register(&mut v);      // MCP-05 (plugins.* plugin.* sampler.* drum.* midi.*)
    v
}

fn register_core(v: &mut Vec<ToolDef>) {
    v.push(ToolDef {
        name: "core.echo",
        description: "Round-trip a string through the live app (bridge demo / smoke test). \
                      Read-only: touches no state.",
        schema: json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "the text to echo back"},
            },
            "required": ["text"],
        }),
        read: true,
        destructive: false,
        idempotent: true,
        execute: echo,
    });
}

/// MCP-01's demo tool (card AC). `core.` domain so the name passes the
/// design's own `^[a-z_]+\.[a-z_.]+$` pin (bare `echo` would not).
fn echo(_app: &mut App, args: &Value) -> ToolResult {
    let text = args
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| ToolError::invalid("core.echo needs a string \"text\" argument"))?;
    Ok(json!({
        "echo": text,
        "server": format!("slooper {}", env!("CARGO_PKG_VERSION")),
    }))
}
