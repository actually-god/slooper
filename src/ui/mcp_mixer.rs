//! v110/MCP-04 — direct subwindow controls over MCP: `mixer.*`, `chain.*`,
//! `assets.*` (board card t_e0c0ad5e; spine docs/MCP-DESIGN.md D3–D7).
//!
//! A CHILD module of `ui` on purpose (the MCP-03 precedent): every write
//! rides the exact setter seams the mouse path uses — `set_trk_vol/gain/
//! pan/color`, the v110 `set_master_midi`/`set_sc_send_mask` homes,
//! `add_fx_script`/`add_fx_plugin`/`remove_fx_row`, `ensure_asset`/
//! `rack_del`/`remove_asset_at`, and the rack-drop assignment seams
//! (`sampler_set_asset`, `sampler_map_drop_asset`, `assign_pad_file`, the
//! lane_row clip contract). No mirror is ever poked directly, no RT rule
//! is touched: handlers run ONLY on the frame-loop drain thread (D3).
//!
//! Units (MCP-00 D5): the native currency of each seam — mixer values are
//! MIDI-range ints 0..=127 (`vol` 0=-inf/115=0 dB, `gain` 80=0 dB, `pan`
//! 64=centre; the same convention `settings.set` exposes for the backing
//! fields), `color` is the CLIP_PALETTE index, `sc_send` is an ARRAY of
//! destination track indices (JSON numbers cannot carry the u128 mask).
//! Every write echoes the post-change state re-read through the same
//! getters the read tools use (observed-then-ack), plus `app.status`.

use serde_json::{Value, json};

use crate::mcp::tools::{ToolDef, ToolError, ToolResult};

use super::{
    App, ChainRow, CLIP_PALETTE_NAMES, MAX_LOOPS, SmapDrop, chain_row_label,
};

#[cfg(test)]
mod tests;

/// Copy reused by list/get/set so the units never drift between tools.
const UNITS: &str = "mixer values are MIDI-range ints 0..=127 (vol: 0=-inf, 115=0 dB; \
                     gain: 80=0 dB; pan: 64=centre); color = palette index 0..9 or null; \
                     sc_send = array of destination track indices";

// ── registration ──────────────────────────────────────────────────────────

pub fn register(v: &mut Vec<ToolDef>) {
    v.push(ToolDef {
        name: "mixer.list",
        description: "Every track's live mixer state (vol/gain/pan/color/sidechain sends) \
                      plus the master-out volume, in the seams' native units. Read-only.",
        schema: json!({"type": "object", "properties": {}}),
        read: true,
        destructive: false,
        idempotent: true,
        execute: mixer_list,
    });
    v.push(ToolDef {
        name: "mixer.get",
        description: "One track's mixer state (omit `track` for the master-out entry). \
                      Read-only.",
        schema: json!({
            "type": "object",
            "properties": {
                "track": {"type": "integer", "description": "0-based track index"},
            },
        }),
        read: true,
        destructive: false,
        idempotent: true,
        execute: mixer_get,
    });
    v.push(ToolDef {
        name: "mixer.set",
        description: "Write one mixer control through the GUI's own setter seams. Controls: \
             vol | gain | pan | color | sc_send (needs track) and master_vol (no track). \
             Mixer values are MIDI-range ints 0..=127 (vol: 0=-inf, 115=0 dB; gain: \
             80=0 dB; pan: 64=centre); color = palette index 0..9 or null; sc_send = \
             array of destination track indices. Echoes the observed post-change state.",
        schema: json!({
            "type": "object",
            "properties": {
                "track": {"type": "integer", "description": "0-based track index (omit only for master_vol)"},
                "control": {"type": "string", "description": "vol | gain | pan | color | sc_send | master_vol"},
                "value": {"description": "MIDI 0..=127 int; color: palette int 0..9 or null; sc_send: array of destination track indices"},
            },
            "required": ["control", "value"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: mixer_set,
    });
    v.push(ToolDef {
        name: "chain.catalog",
        description: "What chain.pick can place: effect scripts (id = stem) and scanned \
                      plugins (id = file path; name also accepted), with kind, instrument \
                      flag and scan errors — the same two combos the MIXER CHAIN window \
                      shows. Read-only.",
        schema: json!({"type": "object", "properties": {}}),
        read: true,
        destructive: false,
        idempotent: true,
        execute: chain_catalog,
    });
    v.push(ToolDef {
        name: "chain.list",
        description: "One track's live effect chain: every slot with kind, name, id, \
                      status (loading | ready | failed | dead), bypass and editor_open. \
                      Read-only.",
        schema: json!({
            "type": "object",
            "properties": {
                "track": {"type": "integer", "description": "0-based track index"},
            },
            "required": ["track"],
        }),
        read: true,
        destructive: false,
        idempotent: true,
        execute: chain_list,
    });
    v.push(ToolDef {
        name: "chain.pick",
        description: "Append an effect to a track's chain — the exact \u{201c}+ effect\u{201d} / \
                      \u{201c}+ VST plugin\u{201d} combo code path. `plugin` is an id or exact \
                      name from chain.catalog. Scripts ack status=\u{201c}added\u{201d}; \
                      plugins ack status=\u{201c}loading\u{201d} (async probe on the plugin \
                      thread \u{2014} poll chain.list until status=\u{201c}ready\u{201d}). \
                      Optional `slot` relocates the new row (insert semantics). \
                      Instruments are refused \u{2014} they bind via \u{266a}."
        ,
        schema: json!({
            "type": "object",
            "properties": {
                "track": {"type": "integer", "description": "0-based track index"},
                "plugin": {"type": "string", "description": "script stem or plugin path/name from chain.catalog"},
                "slot": {"type": "integer", "description": "optional final slot index (default: append)"},
            },
            "required": ["track", "plugin"],
        }),
        read: false,
        destructive: false,
        idempotent: false,
        execute: chain_pick,
    });
    v.push(ToolDef {
        name: "chain.remove",
        description: "Remove one chain slot (the row's \u{2715} button seam: rebuild + \
                      persist + echo; a hosted plugin instance is held and reaped on the \
                      UI thread exactly as the GUI path does).",
        schema: json!({
            "type": "object",
            "properties": {
                "track": {"type": "integer", "description": "0-based track index"},
                "slot": {"type": "integer", "description": "slot index from chain.list"},
            },
            "required": ["track", "slot"],
        }),
        read: false,
        destructive: true,
        idempotent: false,
        execute: chain_remove,
    });
    v.push(ToolDef {
        name: "assets.list",
        description: "The asset rack: every scanned rack folder (files with kind and \
                      path) and the live asset table (asset = 0-based table id). \
                      Read-only.",
        schema: json!({"type": "object", "properties": {}}),
        read: true,
        destructive: false,
        idempotent: true,
        execute: assets_list,
    });
    v.push(ToolDef {
        name: "assets.import",
        description: "Import a file into the live rack (the OS-drop seam: `ensure_asset` \
                      decodes + dedups; the source file is NEVER modified) or add a \
                      folder as a new rack root (the drop-a-folder seam). Echoes the \
                      new table entry.",
        schema: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "file (.wav/.aiff/.flac/.mid/.slauto \u{2026}) or folder path"},
            },
            "required": ["path"],
        }),
        read: false,
        destructive: false,
        idempotent: true,
        execute: assets_import,
    });
    v.push(ToolDef {
        name: "assets.remove",
        description: "Delete by table id (`asset`: table-only removal, ids remap \u{2014} the \
                      remove_asset_at seam) or by file `path` (the rack \u{2715} seam \u{2014} \
                      deletes from disk, REFUSED unless the file sits inside a scanned \
                      rack folder: foreign files are never mutated).",
        schema: json!({
            "type": "object",
            "properties": {
                "asset": {"type": "integer", "description": "asset table id from assets.list"},
                "path": {"type": "string", "description": "rack file path (must be inside a rack folder)"},
            },
        }),
        read: false,
        destructive: true,
        idempotent: false,
        execute: assets_remove,
    });
    v.push(ToolDef {
        name: "assets.assign",
        description: "The rack-drag drop seams as a direct call. target=\u{201c}sampler\u{201d}: \
             becomes the bound script synth's asset (needs a script synth on the track). \
             target=\u{201c}keymap\u{201d}: adds a sampler key-map slot (C3, the \u{201c}no \
             samples mapped\u{201d} drop zone). target=\u{201c}pad\u{201d} (+ pad: N): the drum \
             kit pad sound (the per-pad \u{201c}wav\u{201d} button seam). \
             target=\u{201c}channel\u{201d} (+ optional lane, pos in samples): places the \
             asset as a clip on the arrange grid (the lane-strip drop contract). \
             Source = `asset` (table id) or `path` (imported first via the same seam). \
             Mixer values are MIDI-range ints 0..=127; asset ids are table indices.",
        schema: json!({
            "type": "object",
            "properties": {
                "target": {"type": "string", "description": "sampler | keymap | pad | channel"},
                "track": {"type": "integer", "description": "0-based track index"},
                "asset": {"type": "integer", "description": "asset table id (or give path)"},
                "path": {"type": "string", "description": "file to import-then-assign (or give asset)"},
                "pad": {"type": "integer", "description": "pad index (target=pad)"},
                "lane": {"type": "integer", "description": "lane index, default 0 (target=channel)"},
                "pos": {"type": "integer", "description": "clip position in samples, default 0 (target=channel)"},
            },
            "required": ["target", "track"],
        }),
        read: false,
        destructive: false,
        idempotent: false,
        execute: assets_assign,
    });
}

// ── shared arg helpers ────────────────────────────────────────────────────

fn want_track(app: &App, args: &Value) -> Result<usize, ToolError> {
    let t = args
        .get("track")
        .and_then(|t| t.as_u64())
        .ok_or_else(|| ToolError::invalid("needs an integer \"track\" (0-based)"))?;
    if t >= app.n_tracks as u64 {
        return Err(ToolError::out_of_range(format!(
            "track {t} does not exist \u{2014} this project has {} track(s): 0..{}",
            app.n_tracks,
            app.n_tracks - 1
        )));
    }
    Ok(t as usize)
}

fn want_u127(args: &Value, key: &str, control: &str) -> Result<u8, ToolError> {
    let raw = args
        .get(key)
        .ok_or_else(|| ToolError::invalid(format!("needs a \"{key}\" argument")))?;
    let n = raw
        .as_u64()
        .ok_or_else(|| ToolError::invalid(format!("{control}: {key} must be an integer 0..=127, got {raw}")))?;
    if n > 127 {
        return Err(ToolError::out_of_range(format!(
            "{control}: {key} must be 0..=127 (MIDI currency), got {n}"
        )));
    }
    Ok(n as u8)
}

fn want_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|s| s.as_str())
        .ok_or_else(|| ToolError::invalid(format!("needs a string \"{key}\" argument")))
}

/// sc_send array -> mask (a track cannot key itself; the GUI chips enforce
/// the same rule at 29.5k).
fn sc_mask(app: &App, track: usize, value: &Value) -> Result<u128, ToolError> {
    let arr = value.as_array().ok_or_else(|| {
        ToolError::invalid("sc_send wants an array of destination track indices, e.g. [3] \u{b7} [] clears")
    })?;
    let mut m = 0u128;
    for d in arr {
        let Some(d) = d.as_u64() else {
            return Err(ToolError::invalid(format!("sc_send entries must be integers, got {d}")));
        };
        if d >= app.n_tracks as u64 {
            return Err(ToolError::out_of_range(format!(
                "sc_send destination {d} does not exist \u{2014} this project has {} track(s)",
                app.n_tracks
            )));
        }
        let d = d as usize;
        if d == track {
            return Err(ToolError::out_of_range("a track cannot key itself"));
        }
        m |= 1u128 << d;
    }
    Ok(m)
}

fn sc_dests(m: u128) -> Vec<u32> {
    (0..MAX_LOOPS)
        .filter(|s| m & (1u128 << s) != 0)
        .map(|s| s as u32)
        .collect()
}

/// One track's mixer row — THE read surface both list/get and every write
/// ack share (observed-then-ack: the ack re-runs this after the seam).
fn mixer_row(app: &App, t: usize) -> Value {
    json!({
        "track": t,
        "vol": app.trk_vol[t],
        "gain": app.trk_gain[t],
        "pan": app.trk_pan[t],
        "color": app.trk_color[t],
        "sc_send": sc_dests(app.sc_send[t]),
    })
}

fn asset_kind_json(a: &crate::assets::Asset) -> &'static str {
    if a.is_midi() {
        "midi"
    } else if a.is_auto() {
        "auto"
    } else {
        "audio"
    }
}

// ── mixer.* ───────────────────────────────────────────────────────────────

fn mixer_list(app: &mut App, _args: &Value) -> ToolResult {
    let tracks: Vec<Value> = (0..app.n_tracks).map(|t| mixer_row(app, t)).collect();
    Ok(json!({
        "tracks": tracks,
        "n_tracks": app.n_tracks,
        "master": {"vol": app.master_midi},
        "units": UNITS,
    }))
}

fn mixer_get(app: &mut App, args: &Value) -> ToolResult {
    if args.get("track").is_some() {
        let t = want_track(app, args)?;
        let mut out = mixer_row(app, t);
        out["units"] = json!(UNITS);
        return Ok(out);
    }
    Ok(json!({
        "master": {"vol": app.master_midi},
        "n_tracks": app.n_tracks,
        "units": UNITS,
        "note": "pass \"track\" for a track row",
    }))
}

fn mixer_set(app: &mut App, args: &Value) -> ToolResult {
    let control = want_str(args, "control")?;
    let value = args
        .get("value")
        .ok_or_else(|| ToolError::invalid("mixer.set needs {control, value}"))?;
    let echo = match control {
        "master_vol" => {
            let v = want_u127(args, "value", control)?;
            app.set_master_midi(v);
            app.status = format!("MCP: master volume = {v}");
            json!({"control": control, "master": {"vol": app.master_midi}})
        }
        "vol" | "gain" | "pan" => {
            let t = want_track(app, args)?;
            let v = want_u127(args, "value", control)?;
            match control {
                "vol" => app.set_trk_vol(t, v),
                "gain" => app.set_trk_gain(t, v),
                "pan" => app.set_trk_pan(t, v),
                _ => unreachable!(),
            }
            app.status = format!("MCP: track {} {control} = {v}", t + 1);
            json!({"control": control, "track": t, "row": mixer_row(app, t)})
        }
        "color" => {
            let t = want_track(app, args)?;
            let col = if value.is_null() {
                None
            } else {
                let i = value.as_u64().ok_or_else(|| {
                    ToolError::invalid(format!("color must be a palette index 0..{} or null, got {value}", CLIP_PALETTE_NAMES.len() - 1))
                })?;
                if i >= CLIP_PALETTE_NAMES.len() as u64 {
                    return Err(ToolError::out_of_range(format!(
                        "color must be 0..{} or null, got {i}",
                        CLIP_PALETTE_NAMES.len() - 1
                    )));
                }
                Some(i as u8)
            };
            app.set_trk_color(t, col);
            app.status = format!(
                "MCP: track {} color = {}",
                t + 1,
                col.map(|i| CLIP_PALETTE_NAMES[i as usize].to_string())
                    .unwrap_or_else(|| "default".into())
            );
            json!({"control": control, "track": t, "row": mixer_row(app, t)})
        }
        "sc_send" => {
            let t = want_track(app, args)?;
            let m = sc_mask(app, t, value)?;
            app.set_sc_send_mask(t, m);
            app.status = format!("MCP: track {} sidechain send -> {:?}", t + 1, sc_dests(m));
            json!({"control": control, "track": t, "row": mixer_row(app, t)})
        }
        other => {
            return Err(ToolError::invalid(format!(
                "unknown mixer control \"{other}\" \u{2014} valid: vol, gain, pan, color, \
                 sc_send (with track) and master_vol (no track)"
            )));
        }
    };
    // Persist exactly like the settings surface does — a mixer move lives
    // through restart (the GUI defers via fx_save_pending; an agent call
    // is discrete, so it lands now).
    app.save_config();
    let mut out = echo;
    out["status"] = json!(app.status);
    out["units"] = json!(UNITS);
    Ok(out)
}

// ── chain.* ───────────────────────────────────────────────────────────────

fn chain_row_json(app: &App, t: usize, k: usize) -> Value {
    let row = match app.fx_rows[t].get(k) {
        Some(r) => r,
        None => return Value::Null,
    };
    let (kind, tag, name, id) = match row {
        ChainRow::Script { stem, .. } => (
            "script",
            "FX",
            app.fx_registry
                .desc(stem)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| stem.clone()),
            stem.clone(),
        ),
        ChainRow::Vst {
            kind: vk,
            name,
            path,
            ..
        } => ("plugin", vk.tag(), name.clone(), path.clone()),
    };
    let (loaded, dead, loading) = match row {
        ChainRow::Script { stem, .. } => (app.fx_registry.desc(stem).is_some(), false, false),
        ChainRow::Vst { entry, open_rx, .. } => (
            entry.is_some(),
            entry.as_ref().is_some_and(|e| e.is_dead()),
            entry.is_none() && open_rx.is_some(),
        ),
    };
    let bypass = match row {
        ChainRow::Script { bypass, .. } | ChainRow::Vst { bypass, .. } => *bypass,
    };
    let editor_open = match row {
        ChainRow::Script { .. } => app
            .fx_editor
            .as_ref()
            .is_some_and(|e| e.loop_i == t && e.slot == k),
        ChainRow::Vst { .. } => app
            .fx_vst_editors
            .get(&(t, k))
            .is_some_and(|w| !w.is_closed()),
    };
    let status = if dead {
        "dead"
    } else if loading {
        "loading"
    } else if !loaded {
        "failed"
    } else {
        "ready"
    };
    json!({
        "slot": k,
        "kind": kind,
        "tag": tag,
        "name": name,
        "id": id,
        "status": status,
        "bypass": bypass,
        "editor_open": editor_open,
    })
}

fn chain_rows(app: &App, t: usize) -> Vec<Value> {
    (0..app.fx_rows[t].len())
        .map(|k| chain_row_json(app, t, k))
        .collect()
}

fn chain_catalog_json(app: &App) -> (Vec<Value>, Vec<Value>) {
    let scripts: Vec<Value> = app
        .fx_registry
        .stems()
        .into_iter()
        .map(|stem| {
            json!({
                "id": stem,
                "name": app.fx_registry.desc(&stem).map(|d| d.name.clone()).unwrap_or_else(|| stem.clone()),
            })
        })
        .collect();
    let plugins: Vec<Value> = app
        .vst_scan
        .iter()
        .map(|e| {
            json!({
                "id": e.path.to_string_lossy(),
                "name": e.name,
                "kind": e.kind.tag(),
                "plugin_id": e.plugin_id,
                "instrument": e.instrument,
                "error": e.error,
            })
        })
        .collect();
    (scripts, plugins)
}

fn chain_catalog(app: &mut App, _args: &Value) -> ToolResult {
    let (scripts, plugins) = chain_catalog_json(app);
    Ok(json!({
        "scripts": scripts,
        "plugins": plugins,
        "scanned": app.vst_scanned,
        "scan_note": app.vst_note,
        "note": "chain.pick accepts a script id or a plugin id/name; entries with an \
                 error or instrument=true are refused (instruments bind via the \u{266a} \
                 synth picker)",
    }))
}

fn chain_list(app: &mut App, args: &Value) -> ToolResult {
    let t = want_track(app, args)?;
    Ok(json!({
        "track": t,
        "rows": chain_rows(app, t),
        "count": app.fx_rows[t].len(),
    }))
}

fn chain_pick(app: &mut App, args: &Value) -> ToolResult {
    let t = want_track(app, args)?;
    let needle = want_str(args, "plugin")?;
    let slot = match args.get("slot") {
        Some(s) => Some(s.as_u64().ok_or_else(|| {
            ToolError::invalid(format!("slot must be an integer, got {s}"))
        })?),
        None => None,
    };
    let (scripts, plugins) = chain_catalog_json(app);
    // 1) a script stem? (the \u{201c}+ effect\u{201d} combo iterates the SAME stems())
    let is_script = app
        .fx_registry
        .stems()
        .iter()
        .any(|s| s == needle);
    // 2) otherwise resolve against the scan list exactly as the plugin combo
    //    offers them (path id wins; unique-name match as the ergonomic alias).
    let entry = if is_script {
        None
    } else if let Some(e) = app.vst_scan.iter().find(|e| e.path.to_string_lossy() == needle) {
        Some(e.clone())
    } else {
        let hits: Vec<_> = app.vst_scan.iter().filter(|e| e.name == needle).collect();
        match hits.len() {
            0 => {
                return Err(ToolError::not_found(format!(
                    "unknown plugin \"{needle}\" \u{2014} chain.catalog lists {} script(s) + {} \
                     scan entry(s) (a plugin id is its file path)",
                    scripts.len(),
                    plugins.len()
                )));
            }
            1 => Some(hits[0].clone()),
            n => {
                return Err(ToolError::invalid(format!(
                    "\"{needle}\" matches {n} plugins \u{2014} give the exact path id from chain.catalog"
                )));
            }
        }
    };
    if let Some(e) = &entry {
        if let Some(err) = &e.error {
            return Err(ToolError::wrong_kind(format!(
                "\"{}\" failed to open during the scan: {err} \u{2014} fix the folder or rescan",
                e.name
            )));
        }
        if e.instrument {
            return Err(ToolError::invalid(format!(
                "{} is an instrument \u{2014} bind it with the \u{266a} synth picker \
                 (the mixer chain never hosts instruments)",
                e.name
            )));
        }
    }
    // The final slot position must exist AFTER the append.
    let len_after = app.fx_rows[t].len() + 1;
    if let Some(s) = slot {
        if s >= len_after as u64 {
            return Err(ToolError::out_of_range(format!(
                "slot {s} \u{2014} the chain will hold {len_after} slot(s) after this pick"
            )));
        }
    }
    // Ride the exact UI seams (they rebuild, persist and echo themselves).
    let ack_status = if is_script {
        app.add_fx_script(t, needle);
        "added"
    } else {
        app.add_fx_plugin(t, entry.as_ref().expect("resolved entry"));
        // An instrument can sneak past only via the entry check above;
        // add_fx_plugin guards too (belt-and-braces echo, no row added).
        if app.fx_rows[t].len() < len_after {
            return Err(ToolError::wrong_kind(
                app.vst_note.clone(),
            ));
        }
        "loading"
    };
    // Optional relocation (the combos only ever append; `slot` is the MCP
    // ergonomic that matches chain.list indices afterwards).
    let from = len_after - 1;
    if let Some(s) = slot {
        let to = s as usize;
        if to != from {
            let rows = &mut app.fx_rows[t];
            let row = rows.remove(from);
            rows.insert(to, row);
            // v113: ONE drift seam (was: a pending-load-only loop, the v110
            // lesson) — pending loads, curves, bindings and live views all
            // ride the identical map: the fresh row lands on `to`, rows in
            // [to, from) slide down one.
            app.remap_fx_rows(t, &|r| {
                if r == from {
                    Some(to)
                } else if r >= to && r < from {
                    Some(r + 1)
                } else {
                    Some(r)
                }
            });
            app.rebuild_fx(t);
            app.save_config();
        }
    }
    let mut out = json!({
        "track": t,
        "status": ack_status,
        "note": if ack_status == "loading" {
            "async load on the plugin thread \u{2014} poll chain.list until the row's status is \"ready\""
        } else {
            "script effect installed and audible from the next block"
        },
        "rows": chain_rows(app, t),
        "status_line": app.status,
    });
    if ack_status == "loading" {
        // v108 echo-inventory contract: the started-echo lives in the seam.
        out["loading_note"] = json!(app.vst_note);
    }
    Ok(out)
}

fn chain_remove(app: &mut App, args: &Value) -> ToolResult {
    let t = want_track(app, args)?;
    let k = args
        .get("slot")
        .and_then(|s| s.as_u64())
        .ok_or_else(|| ToolError::invalid("chain.remove needs an integer \"slot\""))? as usize;
    let n = app.fx_rows[t].len();
    if k >= n {
        return Err(ToolError::out_of_range(format!(
            "slot {k} \u{2014} the track {} chain has {n} slot(s)",
            t + 1
        )));
    }
    let label = chain_row_label(&app.fx_rows[t][k]);
    app.remove_fx_row(t, k);
    Ok(json!({
        "track": t,
        "removed": label,
        "rows": chain_rows(app, t),
        "status": app.status,
    }))
}

// ── assets.* ──────────────────────────────────────────────────────────────

fn assets_list(app: &mut App, _args: &Value) -> ToolResult {
    // Fresh scan first (asset.rs: scanning only LISTs candidate files and
    // is cheap) — an agent must never see a stale folder snapshot.
    for f in app.rack.iter_mut() {
        f.rescan();
    }
    let racks: Vec<Value> = app
        .rack
        .iter()
        .map(|f| {
            json!({
                "root": f.path,
                "err": f.err,
                "files": f.files.iter().map(|rf| json!({
                    "name": rf.name,
                    "dir": rf.dir,
                    "path": rf.path.to_string_lossy(),
                    "kind": if rf.is_midi { "midi" } else if rf.is_auto { "auto" } else { "audio" },
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let assets: Vec<Value> = app
        .assets
        .iter()
        .enumerate()
        .map(|(k, a)| {
            json!({
                "asset": k,
                "name": a.name,
                "kind": asset_kind_json(a),
                "path": a.path.to_string_lossy(),
                "samples": a.samples(),
                "dirty": a.dirty,
            })
        })
        .collect();
    Ok(json!({
        "racks": racks,
        "rack_count": racks.len(),
        "assets": assets,
        "asset_count": assets.len(),
        "note": "asset ids are table indices and remap when an asset is removed; \
                 re-read assets.list after every removal",
    }))
}

fn importable(path: &std::path::Path) -> bool {
    crate::assets::is_audio_file(path)
        || crate::assets::is_midi_file(path)
        || crate::assets::is_auto_file(path)
}

fn assets_import(app: &mut App, args: &Value) -> ToolResult {
    let raw = want_str(args, "path")?;
    let p = std::path::PathBuf::from(raw);
    if !p.exists() {
        return Err(ToolError::invalid(format!(
            "\u{201c}{raw}\u{201d} does not exist"
        )));
    }
    if p.is_dir() {
        // The drop-a-folder seam verbatim: new rack root + rescan + persist.
        let mut f = crate::assets::RackFolder::new(p.to_string_lossy().into_owned());
        f.rescan();
        let n = f.files.len();
        let label = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string());
        app.rack.push(f);
        app.save_config();
        app.status = format!("rack: added folder {label} ({n} file(s))");
        return Ok(json!({
            "kind": "rack_folder",
            "root": raw,
            "files": n,
            "status": app.status,
        }));
    }
    if !importable(&p) {
        return Err(ToolError::invalid(format!(
            "\u{201c}{}\u{201d} is not an importable type (audio / .mid / .slauto)",
            p.display()
        )));
    }
    // The OS-drop file seam verbatim: ensure_asset decodes + dedups +
    // selects; the source file is only READ.
    let Some(k) = app.ensure_asset(&p) else {
        return Err(ToolError::invalid(format!(
            "could not decode \u{201c}{}\u{201d}",
            p.display()
        )));
    };
    let name = app
        .assets
        .get(k)
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let kind = app.assets.get(k).map(asset_kind_json).unwrap_or("audio");
    app.status = format!("rack: imported {name}");
    // The GUI defers (its next edit persists); an agent call is discrete.
    app.save_config();
    app.push_project();
    Ok(json!({
        "kind": "asset",
        "asset": k,
        "name": name,
        "asset_kind": kind,
        "path": raw,
        "status": app.status,
    }))
}

fn in_rack(app: &App, p: &std::path::Path) -> Option<String> {
    let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    for f in &app.rack {
        let root = std::path::PathBuf::from(&f.path);
        let rcanon = root.canonicalize().unwrap_or(root);
        if canon.starts_with(&rcanon) {
            return Some(f.path.clone());
        }
    }
    None
}

fn assets_remove(app: &mut App, args: &Value) -> ToolResult {
    if args.get("asset").is_some() {
        let k = args["asset"]
            .as_u64()
            .ok_or_else(|| ToolError::invalid("asset must be an integer table id"))?
            as usize;
        if k >= app.assets.len() {
            return Err(ToolError::out_of_range(format!(
                "asset {k} \u{2014} the table holds {} entries (ids 0..{})",
                app.assets.len(),
                app.assets.len().saturating_sub(1)
            )));
        }
        let name = app.assets[k].name.clone();
        app.remove_asset_at(k); // remaps referencing views (its own contract)
        app.save_config();
        app.status = format!("MCP: removed asset \u{201c}{name}\u{201d}");
        return Ok(json!({
            "table_removal": true,
            "name": name,
            "asset_count": app.assets.len(),
            "status": app.status,
            "note": "asset ids remap \u{2014} re-read assets.list",
        }));
    }
    if let Some(raw) = args.get("path").and_then(|p| p.as_str()) {
        let p = std::path::PathBuf::from(raw);
        if !p.exists() {
            return Err(ToolError::not_found(format!("\u{201c}{raw}\u{201d} does not exist")));
        }
        // THE foreign-file guard: rack_del deletes from disk, so MCP only
        // ever points it inside a scanned rack root.
        let Some(root) = in_rack(app, &p) else {
            let roots: Vec<&str> = app.rack.iter().map(|f| f.path.as_str()).collect();
            return Err(ToolError::invalid(format!(
                "\u{201c}{raw}\u{201d} is not inside a rack folder \u{2014} foreign files \
                 are never deleted. Rack roots: {roots:?}"
            )));
        };
        app.rack_del(&p.to_string_lossy()); // rescan + push_project + echo (its seam)
        app.save_config();
        return Ok(json!({
            "deleted_file": raw,
            "rack_root": root,
            "asset_count": app.assets.len(),
            "status": app.status,
        }));
    }
    Err(ToolError::invalid("assets.remove needs {asset: id} or {path: \"rack file\"}"))
}

fn assets_assign(app: &mut App, args: &Value) -> ToolResult {
    let target = want_str(args, "target")?;
    let t = want_track(app, args)?;
    if args.get("asset").is_some() == args.get("path").is_some() {
        return Err(ToolError::invalid(
            "assets.assign needs EXACTLY ONE source: {asset: id} or {path: \"file\"}",
        ));
    }
    let a = if let Some(id) = args.get("asset").and_then(|a| a.as_u64()) {
        if id as usize >= app.assets.len() {
            return Err(ToolError::out_of_range(format!(
                "asset {id} \u{2014} the table holds {} entries",
                app.assets.len()
            )));
        }
        id as usize
    } else {
        let raw = want_str(args, "path")?;
        let p = std::path::PathBuf::from(raw);
        if !p.exists() {
            return Err(ToolError::invalid(format!("\u{201c}{raw}\u{201d} does not exist")));
        }
        if !importable(&p) {
            return Err(ToolError::invalid(format!(
                "\u{201c}{}\u{201d} is not an importable type (audio / .mid / .slauto)",
                p.display()
            )));
        }
        app.ensure_asset(&p)
            .ok_or_else(|| ToolError::invalid(format!("could not decode \u{201c}{raw}\u{201d}")))?
    };
    let asset_name = app
        .assets
        .get(a)
        .map(|x| x.name.clone())
        .unwrap_or_default();
    let is_audio = !app.assets[a].is_midi() && !app.assets[a].is_auto();
    let mut out = json!({"target": target, "track": t, "asset": a, "name": asset_name});
    match target {
        "sampler" => {
            if !is_audio {
                return Err(ToolError::wrong_kind(format!(
                    "\u{201c}{asset_name}\u{201d} is not an audio asset"
                )));
            }
            let scripted = app
                .track_synth_inst
                .get(t)
                .and_then(|o| o.as_ref())
                .is_some_and(|h| matches!(&**h, crate::audio::SynthHandle::Script(_)));
            if !scripted {
                return Err(ToolError::wrong_kind(format!(
                    "track {} has no script synth bound \u{2014} the sampler assignment \
                     needs the sampler/drum script synth (bind it with the \u{266a} picker)",
                    t + 1
                )));
            }
            app.sampler_set_asset(t, a);
            app.status = format!("MCP: track {} sampler -> \u{201c}{asset_name}\u{201d}", t + 1);
            out["slot0_asset"] = json!(app
                .sampler_maps
                .get(t)
                .and_then(|m| m.as_ref())
                .and_then(|m| m.slots.first())
                .map(|s| s.asset.clone()));
            // The LIVE synth param the seam wrote (source of truth when no
            // key-map was opened yet — the legacy-single-asset route).
            out["synth_param0"] = app
                .track_synth_inst
                .get(t)
                .and_then(|h| h.as_ref())
                .map(|h| match &**h {
                    crate::audio::SynthHandle::Script(i) => {
                        json!(i.values().first().copied())
                    }
                    crate::audio::SynthHandle::Vst(_) => Value::Null,
                })
                .unwrap_or(Value::Null);
        }
        "keymap" => {
            if !is_audio {
                return Err(ToolError::wrong_kind(format!(
                    "\u{201c}{asset_name}\u{201d} is not an audio asset"
                )));
            }
            // The key-map window's \u{201c}no samples mapped\u{201d} drop zone verbatim:
            // first layer at C3 (sampler_map_drop_asset seeds the map,
            // refuses non-audio itself, pushes to the live synth, persists).
            app.sampler_map_drop_asset(t, SmapDrop::Empty, a);
            out["slots"] = json!(app
                .sampler_maps
                .get(t)
                .and_then(|m| m.as_ref())
                .map(|m| m.slots.len()));
        }
        "pad" => {
            let p = args
                .get("pad")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| ToolError::invalid("target=pad needs an integer \"pad\""))?
                as usize;
            if p >= app.kit(t).len() {
                return Err(ToolError::out_of_range(format!(
                    "pad {p} \u{2014} track {}'s kit has {} pad(s)",
                    t + 1,
                    app.kit(t).len()
                )));
            }
            if !is_audio {
                return Err(ToolError::wrong_kind(format!(
                    "\u{201c}{asset_name}\u{201d} is not an audio asset"
                )));
            }
            let src = app.assets[a].path.clone();
            // The per-pad \u{201c}wav\u{201d} button / OS-drop flow verbatim
            // (kit take-put, ensure_rows, persist, echo).
            let mut kit = std::mem::take(app.kit_mut(t));
            let ok = app.assign_pad_file(&mut kit, p, &src).is_some();
            *app.kit_mut(t) = kit;
            if !ok {
                return Err(ToolError::wrong_kind(format!(
                    "pad {} of track {} refused \u{201c}{asset_name}\u{201d}",
                    p + 1,
                    t + 1
                )));
            }
            app.drum.ensure_rows(app.kit(t).len());
            app.save_config();
            app.status = format!("MCP: drum kit track {} pad {} -> \u{201c}{asset_name}\u{201d}", t + 1, p + 1);
            out["pad"] = json!(p);
            out["pad_asset_path"] = json!(app.kit(t)[p].asset_path.clone());
        }
        "channel" => {
            let lane = args.get("lane").and_then(|l| l.as_u64()).unwrap_or(0) as usize;
            let pos = args.get("pos").and_then(|p| p.as_u64()).unwrap_or(0);
            if app.arr.tracks.get(t).is_none() {
                return Err(ToolError::out_of_range(format!(
                    "track {t} has no arrange row yet"
                )));
            }
            // The lane-strip rack-drop contract (27049+): insert, select,
            // persist, echo — only the pointer-snapped pos becomes an
            // explicit sample position.
            app.arr.insert(t, lane, crate::arrange::Clip::new(a as u32, pos));
            let li = app.arr.tracks[t].lanes[lane].clips.len() - 1;
            app.select_one((t, lane, li));
            app.push_project();
            app.status = format!(
                "MCP: clip placed on track {} \u{b7} {}",
                t + 1,
                if lane == 0 { "track" } else { "child lane" }
            );
            out["lane"] = json!(lane);
            out["clip"] = json!({"slot": li, "pos": pos, "asset": a});
        }
        other => {
            return Err(ToolError::invalid(format!(
                "unknown assign target \"{other}\" \u{2014} valid: sampler, keymap, pad, channel"
            )));
        }
    }
    out["status"] = json!(app.status);
    Ok(out)
}
