# Sampler crossfade curves — design (SAMP-XF-0)

Status: **accepted design; binding** for SAMP-XF-1 (engine curves,
t_16b88781), SAMP-XF-2 (picker + preview UI, t_c5364e27), SAMP-XF-3
(custom curve presets: data model + eval + persistence, t_2330024a),
SAMP-XF-4 (preset management UI, t_72c1ddbc) and SAMP-LP-3 (loop-seam
crossfade shape, t_0743a172). Filed by plugin-designer 2026-11-28 from
the user's request: *crossfades should offer selectable curve types like
the automation manager, and user-drawn curves should be savable with the
instrument.*

No product code in this document's PR. Every decision below is a law
("MUST / never / exactly"); downstream cards implement, they do not
re-decide. Where a card body contradicts this document, **this document
wins** (the contradictions are named explicitly in § 8).

Terms: **root axis** = crossfade between key zones (the two roots that
bracket a note); **velocity axis** = crossfade between velocity layers
inside one root (v52 R2); **blend fraction** `t` = the axis position
already computed by today's code (0 = fully LOW side, 1 = fully HIGH
side); **crossfade curve** = a warp `f: [0,1] → [0,1]` plus the rule
that turns it into the two voice weights.

## 1. Today (grounding)

* Root crossfade (`synths/sampler.rs`, `note_on`, mode 1): nearest root
  at/below the note and nearest above it, `w_hi = ((n − lo.root) /
  span).clamp(0, 1)`, lower side gets `1 − w_hi`. Weights are multiplied
  into each voice's gain (`start_voice`'s `w`, then
  `v.vel = vel_gain(vel, slot.vel) * w`) **once per note-on** — the
  blend is static across the note's life, not per sample.
* Velocity crossfade (`crossfade_layers`): the two bracketing layers,
  `t = (v − lv) / (hv − lv)`, weights `(1 − t, t)`. Root-axis and
  axis-axis weights multiply (`ww = (1 − w_hi) * w_layer`).
* Map plumbing (all existing, all reused here): the per-track map lives
  in `config::SamplerMapSave { slots, mode }` inside `Config.sampler_maps`
  (project JSON, `src/config.rs`); the UI pushes it through
  `SynthHandle::set_map(slots, mode)` → `map_mode: AtomicU32` whose low
  8 bits are the mode and bit 31 (`MAP_EXISTS`) means "a map exists"
  (`src/synth.rs`); the script reads it in `sync_map` via
  `SynthHostV1::load_map(_, _, bits)` (`mode = bits & 0xFF`).
  Bits **8..30 of the reply are unused by any code today**.
* The map window paints the mode as a two-button selectable pair
  ("hard cutoff" / "crossfade", `src/ui.rs` ~16229) and echoes every
  change into `self.status` ("sampler map: mode · crossfade").
* `src/sample_edit.rs`: **per-asset** render edits (`SampleEdit`,
  keyed by asset path in `project.sample_edits`), applied **offline**
  (reverse/stretch/fades). It already ships a 7-arm fade-shape enum
  (`Curve::gain`) for fades. It is a `Copy` struct; every field must be
  listed in every literal (repo law).
* `src/automation.rs`: the automation curve model — `Node { t, v, c }`
  where `c ∈ [−1..=1]` is the continuous **curvature of the segment
  leading into the node**, evaluated by `warp(c, u)` (c = 0 identity,
  c = +1 ease-out cubic, c = −1 ease-in cubic, endpoints exact,
  monotonic for every c); `Preset` = 9 one-click shapes; `.slauto` =
  one JSON asset per curve, rack-managed.
* Script synths compile standalone (`include!("synth_api.rs")`): a
  script CANNOT import host modules. The host vtable grows by APPENDING
  fields — v164 added `tune_key` with `SLOOPER_SYNTH_ABI` still 1, and
  old script copies of `synth_api.rs` keep working (they never read the
  new tail). `src/automation.rs`'s `warp` is f32-only and RT-safe; the
  script runtime is a separate compilation unit, so any math it needs
  must exist inside the script (same as `vel_gain` etc. do today).

## 2. Decision (a) — curve representation: enum + borrowed curvature

**Neither a full reuse of `automation::Node` nor a bare enum: a new
lightweight module `src/xfcurve.rs` holding (i) a closed enum of
built-in blend curves and (ii) a custom curve = a node list that reuses
the automation CURVATURE LAW (`c` semantics and `warp(c, u)` — host side
calls `automation::warp` directly, no formula fork), but NOT the
automation types.**

Rejected alternatives, on the record:
* *Reuse `Automation` as-is*: wrong units (`t` is a sample position,
  `v` an absolute target value in the target's unit), rack-asset
  lifecycle (`AssetKind::Automation`, path-keyed) bleeding into a
  per-track instrument setting, and a target id that means nothing
  here. Rejected.
* *Enum only*: kills the user's actual ask (custom drawn curves,
  XF-3). Rejected.
* *Reuse `sample_edit::Curve`*: its shapes are fade gains (`Flat` ≡
  no-crossfade, `Log` = √t) evaluated as `gain(t)`, and the sampler
  script cannot link it anyway; also `Curve` is Copy/serde-pinned into
  every SampleEdit round-trip. Rejected as the enum base; naming below
  stays compatible.

Wire + module contract (law):

```rust
// src/xfcurve.rs  (new; the single source of truth host-side)
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum XfCurve { Linear, Sine, Cosine, ExpIn, ExpOut, Custom }
// Custom serializes as the string "custom" but NEVER appears in the
// u8 transport (see § 4): on the wire Custom is implied by a non-empty
// node table; the u8 id space keeps 0..=4 + 255=custom.

pub struct XfNode { pub t: f32, pub v: f32, pub c: f32 } // all normalized, § 5
pub fn weights(curve: XfCurve, nodes: &[XfNode], t: f32) -> (f32, f32); // (w_lo, w_hi)
```

`weights` is pure, allocation-free, f32, endpoint-exact, and is the
ONLY place a crossfade weight pair may come from (both the engine
preview UI and the sampler script's in-script duplicate compute the
same table below to ≤1 ulp; § 6).

## 3. Decision (b) — param surface: names, ranges, defaults

One selector — **"xf curve"** — governs BOTH blend axes (root axis and
velocity axis; each axis evaluates its own fraction `t` through the
same curve, then the products multiply exactly as today's two linear
factors do). Two selectors would need a user request; adding one later
is additive.

Built-ins (t ∈ [0,1], LOW→HIGH blend position; every arm is endpoint-
exact `f(0)=0, f(1)=1` and monotonic):

| id | name       | w_hi(t)                            | w_lo(t)            |
|----|------------|------------------------------------|--------------------|
| 0  | `linear`   | t                                  | 1 − t              |
| 1  | `sine`     | sin(π t / 2)  (equal-power pair)   | cos(π t / 2)       |
| 2  | `cosine`   | (1 − cos π t) / 2  (smoothstep law)| 1 − w_hi           |
| 3  | `exp in`   | (e^{4t} − 1) / (e⁴ − 1)            | 1 − w_hi           |
| 4  | `exp out`  | 1 − f_exp_in(1 − t)                | 1 − w_hi           |
| 255| `custom`   | § 5 node eval                      | 1 − w_hi           |

* id 0 `linear` is the **default everywhere** and MUST reproduce
  today's math **bit-exactly** (§ 6). `sine` is the ONLY equal-power
  arm (its complement is cos, not 1−sin — the others are constant-sum
  by law); this is what gives SAMP-LP-3 its equal-power seam default
  (§ 7).
* UI: a ComboBox in the sampler map window, directly under the
  hard-cutoff/crossfade pair, labels exactly the names above (ASCII,
  glyph-safe); selecting it writes the field, repaints, and echoes
  `sampler map: xf curve · <name>` into `self.status` (echo-on-action
  law). The picker is **live**: the map re-push on the next
  note-on/process makes it audible immediately (same path as mode —
  satisfies XF-2's "audible on the next note" without any engine poll
  change). The ComboBox shows the curve name; `custom` shows the saved
  preset name instead (`sampler map: xf curve · soft hold`).
* Transport: `SynthHandle::map_mode` bits 8..15 carry the u8 id
  (0..=4, 255); bit 31 unchanged; bits 16..30 must stay 0. Host writes
  them even when publishing a map-less/empty map (bits then read mode
  0 + curve 0). The script reads `curve = (bits >> 8) & 0xFF`. This is
  **version-free**: an old script masks `& 0xFF` and never sees the new
  bits; a new script against an old project gets 0 = `linear` = old
  behavior. `sampler.rs`'s `raw_mode` change-detect must become a full
  `map_bits` compare (curve changes must re-push state exactly like
  mode changes do).
* **The curve is map state, NOT a legacy synth knob.** It rides the
  same channel as `mode` (bits) and is saved in the same JSON record
  (§ 4); it is NOT appended to the script's `PARAMS` array. See § 8
  first bullet — this is a deliberate correction of the card wording.

## 4. Decision (c) — persistence: SamplerMapSave, NOT SampleEdit

**The curve selection and the custom node tables persist inside
`config::SamplerMapSave` (project JSON, per track). They do NOT go into
`SampleEdit`.** Grounds: `SampleEdit` is keyed by ASSET PATH and baked
OFFLINE; the blend curve is per-track PLAYBACK behavior of the sampler
section. Two sampler tracks sharing one asset would fight over one
per-asset field, and moving live-blend state into the offline render
edit would make the same wav behave differently when edited for clip
use. This contradicts the card wording in SAMP-XF-2 ("SampleEdit
persistence") and the XF-0 card's parenthetical; the correction is
deliberate and final. (`SampleEdit` keeps its own fade `Curve` — fades
and crossfades are different features with different homes; do not
merge them.)

`SamplerMapSave` gains (serde defaults = today's behavior):

```json
{ "slots": [ …existing… ],
  "mode": 1,
  "xf_curve": 0,
  "xf_custom_name": "soft hold",
  "xf_curves": [
    { "name": "soft hold",
      "nodes": [ { "t": 0.0,  "v": 0.0,  "c": 0.0 },
                 { "t": 0.62, "v": 0.25, "c": 0.8  },
                 { "t": 1.0,  "v": 1.0,  "c": 0.0 } ] }
  ] }
```

* `xf_curve: u8` = the built-in id (§ 3), `#[serde(default)]`.
  **0 = linear = every pre-series project loads bit-exact — the whole
  migration is one serde default; there is no migration code and the
  Project `VERSION` stays 3** (field-additive idiom, same as MIX-RT-0).
  The id never gates on version equality (seed law).
* `xf_curves: Vec<NamedXfCurve>` = the track's custom library,
  `#[serde(default, skip_serializing_if = "Vec::is_empty")]`.
* `xf_custom_name: String` = which library entry the engine evaluates
  when the selection is `custom`; `#[serde(default, skip_serializing_if
  = "String::is_empty")]`. Selection semantics: `xf_curve` names the
  KIND; when it is 255, `xf_custom_name` names the ENTRY. A dangling
  name (deleted/mis-edited) degrades to `linear` at eval time — never
  a panic, never silence.
* Custom nodes are NOT duplicated into the active-field; one table per
  library entry, referenced by name. Editing the entry mutates it in
  place (the selection sees it live, exactly like rack edits).

## 5. Custom curve model (feeds SAMP-XF-3)

* `XfNode { t, v, c }`, all f32: `t ∈ [0,1]` normalized position on the
  blend axis, `v ∈ [0,1]` the HIGH-side weight there, `c ∈ [−1,1]` the
  segment curvature with **exactly `automation::Node::c` semantics**
  (documented in that struct: 0 = line, + soft arrival, − rushing
  arrival; ±1 = the legacy cubics). Host eval: same binary-search
  structure as `Automation::eval` (sort, ≥2 nodes, first t = 0, last
  t = 1 enforced by a `normalize` that mirrors `Automation::normalize`),
  per-segment warp = `automation::warp(b.c, u)` — the SAME function, so
  XF-3's "identical to automation.rs eval" pin is exact by
  construction, not by coincidence.
* Endpoint law: `normalize` forces the first node (0, 0) and the last
  (1, 1) (their `c` stays editable — the arrival shape into t=1 is the
  whole point). Interior nodes: `t` sorted-unique (dedup keeps the
  later), `v` clamped 0..1 but NOT monotonic-forced (a dip is a legal
  duck-crossfade); the editor clamps what the user can type/drag.
  Node cap: **32** (enforced by the editor and by `normalize`; the
  script's receive buffer is 64, defensively).
* Library management (host-side API, XF-3; UI in XF-4): add / list /
  rename / delete `NamedXfCurve` on the track's `SamplerMapSave`.
  Names pass through the existing `sanitize_name` path (orphan-file
  lesson cf. t_09676c12) and are unique within the track: collisions
  take the " 2" suffix loop (v172 collision-suffix idiom).
  **Rename** = edit + sanitize + collision-suffix; if the entry was
  selected, `xf_custom_name` follows in the same operation (rename can
  never strand a selection). **Delete** = remove the entry; if it was
  selected, the selection falls back to `linear` (id 0) with a status
  echo. Both echo into `self.status`.
* **Inline per track, not a shared library** — the answer to the
  card's (d). A track's curves travel with the project because they
  live inside the track's map record: no new file kind, no rack
  section, no orphan-file class, no cross-project name collisions
  possible. Duplication cost across tracks is a few hundred JSON
  bytes. A global shared curve library is explicitly DEFERRED (not
  rejected forever): if ever wanted it is an additive Config field +
  "copy into track" action, and nothing in this design blocks it.
* Script transfer: new APPENDED field on `SynthHostV1`
  (`synths/synth_api.rs`, ABI stays 1 — v164 `tune_key` precedent):

  ```rust
  /// vNN (SAMP-XF-3): the selected custom crossfade curve, nodes
  /// sorted by t, repr(C) {t,v,c} f32. Returns n (0 = none/unused).
  /// Valid synchronously inside any callback, like load_asset.
  pub load_xf: extern "C" fn(nodes: *mut XfNodeV1, max: u32) -> u32,
  ```

  The host answers it from the same live `SamplerMapSave` the map
  picker edits (zero-copy snapshot like `map_mode`). `sync_map` polls
  it exactly like it polls the map: compare the packed table against
  the cached one; equal → nothing, changed → rebuild the tiny local
  copy. Old scripts never reference the field; third-party scripts
  pick the new `synth_api.rs` copy on boot seed as usual. `sampler.rs`
  bumps its `slooper-synth-version:` stamp (10 → 11) as the
  ours-theirs marker ONLY — staleness stays content-diff (seed law).

## 6. Bit-exact default law (feeds SAMP-XF-1 pins)

`xf_curve = 0` MUST reproduce today's output **bit-for-bit on a fixed
buffer**:
* Root axis keeps the existing expressions verbatim —
  `w_hi = ((n − lo.root) / span).clamp(0.0, 1.0)` with the `span >
  1e-6` guard, weights applied as `(1.0 − w_hi) * w` / `w_hi * w`. The
  linear arm returns `t` itself; the implementation must NOT re-derive
  the complement (e.g. never `sin(0)`-style equivalents, never
  `1 − (1 − t)`).
* Velocity axis keeps `(1.0 − t, t)` verbatim (`crossfade_layers`).
* Pin shape (XF-1): render a fixed two-root, two-layer map with a fixed
  note/velocity sweep through HEAD's compiled script output vs the new
  one, assert byte-identical f32; then per curve, eval pins at
  t ∈ {0, 0.25, 0.5, 0.75, 1} and monotonicity for every built-in
  (custom is exempt from monotonicity, not from endpoints).
* Cross-OS precision law: bit-exact asserts ONLY for `linear` (pure
  arithmetic) and the endpoint values; any assert touching sin/cos/powf
  uses ≤1 ulp tolerance (Windows and glibc libm differ in the last
  bit; the windows-native suite is the parity arbiter and must not be
  handed a libm-dependent flake).

## 7. Decision (e) — loop-crossfade reuse surface (feeds SAMP-LP-3)

SAMP-LP-3 evaluates the seam weights through the SAME `XfCurve` id
space and the SAME `weights()` table — no second enum, no seam-only
math:
* The seam is LOW = the tail (pre-seam) and HIGH = the head
  (post-wrap), `u` ramps 0 → 1 across the seam span, weights =
  `weights(curve, nodes, u)` **per sample** (unlike the note-on blend,
  the seam crossfade is a per-sample ramp — `weights` is closed-form /
  binary-search, allocation-free and RT-safe at 32 nodes).
* Default for the seam field is **`sine` (id 1, equal-power)** — a
  per-call default, not a different enum:
  `#[serde(default = "default_loop_xf")] fn default_loop_xf() -> u8 {
  1 }`. The field itself lives on LP-1's loop-region record (LP-1 is
  not owned by this doc); SAMP-XF-3's custom library is reachable from
  it exactly as
  § 4's pair: `loop_xf_curve` (u8 id) + `loop_xf_name` (String, used
  only at 255, evaluated against the track's `xf_curves` — a seam may
  reference the same named custom curves).
* Hard-cutoff maps never crossfade anywhere; curve fields are inert
  when `mode = 0` (and a track with `mode = 0` MUST render bit-exact
  regardless of curve fields — pin that).

## 8. Card-wording corrections (this document wins)

* XF-1 "expose the selection as a sampler param so it surfaces in the
  params UI automatically" → **it is map state, like `mode`** (map
  window picker + map bits + SamplerMapSave). Appending a 4th `PARAMS`
  knob would give script synths a fractional f32 slider for a 6-value
  enum (no enum-metadata path exists for script-synth params), would
  collide with the map-is-authoritative rule, and would NOT persist
  anything (params ride the track's synth values, not the map record).
  XF-2's "controls the XF-1 param live" reads as "controls the XF-1
  selection live" — still satisfied (§ 3 live picker).
* XF-2 "SampleEdit persistence" and XF-0's parenthetical → **not
  SampleEdit**, per § 4.
* Everything else in the five card bodies stands and is made
  implementable by the sections cited in § 9.

## 9. Cards this document unblocks (decisions as law)

* **SAMP-XF-1** (t_16b88781): § 2 (eval shape), § 3 (built-in table +
  bit transport), § 6 (bit-exact default + cross-OS tolerance), § 1
  (once-per-note weighting stays; seam does not exist yet).
* **SAMP-XF-2** (t_c5364e27): § 3 (picker home, names, live behavior,
  status echo), § 4 (what it writes), § 8 (param wording corrected).
  The mini preview renders § 3's table; when the selection is `custom`
  the preview IS the node editor (§ 5 model, endpoints locked).
* **SAMP-XF-3** (t_2330024a): § 2 (warp reuse), § 4 (JSON layout +
  serde-default migration), § 5 (node model, library semantics,
  `load_xf` transport, version-stamp rule).
* **SAMP-XF-4** (t_72c1ddbc): § 5 (save-as/apply/rename/delete law),
  § 4 (what persists), sanitize/suffix idiom.
* **SAMP-LP-3** (t_0743a172): § 7 (same enum, per-sample eval,
  equal-power default via serde default fn, custom-by-name reuse).

## 10. Out of scope

* Automating the curve id/value (no `synth:`-style automation target).
* Per-zone-pair or per-layer-pair curves (one selection per track map).
* A global/shared curve library across projects (deferred; additive if
  ever wanted, § 5).
* Importing `.slauto` curves as crossfade presets (possible future card;
  the curvature units match, so an importer would be trivial).
* Changing hard-cutoff behavior or `SampleEdit` in any way.
