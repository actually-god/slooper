//! Dump the live MCP tool table to stdout as pretty JSON.
//!
//! This is the regeneration source for `docs/MCP.md` (MCP-06): the doc's
//! tool sections are derived from the registry itself, never hand-typed,
//! so the reference cannot silently drift from shipped behavior. The
//! `mcp_docs_cover_live_tool_table` pin fails CI if a tool is missing
//! from the doc; re-run this example and re-fold the doc when it does.
//!
//! ```sh
//! cargo run --example mcp_tool_dump > /tmp/mcp-tools.json
//! ```

use serde_json::json;

use slooper::mcp::tools;

fn main() {
    let rows: Vec<_> = tools::table()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "schema": t.schema,
                "read": t.read,
                "destructive": t.destructive,
                "idempotent": t.idempotent,
            })
        })
        .collect();
    let out = serde_json::to_string_pretty(&json!({ "count": rows.len(), "tools": rows }))
        .expect("serializable");
    println!("{out}");
}
