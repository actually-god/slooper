//! v128/MCP-05 — full control of the NATIVE (bundled script) plugins over
//! MCP (board card t_3f374341; docs/MCP-DESIGN.md §10): `plugins.*`,
//! `plugin.*`, `sampler.*`, `drum.*`, `midi.*`.
//!
//! Contract inherited from the spine (do not re-litigate): tools execute
//! ONLY on the UI frame-loop thread; reads AND writes go through the
//! ui.rs `mcp05_*` seams — the same code paths the GUI itself drives
//! (`apply_target_value`, `push_sampler_map`, `drum_resolve`,
//! `update_piano_roll_asset` → `push_project`, the v56/v65/v69/v34 preset
//! stores). Writes are observed-then-ack (the ack re-reads through the
//! same getter the read tools use) and every accepted write echoes
//! `app.status` (goal-1 doctrine).
//!
//! **Param currency (v128 decision, REQUIREMENTS.md deviation #1):**
//! values are the param's DECLARED desc range — the engine's actual
//! storage currency (`set_param` stores the desc value; preset files and
//! `plugin.params` read-backs speak the same numbers). Every schema entry
//! carries `min`/`max`/`unit`/`x-slooper-unit`/`step`/`labels` so a client
//! never guesses; enum params must land on an enum slot.
//!
//! Hosted plugins (VST/CLAP) are deliberately OUT: they answer
//! `wrong_kind` (their editors own their parameters; MCP-05 speaks the
//! native registry instances only). Packed sampler words never surface —
//! slots are decomposed `{asset, root, vel}` exactly like the design's D4.

use serde_json::{json, Value};

use crate::audio::SynthHandle;
use crate::config::SamplerSlotSave;
use crate::fx;
use crate::mcp::tools::{ToolDef, ToolError, ToolResult};
use crate::midipat::{Note, PPQ};
use crate::presetbar;
use crate::ui::App;

#[cfg(test)]
mod tests;

// ── arg plumbing ────────────────────────────────────────────────────────

fn req_u64(args: &Value, key: &str) -> Result<u64, ToolError> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ToolError::invalid(format!("{key} is required (non-negative integer, 0-based)")))
}

fn opt_usize(args: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|n| Some(n as usize))
            .ok_or_else(|| ToolError::invalid(format!("{key} must be a non-negative integer"))),
    }
}

fn need_f32(args: &Value, key: &str) -> Result<f32, ToolError> {
    args.get(key)
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
        .filter(|v| v.is_finite())
        .ok_or_else(|| ToolError::invalid(format!("{key} is required (finite number)")))
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::invalid(format!("{key} is required (string)")))
}

fn track_arg(app: &App, args: &Value) -> Result<usize, ToolError> {
    let t = req_u64(args, "track")? as usize;
    let n = app.mcp05_n_tracks();
    if t >= n {
        return Err(ToolError::out_of_range(format!(
            "track {t} — the project has {n} tracks, addressed 0..{}",
            n - 1
        )));
    }
    Ok(t)
}

// ── parameter metadata (ParamDesc/ParamMetaV1 -> schema) ────────────────

#[derive(Clone)]
struct PMeta {
    label: String,
    min: f32,
    max: f32,
    default: f32,
    unit: u32,
    step: f32,
    labels: Vec<String>,
}

/// `SLOOPER_UNIT_*` -> name (docs/EFFECT-SUITE.md / effect_api.rs codes).
fn unit_name(u: u32) -> &'static str {
    match u {
        1 => "percent",
        2 => "db",
        3 => "hz",
        4 => "ms",
        5 => "semitones",
        6 => "beats",
        7 => "times",
        8 => "samples",
        9 => "seconds",
        _ => "none",
    }
}

fn meta_json(i: usize, m: &PMeta) -> Value {
    let mut v = json!({
        "index": i,
        "name": m.label,
        "min": m.min,
        "max": m.max,
        "default": m.default,
        "unit": unit_name(m.unit),
        "x-slooper-unit": m.unit,
    });
    if m.step > 0.0 {
        v["step"] = json!(m.step);
    }
    if !m.labels.is_empty() {
        v["labels"] = json!(m.labels);
    }
    v
}

/// Human string via the SAME formatter the GUI uses (v70 doctrine):
/// enum -> the chosen label; declared unit -> engineering text.
fn human(m: &PMeta, v: f32) -> String {
    if !m.labels.is_empty() {
        let n = m.labels.len();
        let idx = if n == 1 || (m.max - m.min).abs() < 1e-9 {
            0
        } else {
            (((v - m.min) / (m.max - m.min)) * (n as f32 - 1.0))
                .round()
                .clamp(0.0, (n - 1) as f32) as usize
        };
        return m.labels[idx.min(n - 1)].clone();
    }
    fx::fmt_param_value(v, m.unit)
}

/// Enum params must land on a declared slot (equally spaced min..max).
fn validate_value(m: &PMeta, v: f32) -> Result<f32, ToolError> {
    if !v.is_finite() {
        return Err(ToolError::invalid("value must be finite"));
    }
    let lo = m.min.min(m.max);
    let hi = m.min.max(m.max);
    if v < lo || v > hi {
        return Err(ToolError::out_of_range(format!(
            "param \"{}\" takes {lo}..={hi}{} — got {v}",
            m.label,
            if m.unit != 0 {
                format!(" ({})", unit_name(m.unit))
            } else {
                String::new()
            }
        )));
    }
    let n = m.labels.len();
    if n >= 2 {
        let spacing = (hi - lo) / (n as f32 - 1.0);
        let idx = (((v - lo) / spacing.max(1e-9)).round()).clamp(0.0, (n - 1) as f32);
        let snapped = lo + idx * spacing;
        if (v - snapped).abs() > (spacing / 100.0).max(1e-4) {
            let opts: Vec<String> = m
                .labels
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{l}={}", lo + i as f32 * spacing))
                .collect();
            return Err(ToolError::out_of_range(format!(
                "param \"{}\" is an enum: value must land on one of {}",
                m.label,
                opts.join(", ")
            )));
        }
        return Ok(snapped);
    }
    Ok(v)
}

// ── target resolution ───────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Target {
    Synth { track: usize },
    Fx { track: usize, row: usize },
}

impl Target {
    fn json(&self) -> Value {
        match *self {
            Target::Synth { track } => json!({"kind": "synth", "track": track}),
            Target::Fx { track, row } => json!({"kind": "fx", "track": track, "row": row}),
        }
    }
}

fn resolve_target(app: &App, args: &Value) -> Result<Target, ToolError> {
    let track = track_arg(app, args)?;
    match opt_usize(args, "row")? {
        None => Ok(Target::Synth { track }),
        Some(row) => {
            let (name, _p, _b, is_vst) = app.mcp05_fx_row(track, row).ok_or_else(|| {
                ToolError::not_found(format!(
                    "track {track} chain has {} slot(s); row {row} does not exist (0-based)",
                    app.mcp05_fx_chain_len(track)
                ))
            })?;
            if is_vst {
                return Err(ToolError::wrong_kind(format!(
                    "track {track} row {row} hosts the plugin \"{name}\": these tools cover \
                     NATIVE slooper scripts only — hosted plugins keep their own editor"
                )));
            }
            Ok(Target::Fx { track, row })
        }
    }
}

/// Identity + metas + live values for a resolved target (one read source
/// for every plugin.* tool — the observed-ack re-read calls the same fn).
fn target_read(app: &App, tgt: &Target) -> Result<(String, Vec<PMeta>, Vec<f32>), ToolError> {
    match *tgt {
        Target::Synth { track } => {
            let h = app.mcp05_synth_handle(track).ok_or_else(|| {
                ToolError::not_found(format!(
                    "track {track} has no live instrument — bind one first (the tools speak \
                     live instances; plugins.instances lists what exists)"
                ))
            })?;
            let inst = match h.as_ref() {
                SynthHandle::Script(i) => i.clone(),
                SynthHandle::Vst(e) => {
                    return Err(ToolError::wrong_kind(format!(
                        "track {track} hosts the instrument \"{}\": these tools cover NATIVE \
                         slooper scripts only",
                        e.name()
                    )))
                }
            };
            let stem = app.mcp05_track_synth_stem(track).unwrap_or_else(|| "piano".into());
            let desc = app.mcp05_synth_desc(&stem).ok_or_else(|| {
                ToolError::not_found(format!("synth script \"{stem}\" is not installed"))
            })?;
            let metas = desc
                .params
                .iter()
                .map(|p| PMeta {
                    label: p.label.clone(),
                    min: p.min,
                    max: p.max,
                    default: p.default,
                    unit: 0,
                    step: 0.0,
                    labels: Vec::new(),
                })
                .collect();
            Ok((stem, metas, inst.values()))
        }
        Target::Fx { track, row } => {
            let (stem, rowparams, _, is_vst) = app.mcp05_fx_row(track, row).ok_or_else(|| {
                ToolError::not_found(format!("track {track} chain row {row} does not exist"))
            })?;
            if is_vst {
                return Err(ToolError::wrong_kind(format!(
                    "track {track} row {row} \"{stem}\" is a hosted plugin (native scripts only)"
                )));
            }
            let desc = app.mcp05_fx_desc(&stem).ok_or_else(|| {
                ToolError::not_found(format!(
                    "effect script \"{stem}\" is not in the effects folder"
                ))
            })?;
            let metas = desc
                .params
                .iter()
                .map(|p| PMeta {
                    label: p.label.clone(),
                    min: p.min,
                    max: p.max,
                    default: p.default,
                    unit: p.unit,
                    step: p.step,
                    labels: p.labels.clone(),
                })
                .collect();
            let n = desc.params.len();
            let values = match app.mcp05_fx_live(track, row) {
                Some(inst) => (0..n).map(|k| inst.param(k)).collect(),
                None => {
                    let mut v = rowparams;
                    v.resize(n, 0.0);
                    v
                }
            };
            Ok((stem, metas, values))
        }
    }
}

fn params_json(tgt: &Target, stem: &str, metas: &[PMeta], values: &[f32]) -> Value {
    let params: Vec<Value> = metas
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let v = values.get(i).copied().unwrap_or(m.default);
            let mut e = meta_json(i, m);
            e["value"] = json!(v);
            e["human"] = json!(human(m, v));
            e
        })
        .collect();
    json!({
        "target": tgt.json(),
        "stem": stem,
        "params": params,
        "count": params.len(),
        "currency": "values are in each param's declared min..max range (the engine's storage \
                      currency); enum params (labels) must land on a labeled slot",
    })
}

// ── asset resolution (rack ids ARE table indices — hard rule) ───────────

/// Resolve `asset_id` (rack table index) OR `path` (exact rack path) into a
/// canonical path string. Audio assets only; both must already be in the
/// rack (an agent makes ids come from the read tools; imports are MCP-04).
fn asset_path_arg(app: &App, args: &Value) -> Result<Option<String>, ToolError> {
    let id = args.get("asset_id").cloned();
    let path = args.get("path").cloned();
    match (id, path) {
        (Some(v), None) => {
            let id = v.as_u64().ok_or_else(|| {
                ToolError::invalid("asset_id must be a non-negative integer (rack table index)")
            })? as usize;
            let (p, is_pattern, is_auto) = app.mcp05_asset_entry(id).ok_or_else(|| {
                ToolError::out_of_range(format!(
                    "asset_id {id} is not in the rack — read tools list the ids; importing \
                     samples is assets.* (MCP-04)"
                ))
            })?;
            if is_pattern || is_auto {
                return Err(ToolError::wrong_kind(format!(
                    "asset {id} is a MIDI/automation asset, not an audio sample"
                )));
            }
            Ok(Some(p.to_string_lossy().into_owned()))
        }
        (None, Some(v)) => {
            let s = v.as_str().ok_or_else(|| ToolError::invalid("path must be a string"))?;
            if app.mcp05_asset_index(s).is_none() {
                let stems = app.mcp05_asset_stems();
                return Err(ToolError::not_found(format!(
                    "no rack asset with path \"{s}\" — rack holds: {}",
                    if stems.is_empty() { "(empty)".into() } else { stems.join(", ") }
                )));
            }
            Ok(Some(s.to_string()))
        }
        (None, None) => Ok(None),
        _ => Err(ToolError::invalid("give asset_id OR path, not both")),
    }
}

fn u8_arg(args: &Value, key: &str, min: u64, max: u64) -> Result<Option<u8>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                ToolError::invalid(format!("{key} must be an integer ({min}..={max})"))
            })?;
            if !(min..=max).contains(&n) {
                return Err(ToolError::out_of_range(format!(
                    "{key} must be {min}..={max}, got {n}"
                )));
            }
            Ok(Some(n as u8))
        }
    }
}

// ── sampler / drum / midi read helpers (single source for reads + acks) ──

fn smap_json(app: &App, t: usize) -> Value {
    let map = app.mcp05_sampler_map(t);
    let slots: Vec<Value> = map
        .as_ref()
        .map(|m| {
            m.slots
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let id = app.mcp05_asset_index(&s.asset);
                    json!({
                        "slot": i,
                        "asset": s.asset,
                        "asset_id": id,
                        "in_rack": id.is_some(),
                        "root": s.root,
                        "vel": s.vel,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mode = map.as_ref().map(|m| m.mode).unwrap_or(0);
    // Velocity layers grouped by root note (the v52 view).
    let mut roots: Vec<u8> = map
        .as_ref()
        .map(|m| {
            let mut r: Vec<u8> = m.slots.iter().map(|s| s.root).collect();
            r.sort();
            r.dedup();
            r
        })
        .unwrap_or_default();
    if roots.len() > 8192 {
        roots.truncate(8192);
    }
    let layers: Vec<Value> = roots
        .iter()
        .map(|r| {
            let ls: Vec<Value> = map
                .as_ref()
                .unwrap()
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.root == *r)
                .map(|(i, s)| json!({"slot": i, "vel": s.vel, "asset_id": app.mcp05_asset_index(&s.asset)}))
                .collect();
            json!({"root": r, "layers": ls})
        })
        .collect();
    json!({
        "track": t,
        "has_map": map.is_some(),
        "mode": mode,
        "mode_name": if mode == 0 { "cutoff" } else { "crossfade" },
        "slots": slots,
        "count": slots.len(),
        "layers_by_root": layers,
        "note": "vel is the layer's recorded-dynamics unity point (v52: hard cutoff picks the \
                 highest layer at-or-below the note velocity; mode 1 crossfades the bracketing \
                 layers). Slots are dense 0..n; asset_id is the rack table index (null = file \
                 currently missing from the rack).",
    })
}

fn kit_json(app: &App, t: usize) -> Value {
    let pads: Vec<Value> = app
        .mcp05_kit(t)
        .iter()
        .enumerate()
        .map(|(i, (p, id))| {
            json!({
                "pad": i,
                "name": p.name,
                "asset": p.asset,
                "asset_id": id,
                "vel": p.vel,
                "muted": p.muted,
            })
        })
        .collect();
    json!({"track": t, "pads": pads, "count": pads.len()})
}

fn pattern_json(app: &App, id: usize) -> Result<Value, ToolError> {
    if app.mcp05_asset_entry(id).is_none() {
        return Err(ToolError::not_found(format!("asset {id} is not in the rack")));
    }
    let pat = app.mcp05_pattern(id).ok_or_else(|| {
        ToolError::wrong_kind(format!("asset {id} is not a MIDI pattern asset"))
    })?;
    let notes: Vec<Value> = pat
        .notes
        .iter()
        .take(4096)
        .map(|n| {
            json!({"note": n.note, "vel": n.vel, "start": n.start, "len": n.len,
                   "ch": n.ch, "press": n.press})
        })
        .collect();
    Ok(json!({
        "asset": id,
        "bpm": pat.bpm,
        "len_ticks": pat.len_ticks,
        "tick": "start/len are MIDI ticks (PPQ = {PPQ})",
        "note_count": pat.notes.len(),
        "notes": notes,
        "truncated": pat.notes.len() > 4096,
    }))
}

/// Pattern target gate: id exists AND carries a pattern (else wrong_kind).
fn pattern_target(app: &App, args: &Value) -> Result<usize, ToolError> {
    let id = req_u64(args, "asset")? as usize;
    let (_p, is_pattern, is_auto) = app.mcp05_asset_entry(id).ok_or_else(|| {
        ToolError::not_found(format!(
            "asset {id} is not in the rack (ids are 0-based table indices)"
        ))
    })?;
    if !is_pattern {
        return Err(ToolError::wrong_kind(format!(
            "asset {id} is {} — midi tools speak MIDI pattern assets (drum patterns and \
             piano-roll clips)",
            if is_auto { "an automation curve" } else { "an audio sample" }
        )));
    }
    Ok(id)
}

fn sampler_track_arg(app: &App, args: &Value) -> Result<usize, ToolError> {
    let t = track_arg(app, args)?;
    if !app.mcp05_is_sampler_track(t) {
        return Err(ToolError::wrong_kind(format!(
            "track {t} does not play the bundled sampler (only its tracks own a key map) — \
             bind synth \"sampler\" first; plugins.instances lists what is bound"
        )));
    }
    Ok(t)
}

// ── tool handlers ────────────────────────────────────────────────────────

fn plugins_list(app: &mut App, args: &Value) -> ToolResult {
    let kind = args.get("kind").cloned().unwrap_or(json!("all"));
    let kind = kind.as_str().unwrap_or("all");
    if !matches!(kind, "all" | "effect" | "synth") {
        return Err(ToolError::invalid("kind must be \"all\", \"effect\" or \"synth\""));
    }
    let mut effects = Vec::new();
    let mut synths = Vec::new();
    if kind != "synth" {
        for stem in app.mcp05_fx_registry_stems() {
            if let Some(d) = app.mcp05_fx_desc(&stem) {
                effects.push(json!({
                    "stem": stem,
                    "kind": "effect",
                    "name": d.name,
                    "author": d.author,
                    "description": d.description,
                    "sidechain": d.sidechain,
                    "stereo": d.stereo,
                    "params": d.params.iter().enumerate()
                        .map(|(i, p)| meta_json(i, &PMeta {
                            label: p.label.clone(),
                            min: p.min,
                            max: p.max,
                            default: p.default,
                            unit: p.unit,
                            step: p.step,
                            labels: p.labels.clone(),
                        }))
                        .collect::<Vec<_>>(),
                }));
            }
        }
    }
    if kind != "effect" {
        for stem in app.mcp05_synth_registry_stems() {
            if let Some(d) = app.mcp05_synth_desc(&stem) {
                synths.push(json!({
                    "stem": stem,
                    "kind": "synth",
                    "name": d.name,
                    "author": d.author,
                    "description": d.description,
                    "params": d.params.iter().enumerate()
                        .map(|(i, p)| meta_json(i, &PMeta {
                            label: p.label.clone(),
                            min: p.min,
                            max: p.max,
                            default: p.default,
                            unit: 0,
                            step: 0.0,
                            labels: Vec::new(),
                        }))
                        .collect::<Vec<_>>(),
                }));
            }
        }
    }
    Ok(json!({
        "effects": effects,
        "synths": synths,
        "count": effects.len() + synths.len(),
        "note": "the installed registry catalog (bundled scripts + the user's own scripts). \
                 Instantiate via plugin.preset.load / chain picking (chain.* is MCP-04).",
    }))
}

fn plugins_instances(app: &mut App, args: &Value) -> ToolResult {
    let only = opt_usize(args, "track")?;
    if let Some(t) = only {
        if t >= app.mcp05_n_tracks() {
            return Err(ToolError::out_of_range(format!(
                "track {t} — the project has {} tracks",
                app.mcp05_n_tracks()
            )));
        }
    }
    let mut out = Vec::new();
    for t in 0..app.mcp05_n_tracks() {
        if only.is_some_and(|o| o != t) {
            continue;
        }
        if let Some(h) = app.mcp05_synth_handle(t) {
            let stem = app.mcp05_track_synth_stem(t).unwrap_or_else(|| "piano".into());
            match h.as_ref() {
                SynthHandle::Script(_) => out.push(
                    json!({"kind": "synth", "track": t, "stem": stem, "native": true}),
                ),
                SynthHandle::Vst(e) => out.push(json!({
                    "kind": "synth", "track": t, "stem": stem,
                    "name": e.name(), "native": false,
                })),
            }
        }
        for r in 0..app.mcp05_fx_chain_len(t) {
            if let Some((stem, _p, bypass, is_vst)) = app.mcp05_fx_row(t, r) {
                let mut e =
                    json!({"kind": "fx", "track": t, "row": r, "stem": stem,
                           "native": !is_vst, "bypass": bypass});
                if is_vst {
                    e["note"] = json!("hosted plugin — use its editor; native tools answer wrong_kind");
                }
                out.push(e);
            }
        }
    }
    Ok(json!({"instances": out, "count": out.len()}))
}

fn plugin_describe(app: &mut App, args: &Value) -> ToolResult {
    let tgt = resolve_target(app, args)?;
    let (stem, metas, _v) = target_read(app, &tgt)?;
    let mut e = json!({"target": tgt.json(), "stem": stem});
    match tgt {
        Target::Fx { track, row } => {
            if let Some((_, _, bypass, _)) = app.mcp05_fx_row(track, row) {
                e["bypass"] = json!(bypass);
            }
            if let Some(d) = app.mcp05_fx_desc(&stem) {
                e["name"] = json!(d.name);
                e["author"] = json!(d.author);
                e["description"] = json!(d.description);
                e["sidechain"] = json!(d.sidechain);
                e["stereo"] = json!(d.stereo);
            }
        }
        Target::Synth { .. } => {
            if let Some(d) = app.mcp05_synth_desc(&stem) {
                e["name"] = json!(d.name);
                e["author"] = json!(d.author);
                e["description"] = json!(d.description);
            }
        }
    }
    e["params"] = json!(metas
        .iter()
        .enumerate()
        .map(|(i, m)| meta_json(i, m))
        .collect::<Vec<_>>());
    Ok(e)
}

fn plugin_params(app: &mut App, args: &Value) -> ToolResult {
    let tgt = resolve_target(app, args)?;
    let (stem, metas, values) = target_read(app, &tgt)?;
    Ok(params_json(&tgt, &stem, &metas, &values))
}

fn plugin_param_get(app: &mut App, args: &Value) -> ToolResult {
    let tgt = resolve_target(app, args)?;
    let k = req_u64(args, "param")? as usize;
    let (stem, metas, values) = target_read(app, &tgt)?;
    let m = metas.get(k).ok_or_else(|| {
        ToolError::out_of_range(format!(
            "param {k} — \"{stem}\" has {} params (0..{})",
            metas.len(),
            metas.len().saturating_sub(1)
        ))
    })?;
    let v = values.get(k).copied().unwrap_or(m.default);
    let mut e = meta_json(k, m);
    e["value"] = json!(v);
    e["human"] = json!(human(m, v));
    e["target"] = tgt.json();
    Ok(e)
}

fn plugin_param_set(app: &mut App, args: &Value) -> ToolResult {
    let tgt = resolve_target(app, args)?;
    let k = req_u64(args, "param")? as usize;
    let v = need_f32(args, "value")?;
    let (stem, metas, _old) = target_read(app, &tgt)?;
    let m = metas.get(k).ok_or_else(|| {
        ToolError::out_of_range(format!(
            "param {k} — \"{stem}\" has {} params (0..{})",
            metas.len(),
            metas.len().saturating_sub(1)
        ))
    })?;
    let v = validate_value(m, v)?;
    let applied = match tgt {
        Target::Fx { track, row } => app.mcp05_fx_set_param(track, row, k, v),
        Target::Synth { track } => app.mcp05_synth_set_param(track, k, v),
    };
    if !applied {
        let st = app.status.clone();
        return Err(ToolError::invalid(format!("the GUI setter seam refused the write ({st})")));
    }
    // Observed-then-ack: re-read through the same getter the reads use.
    let (_stem2, metas2, values2) = target_read(app, &tgt)?;
    let got = values2.get(k).copied().unwrap_or(0.0);
    let echo = format!("MCP: {stem} \"{}\" set to {}", m.label, human(m, got));
    app.status = echo.clone();
    Ok(json!({
        "target": tgt.json(),
        "param": k,
        "name": m.label,
        "value": got,
        "human": human(metas2.get(k).unwrap_or(m), got),
        "status": echo,
    }))
}

/// Which preset storage a `plugin.preset.*` call targets.
enum Surface {
    Fx { track: usize, row: usize, stem: String },
    Synth { track: usize, stem: String },
    SamplerMap { track: usize },
}

fn preset_surface(app: &App, args: &Value) -> Result<Surface, ToolError> {
    let track = track_arg(app, args)?;
    if let Some(row) = opt_usize(args, "row")? {
        let (stem, _, _, is_vst) = app.mcp05_fx_row(track, row).ok_or_else(|| {
            ToolError::not_found(format!("track {track} chain row {row} does not exist"))
        })?;
        if is_vst {
            return Err(ToolError::wrong_kind(
                "that chain slot hosts a plugin — native preset stores only",
            ));
        }
        return Ok(Surface::Fx { track, row, stem });
    }
    let surface = args.get("surface").and_then(|s| s.as_str()).unwrap_or("params");
    if surface == "map" {
        if !app.mcp05_is_sampler_track(track) {
            let stem = app.mcp05_track_synth_stem(track).unwrap_or_else(|| "none".into());
            return Err(ToolError::wrong_kind(format!(
                "surface \"map\" needs a sampler track (track {track} plays \"{stem}\")"
            )));
        }
        return Ok(Surface::SamplerMap { track });
    }
    if surface != "params" {
        return Err(ToolError::invalid("surface must be \"params\" or \"map\""));
    }
    let stem = app.mcp05_track_synth_stem(track).ok_or_else(|| {
        ToolError::not_found(format!("track {track} has no bound synth script"))
    })?;
    Ok(Surface::Synth { track, stem })
}

fn plugin_preset_list(app: &mut App, args: &Value) -> ToolResult {
    match preset_surface(app, args)? {
        Surface::Fx { stem, .. } => {
            let fx_dir = app.mcp05_fx_dir();
            let user = fx::list_user_presets(&fx_dir, &stem);
            let builtin: Vec<Value> = fx::builtin_presets(&stem)
                .into_iter()
                .map(|(n, ps)| {
                    json!({"name": n, "params": ps.into_iter()
                        .map(|(l, v)| json!({l: v})).collect::<Vec<_>>()})
                })
                .collect();
            Ok(json!({
                "storage": "fx-user",
                "dir": fx::user_presets_dir(&fx_dir).display().to_string(),
                "presets": user,
                "builtin": builtin,
                "note": "builtin factory presets are read-only (load lands their values on the \
                         live instance); user presets round-trip through this folder.",
            }))
        }
        Surface::Synth { stem, .. } => {
            let dir = app.mcp05_synth_preset_dir(&stem);
            Ok(json!({
                "storage": "synth",
                "dir": dir.display().to_string(),
                "presets": app.mcp05_synth_preset_list(&stem),
                "note": "one folder per script synth; positional value tables seeded against \
                         the live desc (older/newer layouts degrade to defaults).",
            }))
        }
        Surface::SamplerMap { .. } => {
            let dir = App::mcp05_sampler_preset_dir();
            Ok(json!({
                "storage": "sampler-map",
                "dir": dir.display().to_string(),
                "presets": App::mcp05_sampler_preset_list(),
                "note": "named sampler KEY MAPS (v56 storage — the same folder the sampler \
                         window's preset bar shows).",
            }))
        }
    }
}

fn plugin_preset_save(app: &mut App, args: &Value) -> ToolResult {
    let raw = req_str(args, "name")?;
    match preset_surface(app, args)? {
        Surface::Fx { track, row, stem } => {
            let (_s, metas, values) = target_read(app, &Target::Fx { track, row })?;
            let pairs: Vec<(String, f32)> = metas
                .iter()
                .enumerate()
                .map(|(i, m)| (m.label.clone(), values.get(i).copied().unwrap_or(m.default)))
                .collect();
            let text = fx::preset_to_json(&stem, raw, &pairs).map_err(ToolError::invalid)?;
            let fx_dir = app.mcp05_fx_dir();
            let path = fx::user_preset_path(&fx_dir, &stem, raw);
            fx::ensure_user_presets_dir(&fx_dir).map_err(|e| {
                ToolError::invalid(format!("cannot open the preset folder: {e}"))
            })?;
            std::fs::write(&path, text).map_err(|e| {
                ToolError::invalid(format!("preset write failed: {e}"))
            })?;
            let echo = format!("MCP: fx preset saved: {}", path.file_stem().unwrap_or_default().to_string_lossy());
            app.status = echo.clone();
            Ok(json!({"storage": "fx-user", "name": file_stem(&path), "path": path.display().to_string(),
                      "params": pairs.len(), "status": echo}))
        }
        Surface::Synth { track, stem } => {
            if app.mcp05_synth_handle(track).is_none() {
                return Err(ToolError::not_found(format!("track {track} has no live instrument")));
            }
            app.mcp05_synth_preset_save(track, raw);
            let name = presetbar::sanitize(raw);
            if name.is_empty() || !app.mcp05_synth_preset_list(&stem).contains(&name) {
                let st = app.status.clone();
                return Err(ToolError::invalid(format!("preset save refused ({st})")));
            }
            let echo = format!("MCP: synth preset saved: {name}");
            app.status = echo.clone();
            Ok(json!({"storage": "synth", "name": name, "dir": app.mcp05_synth_preset_dir(&stem).display().to_string(),
                      "status": echo}))
        }
        Surface::SamplerMap { track } => {
            if app.mcp05_sampler_map(track).is_none() {
                return Err(ToolError::invalid(format!(
                    "track {track} has no sampler key map yet — create one with sampler.map.set"
                )));
            }
            app.mcp05_sampler_preset_save(track, raw);
            let name = presetbar::sanitize(raw);
            if name.is_empty() || !App::mcp05_sampler_preset_list().contains(&name) {
                let st = app.status.clone();
                return Err(ToolError::invalid(format!("preset save refused ({st})")));
            }
            let echo = format!("MCP: sampler map preset saved: {name}");
            app.status = echo.clone();
            Ok(json!({"storage": "sampler-map", "name": name,
                      "dir": App::mcp05_sampler_preset_dir().display().to_string(), "status": echo}))
        }
    }
}

fn plugin_preset_load(app: &mut App, args: &Value) -> ToolResult {
    let raw = req_str(args, "name")?;
    match preset_surface(app, args)? {
        Surface::Fx { track, row, stem } => {
            let fx_dir = app.mcp05_fx_dir();
            let path = fx::user_preset_path(&fx_dir, &stem, raw);
            let text = std::fs::read_to_string(&path).map_err(|_| {
                ToolError::not_found(format!(
                    "no fx preset \"{}\" for \"{stem}\" — available: {:?}",
                    fx::sanitize_preset_name(raw),
                    fx::list_user_presets(&fx_dir, &stem)
                ))
            })?;
            let pairs = fx::preset_from_json(&text).map_err(ToolError::invalid)?;
            let applied = app
                .mcp05_fx_apply_preset(track, row, &pairs)
                .map_err(ToolError::invalid)?;
            let tgt = Target::Fx { track, row };
            let (_s, metas, values) = target_read(app, &tgt)?;
            let echo = format!("MCP: fx preset \"{}\" loaded on track {} row {} ({applied} params)",
                fx::sanitize_preset_name(raw), track + 1, row);
            app.status = echo.clone();
            Ok(json!({"storage": "fx-user", "name": fx::sanitize_preset_name(raw),
                      "applied": applied, "observed": params_json(&tgt, &stem, &metas, &values),
                      "status": echo}))
        }
        Surface::Synth { track, stem } => {
            let name = presetbar::sanitize(raw);
            if !app.mcp05_synth_preset_list(&stem).contains(&name) {
                return Err(ToolError::not_found(format!(
                    "no synth preset \"{name}\" for \"{stem}\" — available: {:?}",
                    app.mcp05_synth_preset_list(&stem)
                )));
            }
            app.mcp05_synth_preset_load(track, &name);
            let tgt = Target::Synth { track };
            let (_s, metas, values) = target_read(app, &tgt)?;
            let echo = format!("MCP: synth preset {name} loaded on track {}", track + 1);
            app.status = echo.clone();
            Ok(json!({"storage": "synth", "name": name,
                      "observed": params_json(&tgt, &stem, &metas, &values), "status": echo}))
        }
        Surface::SamplerMap { track } => {
            let name = presetbar::sanitize(raw);
            if !App::mcp05_sampler_preset_list().contains(&name) {
                return Err(ToolError::not_found(format!(
                    "no sampler map preset \"{name}\" — available: {:?}",
                    App::mcp05_sampler_preset_list()
                )));
            }
            app.mcp05_sampler_preset_load(track, &name);
            let st = app.status.clone();
            if !st.contains("loaded") {
                return Err(ToolError::invalid(format!("preset load failed ({st})")));
            }
            let echo = format!("MCP: sampler map preset {name} loaded on track {}", track + 1);
            app.status = echo.clone();
            Ok(json!({"storage": "sampler-map", "name": name,
                      "observed": smap_json(app, track), "status": echo}))
        }
    }
}

fn file_stem(p: &std::path::Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

// ── sampler ──────────────────────────────────────────────────────────────

fn sampler_map_get(app: &mut App, args: &Value) -> ToolResult {
    let t = sampler_track_arg(app, args)?;
    Ok(smap_json(app, t))
}

fn sampler_map_set(app: &mut App, args: &Value) -> ToolResult {
    let t = sampler_track_arg(app, args)?;
    let mut map = app.mcp05_sampler_map(t).unwrap_or_default();
    let mut what = String::new();
    if let Some(mode) = args.get("mode") {
        let m = mode.as_u64().ok_or_else(|| {
            ToolError::invalid("mode must be 0 (cutoff) or 1 (crossfade)")
        })?;
        if m > 1 {
            return Err(ToolError::out_of_range("mode must be 0 (cutoff) or 1 (crossfade)"));
        }
        map.mode = m as u8;
        what = format!("mode {m}");
    }
    if let Some(slot) = opt_usize(args, "slot")? {
        if slot > map.slots.len() {
            return Err(ToolError::out_of_range(format!(
                "slot {slot} — the map holds {} dense slots; pass slot {} to append, or read \
                 sampler.map.get first",
                map.slots.len(),
                map.slots.len()
            )));
        }
        let path = asset_path_arg(app, args)?;
        let root = u8_arg(args, "root", 0, 127)?;
        let vel = u8_arg(args, "vel", 1, 127)?;
        if path.is_none() && root.is_none() && vel.is_none() {
            return Err(ToolError::invalid(
                "a slot edit needs at least one of asset_id/path, root or vel",
            ));
        }
        if slot == map.slots.len() {
            map.slots.push(SamplerSlotSave { asset: String::new(), root: 60, vel: 127 });
        }
        let s = &mut map.slots[slot];
        if let Some(p) = path {
            s.asset = p;
        }
        if let Some(r) = root {
            s.root = r;
        }
        if let Some(v) = vel {
            s.vel = v;
        }
        if !what.is_empty() {
            what.push_str(", ");
        }
        what.push_str(&format!("slot {slot}"));
    } else if args.get("asset_id").is_some() || args.get("path").is_some() || args.get("root").is_some() || args.get("vel").is_some() {
        return Err(ToolError::invalid(
            "asset/root/vel edit one slot and need \"slot\"; only mode is map-level",
        ));
    }
    if what.is_empty() {
        return Err(ToolError::invalid("nothing to do — pass slot{asset_id|path,root,vel} or mode"));
    }
    app.mcp05_sampler_map_commit(t, map);
    let echo = format!("MCP: sampler map track {} — {what} applied", t + 1);
    app.status = echo.clone();
    let mut ack = smap_json(app, t);
    ack["status"] = json!(echo);
    Ok(ack)
}

fn sampler_layers_add(app: &mut App, args: &Value) -> ToolResult {
    let t = sampler_track_arg(app, args)?;
    let mut map = app.mcp05_sampler_map(t).unwrap_or_default();
    let path = asset_path_arg(app, args)?
        .ok_or_else(|| ToolError::invalid("asset_id or path is required"))?;
    let root = req_u64(args, "root")?;
    if root > 127 {
        return Err(ToolError::out_of_range(format!("root must be 0..=127, got {root}")));
    }
    let vel = u8_arg(args, "vel", 1, 127)?.unwrap_or(127);
    map.slots.push(SamplerSlotSave { asset: path, root: root as u8, vel });
    let slot = map.slots.len() - 1;
    app.mcp05_sampler_map_commit(t, map);
    let echo = format!("MCP: sampler layer slot {slot} (root {root}, vel {vel}) added on track {}", t + 1);
    app.status = echo.clone();
    let mut ack = smap_json(app, t);
    ack["added_slot"] = json!(slot);
    ack["status"] = json!(echo);
    Ok(ack)
}

fn sampler_layers_remove(app: &mut App, args: &Value) -> ToolResult {
    let t = sampler_track_arg(app, args)?;
    let mut map = app.mcp05_sampler_map(t).ok_or_else(|| {
        ToolError::not_found(format!("track {t} has no sampler key map"))
    })?;
    let slot = req_u64(args, "slot")? as usize;
    if slot >= map.slots.len() {
        return Err(ToolError::out_of_range(format!(
            "slot {slot} — the map holds {} dense slots (0..{})",
            map.slots.len(),
            map.slots.len().saturating_sub(1)
        )));
    }
    map.slots.remove(slot);
    app.mcp05_sampler_map_commit(t, map);
    let echo = format!("MCP: sampler slot {slot} removed on track {}", t + 1);
    app.status = echo.clone();
    let mut ack = smap_json(app, t);
    ack["removed_slot"] = json!(slot);
    ack["status"] = json!(echo);
    Ok(ack)
}

// ── drum kits (v69 user kits, pads same spirit) ─────────────────────────

fn drum_kit_get(app: &mut App, args: &Value) -> ToolResult {
    let t = track_arg(app, args)?;
    Ok(kit_json(app, t))
}

fn drum_pad_set(app: &mut App, args: &Value) -> ToolResult {
    let t = track_arg(app, args)?;
    let pad = req_u64(args, "pad")? as usize;
    let kit = app.mcp05_kit(t);
    if pad >= kit.len() {
        return Err(ToolError::out_of_range(format!(
            "pad {pad} — track {t}'s kit has {} pads (0..{})",
            kit.len(),
            kit.len().saturating_sub(1)
        )));
    }
    let path = asset_path_arg(app, args)?;
    let vel = u8_arg(args, "vel", 1, 127)?;
    let muted = match args.get("muted") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_bool().ok_or_else(|| ToolError::invalid("muted must be a boolean"))?),
    };
    let name = match args.get("name") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let s = v.as_str().ok_or_else(|| ToolError::invalid("name must be a string"))?;
            let s = s.trim();
            if s.is_empty() || s.chars().count() > 32 {
                return Err(ToolError::out_of_range("pad name must be 1..=32 chars"));
            }
            Some(s.to_string())
        }
    };
    if path.is_none() && vel.is_none() && muted.is_none() && name.is_none() {
        return Err(ToolError::invalid("nothing to do — pass asset_id/path, vel, muted or name"));
    }
    if !app.mcp05_pad_set(t, pad, name.as_deref(), path.as_deref(), vel, muted) {
        return Err(ToolError::out_of_range(format!("pad {pad} went out of range mid-write")));
    }
    let echo = format!("MCP: track {} pad {} updated ({} pads in kit)", t + 1, pad, kit.len());
    app.status = echo.clone();
    let mut ack = kit_json(app, t);
    ack["pad"] = json!(pad);
    ack["status"] = json!(echo);
    Ok(ack)
}

fn drum_preset_list(_app: &mut App, _args: &Value) -> ToolResult {
    Ok(json!({
        "storage": "drum-kit",
        "dir": App::mcp05_kit_preset_dir().display().to_string(),
        "presets": App::mcp05_kit_preset_list(),
        "note": "named kits (v69 storage); pads carry rack paths and re-link per project.",
    }))
}

fn drum_preset_save(app: &mut App, args: &Value) -> ToolResult {
    let t = track_arg(app, args)?;
    let raw = req_str(args, "name")?;
    app.mcp05_kit_preset_save(t, raw);
    let name = presetbar::sanitize(raw);
    if name.is_empty() || !App::mcp05_kit_preset_list().contains(&name) {
        let st = app.status.clone();
        return Err(ToolError::invalid(format!("kit preset save refused ({st})")));
    }
    let echo = format!("MCP: drum kit preset saved: {name}");
    app.status = echo.clone();
    Ok(json!({"storage": "drum-kit", "name": name,
              "dir": App::mcp05_kit_preset_dir().display().to_string(), "status": echo}))
}

fn drum_preset_load(app: &mut App, args: &Value) -> ToolResult {
    let t = track_arg(app, args)?;
    let name = presetbar::sanitize(req_str(args, "name")?);
    if !App::mcp05_kit_preset_list().contains(&name) {
        return Err(ToolError::not_found(format!(
            "no kit preset \"{name}\" — available: {:?}",
            App::mcp05_kit_preset_list()
        )));
    }
    app.mcp05_kit_preset_load(t, &name);
    let st = app.status.clone();
    if !st.starts_with("drum kit preset loaded") {
        return Err(ToolError::invalid(format!("kit preset load failed ({st})")));
    }
    let echo = format!("MCP: drum kit {name} loaded on track {}", t + 1);
    app.status = echo.clone();
    let mut ack = kit_json(app, t);
    ack["preset"] = json!(name);
    ack["status"] = json!(echo);
    Ok(ack)
}

// ── midi patterns (note get/set rebuild via the roll's own seam) ────────

fn midi_get(app: &mut App, args: &Value) -> ToolResult {
    let id = pattern_target(app, args)?;
    pattern_json(app, id)
}

fn midi_note_set(app: &mut App, args: &Value) -> ToolResult {
    let id = pattern_target(app, args)?;
    let note = req_u64(args, "note")?;
    if note > 127 {
        return Err(ToolError::out_of_range(format!("note must be 0..=127, got {note}")));
    }
    let start = args.get("start").map(|_| req_u64(args, "start")).transpose()?.unwrap_or(0);
    let len = args.get("len").map(|_| req_u64(args, "len")).transpose()?.unwrap_or(PPQ as u64);
    let vel = u8_arg(args, "vel", 1, 127)?.unwrap_or(100);
    let ch = u8_arg(args, "ch", 0, 15)?.unwrap_or(0);
    let press = u8_arg(args, "press", 0, 127)?.unwrap_or(0);
    if start > 64 * 4 * PPQ as u64 * 64 || len == 0 || len > 64 * 4 * PPQ as u64 * 64 {
        return Err(ToolError::out_of_range(format!(
            "start/len must fit the pattern grid (1..={} ticks at PPQ {})",
            64 * 4 * PPQ as u64 * 64,
            PPQ
        )));
    }
    let mut pat = app.mcp05_pattern(id).ok_or_else(|| {
        ToolError::wrong_kind(format!("asset {id} has no pattern in memory"))
    })?;
    // ONE note per (pitch, start): an overwrite replaces (idempotent).
    pat.notes.retain(|n| !(n.note as u64 == note && n.start as u64 == start));
    pat.notes.push(Note {
        ch,
        note: note as u8,
        vel,
        press,
        start: start as u32,
        len: len as u32,
        bend: None,
    });
    app.mcp05_pattern_commit(id, pat);
    let after = app.mcp05_pattern(id).ok_or_else(|| {
        let st = app.status.clone();
        ToolError::invalid(format!("pattern commit was refused ({st})"))
    })?;
    let Some(n) = after.notes.iter().find(|n| n.note as u64 == note && n.start as u64 == start)
    else {
        let st = app.status.clone();
        return Err(ToolError::invalid(format!("pattern commit did not land ({st})")));
    };
    let echo = format!("MCP: midi note {note}@{start} set in pattern asset {id} ({} notes)", after.notes.len());
    app.status = echo.clone();
    Ok(json!({
        "asset": id, "note": note, "start": start, "len": n.len, "vel": n.vel,
        "note_count": after.notes.len(), "status": echo,
    }))
}

fn midi_note_remove(app: &mut App, args: &Value) -> ToolResult {
    let id = pattern_target(app, args)?;
    let note = req_u64(args, "note")?;
    if note > 127 {
        return Err(ToolError::out_of_range(format!("note must be 0..=127, got {note}")));
    }
    let start = opt_usize(args, "start")?;
    let mut pat = app.mcp05_pattern(id).ok_or_else(|| {
        ToolError::wrong_kind(format!("asset {id} has no pattern in memory"))
    })?;
    let before = pat.notes.len();
    pat.notes.retain(|n| {
        !(n.note as u64 == note
            && start.map(|s| n.start as usize == s).unwrap_or(true))
    });
    let removed = before - pat.notes.len();
    if removed == 0 {
        return Err(ToolError::not_found(format!(
            "no note {note}{} in pattern asset {id}",
            start.map(|s| format!("@{s}")).unwrap_or_default()
        )));
    }
    app.mcp05_pattern_commit(id, pat);
    let after = app.mcp05_pattern(id).unwrap_or_else(|| crate::midipat::Pattern::new(120));
    let echo = format!("MCP: removed {removed} note(s) {note} from pattern asset {id}");
    app.status = echo.clone();
    Ok(json!({"asset": id, "removed": removed, "note_count": after.notes.len(), "status": echo}))
}

// ── registration ─────────────────────────────────────────────────────────

const TRACK_ARG: &str = "track";

pub fn register(v: &mut Vec<ToolDef>) {
    // helper: object schema with typed properties + required list.
    fn schema(props: Value, req: &[&str]) -> Value {
        json!({"type": "object", "properties": props, "required": req})
    }
    let t = |p: (&str, &str)| json!({ "type": "integer", "minimum": 0, "description": p.1 });
    let props_base = json!({
        TRACK_ARG: t(("track", "0-based track index")),
        "row": { "type": "integer", "minimum": 0,
                 "description": "0-based FX chain row; OMIT for the track's synth target" },
    });
    let mut props_pg = props_base.clone();
    props_pg["param"] = json!({"type": "integer", "minimum": 0, "description": "0-based param index (see plugin.params)"});
    let mut props_ps = props_pg.clone();
    ps_value(&mut props_ps);

    let mut push = |name, description, schema, read, destructive, idempotent, execute| {
        v.push(ToolDef { name, description, schema, read, destructive, idempotent, execute });
    };

    push(
        "plugins.list",
        "The installed native-plugin catalog: every bundled effect and synth script (plus the \
         user's own scripts) with its full param schema (name, min/max, unit, step, enum \
         labels). Auto-derived from the live registries — always complete. Read-only.",
        schema(json!({"kind": {"type": "string", "enum": ["all", "effect", "synth"],
                                "description": "filter the catalog (default all)"}}), &[]),
        true, false, true,
        plugins_list,
    );
    push(
        "plugins.instances",
        "Every instantiated plugin in the live project: each track's instrument and each FX \
         chain slot, with its address ({track} or {track,row}) for the plugin.* tools. Hosted \
         plugins are listed but flagged native:false. Read-only.",
        schema(json!({TRACK_ARG: t((TRACK_ARG, "0-based track index (omit for all tracks)"))}), &[]),
        true, false, true,
        plugins_instances,
    );
    push(
        "plugin.describe",
        "Identity + param schema of one live native plugin instance (track synth when \"row\" \
         is omitted; FX chain slot otherwise): stem, author, sidechain/stereo flags, and per \
         param index/name/min/max/default/unit/step/enum labels. Read-only.",
        schema(props_base.clone(), &["track"]),
        true, false, true,
        plugin_describe,
    );
    push(
        "plugin.params",
        "Live values + full metadata of every param of one native plugin instance. Values are \
         in each param's declared min..max range (the engine's own storage currency — the same \
         numbers preset files carry); enums report the chosen label as \"human\". Read-only.",
        schema(props_base.clone(), &["track"]),
        true, false, true,
        plugin_params,
    );
    push(
        "plugin.param.get",
        "Read one param (value + human string + schema entry) of a native plugin instance. \
         Read-only.",
        schema(props_pg.clone(), &["track", "param"]),
        true, false, true,
        plugin_param_get,
    );
    push(
        "plugin.param.set",
        "Set one param on the LIVE instance through the GUI's own setter seam (rows, live \
         atomics and persistence all move; the next RT block hears it). Value in the param's \
         declared range; enum params must land on a labeled slot. Ack echoes the observed \
         post-change value.",
        schema(props_ps.clone(), &["track", "param", "value"]),
        false, false, true,
        plugin_param_set,
    );
    push(
        "plugin.preset.list",
        "Named presets for the target instance's REAL storage: FX chain slot -> the fx user-\
         preset folder (+ built-in factory presets), track synth -> data-dir synth-presets, \
         sampler track with surface=\"map\" -> the sampler key-map presets. Read-only.",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index")),
            "row": props_base["row"].clone(),
            "surface": {"type": "string", "enum": ["params", "map"],
                        "description": "synth target only: \"params\" slider presets (default) \
                                        or \"map\" sampler key-map presets (sampler tracks)"},
        }), &["track"]),
        true, false, true,
        plugin_preset_list,
    );
    push(
        "plugin.preset.save",
        "Snapshot the instance's live state into its real preset-bar storage (same sanitizer, \
         same folders the GUI bar uses). Names are sanitized; the ack returns the stored name.",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index")),
            "row": props_base["row"].clone(),
            "name": {"type": "string", "description": "preset name (sanitized on disk)"},
            "surface": {"type": "string", "enum": ["params", "map"]},
        }), &["track", "name"]),
        false, false, true,
        plugin_preset_save,
    );
    push(
        "plugin.preset.load",
        "Load a named preset onto the live instance through the GUI's own load path (fxgui's \
         shared pair applier / the synth & sampler map preset loaders). Ack carries the \
         observed post-load state.",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index")),
            "row": props_base["row"].clone(),
            "name": {"type": "string"},
            "surface": {"type": "string", "enum": ["params", "map"]},
        }), &["track", "name"]),
        false, false, true,
        plugin_preset_load,
    );
    push(
        "sampler.map.get",
        "The bundled sampler's key map for a track, DECOMPOSED: dense slots of \
         {slot, asset path, asset_id (rack index, null = missing), root 0..127, vel 1..127} \
         plus mode (0 cutoff / 1 crossfade) and the per-root velocity-layer view. Read-only.",
        schema(json!({TRACK_ARG: t((TRACK_ARG, "0-based track index (must play the sampler)"))}), &["track"]),
        true, false, true,
        sampler_map_get,
    );
    push(
        "sampler.map.set",
        "Edit one sampler key-map slot (or the map mode) and publish it through the v51 seam \
         — the live instance hears it immediately. slot == count appends. Assets must already \
         be in the rack (asset_id table index, or exact path). vel is the layer's recorded-
         dynamics unity point.",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index (must play the sampler)")),
            "slot": {"type": "integer", "minimum": 0, "description": "dense slot index (== count appends)"},
            "asset_id": {"type": "integer", "minimum": 0, "description": "rack table index (audio asset)"},
            "path": {"type": "string", "description": "exact rack asset path (alternative to asset_id)"},
            "root": {"type": "integer", "minimum": 0, "maximum": 127, "description": "MIDI root note"},
            "vel": {"type": "integer", "minimum": 1, "maximum": 127},
            "mode": {"type": "integer", "minimum": 0, "maximum": 1, "description": "0 cutoff, 1 crossfade"},
        }), &["track"]),
        false, false, true,
        sampler_map_set,
    );
    push(
        "sampler.layers.add",
        "Add a velocity LAYER: append a slot on a root note (several slots share a root and \
         switch by note velocity per the map mode). Publishes through the v51 seam.",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index (must play the sampler)")),
            "root": {"type": "integer", "minimum": 0, "maximum": 127},
            "asset_id": {"type": "integer", "minimum": 0},
            "path": {"type": "string"},
            "vel": {"type": "integer", "minimum": 1, "maximum": 127,
                    "description": "unity point of this layer (default 127)"},
        }), &["track", "root"]),
        false, false, false,
        sampler_layers_add,
    );
    push(
        "sampler.layers.remove",
        "Remove one key-map slot by dense index (layers shift down; the map re-publishes).",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index (must play the sampler)")),
            "slot": {"type": "integer", "minimum": 0},
        }), &["track", "slot"]),
        false, true, true,
        sampler_layers_remove,
    );
    push(
        "drum.kit.get",
        "One track's drum kit (v30 R1 kits are per track): pads with name, rack path, \
         asset_id, default velocity and mute. Read-only.",
        schema(json!({TRACK_ARG: t((TRACK_ARG, "0-based track index"))}), &["track"]),
        true, false, true,
        drum_kit_get,
    );
    push(
        "drum.pad.set",
        "Edit one drum pad (sound via rack asset_id/path, default velocity 1..127, mute, \
         name) through the kit's own resolve+persist seam.",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index")),
            "pad": {"type": "integer", "minimum": 0},
            "asset_id": {"type": "integer", "minimum": 0},
            "path": {"type": "string"},
            "vel": {"type": "integer", "minimum": 1, "maximum": 127},
            "muted": {"type": "boolean"},
            "name": {"type": "string", "maxLength": 32},
        }), &["track", "pad"]),
        false, false, true,
        drum_pad_set,
    );
    push(
        "drum.preset.list",
        "Named drum-kit presets (the v69 user-kit folder). Read-only.",
        schema(json!({}), &[]),
        true, false, true,
        drum_preset_list,
    );
    push(
        "drum.preset.save",
        "Save one track's kit as a named preset (v69 storage, shared sanitizer).",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index")),
            "name": {"type": "string"},
        }), &["track", "name"]),
        false, false, true,
        drum_preset_save,
    );
    push(
        "drum.preset.load",
        "Load a named kit preset onto a track through the v69 loader (pads re-link against \
         this project's rack). Ack carries the observed kit.",
        schema(json!({
            TRACK_ARG: t((TRACK_ARG, "0-based track index")),
            "name": {"type": "string"},
        }), &["track", "name"]),
        false, false, true,
        drum_preset_load,
    );
    push(
        "midi.get",
        "Read a MIDI pattern asset: bpm, length, and its notes {note, vel, start, len, ch, \
         press} (start/len in ticks, PPQ 480). Read-only.",
        schema(json!({"asset": {"type": "integer", "minimum": 0,
                                "description": "0-based rack id of a MIDI pattern asset"}}), &["asset"]),
        true, false, true,
        midi_get,
    );
    push(
        "midi.note.set",
        "Add/overwrite ONE note (pitch+start is the key) in a pattern asset and commit \
         through the piano roll's in-place seam (.mid save + engine pattern table) — every \
         clip already pointing at the asset plays the update.",
        schema(json!({
            "asset": {"type": "integer", "minimum": 0},
            "note": {"type": "integer", "minimum": 0, "maximum": 127},
            "start": {"type": "integer", "minimum": 0, "description": "ticks (default 0)"},
            "len": {"type": "integer", "minimum": 1, "description": "ticks (default 480 = 1 beat)"},
            "vel": {"type": "integer", "minimum": 1, "maximum": 127, "description": "default 100"},
            "ch": {"type": "integer", "minimum": 0, "maximum": 15},
            "press": {"type": "integer", "minimum": 0, "maximum": 127},
        }), &["asset", "note"]),
        false, false, true,
        midi_note_set,
    );
    push(
        "midi.note.remove",
        "Remove note(s) from a pattern asset: {note} drops the whole pitch row, \
         {note,start} drops one hit. Commits through the roll's in-place seam.",
        schema(json!({
            "asset": {"type": "integer", "minimum": 0},
            "note": {"type": "integer", "minimum": 0, "maximum": 127},
            "start": {"type": "integer", "minimum": 0},
        }), &["asset", "note"]),
        false, true, true,
        midi_note_remove,
    );
}

fn ps_value(props: &mut Value) {
    props["value"] = json!({"type": "number",
        "description": "new value in the param's declared min..max range (enum params: exactly \
                        a labeled slot value)"});
}
