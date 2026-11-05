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
| Convolver | impulse reverb | `convolver.rs` | later (IR-folder capability: scan `~/.local/share/slooper/impulses`, param picks index) |
| Multiband Delay | split echo | `echo3` + `multiband` chain | later (chainable today) |
| Spreader, Stereo Enhancer, Stereo Shaper, PanOMatic pan, Flangus stereo, ping-pong delays | stereo width/image | — | later (**stereo-buffers capability**, see below) |
| Pitch Shifter, Pitcher, Tuner | pitch shift / correction / detect | — | later (pitch-detect capability) |
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
| 3x OSC, SimSynth, PoiZone, BooBass, MiniSynth | subtractive workhorse synth | `tri_osc.rs` (synth API) | later (next tier) |
| Sytrus, Fruity DX10, part of Toxic Biohazard | FM | `fm.rs` | later |
| Harmor, Morphine, Ogun | additive | `additive.rs` | later |
| Plucked!, Sakura | plucked string model (Karplus–Strong) | `pluck.rs` | later |
| Transistor Bass | 303-style bass | `tri_osc.rs` presets | later |
| Fruity Granulizer | granular | `granular.rs` | later |
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
3. **Stereo or multichannel buffers** — Spreader/wideners, true stereo
   chorus/flanger, ping-pong, PanOMatic pan. Engine is mono per chain slot
   today; pan exists per track but effects cannot touch it.
4. **Control-rate OUT** (an effect publishing a CV lane the automation
   system can bind) — Peak/Envelope/Formula-controller duties.
5. **Block (vector) process** — CPU headroom for vocoder/multiband tiers.
6. **File pickers** (or fixed-folder scan) — Convolver IR selection.

## Development cycle notes
- Bundle path: add `effects/<name>.rs` → register in `fx::EXAMPLES`
  (`src/fx.rs`) → the two `stems()` test lists now DERIVE from `EXAMPLES`
  (was a hard-coded-list trap pre-2026-11-05). `seed_api` adds missing
  stock scripts to existing installs, backs up stale ones to `.bak`.
- Verification per plugin: standalone `rustc --edition 2021 --crate-type
  cdylib` clean, then `cargo test fx::` — the real loader compiles/loads
  every bundle; `bundled_effects_produce_sound` renders a fixed fixture
  (tone + percussive bursts) through every bundled effect and asserts
  finite / in-range / non-silent output (plus a live-key pass over the
  v2 sidechain path). Special surfaces pin in
  `bundled_examples_load_with_custom_guis` (waveshaper curve GUI,
  vocoder = second stock ABI-v2, v1 effects stay GUI/sidechain-free).
- Live test: boot the freshly built binary on the X display and capture
  the Effects window (all rows valid), plus one loaded-chain screenshot.
