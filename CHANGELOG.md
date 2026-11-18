# Changelog

All notable changes to slooper are documented here, newest first.
Versions are the git tags (`v<X.Y.Z>`); builds live on
[the releases page](https://github.com/actually-god/slooper/releases).

## [0.23.0] — unreleased

The **workflow release**: every sound gets a preset shelf, hosted
plugins expose their factory programs, the mixer grows a full-page
console, the grid grows color — and nearly every click now answers back.
146 commits since 0.22.0, CI-green on Linux **and** Windows throughout.

### Presets for everything

- **One preset bar, same shape everywhere**: a name box, **save**, a
  **load** picker (delete right inside it), and a reset to defaults.
- New preset kinds since 0.22: **script-synth presets** (per synth),
  **drum kits** (all 16 pad assignments as one named kit), **sampler
  key-map presets**, and **track templates** — a track's instrument +
  FX chain + mixer levels + color, saved once and applied to any track
  from the mixer strip's right-click menu.
- **VST2 / VST3 factory programs**: the PARAMETERS window grew a
  **program** picker for the plugin's own presets. Your pick saves
  with the project, is re-applied on load, and stays in sync both
  ways (pick it in the plugin's own editor → the picker follows).
  Multi-part VST3 instruments included.
- **CLAP presets**: a plugin loading a preset on its own is now echoed
  to the status line and persisted with the project, and the PARAMETERS
  window gained a **load preset…** file picker.
- **Per-slot chain bypass**: silence one effect or plugin inside a
  track's chain without removing it — live and in export alike.

### Mix page, color, and grid workflow

- **MIX page** (`F9` or the MIX toggle on the grid toolbar): the center
  area flips to a full-page mixer console — one channel column per
  track with color chip, record/mute, vertical faders, pan, and chain
  chips.
- **Clip & track colors**: right-click a clip or an empty lane for the
  **Color** submenu (named swatches). Tracks carry a default that their
  clips inherit, and a chip marks colored lanes and mixer headers.
  Colors ride copy/paste, duplicate, split, and the save file.
- **Record groups**: `grp` on any strip adds that track to the record
  group; the top-bar **REC GROUP** master arms and captures every
  member at once.
- **Vertical group drag**: dragging a multi-clip selection now moves it
  across lanes as well as sideways.
- **Mixer layout choice**: the *mixer follows tracks* setting toggles
  between strips mirroring the timeline's track heights and a uniform
  mixer-owned layout.
- **Piano roll upgrades**: right-click a note for the **length menu**
  (whole … 1/64, dotted and triplet columns, double/halve, delete);
  its own **snap** switch on the roll toolbar; **glide notes** — draw
  on the glide lane, or Z-drag from a note to chain slide-to-note;
  a **velocity lane** (bar height = velocity); note **copy/paste across
  tracks**; and double-clicking a MIDI clip opens it straight in the
  roll.

### Sampler & sample editor

- The **key map now lives inside the sampler window**: the full
  128-note range in a real piano layout, key clicks audition the track,
  per-row sample slots with **velocity layers**, and named map presets.
- **Velocity means recorded dynamic**: a layer's `v` value says "this
  sample was recorded at that dynamic" — harder hits push up to ×4
  louder instead of just re-firing the same amplitude.
- **Sample editor, built around the waveform**: a live waveform canvas
  with playhead and click-to-scrub, fades as draggable **curve handles
  right on the wave**, pitch/bend/stretch controls on the canvas — and
  every edit is **audible immediately** while you drag.
- The bundled **TR-808 pack grew to 24 one-shots**, and a ready-made
  **808-Kit** drum preset boots on first run — one click loads 16 pads.
  Any drum pad can also import its own `.wav`.

### Knobs that say what they mean

- All **23 bundled effects** now ship full parameter metadata: real
  **units** (Hz, ms, dB, ×, %) in readouts and tooltips, integer
  steppers where the effect snaps to steps, and **dropdown pickers**
  where a parameter is really a switch or a list.
- **Hosted plugins get the same treatment**: the host reads VST3/CLAP
  parameter metadata, so their sliders show true units and enum
  pickers.
- **The PARAMETERS window handles param-dense plugins**: the whole body
  scrolls, so deep rows can never hide off-screen; a filter box
  narrows the list with a live match count; and the list is honest
  about what it shows — "showing the first 400 of N" on huge plugins,
  an in-window "no parameters" on empty ones.
- **Right-click a hosted parameter** for its automation menu — bind it
  to an automation lane exactly like a stock effect parameter — and
  **double-click** any value to type it in.
- Plugin editors show **live value readouts** on knobs and follow the
  app theme (including the light theme).

### Everything answers you

- A hover-feedback pass across the app: dialogs, menus, pickers and
  action buttons explain themselves, and destructive actions
  (delete/clear/reset) always carry a warning tooltip.
- Every state-changing action echoes to the status line — rack, chain,
  windows, presets — with counts where they matter.
- The asset rack got honest: scan progress shows inside the browsers,
  plugins that failed to load are listed with the reason, and
  truncated names carry full-path tooltips.
- Truncation is honest **everywhere**, not just in the rack: shortened
  names in the chain window, the +effect / +VST browsers, the synth
  picker and the preset dropdowns reveal the full name on hover.
- Plugin editor windows are **titled** (`Name — track N`), raise
  themselves when opened, and report failures with the plugin and slot
  named.

### Stability & correctness

- **Windows**: VST2 plugins' refusal codes were misread as success (a
  32/64-bit ABI gap) — fixed, so failed program and parameter writes
  now correctly report and refuse.
- Plugins whose GUI library tears down the probe child at exit (sfizz
  is the known case) are **no longer falsely banned** from opening
  editors, and the app no longer aborts at quit because of them.
- Upgrades finish the effect re-brand: our old-named stock copies
  (echo3 / panmatic / goodizer) are purged from your effects folder
  instead of cluttering the **+Effect** menu as duplicates — any file
  of yours wearing those names is kept.
- Drum-machine commits outline exactly the clips they stamped and
  explain empty/silent attempts in a dialog instead of one quiet
  status line.

## [0.22.0] — 2026-11-13

The **public-release** build: slooper now ships fully native on
**Windows 10/11**, a 23-effect studio suite, a sample editor, a sampler
key map, and one-click installs for every mainstream package format.
156 commits since 0.21.0, every one covered by CI on **both** operating
systems (Linux suite + Windows-native suite + a cross-compile check).

### Windows 10/11, native

- **No bridge, no Wine**: the app runs as a native GUI on the
  **WASAPI** shared-mode stream (event-driven, realtime scheduling,
  device hot-unplug auto-reconnect). A **native JACK** backend is also
  built in for Windows machines that run a JACK server; the backend is
  selectable in Settings and swaps **live** without losing the session.
- **VST2/VST3 hosting with native editor windows**, embedded per-monitor
  DPI-correct, plus **Windows MIDI** (WinMM bus, a MIDI INPUTS picker,
  and sub-block event timing from the driver's own µs stamps).
- **Crash reporting**: fatal exceptions write a minidump (symbols ship
  as `slooper.pdb`), and a small report window points at the dump.
- Settings and projects round-trip across OSes — a project saved on
  Linux loads unchanged on Windows.

### New features

- **23 bundled studio effects** — every script in `effects/` is now
  part of one stereo-aware protocol (host-enforced ping-pong/fold pins):
  EQ, compressor, limiter, multiband dynamics, transient designer,
  delay, echo + reverb, convolution reverb, chorus, flanger, phaser,
  auto pan, gater, overdrive, wave shaper, crusher, filter, enhancer,
  pitch shifter (with tuner + chromatic correction), frequency shifter,
  stereo spread, vocoder, and a sidechain compressor. Full roster with
  every parameter: [docs/EFFECT-SUITE.md](docs/EFFECT-SUITE.md).
- **Sidechain routing** — every mixer strip has an **SC** input picker;
  scripted effects and the ducking compressor/vocoder consume the key,
  and export ducks exactly like the live mix.
- **Sample editor** — per-asset reverse / ping-pong / fades / pitch /
  bend / time-stretch, live, saved with the project, never touching the
  source file.
- **Sampler key map** — up to 8 rack samples spread across the keyboard
  (draggable root blocks, chromatic or split modes); instant drum kits
  and multisamples.
- **Load `.mid` files into the rack** and drop them on any track; they
  render and export like any other asset.
- **MASTER OUT window** — master fader, peak/RMS meters, **BS.1770-4
  loudness** (momentary/short-term/integrated LUFS + LRA) and a
  real-time spectrum viewer.
- **Dock everything** — every panel and tool window docks to any edge,
  floats free, or **tab-groups**; **layout templates** capture the whole
  arrangement for one-click recall; **Ctrl+K** opens a command palette
  over every window and setting.
- **MPE** — pitch bend + per-note aftertouch reach track instruments
  and hosted plugins per channel; MIDI takes record both lanes.
- **Automation**: LFO / step / curve sources bind to any plugin, synth,
  mixer or effect parameter, live and in export.
- **Selective project load** — pick exactly which slices of a project to
  import (arrangement always; instruments, synths, bindings, mixer,
  effects, drum machine, tempo as checkboxes).
- **Preview bounce (`p`) and loop record (`l`)** — render a track's own
  post-effects bus to a clip, or stack additive passes inside the loop.

### Stability & correctness

- An **FFI exception firewall** around every call into a plugin library:
  a C++ throw inside a bridge or plugin becomes a parked, bypassed slot
  instead of a process death.
- Preview/audition voices are single replacing voices (no zombie
  listeners); native Wayland main window; RT→UI retire queues; xrun
  classification and recovery shared by both audio backends.
- Fixed a long-standing heap-corruption class (editor teardown ordering
  dropped a shared plugin instance before its GUI) — root-caused under
  valgrind, both release pins now run un-quarantined on both OSes.

### Packaging

- GitHub Releases now ship **`.exe` installer** (Windows), **`.deb`**,
  **`.rpm`**, and a portable `tar.zst` (usr/ layout) for Linux —
  each carries the binary, icon, desktop entry and bundled samples.
- The Linux binaries are built on the floor distro and gate-checked
  against it, and smoke-tested in Arch / Debian 12 / Ubuntu 22.04 /
  Fedora 40 containers before publish.
- AUR: the `slooper` package builds from the same release tarball
  (see [docs/INSTALL.md](docs/INSTALL.md)).

## [0.21.0] — 2026-10-31

Drum machine / pattern builder, automation-manager MIDI bindings,
multi-target write automation, clips move between tracks, asset-rack
management (reorder/collapse/rename), sub-folder rack, continuous-curve
automation, the automation manager (metronome lengths, one-click
curves), a UI pass over status honesty and chrome, plugin-window fixes
(8192-param cap, patch persistence, editor crash guard), and the yabridge
self-loop guard.

## [0.20.0] — 2026-10-29

Right-click automation-clip creation; VST3 state-chunk presets reaching
the sound; VST3 editor gestures relayed (parameter cell writes poll).

## [0.19.1] and earlier

The v0.1 loop-pedal model was replaced by the clip-based DAW in v0.2;
versions through v0.19 built the core: the 16-track arrangement model,
asset rack, takes (audio + MIDI), piano roll, script synths + VST2/VST3
hosting, effect scripts with custom GUIs, quantize/snap machinery,
`.slooper` projects, MIDI binding, and the export engine. See
`git log` between tags for the full detail.
