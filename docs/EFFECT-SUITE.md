# The native effect suite

slooper ships **23 scripted effects** — plain Rust files in `effects/`,
seeded into `~/.local/share/slooper/effects` on first launch and re-seeded
(never overwritten) on every app update. Every one of them is **live**: the
same code path runs during playback, monitoring, preview bounce and export.

This document covers the shared protocol every effect follows, the roster,
and what "stereo aware" means concretely in slooper.

## The protocol

An effect is one standalone Rust file with a C ABI (see
`effects/effect_api.rs` for the contract; `effects/template.rs` is the
starting point). Three ABI generations coexist:

- **v1** — `process(in: f32) -> f32` on the summed mono sample. Simple
  effects that are genuinely mono-safe (compressor, EQ, crusher, …).
- **v2** — adds a per-sample `sc: f32` **sidechain key**: the summed audio
  of every track routed to this one through the mixer's **SC** picker
  ([SIDECHAIN.md](SIDECHAIN.md)). Consumers: the ducking compressor and
  the vocoder.
- **v3** — adds a **stereo lane**: v3 slots run a second entry point
  `process_stereo(l: f32, r: f32) -> (l, r)` on every block; the summed-mono
  `process` call is kept only as the fallback path for mono sources and
  for the offline per-lane render check.

The host enforces the protocol with pins that run in CI
(`fx::tests::native_suite_*`): every v3 effect must (a) declare it, (b)
**ring on a stereo tap test** — a one-sample tap with a long feedback loop
must come out of the *opposite* lane (ping-pong / cross-coupled) or the
same lane (true per-side processing), and (c) **fold exactly**: feeding
identical L/R must produce bit-identical L/R, so a mono source never
becomes fake-stereo and a downmix never changes level.

## Roster

Parameter ranges below are what the slider shows; values are native units
(Hz, dB, ms). Every effect carries a **mix** (wet/dry) and an **out** trim
unless noted. Factory presets live beside your own in
`~/.local/share/slooper/effects/user presets/` and are ordinary JSON files
you can edit — see [USAGE.md](USAGE.md#presets).

### Time & space

| Effect | File | What it does | Key params |
| --- | --- | --- | --- |
| **Delay** | `delay.rs` | Studio delay: filtered + saturated feedback, key ducking, stereo ping-pong | time 5–2000 ms · feedback ≤0.95 · tone · low cut · saturation · duck (SC) · ping-pong |
| **Echo + Reverb** | `space_echo.rs` | Dotted-time echo into a damped reverb tail; tape wobble | time · feedback · tone · decay · damp · echo/verb levels · width |
| **convolver** | `convolver.rs` | Convolution reverb: drop `.wav` impulses into `~/.local/share/slooper/impulses`; index 0 is a built-in hall | mix · gain · damp Hz |
| **Chorus Doubler** | `chorus.rs` | Modulated-delay chorus (wide ensemble at long base, flanger territory at short) — per-side tap banks | rate · depth ms · base ms · feedback · tone · voices 1–4 · spread |
| **Flanger** | `flanger.rs` | Short-modulated delay, per-side LFO phase offset | rate · depth · base ms · feedback · voices 1–4 · spread |
| **Phaser** | `phaser.rs` | Allpass ladder, per-side LFO phase offset for true stereo sweep | rate · depth · base Hz · feedback (can go negative) · stages 1–12 · spread |
| **Auto Pan** | `autopan.rs` | Pan + tremolo LFOs with five shapes (sine/tri/saw/square/track-and-hold) and an optional hard-beat mode | pan shape/rate/depth · vol shape/rate/depth · phase off · slew |

### Dynamics

| Effect | File | What it does | Key params |
| --- | --- | --- | --- |
| **Compressor** | `compressor.rs` | Leveler: peak detector, soft-knee ratio, makeup, parallel mix | threshold −60–0 dB · ratio ≤24 · attack · release · knee · makeup · mix |
| **Sidechain Compressor** | `sidechain_comp.rs` | Feed-forward ducking keyed by the mixer **SC** row — key-only at depth 1 (the sidechain demo effect) | key threshold · ratio · attack · release · duck amount |
| **Limiter** | `limiter.rs` | Transparent clipper/limiting with three modes, or a per-side hard clipper for parallel-NY chains | mode · threshold dB · ratio · attack (0.05 ms min) · release |
| **Multiband** | `multiband.rs` | LR-crossover 3-band compressor **or** maximiser with per-band gain and mix | low/mid/high crossover · per-band threshold · per-band gain · ratio · maximise |
| **Transient** | `transient.rs` | Splits attack vs sustain with two envelopes and re-balances them — more punch or more body, after the fact | detect ms · smooth ms · transient 0–2.5 · sustain 0–2.5 |
| **Gater** | `gater.rs` | Rhythmic gate (tempo-synced and free LFO shapes) with a per-side phase offset; can duck on the sidechain key | rate · shape · width/decay · smooth ms · phase · spread |

### Tone & distortion

| Effect | File | What it does | Key params |
| --- | --- | --- | --- |
| **Parametric EQ** | `parametric_eq.rs` | 6-band RBJ biquad EQ (shelf/peak/notch/bandpass per band) with a response curve + live spectrum in its own window | per band: type · freq 20–20k · gain −120..+30 · Q |
| **Overdrive** | `overdrive.rs` | Four distortion characters (soft asym → hard → fold → fuzz) with tone + presence | drive · curve 0–3 · tone · presence |
| **Wave Shaper** | `waveshaper.rs` | Draw your own transfer curve: nine draggable points + drive/mix/out; tube, fuzz, hard-clip, step and fold factory presets | curve p1–p9 · drive ≤16 · dc remove |
| **Crusher** | `crush.rs` | Bit-depth + downsample lo-fi with smoothing and drive | bits 1–16 · downsample ms · smooth · drive |
| **Filter** | `vfilter.rs` | State-variable LP/BP/HP/notch with resonance, drive, slope option and envelope auto-wah | mode · cutoff · resonance · env · attack · release · slope |
| **Enhancer** | `enhancer.rs` | One-knob harmonic exciter: four character bands + an air band | char 0–3 · amount · air · out |

### Pitch & spectral

| Effect | File | What it does | Key params |
| --- | --- | --- | --- |
| **pitch shifter** | `pitchshft.rs` | Grain-based pitch shifter, chromatic corrector and tuner meter — internal YIN detector (mono sources track best); dual-detector with per-side phase offset on stereo | mode · semitones ±24 · fine cents · strength · ref Hz · gate dB · spread |
| **Shifter** | `shifter.rs` | SSB frequency shifter (Hilbert): every partial moves by the same Hz; feedback for resonant metal; independent R-side shift on stereo | shift Hz ±8000 · feedback · spread |
| **Vocoder** | `vocoder.rs` | 18-band vocoder — this track's audio is the **carrier**, route a voice/synth via the mixer **SC** row as the **modulator**; per-band carrier decorrelation keeps stereo alive while a mono downmix stays exact | attack · release · peakiness · band range · brightness · spread |
| **stereo spread** | `spread.rs` | Haas-comb widener with image rotation; real L/R to the track pan | width · haas ms · side lp · rotate hz · flip R |

## Conventions

- **Dry/wet**: every slot mixes internally; the chain list is ordered
  processing order (top of the FX list = first in the path).
- **Headroom**: `out` trims are per-slot; the master out window
  (top bar ▸ Windows) carries the session limiter + meters.
- **Latency**: nothing here adds reported latency; lookahead is zero
  (v3 sidechaining is one audio block late by design — ~2.6 ms at
  48 kHz/128, far under any detector's attack).
- **Preset format**: one JSON per preset in `user presets/`, named
  `{plugin stem}-{preset name}.json`, values in native units — see
  [USAGE.md](USAGE.md#presets).

## Writing your own

Copy `effects/template.rs`, rename, edit — it compiles on the next scan
(`rustc` must be installed; scripts build with `--edition 2021 --crate-type
cdylib`). Keep `process()` allocation-free and non-blocking: it runs in the
realtime path. If you write a time-based effect, declaring v3 is almost
always right: implement `process_stereo` and the fold/loop pins will tell
you the moment stereo breaks.
