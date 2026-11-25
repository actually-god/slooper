<!-- BEGIN AGENT-LOCKED HUMAN HEADER (maintainer-written text — see END marker) -->
<!-- AGENTS: Do NOT edit, reword, reformat, reorder, move, or delete anything -->
<!-- between the BEGIN and END markers. This block is the maintainer's own     -->
<!-- voice, hand-written. If some change elsewhere appears to require touching  -->
<!-- it, leave it untouched and open an issue instead.                          -->
<!-- ======================================================================== -->

## Start Here:

Hey all, this header is just about the only manually written portion of this
entire project. This open-source DAW is primarily built for live MIDI/audio
mixing and performance, while having full capabilities you'd expect out of any
flagship DAW. I do not plan on monetizing any part of nor addons for this
project at any point in the future (it will always remain MIT licensed); I 
simply moved to linux (Arch btw) and wanted a DAW that I actually enjoyed using. 
That said, I highly recommend forking this project to further suit your own workflow needs. 
I've tried to ensure documentation exists everywhere both in code and for the program at large. 
If you have ideas you think would be good to implement on the wider
application, either make a PR or an issue in Github.

**Statement on AI:** I firmly believe in 3 things:

1. Ethically and morally, using AI to directly create any form of art
   (pictures, videos, music, etc) is dubious at best. This does not mean I
   think AI can't be used for tools related to art; a great example of
   this is CorridorKey.
2. The environmental and economic effects of data centers should be
   personally avoided when possible.
3. It's silly to avoid AI as a tool when performing repetitive, deterministic,
   and boring tasks.

With these in mind, this project:

1. Contains no art or samples created by AI (icon was made by an artist friend)
2. Was only built on a local, open source model (Qwen 3.8 Flash Next and 27B);
   no frontier model ever touched this project.
3. Was built almost exclusively with AI.

Anyways, hope everyone enjoys!

— actually-god

<!-- END AGENT-LOCKED HUMAN HEADER -->

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
- **Instruments** — bundled grand piano, sampler (piano-style key map with
  velocity layers, for drums and multisampling), drums kit, pad, and
  fm / additive / pluck / tri-osc / granular script synths,
  or any hosted VSTi/CLAP instrument — native plugin editor windows
  included.
- **Piano roll** — draw and edit notes on the snap grid, one-click quantize,
  a velocity lane, and per-note pitch curves drawn right on the note body
  (`Z`+drag — 808-style slides); previews through the target track's
  synth + FX chain.
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
- **Mixer** — per-track vol/gain/pan, per-track sidechain sends with
  independent per-route receive levels, play gate, record groups, and a
  master fader with peak/RMS/LUFS meters and a spectrum view; full-page
  MIX console on `F9`.
- **Dock everything** — panels dock to any edge, float, or tab-group; named
  layout templates and a `Ctrl+K` command palette.
- **Projects** — the whole session (tracks, clips, FX chains, synths,
  bindings) saves to a single `.slooper` JSON file; audio is never embedded.
- **MIDI binding & shortcuts** — every action binds to a live-captured MIDI
  note/CC/PC or key; everything rebindable and persistent.
- **Agent control over MCP (optional)** — a built-in, loopback-only MCP
  server lets Claude Desktop or a coding agent drive the running instance;
  off by default, with a read-only latch — [docs/MCP.md](docs/MCP.md).
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
| [docs/USAGE.md](docs/USAGE.md) | walkthrough + reference: recording, piano roll, quantize, shortcuts, MIDI control, effect presets |
| [docs/MCP.md](docs/MCP.md) | driving slooper from an AI agent over MCP — enable flow, token, bridge, tool reference |
| [docs/PLUGINS.md](docs/PLUGINS.md) | binary plugin hosting (VST2/VST3/CLAP): scan folders, editors, plugin patches; scripted-synth authoring |
| [docs/EFFECT-SUITE.md](docs/EFFECT-SUITE.md) | the 23 bundled effects, and writing your own |
| [docs/SIDECHAIN.md](docs/SIDECHAIN.md) | sidechain routing |
| [docs/SAMPLER-PITCH.md](docs/SAMPLER-PITCH.md) | per-note pitch curves — the drawn-on-note interaction and engine design |
| [docs/AGENT-GUIDE.md](docs/AGENT-GUIDE.md) | architecture map for automation / AI agents |
| [Wiki](https://github.com/actually-god/slooper/wiki) | step-by-step tutorials |

Release history: [CHANGELOG.md](CHANGELOG.md).
Contributor/testing/packaging details: [docs/AGENT-GUIDE.md](docs/AGENT-GUIDE.md).

## License

slooper is MIT-licensed ([LICENSE](LICENSE)) — free to use, modify, and
redistribute, commercial use included.

## Roadmap

macOS audio backend.
