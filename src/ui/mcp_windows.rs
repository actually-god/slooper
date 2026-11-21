//! v108/MCP-03: the `windows.*` tool family (docs/MCP-DESIGN.md D4/D5,
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

fn need_id<'a>(args: &'a Value) -> Result<&'a str, ToolError> {
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
    if is_docked(app, idx) {
        return Err(ToolError::wrong_kind(format!(
            "window \"{id}\" is docked — docked geometry is owned by the dock; \
             windows.undock first"
        )));
    }
    let x = need_num(args, "x")?;
    let y = need_num(args, "y")?;
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
    if is_docked(app, idx) {
        return Err(ToolError::wrong_kind(format!(
            "window \"{id}\" is docked — docked thickness is set by dock drags; \
             windows.undock first"
        )));
    }
    let w = need_num(args, "w")?.clamp(MIN_W, app.vp_rect[2]);
    let h = need_num(args, "h")?.clamp(MIN_H, app.vp_rect[3]);
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
    // Deterministic rank insertion through the REAL dock path:
    // `dock_at` ranks by the live centers of the edge's tab groups, so the
    // synthetic drop point is placed just past the (rank-1)-th group's
    // center — the same math a drag does, one mouse-truth authority.
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
    let horiz = edge == 2 || edge == 3;
    let mut centers: Vec<f32> = groups
        .iter()
        .filter_map(|g| {
            g.iter()
                .find(|i| app.dock_rects.get(**i).is_some_and(|r| r[2] > 8.0))
                .map(|i| {
                    let r = app.dock_rects[*i];
                    if horiz {
                        r[0] + r[2] * 0.5
                    } else {
                        r[1] + r[3] * 0.5
                    }
                })
        })
        .collect();
    centers.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let want = (rank as usize).min(centers.len());
    let along = if want == 0 {
        f32::MIN / 4.0
    } else if want >= centers.len() {
        f32::MAX / 4.0
    } else {
        centers[want - 1] + 1.0
    };
    let pt = if horiz {
        egui::pos2(along, 0.0)
    } else {
        egui::pos2(0.0, along)
    };
    dock_at(app, idx, edge, pt);
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
