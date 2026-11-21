//! v111/MCP-03: the `windows.*` tool family (docs/MCP-DESIGN.md D4/D5,
//! board card t_5cd0d1e5).
//!
//! Registry model (design D4): every roster window answers to a STABLE
//! machine id — the four primaries by [`PrimKind`] key
//! (`rack`/`binds`/`monitor`/`mixer`), dynamic windows by `DynKind::key()`
//! with the one params extension (`params:synth:<t>` /
//! `params:chain:<t>:<r>`; `win_id`/`win_slot` in ui.rs). Roster indices
//! never surface. Host-side native plugin editor windows (Linux X11
//! top-levels / Windows embedded chrome) are listed and closed here
//! (`vst:<t>`, `vstfx:<t>:<row>`); move/resize/open of editors is a
//! DOCUMENTED v1 boundary — Linux editors are WM-positioned (no host seam
//! exists), Windows chrome is a roster window with the full float/dock
//! path, and editor OPENING is the crash-guarded GUI action.
//!
//! Every write mutates the SAME UI-thread state the mouse uses (D3 — the
//! handlers run inside `McpServer::drain`), echoes the post-change
//! geometry through the same getter `windows.list` uses
//! (observed-then-ack), and status-echoes like every other user action
//! (v72 doctrine).

use serde_json::{json, Value};

use super::*;
use crate::mcp::tools::{ToolDef, ToolError, ToolResult};

/// Title-band margin that must stay inside the viewport when a floating
/// window is moved (reachable header = closable window).
const MIN_VIS: f32 = 60.0;
/// Usability floor / ceiling for `windows.resize` (ceiling = the
/// viewport: a window larger than the screen is a lost window).
const MIN_W: f32 = 120.0;
const MIN_H: f32 = 80.0;

const EDGE_NAMES: [&str; 4] = ["left", "right", "bottom", "top"];

// ── registration (ONE line in mcp::tools::install_all) ───────────────────

pub(crate) fn register(v: &mut Vec<ToolDef>) {
    let id = json!({"type": "string", "description": ID_DESC});
    let f = |d: &str| json!({"type": "number", "description": d});
    v.push(ToolDef {
        name: "windows.list",
        description: LIST_DESC,
        schema: json!({"type": "object", "properties": {}}),
        read: true,
        destructive: false,
        idempotent: true,
        execute: list,
    });
    v.push(ToolDef {
        name: "windows.open",
        description: OPEN_DESC,
        schema: json!({
            "type": "object",
            "properties": {
                "id": id,
                "asset": {"type": "integer", "minimum": 0, "description":
                    "roll/auto only: asset id (assets.list). roll: MIDI asset \
                     (absent = fresh pattern); auto: automation asset"},
            },
            "required": ["id"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: open,
    });
    v.push(ToolDef {
        name: "windows.close",
        description: CLOSE_DESC,
        schema: json!({"type": "object", "properties": {"id": id}, "required": ["id"]}),
        read: false,
        destructive: false,
        idempotent: true,
        execute: close,
    });
    v.push(ToolDef {
        name: "windows.move",
        description: MOVE_DESC,
        schema: json!({
            "type": "object",
            "properties": {"id": id, "x": f("window top-left x"), "y": f("window top-left y")},
            "required": ["id", "x", "y"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: wmove,
    });
    v.push(ToolDef {
        name: "windows.resize",
        description: RESIZE_DESC,
        schema: json!({
            "type": "object",
            "properties": {
                "id": id,
                "w": f("width >=120"),
                "h": f("height >=80"),
            },
            "required": ["id", "w", "h"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: wresize,
    });
    v.push(ToolDef {
        name: "windows.dock",
        description: DOCK_DESC,
        schema: json!({
            "type": "object",
            "properties": {
                "id": id,
                "edge": {"description": "left | right | bottom | top (or 0..3)",
                          "oneOf": [{"type": "string"}, {"type": "integer", "minimum": 0, "maximum": 3}]},
                "rank": {"type": "integer", "minimum": 0, "description":
                          "slot along the edge (0-based); default tail"},
            },
            "required": ["id", "edge"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: wdock,
    });
    v.push(ToolDef {
        name: "windows.undock",
        description: UNDOCK_DESC,
        schema: json!({
            "type": "object",
            "properties": {"id": id, "x": f("optional new top-left x"), "y": f("optional new top-left y")},
            "required": ["id"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: wundock,
    });
    v.push(ToolDef {
        name: "windows.layout.save",
        description: LAYOUT_SAVE_DESC,
        schema: json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "template name (re-capture replaces)"}},
            "required": ["name"],
        }),
        read: false,
        destructive: false,
        idempotent: false,
        execute: layout_save,
    });
    v.push(ToolDef {
        name: "windows.layout.restore",
        description: LAYOUT_RESTORE_DESC,
        schema: json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "saved template name"}},
            "required": ["name"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: layout_restore,
    });
}

const ID_DESC: &str = "stable window id from windows.list (e.g. rack, mixer, \
                       drums, roll, chain:<track>, params:synth:<track>, \
                       params:chain:<track>:<row>, vst:<track>, vstfx:<track>:<row>)";

// ── observation (the getter every echo reads through) ────────────────────

fn rect_json(r: [f32; 4]) -> Value {
    json!([
        (r[0] * 10.0).round() / 10.0,
        (r[1] * 10.0).round() / 10.0,
        (r[2] * 10.0).round() / 10.0,
        (r[3] * 10.0).round() / 10.0,
    ])
}

/// The observed state of one roster slot — `windows.list` rows and every
/// write-echo are built HERE, so "did it really happen?" cannot drift
/// between the read and the ack (design D4 observed-then-ack).
fn entry(app: &App, idx: usize) -> Value {
    let docked = is_docked(app, idx);
    let live = if docked {
        app.dock_rects.get(idx).copied().unwrap_or([0.0; 4])
    } else {
        win_rect(app, idx)
    };
    let rect = if live[2] > 8.0 && live[3] > 8.0 {
        rect_json(live)
    } else if docked {
        // Docked but never measured (opened this frame): no lie, no guess.
        Value::Null
    } else {
        rect_json(float_footprint(app, idx))
    };
    let edge = dock_edge(app, idx).clamp(0, 3) as usize;
    json!({
        "id": win_id(app, idx),
        "kind": if idx < 4 { "primary" } else { "dynamic" },
        "title": win_title(app, idx),
        "open": win_visible(app, idx),
        "docked": docked,
        "edge": dock_edge(app, idx),
        "edge_name": EDGE_NAMES[edge],
        "rank": dock_order(app, idx),
        "rect": rect,
        "float_rect": rect_json(win_rect(app, idx)),
    })
}

fn editor_entry(id: String, w: &crate::vstedit::EditorWindow) -> Value {
    let (wpx, hpx) = w.editor_size();
    json!({
        "id": id,
        "kind": "editor",
        "title": w.identity(),
        "open": !w.is_closed(),
        "path": w.path(),
        "size": [wpx, hpx],
    })
}

fn editor_ids(app: &App) -> Vec<(String, DynKind, bool)> {
    // Deterministic order: HashMap iteration is random; agents cache lists.
    let mut out: Vec<(String, DynKind, bool)> = Vec::new();
    let mut ts: Vec<usize> = app.vst_editors.keys().copied().collect();
    ts.sort_unstable();
    for t in ts {
        // Windows embed chrome appears in `windows` as a roster window —
        // never double-list the same window.
        if !app.dyn_wins.iter().any(|w| w.kind == DynKind::Vst(t)) {
            out.push((format!("vst:{t}"), DynKind::Vst(t), true));
        }
    }
    let mut tr: Vec<(usize, usize)> = app.fx_vst_editors.keys().copied().collect();
    tr.sort_unstable();
    for (t, r) in tr {
        if !app.dyn_wins.iter().any(|w| w.kind == DynKind::VstFx(t, r)) {
            out.push((format!("vstfx:{t}:{r}"), DynKind::VstFx(t, r), false));
        }
    }
    out
}

fn list_entries(app: &App) -> Vec<Value> {
    (0..win_count(app)).map(|i| entry(app, i)).collect()
}

// ── argument plumbing ────────────────────────────────────────────────────

fn need_id(args: &Value) -> Result<&str, ToolError> {
    args.get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::invalid("this tool needs a string \"id\" (see windows.list)"))
}

/// Resolve a stable id to a REGISTERED roster slot; the refusal lists the
/// live ids (design D4: self-documenting errors).
fn resolve(app: &App, id: &str) -> Result<usize, ToolError> {
    if let Some(idx) = win_slot(app, id) {
        return Ok(idx);
    }
    // Editor ids are real ids — they just have no roster slot (v1
    // boundary; close is supported, geometry is not).
    if editor_kind(id).is_some() {
        return Err(ToolError::wrong_kind(
            "native plugin editor windows (kind \"editor\" in windows.editors) \
             are WM-managed top-levels — list/close supported, geometry is not",
        ));
    }
    Err(ToolError::not_found(format!(
        "unknown window id \"{id}\" — live ids: {}",
        win_id_hints(app)
    )))
}

/// Ids that PARSE as editor kinds even while no window is live (so the
/// boundary error is right on closed/never-opened ids too).
fn editor_kind(id: &str) -> Option<DynKind> {
    match DynKind::from_key(id) {
        k @ (Some(DynKind::Vst(_)) | Some(DynKind::VstFx(_, _))) => k,
        _ => None,
    }
}

fn num(args: &Value, key: &str) -> Option<f32> {
    args.get(key).and_then(|v| v.as_f64()).map(|x| x as f32)
}

fn need_num(args: &Value, key: &str) -> Result<f32, ToolError> {
    num(args, key).ok_or_else(|| ToolError::invalid(format!("{key} must be a number")))
}

/// Clamp a wanted top-left so the title band stays reachable (MIN_VIS px
/// of window on every side that could leave the viewport).
fn clamp_pos(app: &App, idx: usize, x: f32, y: f32) -> [f32; 2] {
    let r = win_rect(app, idx);
    let f = float_footprint(app, idx);
    // Only the WIDTH matters for the top-left clamp: the title band is the
    // reachability target (its height is fixed chrome, not window state).
    let w = if r[2] > 8.0 { r[2] } else { f[2] };
    let vp = app.vp_rect;
    let nx = x.clamp(vp[0] + MIN_VIS - w, vp[0] + vp[2] - MIN_VIS);
    let ny = y.clamp(vp[1] + MIN_VIS, vp[1] + vp[3] - MIN_VIS);
    [nx, ny]
}

fn set_float_rect(app: &mut App, idx: usize, r: [f32; 4]) {
    if idx < 4 {
        let f = match idx {
            0 => &mut app.side_wins.rack,
            1 => &mut app.side_wins.binds,
            2 => &mut app.side_wins.monitor,
            _ => &mut app.side_wins.mixer,
        };
        *f = r;
    } else {
        app.dyn_wins[idx - 4].rect = r;
    }
}

/// Mutations persist exactly like the mouse paths: dirty + save (the dock
/// helpers' own pair), and echo through the status line (v72).
fn commit(app: &mut App, status: impl Into<String>) {
    app.side_dirty = true;
    app.status = status.into();
    app.save_config();
}

// ── tools ────────────────────────────────────────────────────────────────

const LIST_DESC: &str = "List every window the agent can address: `windows` = \
                         the dock/floating roster (4 primaries + every dynamic \
                         window opened this session) with id, kind, title, open, \
                         docked, edge_name (left/right/bottom/top), rank along the \
                         edge, rect [x,y,w,h] (live dock rect when docked, float \
                         rect otherwise; null only for a window docked before its \
                         first drawn frame) and float_rect. `editors` = host-side \
                         native plugin editor windows (WM-managed; list/close \
                         only — see windows.move). Read-only: touches nothing.";
fn list(app: &mut App, _args: &Value) -> ToolResult {
    let eds: Vec<Value> = editor_ids(app)
        .into_iter()
        .map(|(id, k, synth)| {
            let w = if synth {
                app.vst_editors.get(&match k {
                    DynKind::Vst(t) => t,
                    _ => unreachable!(),
                })
            } else {
                let (t, r) = match k {
                    DynKind::VstFx(t, r) => (t, r),
                    _ => unreachable!(),
                };
                app.fx_vst_editors.get(&(t, r))
            };
            match w {
                Some(w) => editor_entry(id, w),
                None => Value::Null,
            }
        })
        .filter(|v| !v.is_null())
        .collect();
    Ok(json!({"windows": list_entries(app), "editors": eds}))
}

const OPEN_DESC: &str = "Open a window by id. Primaries (rack/binds/monitor/\
                         mixer) and drums/master/chain:<track>/synth:<track>/\
                         synths:<track> always; roll with optional {asset} (MIDI \
                         asset id — absent = fresh pattern); auto with optional \
                         {asset} (automation asset); params ONLY by target id \
                         (params:synth:<track> or params:chain:<track>:<row>, \
                         needs a live plugin instance); fx:<track>:<row> needs a \
                         script FX on that row. Native plugin EDITOR windows \
                         (vst:*, vstfx:*) are NOT opened here (crash-guarded GUI \
                         action) — an already-open editor id returns its state. \
                         Ack echoes the window through the windows.list getter.";
fn open(app: &mut App, args: &Value) -> ToolResult {
    let id = need_id(args)?;
    if let Some(p) = PrimKind::from_key(id) {
        set_visible(app, p.idx(), true);
        commit(app, format!("window \"{}\" opened", win_title(app, p.idx())));
        return Ok(json!({"window": entry(app, p.idx()), "status": app.status}));
    }
    let kind = DynKind::from_key(id).ok_or_else(|| {
        ToolError::not_found(format!("unknown window id \"{id}\" — live ids: {}", win_id_hints(app)))
    })?;
    // Track-indexed kinds validate BEFORE touching state.
    let track_of = |k: DynKind| -> Option<usize> {
        match k {
            DynKind::Chain(t) | DynKind::Synth(t) | DynKind::Synths(t) | DynKind::Vst(t) => Some(t),
            DynKind::Fx(t, _) | DynKind::VstFx(t, _) => Some(t),
            _ => None,
        }
    };
    if let Some(t) = track_of(kind) {
        if t >= app.n_tracks {
            return Err(ToolError::out_of_range(format!(
                "track {t} out of range (0..{} for {id})",
                app.n_tracks
            )));
        }
    }
    match kind {
        DynKind::Params => {
            if id.starts_with("params:") && params_target(id).is_none() {
                return Err(ToolError::invalid(
                    "bad params id syntax — use params:synth:<track> or \
                     params:chain:<track>:<row> (bare \"params\" re-shows the \
                     current one)",
                ));
            }
            let Some(target) = params_target(id) else {
                if win_slot(app, "params").is_some_and(|i| win_visible(app, i)) {
                    return Ok(json!({"window": entry(app, win_slot(app, "params").unwrap()), "status": app.status}));
                }
                return Err(ToolError::invalid(
                    "params opens ON a plugin: use id params:synth:<track> or \
                     params:chain:<track>:<row> (live plugins: chain/asset tools)",
                ));
            };
            let Some(entry_arc) = app.target_entry(target) else {
                return Err(ToolError::not_found(format!(
                    "no plugin instance at {id} — open one first (chain tools / binding)"
                )));
            };
            app.params_win = Some(target);
            app.params_filter.clear();
            commit(app, format!("{}: parameter window opened", entry_arc.name()));
        }
        DynKind::Drums => {
            app.show_drums = true;
            commit(app, "DRUM MACHINE opened");
        }
        DynKind::Master => {
            app.show_master = true;
            commit(app, "MASTER OUT opened");
        }
        DynKind::Chain(t) => {
            app.fx_window = Some(t);
            commit(app, format!("FX chain opened for track {}", t + 1));
        }
        DynKind::Synth(t) => {
            app.synth_win = Some(t);
            commit(app, format!("synth editor opened for track {}", t + 1));
        }
        DynKind::Synths(t) => {
            app.synth_pick = Some(t);
            commit(app, format!("synth browser opened for track {}", t + 1));
        }
        DynKind::Roll => {
            let asset = match args.get("asset") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let a = v.as_u64().ok_or_else(|| {
                        ToolError::invalid("asset must be an integer asset id (assets.list)")
                    })? as usize;
                    if app.assets.get(a).is_none() {
                        return Err(ToolError::not_found(format!("no asset {a} — see assets.list")));
                    }
                    Some(a as u32)
                }
            };
            app.roll_open_or_retarget(asset);
            // (the GUI seam set its own status echo)
            app.side_dirty = true;
            app.save_config();
        }
        DynKind::Auto => {
            let asset = match args.get("asset") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let a = v.as_u64().ok_or_else(|| {
                        ToolError::invalid("asset must be an integer asset id (assets.list)")
                    })? as usize;
                    match app.assets.get(a) {
                        None => {
                            return Err(ToolError::not_found(format!("no asset {a} — see assets.list")))
                        }
                        Some(x) if x.automation.is_none() => {
                            return Err(ToolError::invalid(format!(
                                "asset {a} is not an automation asset"
                            )))
                        }
                        Some(_) => Some(a),
                    }
                }
            };
            app.open_auto_editor(asset);
            app.side_dirty = true;
            app.save_config();
        }
        DynKind::Fx(t, r) => {
            let is_script = matches!(
                app.fx_rows.get(t).and_then(|rows| rows.get(r)),
                Some(ChainRow::Script { .. })
            );
            let live = app.fx_live.get(t).and_then(|rows| rows.get(r)).is_some_and(|x| x.is_some());
            if !is_script || !live {
                return Err(ToolError::not_found(format!(
                    "no live script FX at track {} row {} — fx rows are loaded by the chain tools",
                    t + 1,
                    r + 1
                )));
            }
            app.open_fx_editor(t, r);
            commit(
                app,
                format!("FX editor opened — track {} row {}", t + 1, r + 1),
            );
        }
        DynKind::Vst(t) => return open_editor_guard(app, id, DynKind::Vst(t)),
        DynKind::VstFx(t, r) => return open_editor_guard(app, id, DynKind::VstFx(t, r)),
    }
    let echo = match win_slot(app, id) {
        Some(idx) => entry(app, idx),
        None => json!({
            "id": id,
            "open": true,
            "registered": false,
            "note": "geometry lands after the next drawn frame — windows.list again",
        }),
    };
    Ok(json!({"window": echo, "status": app.status}))
}

/// windows.open never SPAWNS a plugin editor (opening is the crash-guarded
/// GUI action — MCP-05 territory); an already-live editor id just echoes
/// its state (idempotent discovery), anything else is the documented
/// boundary refusal.
fn open_editor_guard(app: &App, id: &str, kind: DynKind) -> ToolResult {
    if let Some(idx) = win_slot(app, id) {
        if win_visible(app, idx) {
            return Ok(json!({"window": entry(app, idx), "status": app.status}));
        }
    }
    let e = match kind {
        DynKind::Vst(t) => app.vst_editors.get(&t).map(|w| editor_entry(format!("vst:{t}"), w)),
        DynKind::VstFx(t, r) => app
            .fx_vst_editors
            .get(&(t, r))
            .map(|w| editor_entry(format!("vstfx:{t}:{r}"), w)),
        _ => None,
    };
    if let Some(mut e) = e {
        if e["open"].as_bool() == Some(true) {
            e["note"] = json!("editor already open; windows.open never spawns editors");
            return Ok(json!({"editor": e, "status": app.status}));
        }
    }
    Err(ToolError::wrong_kind(format!(
        "windows.open does not spawn native plugin editor windows (id \"{id}\") — \
         opening is the crash-guarded GUI action; live editors appear in \
         windows.editors and windows.close retires them"
    )))
}

/// Target carried by an extended params id (`None` for the bare key).
fn params_target(id: &str) -> Option<GuiTarget> {
    let rest = id.strip_prefix("params:")?;
    let mut it = rest.splitn(2, ':');
    match (it.next()?, it.next()) {
        ("synth", Some(t)) => Some(GuiTarget::Synth(t.parse().ok()?)),
        ("chain", Some(tr)) => {
            let mut p = tr.splitn(2, ':');
            let (t, r) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?);
            Some(GuiTarget::Chain(t, r))
        }
        _ => None,
    }
}

const CLOSE_DESC: &str = "Close a window by id (roster ids AND native plugin \
                          editor ids vst:<track> / vstfx:<track>:<row>, which \
                          close through the same in-place protocol the editor's \
                          X button uses — state chunks are harvested). Closes \
                          are idempotent for the agent: closing a closed window \
                          is a structured not_found, never a side effect. Ack \
                          echoes the closed window's state.";
fn close(app: &mut App, args: &Value) -> ToolResult {
    let id = need_id(args)?;
    if let Some(p) = PrimKind::from_key(id) {
        if !win_visible(app, p.idx()) {
            return Err(ToolError::not_found(format!("window \"{id}\" is not open")));
        }
        set_visible(app, p.idx(), false);
        commit(app, format!("window \"{id}\" closed"));
        return Ok(json!({"window": entry(app, p.idx()), "status": app.status}));
    }
    let Some(kind) = DynKind::from_key(id) else {
        return Err(ToolError::not_found(format!(
            "unknown window id \"{id}\" — live ids: {}",
            win_id_hints(app)
        )));
    };
    // Host-side editors without roster chrome (the Linux reality): close
    // through the SHARED toggle-close seam so the reaper protocol is exact.
    match kind {
        DynKind::Vst(t)
            if !app.dyn_wins.iter().any(|w| w.kind == DynKind::Vst(t)) =>
        {
            if let Some(ident) = app.close_live_synth_editor(t) {
                return Ok(json!({
                    "editor": {"id": format!("vst:{t}"), "kind": "editor", "title": ident, "open": false},
                    "status": app.status,
                }));
            }
            return Err(ToolError::not_found(format!("no live plugin editor at vst:{t}")));
        }
        DynKind::VstFx(t, r)
            if !app.dyn_wins.iter().any(|w| w.kind == DynKind::VstFx(t, r)) =>
        {
            if let Some(ident) = app.close_live_fx_vst_editor(t, r) {
                return Ok(json!({
                    "editor": {"id": format!("vstfx:{t}:{r}"), "kind": "editor", "title": ident, "open": false},
                    "status": app.status,
                }));
            }
            return Err(ToolError::not_found(format!(
                "no live plugin editor at vstfx:{t}:{r}"
            )));
        }
        _ => {}
    }
    let Some(idx) = win_slot(app, id) else {
        return Err(ToolError::not_found(format!(
            "window \"{id}\" never opened this session (windows.open first)"
        )));
    };
    if !win_visible(app, idx) {
        return Err(ToolError::not_found(format!("window \"{id}\" is not open")));
    }
    dyn_set(app, kind, false);
    commit(app, format!("window \"{}\" closed", win_title(app, idx)));
    Ok(json!({"window": entry(app, idx), "status": app.status}))
}

const MOVE_DESC: &str = "Move a FLOATING window's top-left (viewport \
                         coordinates, screen points). The position is clamped \
                         so the title band stays reachable. Docked windows \
                         refuse (wrong_kind) — docked geometry belongs to the \
                         dock; call windows.undock first. Native editor \
                         windows (windows.editors) refuse: they are \
                         WM-managed top-levels (v1 boundary). Ack echoes the \
                         post-change rect.";
fn wmove(app: &mut App, args: &Value) -> ToolResult {
    let id = need_id(args)?;
    if editor_kind(id).is_some() {
        return Err(ToolError::wrong_kind(
            "native plugin editor windows are WM-managed top-levels — \
             windows.move does not apply (v1 boundary; windows.close works)",
        ));
    }
    let idx = resolve(app, id)?;
    // Argument parsing BEFORE state checks: a missing coordinate is a
    // parameter bug regardless of where the window sits.
    let x = need_num(args, "x")?;
    let y = need_num(args, "y")?;
    if is_docked(app, idx) {
        return Err(ToolError::wrong_kind(format!(
            "window \"{id}\" is docked — docked geometry is owned by the dock; \
             windows.undock first"
        )));
    }
    let r = win_rect(app, idx);
    let f = float_footprint(app, idx);
    let (w, h) = if r[2] > 8.0 { (r[2], r[3]) } else { (f[2], f[3]) };
    let [nx, ny] = clamp_pos(app, idx, x, y);
    set_float_rect(app, idx, [nx, ny, w, h]);
    commit(
        app,
        format!("window \"{id}\" moved to ({}, {})", nx.round(), ny.round()),
    );
    Ok(json!({"window": entry(app, idx), "status": app.status}))
}

const RESIZE_DESC: &str = "Resize a FLOATING window (w, h in screen points; \
                           clamped to >=120x80 and <= the viewport). Docked \
                           windows refuse (wrong_kind — dock thickness is \
                           drag-only; undock first). Native editor windows \
                           refuse (WM-managed; v1 boundary). Ack echoes the \
                           post-change rect.";
fn wresize(app: &mut App, args: &Value) -> ToolResult {
    let id = need_id(args)?;
    if editor_kind(id).is_some() {
        return Err(ToolError::wrong_kind(
            "native plugin editor windows are WM-managed top-levels — \
             windows.resize does not apply (v1 boundary; windows.close works)",
        ));
    }
    let idx = resolve(app, id)?;
    // Argument parsing BEFORE state checks (see windows.move).
    let w = need_num(args, "w")?.clamp(MIN_W, app.vp_rect[2]);
    let h = need_num(args, "h")?.clamp(MIN_H, app.vp_rect[3]);
    if is_docked(app, idx) {
        return Err(ToolError::wrong_kind(format!(
            "window \"{id}\" is docked — docked thickness is set by dock drags; \
             windows.undock first"
        )));
    }
    let r = win_rect(app, idx);
    let f = float_footprint(app, idx);
    let (x, y) = if r[2] > 8.0 { (r[0], r[1]) } else { (f[0], f[1]) };
    set_float_rect(app, idx, [x, y, w, h]);
    commit(
        app,
        format!("window \"{id}\" resized to {}x{}", w.round(), h.round()),
    );
    Ok(json!({"window": entry(app, idx), "status": app.status}))
}

fn edge_of(v: &Value) -> Option<i16> {
    if let Some(n) = v.as_i64() {
        return (0..=3).contains(&n).then_some(n as i16);
    }
    Some(match v.as_str()?.to_ascii_lowercase().as_str() {
        "left" => 0,
        "right" => 1,
        "bottom" => 2,
        "top" => 3,
        _ => return None,
    })
}

const DOCK_DESC: &str = "Dock a window onto an edge (edge: left|right|bottom|\
                         top or 0..3; rank: insertion slot along the edge, \
                         0-based, default = tail; both echoed post-change). \
         Ranks re-pack deterministically among the edge's tab groups, exactly \
         like a mouse drop. Already-docked ids re-rank/re-edge. Native editor \
         windows refuse (WM-managed; v1 boundary).";
fn wdock(app: &mut App, args: &Value) -> ToolResult {
    let id = need_id(args)?;
    if editor_kind(id).is_some() {
        return Err(ToolError::wrong_kind(
            "native plugin editor windows are WM-managed top-levels — \
             windows.dock does not apply (v1 boundary; windows.close works)",
        ));
    }
    let idx = resolve(app, id)?;
    let edge = args
        .get("edge")
        .and_then(edge_of)
        .ok_or_else(|| ToolError::out_of_range("edge must be left|right|bottom|top (or 0..3)"))?;
    let rank = match args.get("rank") {
        None | Some(Value::Null) => i16::MAX,
        Some(v) => {
            let r = v.as_i64().ok_or_else(|| ToolError::invalid("rank must be an integer >= 0"))?;
            if r < 0 {
                return Err(ToolError::out_of_range("rank must be >= 0"));
            }
            r.min(i16::MAX as i64) as i16
        }
    };
    // Rank-addressed dock: the EXACT writes `dock_at` lands on drop
    // (groups preserved, edge re-packed to contiguous ranks, collapse +
    // stale dock-rect reset, along-axis fresh on float→dock) — but
    // addressed by RANK instead of a drop point, because pre-first-draw
    // frames have no live centers and the mouse's center math would
    // collapse every rank to 0 there. Same terminal state, deterministic.
    dock_pack(app, idx, edge, rank as usize);
    commit(
        app,
        format!(
            "window \"{id}\" docked {} rank {}",
            EDGE_NAMES[edge as usize],
            dock_order(app, idx)
        ),
    );
    Ok(json!({"window": entry(app, idx), "status": app.status}))
}

/// The `dock_at` terminal writes with a direct insertion rank.
fn dock_pack(app: &mut App, idx: usize, edge: i16, want: usize) {
    let was_free = !is_docked(app, idx);
    let n = win_count(app);
    let mut mem: Vec<usize> = (0..n)
        .filter(|i| *i != idx && is_docked(app, *i) && dock_edge(app, *i) == edge)
        .collect();
    mem.sort_by_key(|i| (dock_order(app, *i), *i));
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in mem {
        let o = dock_order(app, i);
        match groups.last_mut() {
            Some(v) if dock_order(app, v[0]) == o => v.push(i),
            _ => groups.push(vec![i]),
        }
    }
    let rank = want.min(groups.len());
    groups.insert(rank, vec![idx]);
    for (r, g) in groups.iter().enumerate() {
        for &i in g {
            if i < 4 {
                app.side_wins.dock[i] = [1, edge, r as i16];
            } else {
                app.dyn_wins[i - 4].dock = [1, edge, r as i16];
            }
        }
    }
    app.dock_collapsed[idx] = false;
    app.dock_rects[idx] = [0.0; 4];
    if was_free {
        reset_dock_along(app, idx, edge);
    }
    app.side_dirty = true;
}

const UNDOCK_DESC: &str = "Float a docked window (optional {x,y} top-left, \
                           clamped so the title band stays reachable; default \
                           = its dock rect, else the stock footprint). \
                           Already-floating ids echo unchanged (idempotent). \
                           Native editor windows refuse (WM-managed; v1 \
                           boundary). Ack echoes the post-change rect.";
fn wundock(app: &mut App, args: &Value) -> ToolResult {
    let id = need_id(args)?;
    if editor_kind(id).is_some() {
        return Err(ToolError::wrong_kind(
            "native plugin editor windows are WM-managed top-levels — \
             windows.undock does not apply (v1 boundary; windows.close works)",
        ));
    }
    let idx = resolve(app, id)?;
    if !is_docked(app, idx) {
        return Ok(json!({"window": entry(app, idx), "status": app.status}));
    }
    let at = match (num(args, "x"), num(args, "y")) {
        (Some(x), Some(y)) => clamp_pos(app, idx, x, y),
        _ => {
            let r = app.dock_rects.get(idx).copied().unwrap_or([0.0; 4]);
            if r[2] > 8.0 {
                [r[0], r[1]]
            } else {
                let f = float_footprint(app, idx);
                [f[0], f[1]]
            }
        }
    };
    undock_to(app, idx, at);
    app.dock_just_floated = Some(idx); // the mouse hand-off, unchanged
    commit(app, format!("window \"{id}\" floated"));
    Ok(json!({"window": entry(app, idx), "status": app.status}))
}

const LAYOUT_SAVE_DESC: &str = "Capture the ENTIRE arrangement (every float \
                                rect, dock edge/rank, tab group and open flag) \
                                into a named layout template, persisted with the \
                                config. Re-capturing a name replaces it. The \
                                stock arrangement auto-persists across restarts \
                                on its own; templates are for variants. Named \
                                layouts also appear in the Windows menu.";
fn layout_save(app: &mut App, args: &Value) -> ToolResult {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("layout save needs a non-empty string \"name\""))?;
    capture_win_layout(app, name.to_string());
    let names: Vec<&str> = app.win_layouts.iter().map(|l| l.name.as_str()).collect();
    Ok(json!({"layout": name, "layouts": names, "status": app.status}))
}

const LAYOUT_RESTORE_DESC: &str = "Apply a saved layout template by name — \
                                   the same machinery the Windows menu uses: \
                                   all rects, dock slots, tab groups and \
                                   visibility (windows that need a live target, \
                                   e.g. params, keep their geometry but reopen \
                                   through their own tool). Unknown names list \
                                   the saved layouts. Ack echoes the roster.";
fn layout_restore(app: &mut App, args: &Value) -> ToolResult {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid("layout restore needs a non-empty string \"name\""))?;
    if !app.win_layouts.iter().any(|l| l.name == name) {
        let names: Vec<&str> = app.win_layouts.iter().map(|l| l.name.as_str()).collect();
        return Err(ToolError::not_found(format!(
            "no layout \"{name}\" — saved layouts: [{}]",
            names.join(", ")
        )));
    }
    apply_win_layout(app, name);
    Ok(json!({
        "layout": name,
        "windows": list_entries(app),
        "editors": [],
        "status": app.status,
    }))
}

// ── v111 quick-tier pins ─────────────────────────────────────────────────
//
// Contract (docs/MCP-DESIGN.md D6): every pin drives `rpc::dispatch` — the
// real dispatcher the frame loop drains — never the tool fns, so gate order,
// taxonomy, the read-only latch and observed-then-ack are exercised exactly
// as production reaches them. `tick` = one drawn frame (registers windows,
// measures dock rects) so geometry asserts read the same values the mouse
// user sees.

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static TICK_T: std::cell::Cell<f64> = const { std::cell::Cell::new(1.0) };
    }

    /// One headless frame at the standard 1440×853 viewport (mirrors the
    /// ui::tests `frame` harness; no input events — registry and geometry
    /// only).
    pub(crate) fn tick(app: &mut App, ctx: &egui::Context) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            time: Some(TICK_T.with(|c| {
                let t = c.get();
                c.set(t + 1.0 / 30.0);
                t
            })),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
    }

    fn resp(app: &mut App, name: &str, args: Value) -> Value {
        let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                         "params":{"name":name,"arguments":args}});
        crate::mcp::rpc::dispatch(&req, app).expect("response to a request")
    }

    fn sc(app: &mut App, name: &str, args: Value) -> Value {
        let r = resp(app, name, args);
        assert_ne!(
            r["result"]["isError"].as_bool(),
            Some(true),
            "pin expects an OK result from {name}: {r}"
        );
        r["result"]["structuredContent"].clone()
    }

    fn wins(app: &mut App) -> Vec<Value> {
        sc(app, "windows.list", json!({}))["windows"]
            .as_array()
            .cloned()
            .unwrap()
    }

    fn find<'a>(list: &'a [Value], id: &str) -> &'a Value {
        list.iter()
            .find(|w| w["id"] == id)
            .unwrap_or_else(|| panic!("no window {id} in {list:?}"))
    }

    fn err(app: &mut App, name: &str, args: Value) -> (String, String) {
        let r = resp(app, name, args);
        let e = &r["result"];
        (
            e["structuredContent"]["error"].as_str().unwrap_or("").to_string(),
            e["content"][0]["text"].as_str().unwrap_or("").to_string(),
        )
    }

    // ── R1/R2: registry + ids ─────────────────────────────────────────────

    #[test]
    fn v111_mcp_windows_list_registry() {
        let mut app = headless_app("v111reg");
        let ctx = egui::Context::default();
        // Stock roster: the four primaries, stable keys, stock dock slots
        // (default_dock = rack left/0, binds bottom/1, monitor bottom/0,
        // mixer right/0) — no dynamic window has registered yet.
        let l = wins(&mut app);
        let ids: Vec<&str> = l.iter().map(|w| w["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["rack", "binds", "monitor", "mixer"], "primary ids are PrimKind keys");
        for w in &l {
            for k in ["id", "kind", "title", "open", "docked", "edge", "edge_name", "rank", "rect", "float_rect"] {
                assert!(
                    !w[k].is_null() || k == "rect" || k == "float_rect",
                    "{}: field {k} missing",
                    w["id"]
                );
            }
            assert_eq!(w["kind"], "primary");
        }
        assert_eq!(find(&l, "rack")["title"], "ASSET RACK");
        assert_eq!(find(&l, "rack")["edge"], 0);
        assert_eq!(find(&l, "rack")["rank"], 0);
        assert_eq!(find(&l, "binds")["edge"], 2);
        assert_eq!(find(&l, "binds")["rank"], 1);
        assert_eq!(find(&l, "mixer")["edge"], 1);
        // open flags mirror the app's own visibility getters (one truth).
        for w in &l {
            let id = w["id"].as_str().unwrap();
            assert_eq!(w["open"].as_bool(), Some(win_visible(&app, win_slot(&app, id).unwrap())));
        }
        // A dynamic kind joins the roster once open + drawn.
        let ack = sc(&mut app, "windows.open", json!({"id": "drums"}));
        assert_eq!(ack["window"]["id"], "drums");
        assert_eq!(ack["window"]["open"], true);
        tick(&mut app, &ctx);
        let l = wins(&mut app);
        let d = find(&l, "drums");
        assert_eq!(d["kind"], "dynamic");
        assert_eq!(d["open"], true);
        assert!(d["rect"].is_array(), "drawn float carries measured geometry: {d}");
        assert_eq!(d["docked"], false, "fresh dyn windows float (dyn default [0,1,0])");
    }

    // ── R3: params key extension, BOTH directions ─────────────────────────

    #[test]
    fn v111_mcp_windows_params_key_both_ways() {
        let mut app = headless_app("v111pk");
        // Config back-compat: the unit kind still parses from its bare key.
        assert_eq!(DynKind::from_key("params"), Some(DynKind::Params));
        assert_eq!(DynKind::from_key("params:chain:2:1"), Some(DynKind::Params));
        assert_eq!(DynKind::from_key("params:synth:9"), Some(DynKind::Params));
        // Registry direction: a live params window REPORTS its extended id…
        app.params_win = Some(GuiTarget::Chain(2, 1));
        let idx = app.dyn_reg(DynKind::Params, "Test — parameters".into());
        assert_eq!(win_id(&app, idx), "params:chain:2:1");
        // …and that id RESOLVES back to the same slot; so does the bare key.
        assert_eq!(win_slot(&app, "params:chain:2:1"), Some(idx));
        assert_eq!(win_slot(&app, "params"), Some(idx));
        app.params_win = Some(GuiTarget::Synth(1));
        assert_eq!(win_id(&app, idx), "params:synth:1");
        // Tool level: the close resolves through the bare key…
        let ack = sc(&mut app, "windows.close", json!({"id": "params"}));
        assert_eq!(ack["window"]["id"], "params");
        assert_eq!(app.params_win, None, "closed through bare id");
        // …and through the extended id.
        app.params_win = Some(GuiTarget::Synth(0));
        let ack = sc(&mut app, "windows.close", json!({"id": "params:synth:0"}));
        assert_eq!(ack["window"]["open"], false);
        assert_eq!(app.params_win, None, "closed through extended id");
        // An open with a target that has no live plugin is honest failure.
        let (kind, msg) = err(&mut app, "windows.open", json!({"id": "params:chain:0:0"}));
        assert_eq!(kind, "not_found", "{msg}");
    }

    // ── R2 + errors: bad ids, bad edges ───────────────────────────────────

    #[test]
    fn v111_mcp_windows_bad_id_and_edge() {
        let mut app = headless_app("v111bad");
        let (kind, msg) = err(&mut app, "windows.move", json!({"id": "nope", "x": 10.0, "y": 10.0}));
        assert_eq!(kind, "not_found");
        assert!(msg.contains("rack") && msg.contains("mixer"), "refusal lists live ids: {msg}");
        let (kind, _) = err(&mut app, "windows.dock", json!({"id": "rack", "edge": "sideways"}));
        assert_eq!(kind, "out_of_range");
        let (kind, _) = err(&mut app, "windows.dock", json!({"id": "rack", "edge": 7}));
        assert_eq!(kind, "out_of_range");
        let (kind, _) = err(&mut app, "windows.move", json!({"id": "rack", "x": 10.0}));
        assert_eq!(kind, "invalid_params");
    }

    // ── R4: open/close round-trip ─────────────────────────────────────────

    #[test]
    fn v111_mcp_windows_open_close_roundtrip() {
        let mut app = headless_app("v111oc");
        let ctx = egui::Context::default();
        for (id, check) in [("drums", "show_drums"), ("master", "show_master")] {
            let ack = sc(&mut app, "windows.open", json!({"id": id}));
            assert_eq!(ack["window"]["open"], true, "{id} open-ack");
            tick(&mut app, &ctx);
            let l = wins(&mut app);
            assert_eq!(find(&l, id)["open"], true, "{id} observed open");
            let _ = check;
        }
        assert!(app.show_drums && app.show_master);
        // close round-trip + idempotence refusal
        let ack = sc(&mut app, "windows.close", json!({"id": "drums"}));
        assert_eq!(ack["window"]["open"], false, "ack echoes the CLOSED state");
        assert!(!app.show_drums);
        let (kind, msg) = err(&mut app, "windows.close", json!({"id": "drums"}));
        assert_eq!(kind, "not_found", "second close is a clean refusal: {msg}");
        // track-indexed kinds validate bounds BEFORE touching state
        let ack = sc(&mut app, "windows.open", json!({"id": "chain:0"}));
        assert_eq!(ack["window"]["open"], true);
        assert_eq!(app.fx_window, Some(0));
        let (kind, msg) = err(&mut app, "windows.open", json!({"id": "chain:99"}));
        assert_eq!(kind, "out_of_range", "{msg}");
        // echo carries the status echo (v72: every MCP write is visible)
        assert!(app.status.contains("chain"), "status echo: {}", app.status);
    }

    // ── R5: move + viewport clamping ──────────────────────────────────────

    #[test]
    fn v111_mcp_windows_move_clamps() {
        let mut app = headless_app("v111mv");
        let _ctx = egui::Context::default();
        sc(&mut app, "windows.undock", json!({"id": "rack"}));
        // Far corner: clamped so >=60 px of the title band stay in-view —
        // the top-left may reach viewport edge minus the margin (the
        // window body may hang off; the header stays grabbable).
        let ack = sc(&mut app, "windows.move", json!({"id": "rack", "x": 99999.0, "y": 99999.0}));
        let r = ack["window"]["rect"].as_array().unwrap().clone();
        assert_eq!((r[0].as_f64().unwrap(), r[1].as_f64().unwrap()), (1440.0 - 60.0, 853.0 - 60.0));
        // Far top-left: title band cannot leave the top-left either.
        let ack = sc(&mut app, "windows.move", json!({"id": "rack", "x": -99999.0, "y": -99999.0}));
        let r = ack["window"]["rect"].as_array().unwrap().clone();
        assert_eq!((r[0].as_f64().unwrap(), r[1].as_f64().unwrap()), (60.0 - 340.0, 60.0));
        // A normal move lands exactly.
        let ack = sc(&mut app, "windows.move", json!({"id": "rack", "x": 300.0, "y": 200.0}));
        let r = ack["window"]["rect"].as_array().unwrap().clone();
        assert_eq!((r[0].as_f64(), r[1].as_f64()), (Some(300.0), Some(200.0)));
        assert!(app.status.contains("moved"));
    }

    #[test]
    fn v111_mcp_windows_docked_move_resize_refused() {
        let mut app = headless_app("v111dm");
        // rack/mixer are docked per the stock layout.
        let (kind, msg) = err(&mut app, "windows.move", json!({"id": "rack", "x": 10.0, "y": 10.0}));
        assert_eq!(kind, "wrong_kind");
        assert!(msg.contains("undock"), "refusal names the way out: {msg}");
        let (kind, _) = err(&mut app, "windows.resize", json!({"id": "mixer", "w": 300.0, "h": 300.0}));
        assert_eq!(kind, "wrong_kind");
        // undock -> resize -> re-dock: the full mouse-free path works.
        let ack = sc(&mut app, "windows.undock", json!({"id": "mixer"}));
        assert_eq!(ack["window"]["docked"], false);
        let ack = sc(&mut app, "windows.resize", json!({"id": "mixer", "w": 10.0, "h": 10.0}));
        let r = ack["window"]["rect"].as_array().unwrap().clone();
        assert_eq!((r[2].as_f64().unwrap(), r[3].as_f64().unwrap()), (120.0, 80.0), "min clamp");
        let ack = sc(&mut app, "windows.resize", json!({"id": "mixer", "w": 99999.0, "h": 99999.0}));
        let r = ack["window"]["rect"].as_array().unwrap().clone();
        assert_eq!((r[2].as_f64().unwrap(), r[3].as_f64().unwrap()), (1440.0, 853.0), "viewport clamp");
        let ack = sc(&mut app, "windows.dock", json!({"id": "mixer", "edge": "left", "rank": 0}));
        assert_eq!(ack["window"]["docked"], true);
        assert_eq!(ack["window"]["edge"], 0);
        assert_eq!(ack["window"]["rank"], 0);
    }

    // ── R7/R8: dock ranks + undock round-trip ─────────────────────────────

    #[test]
    fn v111_mcp_windows_dock_rank() {
        let mut app = headless_app("v111rk");
        // Stock: rack left/0. Pull monitor onto left rank 0 — rack shifts.
        let ack = sc(&mut app, "windows.dock", json!({"id": "monitor", "edge": "left", "rank": 0}));
        assert_eq!(ack["window"]["rank"], 0);
        let l = wins(&mut app);
        assert_eq!(find(&l, "rack")["rank"], 1, "existing occupant re-packed after the insert");
        assert_eq!(find(&l, "rack")["edge"], 0);
        // Tail insert (no rank) goes after every group; ranks stay contiguous.
        let ack = sc(&mut app, "windows.dock", json!({"id": "mixer", "edge": "left"}));
        assert_eq!(ack["window"]["rank"], 2);
        // Over-large rank clamps to the tail deterministically.
        let ack = sc(&mut app, "windows.dock", json!({"id": "binds", "edge": 0, "rank": 999}));
        assert_eq!(ack["window"]["rank"], 3);
        let ranks: Vec<i64> = wins(&mut app)
            .iter()
            .filter(|w| w["edge"] == 0)
            .map(|w| w["rank"].as_i64().unwrap())
            .collect();
        assert_eq!(ranks.len(), 4);
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2, 3], "edge ranks are contiguous 0..n");
    }

    #[test]
    fn v111_mcp_windows_undock_roundtrip() {
        let mut app = headless_app("v111ud");
        let ack = sc(&mut app, "windows.undock", json!({"id": "rack"}));
        assert_eq!(ack["window"]["docked"], false);
        let rect0 = ack["window"]["rect"].clone();
        assert!(rect0.is_array(), "float carries geometry: {ack}");
        // Idempotence: already-floating echoes unchanged, no error.
        let ack2 = sc(&mut app, "windows.undock", json!({"id": "rack"}));
        assert_eq!(ack2["window"]["docked"], false);
        assert_eq!(ack2["window"]["rect"], rect0);
        // Optional position lands (and clamps).
        let ack = sc(&mut app, "windows.dock", json!({"id": "rack", "edge": "right"}));
        assert_eq!(ack["window"]["docked"], true);
        let ack = sc(&mut app, "windows.undock", json!({"id": "rack", "x": 500.0, "y": 120.0}));
        let r = ack["window"]["rect"].as_array().unwrap().clone();
        assert_eq!((r[0].as_f64(), r[1].as_f64()), (Some(500.0), Some(120.0)));
        assert!(app.status.contains("floated"));
    }

    // ── R9: layout save/restore + restart reproduction ────────────────────

    #[test]
    fn v111_mcp_windows_layout_save_restore() {
        let mut app = headless_app("v111lay");
        let ctx = egui::Context::default();
        // A non-trivial arrangement: drums floated at an exact spot/size,
        // rack docked bottom/0, mixer floated.
        sc(&mut app, "windows.open", json!({"id": "drums"}));
        tick(&mut app, &ctx);
        sc(&mut app, "windows.move", json!({"id": "drums", "x": 100.0, "y": 90.0}));
        sc(&mut app, "windows.resize", json!({"id": "drums", "w": 200.0, "h": 150.0}));
        sc(&mut app, "windows.dock", json!({"id": "rack", "edge": "bottom", "rank": 0}));
        sc(&mut app, "windows.undock", json!({"id": "mixer"}));
        let saved = sc(&mut app, "windows.layout.save", json!({"name": "odd"}));
        assert_eq!(saved["layout"], "odd");
        assert_eq!(saved["layouts"].as_array().unwrap().len(), 1);
        // Wreck it: rack back left, mixer docked, drums moved.
        sc(&mut app, "windows.dock", json!({"id": "rack", "edge": "left"}));
        sc(&mut app, "windows.dock", json!({"id": "mixer", "edge": "right"}));
        sc(&mut app, "windows.move", json!({"id": "drums", "x": 600.0, "y": 400.0}));
        // Restore: the arrangement comes back EXACTLY as observed.
        let rest = sc(&mut app, "windows.layout.restore", json!({"name": "odd"}));
        let l = rest["windows"].as_array().unwrap().clone();
        assert_eq!(find(&l, "rack")["edge"], 2);
        assert_eq!(find(&l, "rack")["rank"], 0);
        assert_eq!(find(&l, "rack")["docked"], true);
        assert_eq!(find(&l, "mixer")["docked"], false);
        assert_eq!(find(&l, "drums")["float_rect"], json!([100.0, 90.0, 200.0, 150.0]));
        // Unknown name self-documents the saved set.
        let (kind, msg) = err(&mut app, "windows.layout.restore", json!({"name": "ghost"}));
        assert_eq!(kind, "not_found");
        assert!(msg.contains("odd"), "{msg}");
    }

    #[test]
    fn v111_mcp_windows_layout_restart() {
        let mut app = headless_app("v111rs");
        let ctx = egui::Context::default();
        sc(&mut app, "windows.open", json!({"id": "drums"}));
        tick(&mut app, &ctx);
        sc(&mut app, "windows.move", json!({"id": "drums", "x": 140.0, "y": 80.0}));
        sc(&mut app, "windows.resize", json!({"id": "drums", "w": 260.0, "h": 180.0}));
        sc(&mut app, "windows.dock", json!({"id": "monitor", "edge": "left", "rank": 0}));
        sc(&mut app, "windows.layout.save", json!({"name": "rest"}));
        drop(app);
        // Simulated restart: same config dir, a brand-new App.
        let mut app2 = App::new(crate::audio::Backend::start());
        assert!(
            app2.win_layouts.iter().any(|l| l.name == "rest"),
            "named template survives restart: {:?}",
            app2.win_layouts.iter().map(|l| &l.name).collect::<Vec<_>>()
        );
        let rest = sc(&mut app2, "windows.layout.restore", json!({"name": "rest"}));
        let l = rest["windows"].as_array().unwrap().clone();
        assert_eq!(find(&l, "monitor")["edge"], 0, "dock edge reproduced");
        assert_eq!(find(&l, "monitor")["rank"], 0, "dock rank reproduced");
        assert!(app2.show_drums, "open flags reproduced");
        let ctx2 = egui::Context::default();
        tick(&mut app2, &ctx2);
        let l = wins(&mut app2);
        assert_eq!(
            find(&l, "drums")["float_rect"],
            json!([140.0, 80.0, 260.0, 180.0]),
            "float geometry reproduced after drawing"
        );
    }

    // ── R10: native editor boundary ───────────────────────────────────────

    #[test]
    fn v111_mcp_windows_editor_boundary() {
        let mut app = headless_app("v111ed");
        // No live editors: list carries the (empty) editors array.
        let s = sc(&mut app, "windows.list", json!({}));
        assert_eq!(s["editors"].as_array().unwrap().len(), 0);
        // Geometry tools refuse editor ids with the documented boundary…
        for tool in ["windows.move", "windows.resize", "windows.dock", "windows.undock"] {
            let args = if tool == "windows.move" {
                json!({"id": "vst:0", "x": 1.0, "y": 1.0})
            } else if tool == "windows.resize" {
                json!({"id": "vst:0", "w": 1.0, "h": 1.0})
            } else if tool == "windows.dock" {
                json!({"id": "vst:0", "edge": "left"})
            } else {
                json!({"id": "vstfx:1:2"})
            };
            let (kind, msg) = err(&mut app, tool, args);
            assert_eq!(kind, "wrong_kind", "{tool}: {msg}");
            assert!(msg.contains("WM-managed") || msg.contains("editor"), "{tool}: {msg}");
        }
        // …open refuses to SPAWN one…
        let (kind, msg) = err(&mut app, "windows.open", json!({"id": "vst:0"}));
        assert_eq!(kind, "wrong_kind", "{msg}");
        // …and close is the supported lifecycle half: a clean not_found
        // while nothing is live (the seam goes through the shared GUI
        // toggle-close protocol — the same code path as the X button).
        let (kind, msg) = err(&mut app, "windows.close", json!({"id": "vst:0"}));
        assert_eq!(kind, "not_found", "{msg}");
        let (kind, _) = err(&mut app, "windows.close", json!({"id": "vstfx:1:2"}));
        assert_eq!(kind, "not_found");
    }

    // ── R2/R4: every kind's key round-trips; opens land their app field ───

    #[test]
    fn v111_mcp_windows_registry_keys_roundtrip() {
        // The complete key vocabulary of the registry (v107 D4 ids):
        for key in [
            "params",
            "params:synth:1",
            "params:chain:2:0",
            "drums",
            "master",
            "roll",
            "auto",
            "chain:3",
            "synth:2",
            "synths:1",
            "fx:1:0",
            "vst:0",
            "vstfx:1:2",
        ] {
            assert!(DynKind::from_key(key).is_some(), "registry key {key:?} unparseable");
        }
        // Track-indexed kinds REQUIRE their index (no silent track 0).
        assert_eq!(DynKind::from_key("chain"), None);
        assert_eq!(DynKind::from_key("chain:x"), None);
        // Simultaneous windows get DISTINCT ids resolving to their own
        // slots. The sandbox's DEFAULT open windows (show_master=true per
        // Config::default) are part of the honest baseline.
        let mut app = headless_app("v111keys");
        sc(&mut app, "windows.open", json!({"id": "chain:0"}));
        sc(&mut app, "windows.open", json!({"id": "synth:1"}));
        sc(&mut app, "windows.open", json!({"id": "drums"}));
        let ctx = egui::Context::default();
        tick(&mut app, &ctx);
        let l = wins(&mut app);
        let mut dyn_ids: Vec<&str> = l
            .iter()
            .filter(|w| w["kind"] == "dynamic")
            .map(|w| w["id"].as_str().unwrap())
            .collect();
        dyn_ids.sort_unstable();
        assert_eq!(dyn_ids, ["chain:0", "drums", "master"]);
        // An open that cannot DRAW (empty-track synth editor self-parks —
        // the v35 guard) never registers: the roster shows windows that
        // actually exist, never phantom entries.
        assert!(!dyn_ids.contains(&"synth:1"), "non-drawable window registered anyway");
        for id in dyn_ids {
            let idx = win_slot(&app, id).unwrap_or_else(|| panic!("{id} must resolve"));
            assert_eq!(win_id(&app, idx), id, "id round-trips through win_slot");
        }
    }

    #[test]
    fn v111_mcp_windows_open_dyn_fields() {
        let mut app = headless_app("v111dyn");
        // Every dynamic kind maps 1:1 onto the field the mouse writes.
        sc(&mut app, "windows.open", json!({"id": "chain:2"}));
        assert_eq!(app.fx_window, Some(2));
        sc(&mut app, "windows.open", json!({"id": "synth:1"}));
        assert_eq!(app.synth_win, Some(1));
        sc(&mut app, "windows.open", json!({"id": "synths:0"}));
        assert_eq!(app.synth_pick, Some(0));
        sc(&mut app, "windows.open", json!({"id": "drums"}));
        assert!(app.show_drums);
        // master is default-open (Config::default show_master=true): the
        // MCP open is an idempotent no-op that still acks.
        let ack = sc(&mut app, "windows.open", json!({"id": "master"}));
        assert_eq!(ack["window"]["open"], true, "reopen default-open window");
        assert!(app.show_master);
        // Roll and automation open through the GUI seam fns (fresh pattern
        // / fresh curve — the toolbar-button contract, not a parallel path).
        sc(&mut app, "windows.open", json!({"id": "roll"}));
        assert!(app.piano_roll.is_some(), "roll seam opened a fresh pattern");
        let ack = sc(&mut app, "windows.open", json!({"id": "auto"}));
        assert!(app.auto_edit.is_some(), "auto seam opened a fresh curve");
        assert_eq!((ack["window"]["id"].as_str(), ack["window"]["open"].as_bool()), (Some("auto"), Some(true)));
        // Bad asset references are typed, never panics.
        let (kind, msg) = err(&mut app, "windows.open", json!({"id": "auto", "asset": 99999}));
        assert_eq!(kind, "not_found", "{msg}");
        if !app.assets.is_empty() {
            let (kind, msg) = err(&mut app, "windows.open", json!({"id": "roll", "asset": 99998}));
            assert_eq!(kind, "not_found", "{msg}");
        }
    }

    // ── R12: the read-only latch covers this family ───────────────────────

    #[test]
    fn v111_mcp_windows_read_latch() {
        let mut app = headless_app("v111latch");
        app.mcp_read_only = true;
        let r = resp(&mut app, "windows.open", json!({"id": "drums"}));
        assert_eq!(r["error"]["code"], -32003, "writes refused: {r}");
        assert!(!app.show_drums, "refusal is total — nothing changed");
        let l = wins(&mut app);
        assert_eq!(l.len(), 4, "the read tool still works");
    }

    // ── R11: a REAL client over a REAL socket (slow tier) ─────────────────

    #[test]
    #[ignore = "slow-tier: real socket"]
    fn v111_mcp_windows_socket_e2e() {
        // v107 pump model: the worker parks on the oneshot after queueing,
        // so we sleep, run the frame-loop role (drain + one drawn frame),
        // then read the response. Deterministic, bounded, port 0.
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join("slooper-mcp-v111sock");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(crate::paths::DATA_DIR_ENV, &dir);
        let mut app = headless_app("v111sock");
        let ctx = egui::Context::default();
        app.mcp_port = 0;
        app.mcp_enabled = true;
        app.mcp.as_mut().unwrap().set_wait(std::time::Duration::from_millis(800));
        app.mcp_frame(&ctx);
        let (port, token) = crate::mcp::bridge::read_endpoint().expect("endpoint file");

        let mut call = |body: &str| -> (u16, Option<String>) {
            let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            sock.set_read_timeout(Some(std::time::Duration::from_secs(8))).unwrap();
            let req = format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\
                 \r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(req.as_bytes()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(150));
            {
                let mut srv = app.mcp.take().unwrap();
                srv.drain(&mut app);
                app.mcp = Some(srv);
            }
            tick(&mut app, &ctx); // the frame loop also DRAWS between calls
            let mut raw = Vec::new();
            sock.read_to_end(&mut raw).unwrap();
            let split = raw.iter().position(|b| *b == b'\n').unwrap_or(0);
            let status: u16 = String::from_utf8_lossy(&raw[..split])
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let body = raw
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|p| String::from_utf8_lossy(&raw[p + 4..]).into_owned());
            (status, body)
        };

        // The family is discoverable over the wire.
        let (st, body) = call(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
        );
        assert_eq!(st, 200);
        let names: Vec<String> = serde_json::from_str::<Value>(body.as_ref().unwrap())
            .unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for w in [
            "windows.list",
            "windows.open",
            "windows.close",
            "windows.move",
            "windows.resize",
            "windows.dock",
            "windows.undock",
            "windows.layout.save",
            "windows.layout.restore",
        ] {
            assert!(names.iter().any(|n| n == w), "{w} missing over the wire");
        }

        // Full round-trip, every ack carrying POST-change geometry.
        let mut tcall = |name: &str, args: Value| -> Value {
            let (st, body) = call(
                &json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
                        "params":{"name":name,"arguments":args}})
                    .to_string(),
            );
            assert_eq!(st, 200, "{name} over the wire");
            serde_json::from_str::<Value>(body.as_ref().unwrap()).unwrap()["result"]
                ["structuredContent"]
                .clone()
        };
        let ack = tcall("windows.open", json!({"id": "drums"}));
        assert_eq!(ack["window"]["open"], true);
        let ack = tcall("windows.list", json!({}));
        let l = ack["windows"].as_array().unwrap().clone();
        assert_eq!(find(&l, "drums")["open"], true, "observed by a later read");
        let ack = tcall("windows.undock", json!({"id": "rack"}));
        assert_eq!(ack["window"]["docked"], false);
        let ack = tcall("windows.move", json!({"id": "rack", "x": 300.0, "y": 200.0}));
        assert_eq!(
            ack["window"]["rect"].as_array().unwrap()[0].as_f64().unwrap().round(),
            300.0
        );
        let ack = tcall("windows.dock", json!({"id": "rack", "edge": "left", "rank": 0}));
        assert_eq!((ack["window"]["docked"].as_bool(), ack["window"]["edge"].as_i64()), (Some(true), Some(0)));
        let ack = tcall("windows.layout.save", json!({"name": "sock"}));
        assert_eq!(ack["layout"], "sock");
        let ack = tcall("windows.close", json!({"id": "drums"}));
        assert_eq!(ack["window"]["open"], false);

        drop(app); // McpServer::Drop closes the listener + endpoint file
        std::env::remove_var(crate::paths::DATA_DIR_ENV);
    }
}
