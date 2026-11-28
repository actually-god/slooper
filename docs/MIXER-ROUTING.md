# Mixer routing graph — design (MIX-RT-0)

Status: **accepted design; binding** for MIX-RT-1 (engine, t_06f3524b),
MIX-RT-2 (mixer UI, t_ccd5ffbe), MIX-SC-1 (per-effect sidechain sources,
t_55f6e149) and the SC-2 UI contract (t_fa57df56). Filed by feature-creator
2026-11-28 from the user's request: *"every track rewirable to as many
destinations as it likes, and sidechain listening decoupled from routing."*

No product code in this document's PR. Every decision below is stated as a
law ("MUST / never / exactly"), so the downstream cards implement rather
than re-decide. Terms: **track** = the existing clip/FX/fader container;
**destination** = where a track's audible signal goes; **route/edge** = one
(sender → destination) pair; **bus** = a track used purely as a destination.

## 1. Today (grounding)

* The whole mix is one sum: per track, clips+MIDI accumulate into `acc[t]`
  (mono), then the sample loop applies the ramped trim `av`
  (= `vol × trk_gain`), the scripted chain (`fx_mix`, returns a stereo
  pair), equal-power pan (`pgl/pgr`), and adds the pair into the single
  `play_l / play_r` master sum (`Proc::process_block`, src/audio/mod.rs).
  Tracks with `vst_mask` bits run their chain block-wise afterwards and
  mix back into the same pair. There is no per-track output anywhere else.
* Hardware outputs are a **fixed 16-slot pool** (`MAX_SLOTS`, per backend:
  JACK `out{i}`, WASAPI (endpoint, channel) pairs). The engine writes the
  *master* mix into every enabled slot per its `out_mask_l/out_mask_r`
  bits (L bus, R bus, or mono downmix). Slot→device wiring is a Ports
  window concern, entirely below the engine's track model.
* Sidechain is already a separate graph: per-target `u128` send mask,
  per-edge receive level (`sc_recv`), sender send gain/pan (`sc_out_scale`),
  grow-once `(src, tgt)` edge tables capped only by the structural bound
  `MAX_SC_EDGES = MAX_LOOPS × MAX_LOOPS` (v117), and the v95 tap law
  (post sender gain/pan, **pre** chain, **pre** fader, mute-independent —
  docs/SIDECHAIN.md is the authority).
* Track count is `n_tracks` (≤ `MAX_LOOPS` = 128); adding/removing tracks
  shifts every track-indexed table (the `while t + 1 < self.n_tracks`
  remap sweep in src/ui.rs). Asset ids ARE table indices (skill law).
* Project JSON (`Project`, src/project.rs) is `#[serde(default)]`
  field-additive: fields added since v3 always load on old files, and old
  files load with the field absent. `VERSION` stays 3; defaults, not
  version numbers, are the compatibility mechanism.

## 2. Goal and non-goals

Goal: a track's **audible output** fans out to any number of destinations
(master and/or other tracks), the destination set is editable per track on
the mixer, persists, and default projects behave bit-exactly as today.

Non-goals (v1, explicit): per-edge send trim knobs; bus FX beyond what
tracks already do; per-track direct feeds to individual hardware output
slots; subband/aux sends; stems export changes; MCP surface for routes
(file a follow-up if wanted).

## 3. Destination set — the decision

**A track's destination set is: MASTER, and/or any subset of the other
tracks.** That is the complete v1 surface.

* **Internal buses are realized as tracks** (the FL Studio model): a track
  with no clips, an empty arrangement lane, is exactly a bus — it already
  has a fader, gain, pan, mute, FX chain, meter, name and mixer strip.
  Routing into it, renaming it, or chaining it (bus → bus) needs no new
  concept. No separate `Bus` entity ships.
  *Rejected:* a dedicated Bus object — a second entity with its own
  lifecycle, serialization, mixer layout and remap rules, duplicating a
  container that already exists in every way that matters.
* **Hardware out ports are NOT track destinations in v1.** The slot pool
  carries the master mix (or nothing); that is its contract and the Ports
  window already lets the user wire the 16 slots to any device channels.
  A per-track direct-to-slot feed would need a per-slot source selector,
  16 extra render taps, and per-backend divergence — a distinct feature,
  not part of this graph. If it ever ships, the destination model here
  grows a `slot: N` destination kind without touching anything already
  stored (the u8 destination encoding below leaves room: 128..254 are
  reserved).
  *Rejected for v1:* direct-to-port per-track outputs (scope; see above).
* **Destination encoding** is `u8`: `0..=127` = track index, `255` =
  MASTER, `128..=254` reserved (future: hardware slots). Every destination
  list in code, JSON and UI travels as these u8 values.

## 4. Signal semantics (per-destination gain law)

Two tap points exist in slooper and MUST stay distinct — this is the heart
of the design:

| | tap point | chain? | fader×gain? | pan? | mute? |
|---|---|---|---|---|---|
| routing (this doc) | track output pair, end of chain | yes | yes | yes | yes |
| sidechain (v95, unchanged) | post sender gain/pan, pre chain | no | trk_gain only | fold | no |

Laws:

1. **The routed signal is the track's final audible contribution** —
   exactly the `(pl, pr)` pair today added into `play_l/play_r`: clips→
   `acc`, × ramped `av`, through the track's chain, × `pgl/pgr`. A track
   routed ONLY to master must be **bit-exact** vs today (pin law, § 9).
2. **Edges carry no gain.** Fan-out is a unity copy: routing to 3 places
   does not attenuate by 1/3 and there is no per-edge level in v1. All
   gain semantics stay where the user already controls them: the sender's
   fader/gain/pan ride the routed signal (it was shaped before it
   splits); the destination's fader/gain/pan/FX affect ONLY the
   destination's own clips, never what it received.
   *Rejected for v1:* per-destination send knobs — deferred deliberately
   (mirrors how SC levels arrived later as sc-4/sc-5 once the matrix
   existed). The u8 JSON entry (§ 8) can grow a level without migration.
3. **The destination's mute/transport gate rides the whole sum.** The
   destination's contribution = (own processed pair) + Σ(incoming routed
   pairs), and the existing audibility gate (`transport && track_on`) is
   applied to that sum before it flows further. So muting a bus mutes its
   contents (cascade), while a sender that also routes to master stays
   audible — correct group behaviour, and the asymmetry vs. the fader
   (which does NOT ride received material) is an accepted, pinned design
   asymmetry: keeping it buys the bit-exact default path, zero ABI change
   (script chains keep their mono input), and no double-pan — the sender's
   pan is baked in once, at the sender.
4. **Master is a pure sink.** `play_l/play_r` receive the contributions of
   tracks routed to MASTER. The metronome and input monitoring keep today's
   law: output-mix-only, never routed, never recorded.
5. **No added latency.** Routes must be sample-aligned (this is an audio
   path, unlike the SC bus whose one-block lag is pinned acceptable).
   Implementation shape is MIX-RT-1's call; the constraint: the summing
   stage runs after BOTH per-block mix stages (scripted-inline and
   vst-mask block-wise), evaluating the DAG topologically per block. A
   project whose routes are all default (master-only) MUST take today's
   code path unchanged (fast path + bit-exactness pin).

## 5. Sizing and RT budget (MIX-RT-1)

* Per track: a `u128` **track-destination mask** (bit *d* set = route to
  track *d*), stored as the same atomic lo/hi `u64` pair precedent
  `set_sc_send` uses (there is no `AtomicU128`), **plus** a per-track
  `AtomicU8 master_on` (1 = routed to master; default 1) — `MAX_LOOPS`
  consumes all 128 mask bits, so MASTER rides its own word and the u8
  wire encoding (§ 3) maps `255` to it. Setters clear the self bit
  (precedent: `set_sc_send` clears self) and enforce the § 7
  never-empty law.
* Grow-once engine-owned edge tables `(src, tgt)` exactly like v117's SC
  tables, structural bound `MAX_ROUTE_EDGES = MAX_LOOPS × MAX_LOOPS =
  16384`; an append can never be refused; silent truncation is
  structurally impossible (t_7609b30f law: **no silent caps** — 128-track
  full fan-out fits).
* RT allocates nothing steady-state; materialization (mask scan → edge
  list + topological order) happens when a routing generation counter
  bumps, mirroring the SC block setup; an unrouted project pays ~one
  atomic AND per track (same as SC today).

## 6. Cycle policy (pinned)

The routing graph is a **DAG, enforced everywhere; the RT thread never
checks and never detects.**

1. **Self-route is impossible at the setter** — the setter clears the
   self bit (precedent: `set_sc_send`).
2. **Edit-time (MIX-RT-2 UI):** a destination candidate `D` is offered for
   track `S` enabled only if enabling it cannot close a loop — i.e. `S` is
   not reachable from `D` through the current routes *plus the edit*.
   Disqualified candidates render **disabled with a reason tooltip**
   ("would create a cycle with track N"). No dialogs, no post-hoc error.
3. **Load-time (MIX-RT-1):** a validated project still runs the breaker,
   because JSON is hand-editable and older builds are future-compatible.
   Deterministic law: consider edges in ascending (source, destination)
   u8 order; insert edge-by-edge; drop any edge whose source is reachable
   from its destination through already-accepted edges. Dropped count is
   echoed once in the status strip: `dropped N routing edge(s) — cycle`.
   Re-saving the project writes the pruned (valid) set.
4. **RT:** topological generations are materialized at routing-change
   time; `debug_assert` that every node was emitted by the topo sort (a
   cycle reaching RT is a bug, not a runtime condition).

## 7. Default-route semantics (pinned)

* A track's default destination set is **{MASTER}** — exactly today's
  behaviour.
* **The set is never empty.** Un-checking MASTER is allowed the moment ≥1
  track destination is selected; the UI refuses to remove the last entry
  (refusal echoes in the status strip, per the ts-018 echo law). A loader
  that reads an empty set normalizes it to {MASTER} (defensive; a
  silent-mute destination set must be unreachable by construction).
* **Removing all track destinations re-enables MASTER only when the set
  would otherwise be empty** (rule above); routing a track to buses only
  and MASTER off is a supported, persisted state.
* An **old project (no routes field) is every track {MASTER}** — the
  default, the back-compat story, and the migration target, all one thing.

## 8. Project JSON (serialization + back-compat)

```jsonc
// Project (src/project.rs), additive field, track-indexed like track_synths:
"track_routes": [[255], [255, 3], [1]]   // t0: master. t1: master+track3. t2: bus1 only.
```

* `#[serde(default)] pub track_routes: Vec<Vec<u8>>` — index = track,
  inner = destination u8 list (§ 3). Absent (every pre-feature file) ⇒
  empty ⇒ all tracks default to {MASTER} by the § 7 rule. Old apps ignore
  the unknown field (serde default policy); `VERSION` stays 3 — field
  defaults are this repo's compatibility mechanism, not version gates.
* **Save writes every track explicitly** (master-only = `[255]`), so
  save/load round-trip is byte-stable and "did it persist" is one pin
  (MIX-RT-1: `vNN_rt_json_roundtrip`).
* Loader normalization order: clamp/drop out-of-range dests (≥ `n_tracks`
  but ≠ 255), drop self-dests, dedup, empty ⇒ {MASTER}, then run the § 6
  cycle breaker.
* **Track add/remove/remap:** `track_routes` joins the existing
  track-indexed shift sweep (ui.rs track insert/delete): destinations
  above the removed index shift down, routes to the removed track are
  dropped, then re-normalize. (Asset-ids-are-indices law;
  `vNN_rt_track_remove_remap` pin.)

## 9. Interaction with existing subsystems

* **Track pan/gain/vol/mute:** § 2 table + § 4 laws. Meters keep today's
  law in v1: a strip's meter shows its own signal, not received material
  (documented; a future affordance if ever requested).
* **Recording:** the v42 preview-take tap and take capture are unchanged —
  they tap the track bus before the routing sum; routes never alter what a
  take contains. Takes stay raw.
* **Offline render / export (`render_tracks`):** MUST mirror the routing
  graph (export-speakers-agreement law, learned from v117 where export and
  live diverged on edge capacity). A routing pin pair lives on the export
  mirror (`vNN_rt_export_matches_live_fanout`).
* **Metronome / input monitoring / preview playback:** master-output-only,
  never routed (§ 4.4).
* **Sidechain (the decoupling law, for MIX-SC-1):** the routing table and
  the SC edge tables are **disjoint structures with disjoint setters**.
  Writing routes never writes any SC atomic; writing SC state never
  touches routes. The v95 tap point (docs/SIDECHAIN.md) is independent of
  destination sets — a sender keys its targets regardless of where it is
  routed, and a track's SC feed is independent of what is routed into it.
  MIX-SC-1's per-consumer source lists are keyed by (track, chain slot),
  never by a routing destination. The mixer `(SC)` tag reads ONLY the SC
  tables. Pins: `vNN_rt_sc_independence` (changing routes leaves every SC
  row bit-identical) and the SC-1 card's own reverse pin.

## 10. UI contract (for MIX-RT-2 and MIX-SC-2)

* **Routing control = one popup per mixer strip.** Button label `Route`
  (ASCII; do NOT use arrow glyphs — default-font tofu list, skill law).
  The popup lists **Master** first, then every other live track by name.
  Multi-select interaction is the **t_d4479e3d language**: plain click
  toggles an entry, Ctrl/Shift-click extends — same as the sidechain
  source picker, so users carry one interaction across both features.
  Cycle-disqualified entries are disabled + tooltip (§ 6.2). Removing the
  last entry is refused (§ 7).
* **Fan-out state visible at a glance (MIX-RT-2 AC):** the strip shows a
  compact `R<n>` badge (ASCII, e.g. `R2`) whenever the destination set is
  non-default, hover-text listing the destinations in full. No new lane
  furniture in the arrangement — routing is a mixer concern in v1.
* **Echo:** every toggled destination writes one status-strip line naming
  the raw ids (`route 3 → master off`, `route 3 → bus 1 on`), ts-018
  echo law; refusals echo too ("can't remove the last destination").
* **One write home:** routes get a single seam like the v110/MCP-04 SC-mask
  seam (mirror + engine push + dirty-mark in one call); no direct pokes.
  Edits land next audio block (block-constant law). State persists with
  project save like every other mixer table.
* **MIX-SC-2 (SC listen picker):** lives on the *effect instance* rows of
  the mixer, uses the same t_d4479e3d multi-select language, and never
  displays or mutates routing — a route into a track NEVER implies it is
  an SC source (that is the user-visible face of § 9's decoupling law).
  The routing doc does NOT decide SC-2's effect-GUI removal list (that is
  the ui-designer's card-comment deliverable per the series protocol).

## 11. Downstream cards this gates

| Card | Id | Consumes |
|---|---|---|
| MIX-RT-1 (engine) | t_06f3524b | §§ 3–6, 8, 9 — masks, edge tables, topo summing, cycle breaker, JSON, remap, export mirror, bit-exact fast path |
| MIX-RT-2 (mixer UI) | t_ccd5ffbe | §§ 6.2, 7, 10 — popup + badge + echo + one-write seam |
| MIX-SC-1 (per-effect SC) | t_55f6e149 | § 9 decoupling law + tap point (UNCHANGED; docs/SIDECHAIN.md stays the tap authority) |
| MIX-SC-2 (SC UI) | t_fa57df56 | § 10 last bullet only |

## 12. Rejected alternatives (one line each)

* **Dedicated Bus entity** — duplicates the track container's every
  relevant affordance; FL's track-as-bus collapses the whole lifecycle
  question into features that already ship.
* **Per-track direct hardware-out destinations** — needs per-slot source
  selectors + 16 extra taps + per-backend split; separate feature, future
  `slot:N` u8 space kept open.
* **Pre-fader / pre-chain routing tap (aux-send model)** — users asking
  "route this track over there" mean its sound, not a dry key; the dry key
  is exactly what the *separate, already-correct* SC graph provides.
* **Per-edge send gain in v1** — schema-expressible later without
  migration; shipping the matrix first matches the sc-1→sc-4 history.
* **Mono-folded fan-out** — the contribution is already stereo at the tap
  point; folding would throw away the sender's pan for free pain.
* **One-block-delayed summing (SC-style)** — acceptable for detectors,
  wrong for a parallel audio path (comb vs none); § 4.5 pins the
  latency law instead.
