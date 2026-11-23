# Driving slooper from an agent (MCP)

slooper has a built-in [MCP](https://modelcontextprotocol.io) server. When
you enable it, an MCP client (Claude Desktop, Cline, coding agents…) talks
to **the running instance** and drives it live: settings, subwindow
arrangement, mixer, FX chains, the asset rack, plugin params, the sampler
key map, drum kits and MIDI patterns. It is off by default — no socket is
opened until you tick it in Settings.

The registry today exposes **45 tools**; every one is documented below,
generated from the live registry itself.

## Enable it

Open **Settings ▸ AUTOMATION** (type `mcp` in the settings filter):

- **MCP server (control this instance)** — starts/stops immediately; a
  restart is never needed.
- **port** — loopback port to listen on. `0` (the default) picks any free
  port; the port actually used is always in the endpoint file.
- **read-only (the agent may not write)** — a hard latch: every writing
  tool is refused until you untick it, reads keep working, and the agent's
  connection survives the flip (it applies to the very next tool call).

While running, the section shows `listening on 127.0.0.1:<port> — loopback
only` and the bearer token (hidden until you press *reveal*). The same
values live in the **endpoint file**:

| OS | location |
| --- | --- |
| Linux | `~/.local/share/slooper/mcp-endpoint.json` (honours `$XDG_DATA_HOME`) |
| Windows | `%LOCALAPPDATA%\slooper\mcp-endpoint.json` |

`SLOOPER_DATA_DIR=<dir>` overrides both. The file is JSON
`{port, token, pid, version, name}`, mode 0600 on Unix, and is removed
again when the server stops.

## Connect: the stdio bridge (recommended)

Most clients — Claude Desktop included — speak MCP over a stdio pipe, so
slooper ships a shim that turns that pipe into authenticated HTTP. Claude
Desktop's `claude_desktop_config.json` (same `mcpServers` shape in any
client's `mcp.json`):

```json
{
  "mcpServers": {
    "slooper": {
      "command": "slooper",
      "args": ["--mcp-bridge"]
    }
  }
}
```

Point `command` at your slooper binary (`C:\path\to\slooper.exe` on
Windows). The bridge re-reads the endpoint file **per request**, so an app
restart — new port, new token — is picked up with no client-side changes;
`mcp.json` never embeds the token. Start slooper with MCP enabled first:
until the endpoint file exists the bridge logs a hint to stderr and drops
lines.

## Connect: HTTP directly

For your own client, talk JSON-RPC 2.0 straight to the server:

- `POST http://127.0.0.1:<port>/mcp`
- headers `Authorization: Bearer <token>` and `Content-Type: application/json`
- methods: `tools/list`, `tools/call`, plus `initialize`
  (protocol eras 2024-11-05…2025-11-25, optional) and stateless
  `server/discover` (2026-07-28). JSON responses only — no SSE; `GET`
  answers 405.

Smoke test from a shell (`<port>`/`<token>` from the endpoint file):

```sh
curl -s -X POST http://127.0.0.1:<port>/mcp \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"core.echo","arguments":{"text":"hi"}}}'
```

On Windows the same works with `curl.exe` in PowerShell.

## Guard rails

- **Loopback only** — the bind address `127.0.0.1` is hardcoded; nothing
  reaches the network. Off by default; disabling closes every socket and
  deletes the endpoint file.
- **Requests are checked**: Host allow-list and Origin check (a browser
  page cannot drive-by — 403), then the bearer token (401). The token is a
  same-user secret: it defends against other programs in your browser, not
  against someone who owns your account and your files.
- **Bounded**: at most 8 live connections and 16 queued commands (full
  queue → JSON-RPC `-32001`); a call whose result hasn't reached the app's
  frame loop within 5 s gets `-32002`. Neither hangs the app or the client.
- **The DAW never dies for an agent bug**: every tool runs inside a panic
  firewall (`-32000`, server keeps serving) and touches app state only on
  the UI frame loop, through the same setter seams the GUI uses.
- **Read-only latch**: while ticked, write tools answer `-32003`
  (`read_only`) until you untick it.

## Conventions the tools follow

- **Observed-then-ack** — write tools return the post-change state re-read
  through the same getters the read tools use; if the ack says it, it's
  true.
- **Units** are each setter seam's native currency: mixer values `0..=255`,
  plugin params in their declared `min..max` (presets' own numbers), MIDI
  notes `0..=127`, `start`/`len` in pattern ticks, every id 0-based.
- **Addresses** are stable keys, never slot positions: windows by id
  (`rack`, `binds`, `monitor`, `mixer`, `drums`, `master`,
  `chain:<track>`, `synth:<track>`, `roll:<asset>`, … — `windows.list` is
  the live truth); everything else by flat indices (`track`, `row`, `slot`,
  `pad`, `lane`, `param`).
- **Annotations** — every descriptor carries `readOnlyHint` (the read-only
  latch's source of truth), `destructiveHint`, `idempotentHint`,
  `openWorldHint: false`.
- **Errors** — domain failures come back as a *successful* result with
  `isError: true` and `structuredContent.error` set to one of
  `invalid_params`, `not_found`, `out_of_range`, `wrong_kind`,
  `read_only`, `busy`, plus a message written to be actionable.

## Tool reference

The sections below are the live registry (names, descriptions, argument
schemas and annotations exactly as the server publishes them).
`*read.*` tools never change state and survive the read-only latch;
`*write.*` tools are refused while it is ticked.

### core — smoke test

#### `core.echo`
*read.* Round-trip a string through the live app (bridge demo / smoke test). Read-only: touches no state.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `text` | string | yes | the text to echo back |

### settings.* — persisted configuration

#### `settings.list`
*read.* Every persisted setting with JSON type, current live value, requires-restart flag and unit/enum meta. Auto-derived from the config schema — always complete. Read-only.

No arguments.

#### `settings.get`
*read.* Read one setting's live value + schema entry (name, json_type, requires_restart, unit/enum meta where available). Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `name` | string | yes | setting name from settings.list |

#### `settings.set`
*write.* Write one setting: validated (serde + ranges/enums), applied LIVE through the GUI's own setter seams when the field is hot-appliable, or persisted for the next start when it is not (requires_restart=true is then surfaced here and in settings.list). Echoes the observed post-change state. Unknown fields and bad values are refused with nothing changed.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `name` | string | yes | setting name from settings.list |
| `value` | any JSON | yes | new value; JSON type must match the field's json_type |

### windows.* — subwindow arrangement

#### `windows.list`
*read.* List every window the agent can address: `windows` = the dock/floating roster (4 primaries + every dynamic window opened this session) with id, kind, title, open, docked, edge_name (left/right/bottom/top), rank along the edge, rect [x,y,w,h] (live dock rect when docked, float rect otherwise; null only for a window docked before its first drawn frame) and float_rect. `editors` = host-side native plugin editor windows (WM-managed; list/close only — see windows.move). Read-only: touches nothing.

No arguments.

#### `windows.open`
*write.* Open a window by id. Primaries (rack/binds/monitor/mixer) and drums/master/chain:<track>/synth:<track>/synths:<track> always; roll with optional {asset} (MIDI asset id — absent = fresh pattern); auto with optional {asset} (automation asset); params ONLY by target id (params:synth:<track> or params:chain:<track>:<row>, needs a live plugin instance); fx:<track>:<row> needs a script FX on that row. Native plugin EDITOR windows (vst:*, vstfx:*) are NOT opened here (crash-guarded GUI action) — an already-open editor id returns its state. Ack echoes the window through the windows.list getter.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset` | integer | no | roll/auto only: asset id (assets.list). roll: MIDI asset (absent = fresh pattern); auto: automation asset; min 0 |
| `id` | string | yes | stable window id from windows.list (e.g. rack, mixer, drums, roll, chain:<track>, params:synth:<track>, params:chain:<track>:<row>, vst:<track>, vstfx:<track>:<row>) |

#### `windows.close`
*write.* Close a window by id (roster ids AND native plugin editor ids vst:<track> / vstfx:<track>:<row>, which close through the same in-place protocol the editor's X button uses — state chunks are harvested). Closes are idempotent for the agent: closing a closed window is a structured not_found, never a side effect. Ack echoes the closed window's state.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `id` | string | yes | stable window id from windows.list (e.g. rack, mixer, drums, roll, chain:<track>, params:synth:<track>, params:chain:<track>:<row>, vst:<track>, vstfx:<track>:<row>) |

#### `windows.move`
*write.* Move a FLOATING window's top-left (viewport coordinates, screen points). The position is clamped so the title band stays reachable. Docked windows refuse (wrong_kind) — docked geometry belongs to the dock; call windows.undock first. Native editor windows (windows.editors) refuse: they are WM-managed top-levels (v1 boundary). Ack echoes the post-change rect.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `id` | string | yes | stable window id from windows.list (e.g. rack, mixer, drums, roll, chain:<track>, params:synth:<track>, params:chain:<track>:<row>, vst:<track>, vstfx:<track>:<row>) |
| `x` | number | yes | window top-left x |
| `y` | number | yes | window top-left y |

#### `windows.resize`
*write.* Resize a FLOATING window (w, h in screen points; clamped to >=120x80 and <= the viewport). Docked windows refuse (wrong_kind — dock thickness is drag-only; undock first). Native editor windows refuse (WM-managed; v1 boundary). Ack echoes the post-change rect.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `h` | number | yes | height >=80 |
| `id` | string | yes | stable window id from windows.list (e.g. rack, mixer, drums, roll, chain:<track>, params:synth:<track>, params:chain:<track>:<row>, vst:<track>, vstfx:<track>:<row>) |
| `w` | number | yes | width >=120 |

#### `windows.dock`
*write.* Dock a window onto an edge (edge: left|right|bottom|top or 0..3; rank: insertion slot along the edge, 0-based, default = tail; both echoed post-change). Ranks re-pack deterministically among the edge's tab groups, exactly like a mouse drop. Already-docked ids re-rank/re-edge. Native editor windows refuse (WM-managed; v1 boundary).

| argument | type | required | notes |
| --- | --- | --- | --- |
| `edge` | any JSON | yes | left \| right \| bottom \| top (or 0..3) |
| `id` | string | yes | stable window id from windows.list (e.g. rack, mixer, drums, roll, chain:<track>, params:synth:<track>, params:chain:<track>:<row>, vst:<track>, vstfx:<track>:<row>) |
| `rank` | integer | no | slot along the edge (0-based); default tail; min 0 |

#### `windows.undock`
*write.* Float a docked window (optional {x,y} top-left, clamped so the title band stays reachable; default = its dock rect, else the stock footprint). Already-floating ids echo unchanged (idempotent). Native editor windows refuse (WM-managed; v1 boundary). Ack echoes the post-change rect.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `id` | string | yes | stable window id from windows.list (e.g. rack, mixer, drums, roll, chain:<track>, params:synth:<track>, params:chain:<track>:<row>, vst:<track>, vstfx:<track>:<row>) |
| `x` | number | no | optional new top-left x |
| `y` | number | no | optional new top-left y |

#### `windows.layout.save`
*write, not idempotent.* Capture the ENTIRE arrangement (every float rect, dock edge/rank, tab group and open flag) into a named layout template, persisted with the config. Re-capturing a name replaces it. The stock arrangement auto-persists across restarts on its own; templates are for variants. Named layouts also appear in the Windows menu.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `name` | string | yes | template name (re-capture replaces) |

#### `windows.layout.restore`
*write.* Apply a saved layout template by name — the same machinery the Windows menu uses: all rects, dock slots, tab groups and visibility (windows that need a live target, e.g. params, keep their geometry but reopen through their own tool). Unknown names list the saved layouts. Ack echoes the roster.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `name` | string | yes | saved template name |

### mixer.* — track levels and gates

#### `mixer.list`
*read.* Every track's live mixer state (vol/gain/pan/color/sidechain sends) plus the master-out volume, in the seams' native units. Read-only.

No arguments.

#### `mixer.get`
*read.* One track's mixer state (omit `track` for the master-out entry). Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `track` | integer | no | 0-based track index |

#### `mixer.set`
*write.* Write one mixer control through the GUI's own setter seams. Controls: vol | gain | pan | color | sc_send (needs track) and master_vol (no track). Mixer values are MIDI-range ints 0..=127 (vol: 0=-inf, 115=0 dB; gain: 80=0 dB; pan: 64=centre); color = palette index 0..9 or null; sc_send = array of destination track indices. Echoes the observed post-change state.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `control` | string | yes | vol \| gain \| pan \| color \| sc_send \| master_vol |
| `track` | integer | no | 0-based track index (omit only for master_vol) |
| `value` | any JSON | yes | MIDI 0..=127 int; color: palette int 0..9 or null; sc_send: array of destination track indices |

### chain.* — FX chains

#### `chain.catalog`
*read.* What chain.pick can place: effect scripts (id = stem) and scanned plugins (id = file path; name also accepted), with kind, instrument flag and scan errors — the same two combos the MIXER CHAIN window shows. Read-only.

No arguments.

#### `chain.list`
*read.* One track's live effect chain: every slot with kind, name, id, status (loading | ready | failed | dead), bypass and editor_open. Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `track` | integer | yes | 0-based track index |

#### `chain.pick`
*write, not idempotent.* Append an effect to a track's chain — the exact “+ effect” / “+ VST plugin” combo code path. `plugin` is an id or exact name from chain.catalog. Scripts ack status=“added”; plugins ack status=“loading” (async probe on the plugin thread — poll chain.list until status=“ready”). Optional `slot` relocates the new row (insert semantics). Instruments are refused — they bind via ♪.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `plugin` | string | yes | script stem or plugin path/name from chain.catalog |
| `slot` | integer | no | optional final slot index (default: append) |
| `track` | integer | yes | 0-based track index |

#### `chain.remove`
*write, destructive, not idempotent.* Remove one chain slot (the row's ✕ button seam: rebuild + persist + echo; a hosted plugin instance is held and reaped on the UI thread exactly as the GUI path does).

| argument | type | required | notes |
| --- | --- | --- | --- |
| `slot` | integer | yes | slot index from chain.list |
| `track` | integer | yes | 0-based track index |

### assets.* — the asset rack

#### `assets.list`
*read.* The asset rack: every scanned rack folder (files with kind and path) and the live asset table (asset = 0-based table id). Read-only.

No arguments.

#### `assets.import`
*write.* Import a file into the live rack (the OS-drop seam: `ensure_asset` decodes + dedups; the source file is NEVER modified) or add a folder as a new rack root (the drop-a-folder seam). Echoes the new table entry.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `path` | string | yes | file (.wav/.aiff/.flac/.mid/.slauto …) or folder path |

#### `assets.remove`
*write, destructive, not idempotent.* Delete by table id (`asset`: table-only removal, ids remap — the remove_asset_at seam) or by file `path` (the rack ✕ seam — deletes from disk, REFUSED unless the file sits inside a scanned rack folder: foreign files are never mutated).

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset` | integer | no | asset table id from assets.list |
| `path` | string | no | rack file path (must be inside a rack folder) |

#### `assets.assign`
*write, not idempotent.* The rack-drag drop seams as a direct call. target=“sampler”: becomes the bound script synth's asset (needs a script synth on the track). target=“keymap”: adds a sampler key-map slot (C3, the “no samples mapped” drop zone). target=“pad” (+ pad: N): the drum kit pad sound (the per-pad “wav” button seam). target=“channel” (+ optional lane, pos in samples): places the asset as a clip on the arrange grid (the lane-strip drop contract). Source = `asset` (table id) or `path` (imported first via the same seam). Mixer values are MIDI-range ints 0..=127; asset ids are table indices.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset` | integer | no | asset table id (or give path) |
| `lane` | integer | no | lane index, default 0 (target=channel) |
| `pad` | integer | no | pad index (target=pad) |
| `path` | string | no | file to import-then-assign (or give asset) |
| `pos` | integer | no | clip position in samples, default 0 (target=channel) |
| `target` | string | yes | sampler \| keymap \| pad \| channel |
| `track` | integer | yes | 0-based track index |

### plugins.* — catalog and instances (read-only survey)

#### `plugins.list`
*read.* The installed native-plugin catalog: every bundled effect and synth script (plus the user's own scripts) with its full param schema (name, min/max, unit, step, enum labels). Auto-derived from the live registries — always complete. Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `kind` | string | no | filter the catalog (default all); one of: `all`, `effect`, `synth` |

#### `plugins.instances`
*read.* Every instantiated plugin in the live project: each track's instrument and each FX chain slot, with its address ({track} or {track,row}) for the plugin.* tools. Hosted plugins are listed but flagged native:false. Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `track` | integer | no | 0-based track index (omit for all tracks); min 0 |

### plugin.* — one live native plugin instance

#### `plugin.describe`
*read.* Identity + param schema of one live native plugin instance (track synth when "row" is omitted; FX chain slot otherwise): stem, author, sidechain/stereo flags, and per param index/name/min/max/default/unit/step/enum labels. Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `row` | integer | no | 0-based FX chain row; OMIT for the track's synth target; min 0 |
| `track` | integer | yes | 0-based track index; min 0 |

#### `plugin.params`
*read.* Live values + full metadata of every param of one native plugin instance. Values are in each param's declared min..max range (the engine's own storage currency — the same numbers preset files carry); enums report the chosen label as "human". Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `row` | integer | no | 0-based FX chain row; OMIT for the track's synth target; min 0 |
| `track` | integer | yes | 0-based track index; min 0 |

#### `plugin.param.get`
*read.* Read one param (value + human string + schema entry) of a native plugin instance. Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `param` | integer | yes | 0-based param index (see plugin.params); min 0 |
| `row` | integer | no | 0-based FX chain row; OMIT for the track's synth target; min 0 |
| `track` | integer | yes | 0-based track index; min 0 |

#### `plugin.param.set`
*write.* Set one param on the LIVE instance through the GUI's own setter seam (rows, live atomics and persistence all move; the next RT block hears it). Value in the param's declared range; enum params must land on a labeled slot. Ack echoes the observed post-change value.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `param` | integer | yes | 0-based param index (see plugin.params); min 0 |
| `row` | integer | no | 0-based FX chain row; OMIT for the track's synth target; min 0 |
| `track` | integer | yes | 0-based track index; min 0 |
| `value` | number | yes | new value in the param's declared min..max range (enum params: exactly a labeled slot value) |

#### `plugin.preset.list`
*read.* Named presets for the target instance's REAL storage: FX chain slot -> the fx user-preset folder (+ built-in factory presets), track synth -> data-dir synth-presets, sampler track with surface="map" -> the sampler key-map presets. Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `row` | integer | no | 0-based FX chain row; OMIT for the track's synth target; min 0 |
| `surface` | string | no | synth target only: "params" slider presets (default) or "map" sampler key-map presets (sampler tracks); one of: `params`, `map` |
| `track` | integer | yes | 0-based track index; min 0 |

#### `plugin.preset.save`
*write.* Snapshot the instance's live state into its real preset-bar storage (same sanitizer, same folders the GUI bar uses). Names are sanitized; the ack returns the stored name.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `name` | string | yes | preset name (sanitized on disk) |
| `row` | integer | no | 0-based FX chain row; OMIT for the track's synth target; min 0 |
| `surface` | string | no | one of: `params`, `map` |
| `track` | integer | yes | 0-based track index; min 0 |

#### `plugin.preset.load`
*write.* Load a named preset onto the live instance through the GUI's own load path (fxgui's shared pair applier / the synth & sampler map preset loaders). Ack carries the observed post-load state.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `name` | string | yes |  |
| `row` | integer | no | 0-based FX chain row; OMIT for the track's synth target; min 0 |
| `surface` | string | no | one of: `params`, `map` |
| `track` | integer | yes | 0-based track index; min 0 |

### sampler.* — the sampler key map

#### `sampler.map.get`
*read.* The bundled sampler's key map for a track, DECOMPOSED: dense slots of {slot, asset path, asset_id (rack index, null = missing), root 0..127, vel 1..127} plus mode (0 cutoff / 1 crossfade) and the per-root velocity-layer view. Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `track` | integer | yes | 0-based track index (must play the sampler); min 0 |

#### `sampler.map.set`
*write.* Edit one sampler key-map slot (or the map mode) and publish it through the v51 seam — the live instance hears it immediately. slot == count appends. Assets must already be in the rack (asset_id table index, or exact path). vel is the layer's recorded-
         dynamics unity point.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset_id` | integer | no | rack table index (audio asset); min 0 |
| `mode` | integer | no | 0 cutoff, 1 crossfade; min 0; max 1 |
| `path` | string | no | exact rack asset path (alternative to asset_id) |
| `root` | integer | no | MIDI root note; min 0; max 127 |
| `slot` | integer | no | dense slot index (== count appends); min 0 |
| `track` | integer | yes | 0-based track index (must play the sampler); min 0 |
| `vel` | integer | no | min 1; max 127 |

#### `sampler.layers.add`
*write, not idempotent.* Add a velocity LAYER: append a slot on a root note (several slots share a root and switch by note velocity per the map mode). Publishes through the v51 seam.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset_id` | integer | no | min 0 |
| `path` | string | no |  |
| `root` | integer | yes | min 0; max 127 |
| `track` | integer | yes | 0-based track index (must play the sampler); min 0 |
| `vel` | integer | no | unity point of this layer (default 127); min 1; max 127 |

#### `sampler.layers.remove`
*write, destructive.* Remove one key-map slot by dense index (layers shift down; the map re-publishes).

| argument | type | required | notes |
| --- | --- | --- | --- |
| `slot` | integer | yes | min 0 |
| `track` | integer | yes | 0-based track index (must play the sampler); min 0 |

### drum.* — the drum kit

#### `drum.kit.get`
*read.* One track's drum kit (v30 R1 kits are per track): pads with name, rack path, asset_id, default velocity and mute. Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `track` | integer | yes | 0-based track index; min 0 |

#### `drum.pad.set`
*write.* Edit one drum pad (sound via rack asset_id/path, default velocity 1..127, mute, name) through the kit's own resolve+persist seam.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset_id` | integer | no | min 0 |
| `muted` | boolean | no |  |
| `name` | string | no |  |
| `pad` | integer | yes | min 0 |
| `path` | string | no |  |
| `track` | integer | yes | 0-based track index; min 0 |
| `vel` | integer | no | min 1; max 127 |

#### `drum.preset.list`
*read.* Named drum-kit presets (the v69 user-kit folder). Read-only.

No arguments.

#### `drum.preset.save`
*write.* Save one track's kit as a named preset (v69 storage, shared sanitizer).

| argument | type | required | notes |
| --- | --- | --- | --- |
| `name` | string | yes |  |
| `track` | integer | yes | 0-based track index; min 0 |

#### `drum.preset.load`
*write.* Load a named kit preset onto a track through the v69 loader (pads re-link against this project's rack). Ack carries the observed kit.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `name` | string | yes |  |
| `track` | integer | yes | 0-based track index; min 0 |

### midi.* — MIDI pattern assets (piano-roll data)

#### `midi.get`
*read.* Read a MIDI pattern asset: bpm, length, and its notes {note, vel, start, len, ch, press} (start/len in ticks, PPQ 480). Read-only.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset` | integer | yes | 0-based rack id of a MIDI pattern asset; min 0 |

#### `midi.note.set`
*write.* Add/overwrite ONE note (pitch+start is the key) in a pattern asset and commit through the piano roll's in-place seam (.mid save + engine pattern table) — every clip already pointing at the asset plays the update.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset` | integer | yes | min 0 |
| `ch` | integer | no | min 0; max 15 |
| `len` | integer | no | ticks (default 480 = 1 beat); min 1 |
| `note` | integer | yes | min 0; max 127 |
| `press` | integer | no | min 0; max 127 |
| `start` | integer | no | ticks (default 0); min 0 |
| `vel` | integer | no | default 100; min 1; max 127 |

#### `midi.note.remove`
*write, destructive.* Remove note(s) from a pattern asset: {note} drops the whole pitch row, {note,start} drops one hit. Commits through the roll's in-place seam.

| argument | type | required | notes |
| --- | --- | --- | --- |
| `asset` | integer | yes | min 0 |
| `note` | integer | yes | min 0; max 127 |
| `start` | integer | no | min 0 |

## Troubleshooting

| symptom | meaning / fix |
| --- | --- |
| bridge prints `no usable endpoint file` | slooper isn't running with MCP on — enable it in Settings ▸ AUTOMATION, then the bridge picks the file up per request. |
| `401 Unauthorized` | the token rotated (every server start mints a new one). The bridge re-reads automatically; direct HTTP clients must re-read `mcp-endpoint.json`. |
| status line `MCP could not start on port N` | another program holds the port — pick a different number or leave `0`. |
| `-32001` / `-32002` | the app's frame loop is backed up (heavy render, a modal editor). The app is not dead; retry shortly. |
| `405 Method Not Allowed` | wrong verb or path — it is `POST /mcp` only. |
| write refused with `read_only` | the read-only latch is ticked in Settings ▸ AUTOMATION. |

## See also

- [docs/MCP-DESIGN.md](MCP-DESIGN.md) — the internal architecture and its
  decision list (transport, threading, threat model).
- [docs/AGENT-GUIDE.md](AGENT-GUIDE.md) — the code-level map of the repo.
- This reference is derived from the live tool registry
  (`cargo run --example mcp_tool_dump`); the pin
  `mcp_docs_cover_live_tool_table` fails CI if a shipped tool ever has no
  section here.
