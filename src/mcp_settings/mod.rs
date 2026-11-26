//! v108/MCP-02 — `settings.list` / `settings.get` / `settings.set`
//! (board card t_377dcec9; design spine docs/MCP-DESIGN.md §6 tools-only).
//!
//! The surface is SERDE-DERIVED, never hand-listed: `App::config_snapshot()`
//! (the single capture point `save_config` shares) serializes to a JSON tree
//! whose keys ARE the field list, whose value types ARE the reported
//! `json_type`, and whose values ARE the live state. A new `Config` field
//! appears here automatically; the completeness pins
//! (`v108_settings_list_parity`, `v108_settings_classify_total`) fail until
//! the new field is classified Live or Restart.
//!
//! Tiers (card AC "do not half-apply"):
//! - **Live**: applied immediately through the GUI's own setter seams —
//!   `App::mcp_apply_setting` (ui.rs) routes every field through exactly the
//!   code a user edit runs (set_trk_vol/gain/pan, engine sc_* atomics, the
//!   startup shortcut/MIDI fill, n_tracks + selection clamp, the mcp_* trio
//!   which the frame loop hot-(re)starts). Then `save_config` persists.
//! - **Restart**: the RUNNING app must not pretend — port selections,
//!   backend choice, domain tables (chain/synth/sampler/drum-kit/geometry)
//!   own live machinery that MCP-03/04/05 drive. The new value lands in
//!   config.json NOW plus an entry in `App::mcp_cfg_pending`; `save_config`
//!   re-applies entries so a later save cannot clobber them, and drops an
//!   entry the moment the live snapshot moves off `prev` (user GUI edit =
//!   user owns the field). `requires_restart` is surfaced in every entry,
//!   in `settings.set`'s result, and in the status echo.
//!
//! Validation = real serde: the new value is merged into the captured tree
//! and the WHOLE tree is deserialized back (`from_value::<Config>`) — types,
//! integer ranges, enum shapes all reject through the same rules the config
//! FILE obeys; a refusal changes nothing at all (checked by pins). On top,
//! a small metadata overlay (ranges/enums/units taken from the code's own
//! clamps) rejects out-of-domain values serde would happily accept.
//!
//! Ack = observed-then-ack (design §4): the result re-reads the field
//! through the same snapshot the reads use. Every accepted write echoes
//! `app.status` (goal-1 doctrine; v72 family).

use serde_json::{json, Value};

use crate::config::{Config, McpPending};
use crate::mcp::tools::{ToolDef, ToolError, ToolResult};
use crate::ui::App;

#[cfg(test)]
mod tests;

/// How a field takes effect after `settings.set`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Apply {
    Live,
    Restart,
}

/// The one classification table. MUST cover every serde-serialized Config
/// key (the pins enforce BOTH directions: serialized keys ⊆ Live∪Restart and
/// Live∪Restart ⊆ serialized keys — typos and forgotten fields go red).
pub const LIVE: &[&str] = &[
    "shortcuts",
    "midi",
    "midi_axis",
    "gui_scale",
    "latency_ms",
    "trk_vol",
    "trk_color",
    "trk_h",
    "trk_gain",
    "trk_pan",
    "metro_bpm",
    "metro_sub",
    "metro_num",
    "q_sub",
    "snap_sub",
    "auto_quantize",
    "loop_tl",
    "loop_mode",
    "loop_area_start",
    "loop_area_end",
    "master_vol",
    "monitor_on",
    "midi_live",
    "preview_on",
    "preview_secs",
    "preview_tracks",
    "snap_on",
    "auto_snap_on",
    "auto_snap_sub",
    "roll_vel_step",
    "project_dir",
    "dir_memo",
    "rack_collapsed",
    "rack_dir_order",
    "record_target",
    "rack_group_by_dir",
    "rec_src",
    "rec_prev",
    "rec_loop",
    "value_binds",
    "drum",
    "show_drums",
    "show_master",
    "spectrum_line",
    // v170 (card t_3d0dfae8): child-track removal never-warn flag —
    // pure UI gate, applying it live is correct.
    "lane_rm_never_warn",
    "sc_send",
    "sc_recv_gain",
    "editor_probe",
    "track_bend_ranges",
    // v164 (card t_0c73850c): EDO tuning — apply republishes the atomic.
    "edo_n",
    "edo_root",
    "n_tracks",
    "show_rack",
    "show_mixer",
    "mixer_follow_tracks",
    "show_midi",
    "show_binds",
    "theme",
    "mpe",
    "win_editor_embed",
    "mcp_enabled",
    "mcp_port",
    "mcp_read_only",
];
pub const RESTART: &[&str] = &[
    "ports_in",
    "ports_out",
    "ports_out_lr",
    "vol_scale_v2",
    "rack_folders",
    "fx_dir",
    "vst_dirs",
    "track_fx",
    "track_chain",
    "track_vst",
    "track_synth",
    "track_synth_params",
    "track_synth_states",
    "track_synth_programs",
    "drum_kits",
    "sampler_maps",
    "side_wins",
    "dyn_docks",
    "win_layouts",
    "win_midi_inputs",
    "win_audio_backend",
];

pub fn classify(name: &str) -> Option<Apply> {
    if RESTART.contains(&name) {
        return Some(Apply::Restart);
    }
    if LIVE.contains(&name) {
        return Some(Apply::Live);
    }
    None
}

/// Best-effort unit / enum / range metadata ("where available", card text).
/// Every number here is a clamp that already exists in product code (cited
/// in comments) — never an invented policy.
fn meta(name: &str) -> Value {
    // Subdivision choices derive from the product table itself (no literal).
    let subs: Vec<i64> = crate::ui::SUBDIVISIONS.iter().map(|s| s.0 as i64).collect();
    let meta = |unit: Option<&str>, min: Option<i64>, max: Option<i64>, choices: Option<Value>| -> Value {
        let mut m = serde_json::Map::new();
        if let Some(u) = unit {
            m.insert("unit".into(), json!(u));
        }
        if let Some(v) = min {
            m.insert("min".into(), json!(v));
        }
        if let Some(v) = max {
            m.insert("max".into(), json!(v));
        }
        if let Some(c) = choices {
            m.insert("choices".into(), c);
        }
        Value::Object(m)
    };
    match name {
        "theme" => meta(None, None, None, Some(json!(["dark", "light"]))),
        "loop_mode" => meta(
            None,
            Some(0),
            Some(2),
            Some(json!(["area", "current longest", "whole song"])),
        ),
        "win_audio_backend" => meta(None, None, None, Some(json!(["auto", "wasapi", "jack"]))),
        "metro_bpm" => meta(Some("BPM"), Some(20), Some(300), None), // App::bpm() clamp
        "metro_num" => meta(Some("beats/measure"), Some(1), Some(16), None), // App::beats()
        "gui_scale" => meta(Some("x UI zoom"), None, None, None), // float clamp 0.7..1.6 in domain_check
        "latency_ms" => meta(Some("ms"), Some(0), Some(1000), None), // save_config clamp
        "roll_vel_step" => meta(Some("velocity pts/tick"), Some(1), Some(32), None), // save clamp
        "n_tracks" => meta(Some("tracks"), Some(1), Some(16), None), // Config doc
        "q_sub" | "snap_sub" | "auto_snap_sub" | "metro_sub" => {
            meta(Some("steps/beat"), None, None, Some(json!(subs)))
        }
        "master_vol" | "trk_vol" => meta(Some("MIDI (0=-inf, 115=0 dB)"), None, None, None),
        "trk_gain" => meta(Some("MIDI (80=0 dB)"), None, None, None),
        "trk_pan" => meta(Some("MIDI (64=centre)"), None, None, None),
        "loop_area_start" | "loop_area_end" => meta(Some("samples"), None, None, None),
        "preview_secs" => meta(Some("seconds"), Some(0), None, None),
        _ => Value::Null,
    }
}

/// Domain rejection on top of serde typing (design §6 taxonomy). Ranges come
/// from `meta()` — i.e. from the product's own clamps — and refuse BEFORE
/// anything changes.
fn domain_check(name: &str, value: &Value) -> Option<String> {
    let m = meta(name);
    let m = m.as_object()?;
    if let Some(c) = m.get("choices").and_then(|c| c.as_array()) {
        if !c.contains(value) {
            // loop_mode declares int bounds AND human labels: labels pass
            // only as documentation, the int range below still applies.
            if !(name == "loop_mode" && value.is_number()) {
                return Some(format!(
                    "{name} must be one of {c:?}, got {value}"
                ));
            }
        }
    }
    if let Some(n) = value.as_i64() {
        if let Some(min) = m.get("min").and_then(|v| v.as_i64()) {
            if n < min {
                return Some(format!("{name} must be >= {min}, got {n}"));
            }
        }
        if let Some(max) = m.get("max").and_then(|v| v.as_i64()) {
            if n > max {
                return Some(format!("{name} must be <= {max}, got {n}"));
            }
        }
    } else if let Some(f) = value.as_f64() {
        // gui_scale keeps a FLOAT clamp (0.7..=1.6) in product code.
        if name == "gui_scale" && !(0.7..=1.6).contains(&f) {
            return Some(format!("gui_scale must be within 0.7..=1.6, got {f}"));
        }
    }
    None
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Live tree: snapshot -> serde Value. The field list, types and values all
/// originate HERE — this is the "no hand-maintained list" mechanism.
pub(crate) fn snapshot_tree(app: &App) -> Value {
    serde_json::to_value(app.config_snapshot()).expect("Config serializes")
}

/// One settings.list entry: name, json_type, requires_restart, current
/// (LIVE) value, plus unit/enum meta where available (card §schema).
fn entry(name: &str, value: &Value) -> Result<Value, ToolError> {
    let restart = match classify(name) {
        Some(Apply::Live) => false,
        Some(Apply::Restart) => true,
        // A serialized field with no classification = a new Config field
        // nobody classified. Surface it loudly (the parity pin turns this
        // into a deterministic RED for every future unclassified field).
        None => {
            return Err(ToolError::invalid(format!(
                "internal: setting {name} has no Live/Restart classification"
            )))
        }
    };
    let mut e = json!({
        "name": name,
        "json_type": json_type(value),
        "requires_restart": restart,
        "value": value,
    });
    let m = meta(name);
    if let Some(m) = m.as_object() {
        for (k, v) in m {
            e[k] = v.clone();
        }
    }
    Ok(e)
}

// ── tools ─────────────────────────────────────────────────────────────────

pub fn register(v: &mut Vec<ToolDef>) {
    v.push(ToolDef {
        name: "settings.list",
        description: "Every persisted setting with JSON type, current live value, \
                      requires-restart flag and unit/enum meta. Auto-derived from the \
                      config schema — always complete. Read-only.",
        schema: json!({"type": "object", "properties": {}}),
        read: true,
        destructive: false,
        idempotent: true,
        execute: list,
    });
    v.push(ToolDef {
        name: "settings.get",
        description: "Read one setting's live value + schema entry (name, json_type, \
                      requires_restart, unit/enum meta where available). Read-only.",
        schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "setting name from settings.list"},
            },
            "required": ["name"],
        }),
        read: true,
        destructive: false,
        idempotent: true,
        execute: get,
    });
    v.push(ToolDef {
        name: "settings.set",
        description: "Write one setting: validated (serde + ranges/enums), applied \
                      LIVE through the GUI's own setter seams when the field is \
                      hot-appliable, or persisted for the next start when it is not \
                      (requires_restart=true is then surfaced here and in \
                      settings.list). Echoes the observed post-change state. \
                      Unknown fields and bad values are refused with nothing \
                      changed.",
        schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "setting name from settings.list"},
                "value": {"description": "new value; JSON type must match the field's json_type"},
            },
            "required": ["name", "value"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: set,
    });
}

fn list(app: &mut App, _args: &Value) -> ToolResult {
    let tree = snapshot_tree(app);
    let obj = tree.as_object().expect("Config serializes to an object");
    let mut entries = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        entries.push(entry(k, v)?);
    }
    Ok(json!({
        "settings": entries,
        "count": entries.len(),
        "note": "requires_restart=true fields are applied by the next app start; \
                 all others apply immediately when set",
    }))
}

fn get(app: &mut App, args: &Value) -> ToolResult {
    let name = need_name(args)?;
    let tree = snapshot_tree(app);
    match tree.get(name) {
        Some(v) => entry(name, v),
        None => Err(unknown(name, &tree)),
    }
}

fn set(app: &mut App, args: &Value) -> ToolResult {
    let name = need_name(args)?;
    let value = args
        .get("value")
        .ok_or_else(|| ToolError::invalid("settings.set needs {name, value}"))?;
    let mut tree = snapshot_tree(app);
    let obj = tree.as_object_mut().expect("Config object");
    if !obj.contains_key(name) {
        return Err(unknown(name, &tree));
    }
    let prev = obj[name].clone();
    obj[name] = value.clone();
    // Real serde validation of the WHOLE config — the same rules the config
    // file obeys (types, int widths, shapes of nested tables).
    let cfg: Config = match serde_json::from_value(tree.clone()) {
        Ok(c) => c,
        Err(e) => {
            return Err(ToolError::invalid(format!(
                "invalid value for {name}: {e} — see settings.list for json_type"
            )))
        }
    };
    // Domain layer (ranges/enums from the product's own clamps).
    if let Some(msg) = domain_check(name, value) {
        return Err(ToolError::out_of_range(msg));
    }
    let restart = classify(name) == Some(Apply::Restart);
    if restart {
        // PERSIST only — the running app keeps its live value (a half-
        // applied port/backend/domain-table change is exactly what the
        // card forbids). The overlay keeps the value alive through every
        // later save_config until the app restarts or the user edits the
        // field in the GUI (then the entry drops — user owns it).
        {
            let mut pend = app.mcp_cfg_pending.borrow_mut();
            match pend.iter_mut().find(|p| p.name == name) {
                Some(e) => e.new = value.clone(),
                None => pend.push(McpPending {
                    name: name.to_string(),
                    new: value.clone(),
                    prev,
                }),
            }
        }
        app.save_config();
    } else {
        app.mcp_apply_setting(name, &cfg)
            .map_err(ToolError::invalid)?;
        app.save_config();
    }
    let echo = format!(
        "MCP: {name} = {}{}",
        value,
        if restart { " (applies on next start)" } else { "" }
    );
    app.status = echo.clone();
    // Observed-then-ack: re-read through the SAME snapshot the reads use.
    let live = snapshot_tree(app).get(name).cloned().unwrap_or(Value::Null);
    let mut ack = json!({
        "name": name,
        "applied": if restart { "restart" } else { "live" },
        "requires_restart": restart,
        "value": live,
        "status": echo,
    });
    if restart {
        ack["pending"] = value.clone();
    }
    Ok(ack)
}

fn need_name(args: &Value) -> Result<&str, ToolError> {
    args.get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| ToolError::invalid("settings tools need a string \"name\" argument"))
}

/// Self-documenting refusal (design §5: unknown address keys list the valid
/// ones): 79 names is a lot, but bounded, and one call teaches the client.
fn unknown(name: &str, tree: &Value) -> ToolError {
    let mut keys: Vec<&String> =
        tree.as_object().map(|o| o.keys().collect()).unwrap_or_default();
    keys.sort();
    ToolError::invalid(format!(
        "unknown setting \"{name}\" — valid settings ({}): {}",
        keys.len(),
        keys.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
    ))
}
