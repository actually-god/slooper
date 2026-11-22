# Sidechain routing (v32 R5; tap semantics v95; per-send levels v97; sender send gain/pan v99)

## What sidechaining is

Sidechain (dynamic) processing lets *one* signal control a *different*
signal's processor: the classic four-on-the-floor pump is a compressor on
the pad/bus whose detector is driven by the kick, so every kick transient
ducks the pad. The keying signal never appears at the processor's audio
input — it arrives on a separate "sidechain" path that only the detector
sees. VST plugins expose this as an extra input bus (`sidechain` in the
VST3 spec); sequencers expose it as a routing matrix so any track can key
any other track's compressor.

## Design in slooper

Everything runs on the realtime thread with zero allocations and no locks:

* **Send matrix.** Each track owns a 128-bit send mask (`u128`, stored as
  an atomic lo/hi `u64` pair — there is no `AtomicU128`): bit *k* of
  track *t*'s word means "track *k* keys *t*'s effect chain".
  `Engine::set_sc_send` clears the self-bit on the way in — a track can
  never key itself (that would be ordinary audio feedback through the
  detector).
* **The SC bus.** `Proc` keeps two per-track block buffers: `sc_build`
  accumulates the *current* block's taps, `sc_apply` is what chains
  consume. They swap at the end of every audio block, so a sidechain
  signal is one block late (~2.6 ms at 48 kHz/128) — well below the
  attack time of any real detector, and it buys a strict one-cycle
  feed-forward graph with no loops and no extra latency bookkeeping.
* **The tap (v95 semantics).** Each routed source sends, every sample,

  ```text
  key = dry × track_gain × (pgl + pgr) / sqrt(2)
  ```

  where `dry` is the track's pre-chain sum (clip samples × clip gain ×
  fades + its live/pattern MIDI synth output). The tap sits **after the
  sender's gain and pan stages** — the gain knob and the pan law are
  *preserved on the key* — and **before, and completely independent of,
  the volume fader and every mute** (play switch, automation mute,
  transport stop of the sender aside). Muting the sender in the main mix
  or dragging its fader to zero silences its AUDIO but never its KEY;
  the master gain also never scales the key.
  The pan factor is the track's own mono-downmix law — centre pan =
  unity (pre-v95 routes keep their level), hard L/R = 0.707 — because
  the bus is mono and the downmix is exactly the contribution the
  sender would make to the master bus (minus fader and mute).
  The tap is block-constant in gain/pan (no per-sample ramp): it only
  ever feeds a detector, so steps are inaudible by construction.
  Zero cost when no routing exists: edges expand to (source, target)
  pairs once per block (`e_src`/`e_tgt`, v117: grow-once tables sized to
  the structural bound `MAX_SC_EDGES` = 128×128 — every legal route fits,
  nothing is ever truncated) and are filtered
  to sources that actually carry material this block — a muted or
  stopped source with clips/MIDI still expands edges (its key flows!),
  a source with nothing to play contributes nothing.
* **The consumers.** `fx_mix` and `run_chain_block` pass the track's
  sidechain row down to scripted slots. Plugins see silence for `sc` —
  VST3 sidechain inputs need extra JACK port plumbing and are future
  work (see below).
* **Per-send receive levels (v97).** Each (target, source) pair carries a
  receive level (linear, default 1.0) that scales that source's tap
  *before* the sum — so a target fed by several sources can MIX its keys
  (50 % kick + 100 % bass etc.). The level lives on the receiver's side
  only: the sender's own gain/pan still ride the key per the tap law, and
  its fader/mute still never touch it. RT reads the table once per block
  into the edge list (`e_gain` -> folded into `e_scale`); the live bus and
  the offline mirror apply the identical factor. The slider in the mixer
  (see below) is the only editor; old project/config files carry no
  entries and therefore load at unity — pre-v97 behavior bit-for-bit.
* **Per-sender send levels (v99).** Every SENDER additionally carries a
  general send gain (linear, default 1.0) and a send pan (default centre)
  applied to its tap BEFORE it routes to any receiver, so the full signal
  order is tap → sender gain/pan → per-pair receive level → sum → the
  target's SC row → detector. The pan re-pans the key before the mono
  downmix (the SC bus carries ONE f32 per sample — script ABI v2's `sc`
  argument): fold = (spl + spr)/√2 with the pan-law gains — centre =
  unity, hard L/R = 0.707, exactly the v95 track-pan law. The documented
  consequence: with every consumer mono-summing today, send pan acts as a
  key-level trim (hard pan = 0.707 of the key, like −3 dB of send gain);
  balance semantics activate when a stereo key consumer (plugin sidechain
  bus, below) lands. Both values are block-constant and live + offline
  share one `sc_out_scale` helper, so live and export ducks stay
  bit-identical; a bit-compare fast path keeps untouched senders exactly
  pre-v99 (defaults cost no trig).
* **MIDI senders.** A muted source track's patterns still key the
  target when its synth instance is DEDICATED to that track. Tracks
  sharing one synth instance (e.g. several strips on the default piano)
  cannot be rendered for the key while muted without leaking their
  notes through the audible owner's row — a known limitation: route
  from an audible track or give the sender its own instrument.
  (Poly tracks, whose copies render independently, key regardless.)

## Script ABI v2

A scripted effect opts into the sidechain with a second entry point:

```rust
#[no_mangle]
pub extern "C" fn slooper_effect_info_v2() -> *const EffectInfoV2 { ... }
```

`EffectInfoV2` adds a `sidechain: u32` flag, and the v2 process fn takes
one extra `sc: f32` argument (the summed key for this sample; silence
when nothing is routed). v1 scripts are untouched — the host keeps the
v1 vtable for them and passes no key. The bundled example is
`effects/sidechain_comp.rs`: a feed-forward compressor whose detector
takes `max(|input|, depth × |sc|)`, i.e. key-only ducking at `depth = 1`.

## Offline mirror

`render_tracks` exports with the same tap law: source rows are held
**for every routed source regardless of the play flag**, scaled by
`gain × (pgl + pgr) / sqrt(2)` only (never volume), sample-aligned (the
live one-block detector lag cannot be reproduced without knowing the RT
block size and is not part of what the user hears "as mixed"). Live and
export therefore duck identically.

## Mixer integration

* The chain window has a **Sidechain in** picker: all track numbers,
  click to toggle (self excluded — the mask shows as a tag and keyed
  tracks are listed under the picker). A routed chip carries a per-send
  **level slider** (v97): 0–200 %, `nnn %` readout, hover explains it and
  the status line echoes the pair + percent on release.
* The MIX page (F9) channel columns carry two fixed **send rows** (v99):
  `send` — a 0–200 % `nnn %` slider — and `s-pan` — an L/C/R slider with
  `L40`/`C`/`R40` readout. These are the sender-side companions of the
  v97 receive sliders: one knob shapes the key for EVERY target routing
  this track (`Sidechain in` stays in the TARGET's chain window). Rows
  paint in every column (values persist live even unrouted); hover
  explains the fader/mute independence and the mono-sum fold choice; the
  status line echoes `sidechain send-out T<n>: nn %` / `pan: L<n>` on
  release.
* Sends persist per project (`Config::sc_send`, mirrored into the engine
  on boot and on every change; `write_config_deferred`). Receive levels
  persist sparsely as `Config::sc_recv_gain: Vec<ScRecvGain { t, s, gain, pan }>`
  — only non-unity pairs are written (v97). Per-sender send levels ride
  the SAME Vec as SELF-PAIR entries (`t == s`, v99): the self-bit is
  never a legal route in the send mask, so the slot is provably free;
  `gain` = send gain, `pan` = send pan in −1..=1 (0 = centre; the engine
  and UI use [0,1] with 0.5 = centre). v97-era files carry no self
  entries and no `pan` key and load at unity/centre — pre-v99 behavior
  bit-for-bit.
* The chain window tags effects loaded through the v2 entry so it is
  visible whether an insert can actually *use* a key.

## Limitations / future work

* **Plugins**: VST3/VST2 sidechain inputs (`BusInfo` sidechain bus, extra
  JACK input ports fed from the same bus) are not wired yet — the SC bus
  currently feeds scripted effects only. The mixer model is identical
  either way, so plugin support is plumbing, not redesign.
* The bus is mono (post-pan downmix factor per the tap law above) — the
  v99 send pan is therefore the documented mono fold of that same law:
  until a stereo key consumer exists it is a key-level trim, ±0…−3 dB.
* Muted MIDI sources key only through dedicated synth instances (above).
* 128 tracks max ⇒ exactly one `u128` per track; no per-target edge
  budget tuning needed at this scale. v117: the live edge table carries
  the FULL 128×128 route space (16384 edges, grow-once buffers) — the
  pre-v117 fixed 256-entry table silently dropped everything past the
  cap while the offline export mirror was uncapped, so exports ducked
  but speakers did not; truncation is now structurally impossible.

## Tests

* `v117_live_edge_table_*` (`audio/mod.rs`): the LIVE path driven
  directly (`new_proc` + stub `BlockIo`) — 400-edge and 16,002-edge
  rigs demand every routed target's key arrives (pre-v117 the 256-entry
  table keyed only the first two), plus a source-scan pin that the
  append-time cap never returns.
* `fx.rs`: v2 example compiles and loads, `sidechain` flag set; v1
  scripts still load through the v1 path.
* `e2e_sidechain_ducks_target_track`: real JACK loop — key panned hard
  left, target tone hard right, probe listens to the right bus only (the
  key's own energy can never bias the measurement). Asserts the tone is
  audible unrouted, drops >30 % when the send is armed, and recovers
  after the send is cleared. Sabotage-tested: removing the routing makes
  the pin fail.
* `e2e_v95_sc_muted_sender_still_ducks` / `e2e_v95_sc_zero_volume_sender_ducks`:
  the same rig — then mute (play switch) / fader-to-zero the SENDER: the
  duck must NOT move. `e2e_v95_sc_sender_vol_move_keeps_duck_depth`:
  a 20 dB sender fader move leaves the duck depth unchanged.
* `v95_sc_export_muted_sender_still_keys` / `v95_sc_export_tap_tracks_gain_pan`:
  the offline mirror — exact duck values prove the export tap is
  mute/fader-independent and gain/pan-preserving (depth ∝ gain, hard
  pan × 1/√2, volume has zero effect). `v48_export_sidechain_ducks_target`
  re-targeted to the new contract in the same commit.
* v97 receive levels: `v97_engine_sc_recv_defaults_and_clamp` (unity
  table, clamping, per-pair independence); `v97_sc_export_recv_gain_scales_pair`
  (two senders, exact duck math per pair level, unity = pre-v97 blend);
  `v97_config_sc_recv_gain_old_files_unity_roundtrip` (sparse persistence);
  `v97_ui_send_level_slider_paints_drag_and_echoes` +
  `v97_ui_send_level_hover_copy_present` (paints, drags, echoes, persists);
  `e2e_v97_sc_send_gain_scales_duck` (live bus: 0 un-keys, 0.5 shallows,
  1.0 restores, 2.0 deepens) and `e2e_v97_sc_two_source_mix_swaps_duck`
  (two unequal keys, 50/100 vs 100/50 measurably differ).
* v99 sender send gain/pan: `v99_engine_sc_out_defaults_and_clamps`
  (unity/centre tables, clamps, per-sender independence, send MASK table
  untouched); `v99_sc_export_send_level_scales_both_targets_exactly`
  (one muted sender → two targets: both ducks scale IDENTICALLY, hard
  send pan = exact ×1/√2 fold, zero send opens both, receive levels mix
  UNDER the send); `v99_config_send_entry_roundtrip` (self-pair sparse
  persistence; a v97-era JSON without `pan` loads centred);
  `v99_ui_mix_send_rows_paint_drag_echo` + `v99_ui_mix_send_rows_aligned`
  + `v99_ui_send_row_hover_copy_present` (paints in every column, drags
  mirror + engine + echo, self-pair save round trip, fixed row geometry,
  hover copy exact-once); `e2e_v99_sc_send_gain_moves_both_targets_equally`
  (live JACK, dual-bus probe: a MUTED sender ducks two hard-panned
  targets, half send lightens BOTH together within ratio margin — a
  per-edge misapplication would split them: causal fingerprint — zero
  send opens both, unity restores); and
  `e2e_v99_sc_send_pan_folds_key_level` (hard send pan lightens both
  ducks via the documented mono fold, centre restores, zero + pan
  opens).
