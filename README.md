# slooper

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A clip-based audio DAW written in Rust, for **Linux** (JACK / PipeWire-JACK)
and **Windows** (fully native: WASAPI audio, VST hosting, scripted plugins —
no Wine, no bridge). Both platforms run the same features from the same code
path, tested in CI on both.

**Download:** [the releases page](https://github.com/actually-god/slooper/releases) —
every release ships a Windows `.exe` installer, `.deb`, `.rpm`, and a Linux
tarball.

## Features

- **16-track timeline** — one global transport and one draggable playhead
  over a metronome-defined bar/beat grid; press play or stop anywhere.
- **Clips, not buffers** — drag audio from the asset rack onto any lane;
  overlapping clips add together. Trim, fade, per-clip gain, split at the
  playhead, colors — all non-destructive.
- **Record audio and MIDI** — each take is written as a WAV / `.mid` asset
  into your rack's primary folder and auto-placed as a clip at the playhead.
- **Instruments** — bundled grand piano, sampler (8-slot key map for drums
  and multisampling), drums kit and pad synth, or any hosted VSTi/CLAP
  instrument — native plugin editor windows included.
- **Piano roll** — draw and edit notes on the snap grid, one-click quantize,
  velocity and glide lanes; previews through the target track's synth + FX
  chain.
- **23 bundled effects** — EQ, dynamics, delay/reverb, modulation,
  distortion, pitch and spectral tools, all runtime-compiled, hot-reloadable
  Rust scripts. Write your own with the same ABI.
- **One chain per track** — scripted effects and binary VST2 / VST3 / CLAP
  plugins interleaved in a single ordered list, live and in export; any
  slot can be bypassed without removing it.
- **Presets everywhere** — one preset bar across script synths, drum
  kits, sampler key maps and track templates; hosted plugins load their
  factory programs (VST2/VST3) or preset files (CLAP), and the choice
  persists with the project.
- **Mixer** — per-track vol/gain/pan, play gate, record groups, and a master
  fader with peak/RMS/LUFS meters and a spectrum view; full-page MIX console
  on `F9`.
- **Dock everything** — panels dock to any edge, float, or tab-group; named
  layout templates and a `Ctrl+K` command palette.
- **Projects** — the whole session (tracks, clips, FX chains, synths,
  bindings) saves to a single `.slooper` JSON file; audio is never embedded.
- **MIDI binding & shortcuts** — every action binds to a live-captured MIDI
  note/CC/PC or key; everything rebindable and persistent.
- **Export** — a single track, the whole project, or just the current
  selection as WAV/FLAC (built in) or MP3/OGG (via ffmpeg).
- **Bundled TR-808 pack** — 24 one-shots plus an 808-Kit preset, so a drum
  track makes sound on first launch.

## Install

**Prebuilt (recommended)** — grab the matching asset from
[the latest release](https://github.com/actually-god/slooper/releases/latest):
`.exe` installer (Windows 10/11), `.deb` (Debian/Ubuntu), `.rpm`
(Fedora/openSUSE), or a `tar.zst` that installs on any x86_64 Linux.
Each package carries the binary, desktop entry, icon and bundled samples.

**Arch Linux** — the AUR package is pending publication; until then the
`tar.zst` above installs on Arch the same way (binary + desktop entry +
icon). Current status and PKGBUILD notes: [docs/INSTALL.md](docs/INSTALL.md).

**Build from source** (needs stable Rust, `pkgconf`, and JACK dev headers —
`pipewire-jack` or libjack):

```sh
git clone https://github.com/actually-god/slooper
cd slooper
cargo build --release
./target/release/slooper
```

On Linux, slooper needs a running JACK-compatible server (PipeWire with
`pipewire-jack`, or jack2) — it never starts one for you. On Windows it
needs nothing extra. Full per-distro notes, Windows setup, and
troubleshooting: **[docs/INSTALL.md](docs/INSTALL.md)**.

## Documentation

| Doc | Contents |
| --- | --- |
| [docs/INSTALL.md](docs/INSTALL.md) | install & audio-server setup, Linux + Windows |
| [docs/USAGE.md](docs/USAGE.md) | walkthrough + reference: recording, piano roll, quantize, shortcuts, MIDI control |
| [docs/PLUGINS.md](docs/PLUGINS.md) | scripted synths, VST/CLAP hosting, presets |
| [docs/EFFECT-SUITE.md](docs/EFFECT-SUITE.md) | the 23 bundled effects, and writing your own |
| [docs/SIDECHAIN.md](docs/SIDECHAIN.md) | sidechain routing |
| [docs/AGENT-GUIDE.md](docs/AGENT-GUIDE.md) | architecture map for automation / AI agents |
| [Wiki](https://github.com/actually-god/slooper/wiki) | step-by-step tutorials |

Release history: [CHANGELOG.md](CHANGELOG.md).
Contributor/testing/packaging details: [docs/AGENT-GUIDE.md](docs/AGENT-GUIDE.md).

## License

slooper is MIT-licensed ([LICENSE](LICENSE)) — free to use, modify, and
redistribute, commercial use included.

## Roadmap

macOS audio backend.
