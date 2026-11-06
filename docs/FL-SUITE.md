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
| Fruity Reeverb 2, LuxeVerb, part of Fast Reverb | algorithmic reverb | `space_echo.rs` | done (11 params + space presets GUI) |
| Fruity Chorus, Hyper Chorus, Vintage Chorus | chorus / doubler / ensembles | `chorus.rs` | done |
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
| Fruity Phaser, Vintage Phaser | phaser | `phaser.rs` | done |
| Fruity Flanger, Fruity Flangus | flanger / unison | `flanger.rs` | done |
| Fruity PanOMatic (vol), Gross Beat (gate/stutter lite) | tempo volume-gating | `gater.rs` | done |
| Fruity Delay 2, Fruity Delay 3, Fruity Delay Bank (single-bank core) | delay + filtered feedback | `echo3.rs` | done |
| Frequency Shifter | SSB shift (metallic/vibrato) | `shifter.rs` | done |
| Transient Processor, Transmitter (split lite) | transient/sustain shaping | `transient.rs` | done |
| Fruity Vocoder, Vocodex | vocoder (SC carrier feed) | `vocoder.rs` | done |
| Convolver | impulse reverb | `convolver.rs` | done (partitioned overlap-save FFT, **folder IR capability** — `~/.local/share/slooper/impulses`, env override, index 0 = built-in hall) |
| Multiband Delay | split echo | `echo3` + `multiband` chain | later (chainable today) |
| Spreader, Stereo Enhancer, Stereo Shaper, Flangus stereo, ping-pong delays | stereo width/image | `spread.rs` | done (ABI-v3 reference: mid/side + comb-Haas width, mix/gain; **stereo-pair capability** shipped, see capability log 3) |
| Fruity PanOMatic (pan side; vol side in `gater.rs`) | tempo pan LFO | `panmatic.rs` | done (ABI-v3: equal-power sine/tri/inverse-tri pan + optional volume wheel, tempo-synced) |
| Pitch Shifter, Pitcher, Tuner | pitch shift / correction / detect | `pitchshft.rs` | done (dual delay-line shifter + internal decimated-YIN auto-correct + tuner-meter GUI; pitch detect needs no ABI change — decimate inside `process`, rung 1) |
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
   asserts a v3 script decorrelates). Scripts: `spread.rs`,
   `panmatic.rs`. Write the pair into the L slot + out pointer; never
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
