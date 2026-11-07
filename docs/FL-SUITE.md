# FL Studio native-plugin coverage (the slooper native suite plan)

Goal: match the **tasks** FL Studio's built-in plugin suite can do, not its
plugin-for-plugin layout. One slooper plugin often covers several FL
plugins (consolidated column); GUIs differ by design (scripted child GUIs
or the host's auto-slider window).

Source of the inventory: image-line.com/fl-studio/pricing plugin table
(FL Studio 2026.1 — 71 effects / 39 instruments across editions),
2026-11-05.

## Status legend
- **done** — shipped in the bundled set (compiled + loaded by `fx::tests`,
  seeded to every install by `seed_api`).
- **later** — planned; may need a logged capability first (see Capabilities).
- **host** — covered by hosting/other subsystems (VST scan, chains, mixers).

## Effects

| FL Studio plugin(s) | Task | slooper plugin | Status |
|---|---|---|---|
| Fruity Parametric EQ, Parametric EQ 2 | visual 7-band EQ + analyzer | `parametric_eq.rs` | done (26-param graph GUI, +30 dB, 240-band analyzer) |
| Fruity Reeverb 2, LuxeVerb, part of Fast Reverb | algorithmic reverb | `space_echo.rs` | done tier-5 (ABI-v3 **stereo** FDN: dual decorrelated comb banks + per-side echo, tail-width 0..2 with 0 = bit-exact mono tail; 12 params + space-presets GUI) |
| Fruity Chorus, Hyper Chorus, Vintage Chorus | chorus / doubler / ensembles | `chorus.rs` | done tier-4 (ABI-v3 stereo: `voices` taps/side, R bank phase-spread; dead tier-1 feedback rewritten as a delayed wet-recirculation loop) |
| Fruity Compressor | single-band comp | `compressor.rs` | done |
| Fruity Limiter sidechain duties | ducking comp | `sidechain_comp.rs` | done (ABI-v2 reference) |
| (tremolo starter) | amplitude LFO | `template.rs` | done |
| Fruity Blood Overdrive, Fruity Fast Dist, Fruity Soft Clipper, legacy Overdrive | analog-ish drive / clipper family | `overdrive.rs` | done |
| Fruity WaveShaper, Hardcore (per-stage shaping), LSD | draw-a-curve distortion | `waveshaper.rs` | done (draggable curve GUI) |
| Soundgoodizer, Emphasizer, Low Lifter, part of Maximus | one-knob enhancer / presence | `goodizer.rs` | done |
| Fruity Squeeze | bitcrush / downsample | `crush.rs` | done |
| Fruity Limiter, Emphasis, (legacy) Soft Limiter, noise gate | comp/limiter/gate, ceiling | `limiter.rs` | done |
| Maximus, Fruity Multiband Compressor | 3-band split dynamics | `multiband.rs` | done |
| Fruity Filter, Fruity Love Philter (single core), Fruity Wah | resonant LP/BP/HP + env auto-wah | `vfilter.rs` | done |
| Fruity Phaser, Vintage Phaser | phaser | `phaser.rs` | done tier-5 (ABI-v3 dual path: R cascade runs the LFO `spread` further along, opposite-traveling notches; L path = the historical mono circuit bit-for-bit) |
| Fruity Flanger, Fruity Flangus | flanger / unison | `flanger.rs` | done tier-5 (ABI-v3: ONE shared comb written with the mid, per-side modulated taps + R-side `spread` offset — opposite-sweep sides; tier-4 absolute-index ring read fixed — the comb now actually recirculates; base time slewed) |
| Fruity PanOMatic (vol), Gross Beat (gate/stutter lite) | tempo volume-gating | `gater.rs` | done tier-5b (ABI-v3 dual-lane: one LFO clock, R rail reads it `spread×180°` later with its own edge smoother — 1.0 = antiphase Gross-Beat gating; dry never crosses rails) |
| Fruity Delay 2, Fruity Delay 3, Fruity Delay Bank (single-bank core) | delay + filtered feedback | `echo3.rs` | done tier-4 (ABI-v3 ping-pong: R taps `time×(1+pp)` so repeats trail L→R; tier-1 absolute-index ring read fixed — see rework log) |
| Frequency Shifter | SSB shift (metallic/vibrato) | `shifter.rs` | done tier-5b (ABI-v3 TRUE STEREO: independent Hilbert/oscillator/feedback per side, R = `shift×(1+spread)` — hard-panned input never crosses rails) |
| Transient Processor, Transmitter (split lite) | transient/sustain shaping | `transient.rs` | done |
| Fruity Vocoder, Vocodex | vocoder (SC carrier feed) | `vocoder.rs` | done tier-5b (ABI-v3 carrier decorrelation: shared per-band envelopes + antisymmetric band interleave ⇒ spread splits the sides while the mono downmix stays the v2 circuit; keys off `process_st`'s sc arg — chain row now badges `[fx:st]`, not `[fx:sc]`) |
| Convolver | impulse reverb | `convolver.rs` | done (partitioned overlap-save FFT, **folder IR capability** — `~/.local/share/slooper/impulses`, env override, index 0 = built-in hall) |
| Multiband Delay | split echo | `echo3` + `multiband` chain | later (chainable today) |
| Spreader, Stereo Enhancer, Stereo Shaper, Flangus stereo, ping-pong delays | stereo width/image | `spread.rs` | done (ABI-v3 reference: mid/side + comb-Haas width, mix/gain; **stereo-pair capability** shipped, see capability log 3) |
| Fruity PanOMatic (pan side; vol side in `gater.rs`) | tempo pan LFO | `panmatic.rs` | done (ABI-v3: equal-power sine/tri/inverse-tri pan + optional volume wheel, tempo-synced) |
| Pitch Shifter, Pitcher, Tuner | pitch shift / correction / detect | `pitchshft.rs` | done tier-5b (ABI-v3: two delay lines, ONE modulation clock — R read head offset `spread×180°` (phaser topology); detector tracks the mid; NOTE: spread decorrelates only when the shift actually moves taps — at 0 shift both rails ride the same static delay) |
| Scratcher, Transporter, Wave Traveller, part of Gross Beat | transport scrub / retrigger | — | later (**transport-sync capability**) |
| Peak Controller, Envelope Controller, X-Y/Z, Formula Controller, VFX Envelope/Sequencer/Key Mapper, Control Surface | automation SOURCES | — | later (**control-rate OUT capability**); slooper automation clips + per-param automation cover the DESTINATION side today |
| Fruity Balance / Send / VFX Level Scaler | trim / routing | track gain/volume + sends | host |
| Distructor, Effector (12 FX) | multi-effect presets | chains of the above | host (effect chains) |
| Patcher, VFX Script | plugin-inside-plugin | chains + scripts | host |
| EQUO | morphing graphic EQ | parametric_eq (presets) | done-ish (morph = later GUI pass) |
| Edison, Newtone, Newtime, Fruity Video Player, Wave Candy, Spectroman, dB Meter, Big Clock, Razer Chroma, NoteBooks, HTML NoteBook | editors / meters / utilities, not FX | — | n/a (editor features; master-out meter/spectrum shipped v39) |

## Instruments

| FL Studio plugin(s) | Task | slooper synth | Status |
|---|---|---|---|
| FL Keys | piano | `piano.rs` | done |
| Drumpad, FPC, Fruity Kick, BassDrum, DrumSynth Live | drums | `drums.rs` | done |
| (pad bank), Harmless pads role | pads | `pad.rs` | done |
| DirectWave (player), Channel Sampler, SoundFont Player, FLEX (playback role), Slicex (playback) | sampling | `sampler.rs` (+ sfizz/VST hosting for SF2) | done |
| 3x OSC, SimSynth, PoiZone, BooBass, MiniSynth, Sawer, Kepler Exo, GMS | subtractive workhorse synth | `tri_osc.rs` | done (waveforms + detune + sub + 24 dB env filter + glide; Transistor Bass role via presets) |
| Transistor Bass | 303-style bass | `tri_osc.rs` presets | done (saw + high fenv + resonance + glide) |
| Sytrus, Fruity DX10, part of Toxic Biohazard | FM | `fm.rs` | done (2-op + feedback + dual envs + sub) |
| Harmor, Morphine, Ogun, Harmless | additive | `additive.rs` | done rung 1 (up to 48 partials, two morph level banks + XY morph, formant-bell bank, per-partial decay tilt; rung 2 candidates: unison/detune stack, per-partial vibrato) |
| Plucked!, Sakura | plucked string model (Karplus–Strong) | `pluck.rs` | done (fractional-delay KS, pick comb, body, stiffness) |
| Fruity Granulizer, BeepMap (texture role) | granular | `granular.rs` | done (rack-asset grain clouds, notes = playheads) |
| FLEX, GMS, Kepler, Drumaxx, Harmor et al. (content depth) | preset factories | script presets + hosted Vitalium/Dexed/Geonkick/sfizz set | host |

## Capability log (ABI / engine)
Everything shipped in tier 1 is expressible with the **current** ABI —
deliberate proof that the ladder's rung 1 (no ABI change) carries most of a
commercial suite. Recorded needs for the later tiers, cheapest first (full
ladder: `slooper-effect-authoring` skill §ABI evolution):
1. **Param metadata** (discrete steps / enum labels / units) — nicer auto
   GUI for mode sliders (overdrive character, gater shapes). Workaround
   today: quantize in `process`, label spells out the steps.
2. **MIDI-clock / transport sync** (per-sample block call with pos) —
   sync'd echo/gater phases, Gross-Beat-class scrubbing.
3. **Stereo or multichannel buffers** — **SHIPPED tier 3 as ABI v3**:
   `EffectInfoV3` = the full V2 struct + one appended fn
   `process_st(state, l, r, params, sc) -> l` (out-R through a trailing
   pointer). Prefix layout is compile-time-asserted, so V1/V2 scripts
   load unchanged and the loader sets `desc.stereo` only on a valid
   `slooper_effect_info_v3` export with abi==3. The engine chain now runs
   `(l, r)` per slot: mono slots process the mid and return (y, y); the
   mono per-track fold is bit-true (`play_l += ml*pgl; play_r += mr*pgr`,
   pan-law only in the master fold) — a v1/v2 chain measures L−R = 0 on
   the master buses (pinned by `e2e_tier3_stereo_spread_live`, which ALSO
   asserts a v3 script decorrelates). Scripts (11 total since tier 5b):
   `spread.rs`, `panmatic.rs`, tier-4 `chorus.rs` (per-side tap banks) +
   `echo3.rs` (ping-pong reads), tier-5 `space_echo.rs` (dual-bank
   FDN + per-side echo) + `phaser.rs`/`flanger.rs`, and tier-5b
   `shifter.rs`/`gater.rs`/`vocoder.rs`/`pitchshft.rs`. Write the pair into the L slot +
   out pointer; never
   assume L == R in a v3 slot. FLAG SEMANTICS: `desc.sidechain` is
   true only for the EXACT v2 generation — a v3 script must export the
   v2 prefix, so `process_sc` presence alone proves nothing; a v3
   effect wanting the key signal reads the `sc` argument of
   `process_st` (pinned by `flsuite_chain_window_tags_stereo_script`:
   chain row shows `[fx:st]`, never `[fx:sc,st]`).
4. **Control-rate OUT** (an effect publishing a CV lane the automation
   system can bind) — Peak/Envelope/Formula-controller duties.
5. **Block (vector) process** — CPU headroom for vocoder/multiband tiers.
6. **File pickers** (or fixed-folder scan) — **SHIPPED tier 2 by the
   convolver**: scripts may scan a fixed data folder in `create()`
   (never `process`) and read env overrides — the convention is
   `$SLOOPER_IMPULSES_DIR` > `$XDG_DATA_HOME/slooper/impulses` >
   `~/.local/share/slooper/impulses`, folder + README auto-created on
   first use, everything parsed/precomputed at create so `process`
   stays allocation-free. Same recipe works for future wavetable/preset
   folders.
7. **Additive partial bank** (tier 3 design, no ABI change needed):
   per-voice table of N harmonic oscillators + two morph level tables +
   per-partial decay tilt covers Harmor/Morphine/Ogun/Harmless.

## Development cycle notes
- Bundle path: add `effects/<name>.rs` → register in `fx::EXAMPLES`
  (`src/fx.rs`) → the two `stems()` test lists now DERIVE from `EXAMPLES`
  (was a hard-coded-list trap pre-2026-11-05). `seed_api` adds missing
  stock scripts to existing installs, backs up stale ones to `.bak`.
  Synths: add `synths/<name>.rs` (stamp `// slooper-synth-version: N`
  line 2) → register in `synth::BUNDLED`; the
  `all_bundled_synths_render_sound` gate DERIVES from `BUNDLED`
  (tier-2 rule: bundling = covered, mirrors the effect side).
- Verification per plugin: standalone `rustc --edition 2021 --crate-type
  cdylib -C opt-level=2` clean (that IS the app's command for scripts),
  then `cargo test fx::` — the real loader compiles/loads
  every bundle; `bundled_effects_produce_sound` renders a fixed fixture
  (tone + percussive bursts) through every bundled effect and asserts
  finite / in-range / non-silent output (plus a live-key pass over the
  v2 sidechain path, plus an STEREO pass: every desc.stereo bundle runs
  the pair path with DEFAULT param values and must stay non-silent AND
  decorrelate L/R — seed defaults, a zeroed param array legitimately
  mutes any effect whose gain default is > 0). Special surfaces pin in
  `bundled_examples_load_with_custom_guis` (waveshaper curve GUI,
  vocoder = second stock ABI-v2, v1 effects stay GUI/sidechain-free).
- Tier-2 patterns (2026-11-05): FFT scripts work (radix-2 + precomputed
  tables, ~600 flops/sample under opt-level=2) — but NORMALIZE
  convolution IRs by ENERGY not peak (a peak-normalized 1.2 s noise IR
  is ~20× too hot; the produce_sound gate caught peak 12.7). Trap:
  `usize::to_le_bytes()` is EIGHT bytes — binary-format writers must
  cast to u32 first.
- Live-RT proof per tier-2 plugin: `e2e_tier2_new_bundled_plugins_live`
  (tests/e2e.rs) drives the public command queue against the running
  audio server — every new synth through the UI preview bounce and
  tri_osc→Convolver on the RT thread. This gate caught a REAL bug:
  granular spawned grains at block rate, collapsing into one burst on
  the 1.5 s preview block — grain starts are now sample-exact
  (`Grain.pre`). Rule: any time-based scheduling in a script must be
  SAMPLE-exact; host blocks range from ~1 ms (RT) to seconds (preview).
  Granular test assets must be constant-energy (note→playhead mapping
  can park a grain on a sine's zero crossing → legitimate silence).
- Tier-3 patterns (2026-11-05): the ABI-v3 stereo export needs the
  `(info, info_v2, info_v3, gui)` symbol quartet — the loader accepts
  v3 only with all prefix versions present (mirror the sidechain_comp
  dual-export source exactly, incl. `desc.stereo` only from a valid
  v3 struct). Engine-side: mono slots MUST return (mid, mid) and the
  per-track fold multiplies L by pgl and R by pgr separately — never
  re-sum to mono before the master fold. GUI meters read state through
  the same atomics/audio_state pattern as v1 (ABI bump ≠ GUI change).
  UI picker marks v3 scripts with an (ST) suffix, same as (SC).
  GUI-test trap: plugin-editor pins need a real DISPLAY (:1 on this
  host); headless they fail deterministically for the wrong reason.
- Live test: boot the freshly built binary on the X display and capture
  the Effects window (all rows valid), plus one loaded-chain screenshot.

## Tier-4 rework log (2026-11-06 — the standing "re-check with fresh eyes" pass)

The whole bundle was re-audited script by script (string-literal
non-ASCII sweep: clean — all non-ASCII sits in comments; NaN guards,
`one_c`-style true-Hz poles, param-index discipline: clean except as
below). Two SHIPPED-BUT-DEAD bug classes surfaced, both invisible to the
non-silence gate:

- **Absolute ring reads** (`echo3.rs`, `flanger.rs`): `read_lin` computed
  the tap index as `buf[d]` instead of `(widx − d) mod MAX`. The tap then
  heard input only at times `t ≡ d (mod MAX)` — a feedback loop fed this
  way is STARVED (impulse → dry only, measured peak = exactly the dry
  path), and continuous output became one scattered smear that swept the
  ring once per wrap. RMS-based produce-sound gates pass smears. Fix:
  head-relative reads in both scripts (chorus/space_echo/pitchshft/
  granular/pluck were already correct — audited).
- **Self-chasing feedback** (`chorus.rs`): `lp += k*(fb − lp)` with
  `fb = lp` at the end of the same block freezes the filter at 0 — the
  feedback slider had NO effect at any value since tier 1. Replaced with
  delayed wet-recirculation (the tap average recirculates through the
  tone lowpass; period = tap delay, like the BBD classics).
- **Gate lesson**: every script with a feedback loop now needs an
  IMPULSE-STRUCTURE pin, not an energy pin — `fx::tests::
  tier4_feedback_loops_ring` asserts discrete echo3 echoes with the
  correct decay ratio, chorus tail sustain, flanger comb ringing, and
  that the ping-pong R echo arrives LATE (a smear cannot fake any of it).
  When adding a delay/feedback script, copy that test's shape.
- **Zipper-click sweep**: raw delay-time sliders move a tap by whole
  samples per frame — `echo3`/`chorus`/`flanger` nominal times are now
  slew-limited (~3–5 ms time constant). Modulation depth stays un-slewed
  (that IS the effect).
- **Stereo upgrades** (rung 3, zero ABI change — the capability shipped
  in tier 3 was simply unused): `echo3.rs` gained ping-pong (R taps
  `time × (1 + ping-pong)`; 0 = parallel, 1 = hard alternate) and
  `chorus.rs` became the Hyper-Chorus per-side tap bank (`voices` taps
  per side, R bank offset `spread × 180°`). The v3 script list is now
  spread / panmatic / chorus / echo3 — `bundled_effects_produce_sound`
  asserts `st_checked >= 4`, and `e2e_tier4_stereo_echo_chorus_live`
  proves both decorrelate on the LIVE master buses.
- Appended params are persistence-safe: chain slots pad stored values
  with descriptor defaults (`fx.rs` `slot values`), so new sliders must
  only be APPENDED (indices are sticky per script stem).
- UI pin `flsuite_chain_window_tags_stereo_script` updated: the mono
  counterexample is now `compressor` (chorus carries the badge).

## Tier-5 stereo reverb log (2026-11-06 — `space_echo.rs` → ABI v3)

The FL rooms (Reeverb 2 / LuxeVerb) are stereo devices; the tier-1 script
was mid-only. Rebuilt as: per-side echo paths (own delay/tone pole/DC
blocker — a hard-panned source now ECHOES to its side, measured R-energy
exactly 0 for an L-only impulse), and an FDN built from TWO comb banks —
L bank carries the tier-1 tunings verbatim, R bank is spread (`BANK_SPREAD`
= 1.031 lengths, `BANK_PHASE` = 2.3 rad modulation offset) so identical
feeds decorrelate within a few loop rounds. The `width` param (11, APPENDED
— indices sticky) cross-mixes the bank sums: `L' = a(1+w)/2 + b(1−w)/2`.
`ap_r` reuses the L diffuser tunings DELIBERATELY: at width 0 both sides
then receive the identical sum through identical chains → the tail is
BIT-EXACTLY mono (pinned). Width default 1.0 keeps the v1 `process`
fallback (call `process_st(x,x)`, return LEFT) equal to the historical
circuit — every pre-tier-5 forensic pin (v13 DC wash, v14 snap glitch,
mode density, wet tail) passes unchanged at default params.

- Gate design (`tier5_space_echo_stereo_tail`, impulse-structure shape per
  tier-4 rule): echo measured with VERB OFF (discrete repeats, ratio
  0.232); decorrelation measured with ECHO OFF — the echo component is
  100 % correlated BY DESIGN (shared taps) and sank the ratio to 0.17 when
  both were open. Verb-only window 0.5–0.9 s measured D/tail = 1.417
  (= √2, textbook full decorrelation); width-0 run asserted BIT-equality
  (`to_bits()`), not a threshold. Sabotage-verified BOTH ways: identical
  banks (spread AND phase zeroed — phase alone still decorrelates, so
  sabotage must neutralize every differentiator) → decor pin fails
  (dif 0); `ap_r` tuning +1 → width-0 pin fails. A "stereo" tail that is
  really the same mono signal twice passes energy gates; it cannot pass
  this pair.
- The ST-fixture gate in `bundled_effects_produce_sound` now demands
  `st_checked >= 5` (spread/panmatic/chorus/echo3/space_echo);
  `e2e_tier5_stereo_reverb_live` proves real master-bus decorrelation on
  the RT thread. Host `builtin_presets` mirror and `v10_eq_ships_default_presets`
  carry the 12th value (that pin caught the drift for free).
- GUI: knob row pitch 66 → 61 px for twelve knobs, new `width` knob, tail
  wedge drawn as an L/R pair. All pins are galley/label-based, so pitch
  changes are layout-safe (verified).
- CPU note: dual bank doubles reverb cost (16 combs + 12 diffusers at
  ~200 flops/sample — fine per the convolver precedent); a future block-
  process ABI rung (5) is the lever if that ever bites.

## Tier-5 stereo modulation log (2026-11-06 — `phaser.rs` + `flanger.rs` → ABI v3)

Candidate #2 from the tier-5 list, same ladder rung (rung 3 already
shipped — zero ABI/engine change, mirror the spread.rs export trio).
Both scripts got ONE appended param, `spread` (index 7, default 0.5) —
persistence-safe by the append rule; the `bundled_effects_produce_sound`
roster gate moved `st_checked >= 5 → 7`.

- **Phaser = dual-path Vintage Phaser**: the all-pass cascade runs twice
  (per-side state), the R path reads the SAME LFO clock `spread` further
  along (one clock, two read heads — drift impossible by construction).
  L path unchanged → the v1 fallback (`process_st(x,x)`, return LEFT)
  is the historical mono circuit. Measured on LCG noise: spread 0 →
  L/R bit-identical (0 differing samples / 14400); spread 0.5 →
  D/RMS 1.27 ≈ full decorrelation.
- **Flanger = shared comb, opposite-sweep taps**: ONE feedback ring
  written with the MID `(l+r)/2`, voice taps read per side with the R
  bank offset by `spread`, dry strictly per-side. For identical inputs
  the mid IS the tier-4 mono write and the L taps carry the tier-4
  phase — `tier4_feedback_loops_ring` passes unchanged (fallback
  identity pinned bit-for-bit in `tier5_phaser_flanger_stereo`).
  Measured: spread 0 bit-identical; spread 0.5 D/RMS 0.67 (comb taps
  decorrelate less than the phaser's resonant peaks — different physics,
  threshold 0.3 keeps margin); hard-panned L impulse: R energy exactly
  0 in the dry window, comb tail balanced L≈R within 2x (topology IS
  mono — a wet-only-left flanger would fake image preservation).
- Pins: `fx::tests::tier5_phaser_flanger_stereo` (structure shape:
  bit-exactness at spread 0, decorrelation floors, image split, fallback
  identity, param-table freeze for 0..7). Sabotage-verified THREE ways
  before trusting: flanger R-offset zeroed → decor pin fails (D/RMS 0);
  R dry fed from xl → image pin fails; phaser R-offset zeroed → decor
  pin fails. Restored tree re-green.
- Probe-tooling trap (reusable): ctypes FFI probes must read DESC
  *contents* via `ctypes.cast(addr, POINTER(c_void_p))[0]` — `cast(addr,
  c_void_p).value` returns the SLOT ADDRESS, so a "function pointer"
  turned out to be the static-data region (segfault that looks like an
  effect bug). And never end a ctypes probe without `os._exit(0)`:
  interpreter teardown dlcloses the Rust cdylib (the never-dlclose rule).

## Tier-5b stereo finals log (2026-11-07 — `shifter` / `gater` / `vocoder` / `pitchshft` → ABI v3)

Candidate #5 done — the last four mono scripts joined rung 3 (zero ABI
change, mirror the spread.rs export quartet + one appended `spread`,
default 0.5; `bundled_effects_produce_sound` gate: `st_checked >= 11`).
Structural pins `tier5b_stereo_mods` + `tier5b_params_appended_only`
(param tables frozen 0..N-1); measured decor: shifter 1.41, gater 0.93
(antiphase at spread 1 = 2.00), vocoder 0.43, pitchshft 1.43.

- **shifter = TRUE stereo dual SSB**: a full Hilbert pair + rotator +
  feedback loop PER SIDE (R shifts `shift × (1+spread)`). A hard-panned
  input measures EXACTLY zero on the other rail — pinned at 12000
  samples (see window rule below).
- **gater = dual-lane Gross Beat**: ONE LFO clock, the R lane reads it
  `spread × 0.5` cycles further along with its own attack/release
  smoother (1.0 = antiphase: L open while R closed — measured D/RMS
  2.00 = the textbook maximum).
- **vocoder = carrier decorrelation with a mono guarantee**: one shared
  per-band envelope (the articulation can never wobble the image),
  per-side carrier biquads, and the rails interleave band signs —
  L weights band i by (1+spread·(−1)^i), R by (1−spread·(−1)^i). The
  side component is ANTISYMMETRIC, so (L+R)/2 equals the v2 mono circuit
  at ANY spread (pinned: max error 1.1e-8 on a 0.25 peak; the per-side
  tilt one-poles stay linear, so the downmix commutes through them).
  FLAG CONSEQUENCE (tier-3b semantics): vocoder now loads through the
  v3 entry, so `desc.sidechain == false` and the UI badges it (ST),
  never (SC) — it keys off the `sc` ARGUMENT of process_st, which the
  engine always supplies (`fx_mix`/`run_chain_block`).
- **pitchshft = two delay lines, ONE modulation clock** (phaser
  topology): the R read head reads the same saw phase `spread × 0.5`
  periods further along through its own delay line + tone LP; the YIN
  detector tracks the MID (identical inputs ⇒ bit-identical to the
  tier-3 mono circuit, pinned). GUI gained a `spread` slider
  (want_height 240 → 336). **Semantics**: spread only moves the WET —
  and at 0 shift the taps barely travel, so the e2e seeds semitones 5
  to test with real tap movement.

Harness fixes this tier exposed:
- **produce_sound ST pass must feed a LIVE key** (`sc = x`): a v3
  script that consumes the sidechain (vocoder) reads the `sc` argument
  of process_st, and sc=0 legitimately mutes it — the non-silence gate
  must not mistake design for failure.
- **Image windows must out-run the script's deepest internal delay**:
  the first sabotage matrix MISSED a pitchshft wet cross-feed because
  the image impulse ran 4000 samples while the mod delay line is 6000
  deep — no sabotage could reach R. All image pins now run 12000
  samples. Rule: image/measurement window > deepest delay line, always.
- Sabotage matrix (7 mechanisms: per-script decor engine, shifter &
  pitchshft wet cross-feed, vocoder downmix antisymmetry) — after the
  window fix ALL SEVEN collapse their pins; probe tooling note: ctypes
  v3 struct slots are 6 create / 7 destroy / 8 process / 9 process_sc /
  10 process_st (5 is the params POINTER — calling it segfaults), and
  a probe ending in `os._exit(0)` must `sys.stdout.flush()` FIRST or
  every print vanishes.
- Live proof: `e2e_tier5b_stereo_mods_live` runs all four through the
  RT thread on the master buses. The vocoder pass keys from a SECOND
  track (same asset, vol-gated): `set_sc_send(i, mask)` masks off bit i
  — a self-send is impossible by construction, and the sc tap is
  post-trim so track volume doubles as the key gate.
- **MONITOR-STEAL BUG (found by that e2e, fixed same day)**: with no
  input connected, the handler still ran the SELECTED track's chain
  every sample on silence (`mon = fx_mix(sel, 0, 0.0)`) through the
  SAME script instances as the playback path. The interleaved
  (signal, 0, signal, 0…) stream at 2x the sample rate decimates
  narrow band-pass state (a vocoder band's output fell ~25x — its wet
  went D/S 0.4 → 0.0003 on the live bus) and starves envelope
  followers. Fix: the monitor chain runs only while inputs are
  connected AND per sample only while the input (or the monitor's own
  decaying tail) is non-zero. Rule for future scripts: the playback
  chain's instance must stay single-writer; anything else that wants
  the effect `spawn_fresh`es its own instance (offline render already
  does). The e2e only exposed this because it was the first
  sidechain-CONSUMING script placed on the selected track.

## Tier-5 candidates (next rework cycle, in value order)

1. ~~`space_echo.rs` → ABI v3 stereo reverb~~ **DONE 2026-11-06** (see
   tier-5 log above).
2. ~~`phaser.rs` / `flanger.rs` v3 width modes (Vintage Phaser dual
   path)~~ **DONE 2026-11-06** (see stereo modulation log above).
3. Param metadata (rung 1): enum labels for the mode sliders (overdrive
   character, gater shapes, shifter modes) — proper ABI bump, design in
   `effect_api.rs` first.
4. Transport sync (rung 2) — unlocks the Scratcher/Gross-Beat row and
   tempo-phase gater/echo.
5. ~~Remaining mono scripts for the rung-3 pass (value order): `shifter.rs`
   (true stereo SSB — FL's freq shifter is per-side), `gater.rs`
   (per-side step gates), `pitchshft.rs` (dual delay-line already — just
   needs independent R modulation to spread), `vocoder.rs` (carrier
   decorrelation). Each = mirror spread.rs trio + one appended `spread`
   + structure pins per the tier-4/5 shape.~~ **DONE 2026-11-07** (see
   tier-5b log above — ALL bundled effects that can hold a stereo image
   are now ABI v3; the remaining v1/v2 scripts are per-mid by nature:
   dynamics/split/shaping, where a stereo pair adds nothing).
- Live smoke 2026-11-06 (tier-4 binary sha 707a3f3d…): boots on :1, seeds the new echo3/chorus into ~/.local/share/slooper/effects (backups .bak present), full clean window render, 0 kernel conflicts, clean exit.
