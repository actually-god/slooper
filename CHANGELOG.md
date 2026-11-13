# Changelog

All notable changes to slooper are documented here, newest first.
Versions are the git tags (`v<X.Y.Z>`); builds live on
[the releases page](https://github.com/actually-god/slooper/releases).

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
