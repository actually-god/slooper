# Installing slooper

slooper ships for **Linux** and **Windows** (both x86_64). On Linux it
talks to a **JACK** audio server, so the Linux sections below assume you
have — or will install — either
**PipeWire** (recommended; its `pipewire-jack` shim exposes the JACK API) or
classic **jack2**. The app connects with `NO_START`: it **never spawns an
audio server itself**; if none is running it reports the error and stays idle.

For day-to-day operation see [USAGE.md](USAGE.md); for repo/development
notes see [AGENT-GUIDE.md](AGENT-GUIDE.md).

## Arch Linux (and Arch derivatives, e.g. CachyOS/Manjaro)

Once published, slooper installs from the **AUR** like any other package:

```sh
yay -S slooper        # or: paru -S slooper
```

To build the package yourself (or before the AUR package exists), the
`PKGBUILD` lives in the AUR package repository
(`https://aur.archlinux.org/slooper.git`) — **not** in this source repo:

```sh
git clone https://aur.archlinux.org/slooper.git
cd slooper
makepkg -si              # fetches the tagged source tarball from GitHub,
                         # builds with cargo, installs binary + desktop entry + icon
```

Notes about that PKGBUILD:

- It builds from the GitHub **tag tarball** `v<version>`, so the package always
  matches a released version (this is not a `-git` package).
- Runtime dep is the shared library `libjack.so` — satisfied by **either**
  `pipewire-jack` or the official JACK packages, so it does not force one
  implementation on you.
- `optdepends`: `rust` (only needed to compile user effect scripts at runtime)
  and `ffmpeg` (only for MP3/OGG export). The app runs fine without both.

## Any Linux distro — prebuilt binary

Grab `slooper-<version>-linux-x86_64.tar.zst` from
[the GitHub Releases page](https://github.com/actually-god/slooper/releases).
It carries an FHS `usr/` tree (same layout as the Arch package), so one
command installs binary + desktop entry + icon on any x86_64 distro:

```sh
sudo tar -C / --zstd -xf slooper-<version>-linux-x86_64.tar.zst
# uninstall again:
sudo rm /usr/bin/slooper /usr/share/applications/slooper.desktop \
        /usr/share/pixmaps/slooper.png
```

Per-distro runtime packages (see the contract below; Arch users should
just use the AUR package above, which pulls everything):

```sh
# Debian 12 / Ubuntu 22.04+:
sudo apt-get install -y libx11-6 libstdc++6 pipewire-jack
# Fedora:
sudo dnf install libX11 libstdc++ pipewire-jack
```

### The dependency contract (why one binary runs on every distro)

A Linux binary permanently carries the **versioned symbols of its build
machine's libc**: a slooper built on Arch today demands `GLIBC_2.43` and
dies on Debian 12 at exec with `version 'GLIBC_2.43' not found`. So the
single tar.zst runs everywhere **only because it is built on the floor
distro** — the oldest glibc we support (Ubuntu 22.04, glibc 2.35; Debian
12 ships 2.36, every newer distro more). CI enforces this twice per
release: the shipped exe's symbol demand is asserted against the floor,
then the packaged exe is load-and-probe smoke-tested in Arch, Debian 12,
Ubuntu 22.04 and Fedora containers (`WINDOWS-PORT.md` W13a/W13b). If you
rebuild from source, your binary demands *your* distro's glibc — build on
the oldest machine it must run on, or just use the release asset.

What the release binary actually loads at exec (measured from the shipped
artifact's `NEEDED` set), and the packages that satisfy it:

| Load-time library | Satisfied by (Debian/Ubuntu · Fedora · Arch) |
| --- | --- |
| `libc`, `libm` (GLIBC ≥ 2.35) | base system, always present |
| `libstdc++`, `libgcc_s` (plain `GLIBCXX_3.4` demand) | `libstdc++6` · `libstdc++` · base |
| `libX11` | `libx11-6` · `libX11` · `libx11` |

Everything else is **dlopen'd at runtime**, never a load-time dependency —
the binary loads (and the smoke containers prove it with none of these
installed):

| dlopen'd | Needed for |
| --- | --- |
| `libjack.so` (the JACK API) | audio, in practice: install `pipewire-jack` or `jack2` — slooper never spawns a server itself |
| Wayland client libs | native Wayland; without them the GUI uses X11/XWayland |
| GL / EGL drivers | any display at all (software `llvmpipe` works — see `SLOOPER_GPU_GL`) |

## Any Linux distro — build from source

Requirements:

| What | Why | Example packages (Arch / Debian·Ubuntu / Fedora) |
| --- | --- | --- |
| Rust toolchain (stable) | build (and to compile effect scripts at runtime — optional feature) | `rust` / `rustc` + `cargo` / `rust` |
| cargo | build | (with toolchain) |
| JACK headers + pkg-config file | the `jack` crate links against libjack at build time | `jack` / `libjack-jackd2-dev` / `jackaudio-devel` |
| pkgconf | finds the JACK `.pc` file | `pkgconf` / `pkgconf` / `pkgconf-pkgconf` |
| A C toolchain (linker) | standard cargo linking | `base-devel` / `build-essential` / `@development-tools` |

No other system libraries are needed — the GUI stack (egui/eframe over
glow/winit) is pure Rust and dlopens GL/X11/Wayland at runtime.

```sh
git clone https://github.com/actually-god/slooper
cd slooper
cargo build --release
./target/release/slooper          # run directly, or install:
cargo install --path .               # puts the binary in ~/.cargo/bin
```

To get the application-menu entry and icon as well (system-wide install):

```sh
sudo install -Dm755 target/release/slooper /usr/bin/slooper
sudo install -Dm644 slooper.desktop /usr/share/applications/slooper.desktop
sudo install -Dm644 assets/icon.png \
  /usr/share/icons/hicolor/256x256/apps/slooper.png
```

## Audio server setup

slooper is just another JACK client — whatever works for other DAWs works
for it.

**PipeWire (recommended).** Install PipeWire with the JACK support package
(`pipewire-jack` on Arch/DP/Fedora — the package that provides `libjack.so`).
Start a session as usual; PipeWire answers JACK clients automatically.

**jack2.** Install `jack2`, then start a server before launching slooper,
e.g. `jackd -d alsa` or via your GUI of choice (qjackctl).

On launch, slooper prints `JACK client 'slooper' active @ <rate> Hz` and
shows `JACK ready` in the top bar. If no server is up, you'll see a connection
error instead — start your server, then relaunch.

## Optional runtime packages

- **rustc** — the per-loop effects framework compiles user effect scripts with
  the system `rustc` (`--edition 2021 --crate-type cdylib`). Without rustc the
  app runs normally; custom effects just list as "compiler not found".
- **ffmpeg** — enables exporting loops/projects as MP3 or OGG Vorbis. WAV and
  FLAC export are built in and never need ffmpeg.

## Files slooper creates

| Path | Purpose |
| --- | --- |
| `$XDG_CONFIG_HOME/slooper/config.json` (fallback `~/.config/slooper/config.json`) | port selections, MIDI/keyboard bindings, effect parameter values, metronome/quantize settings |
| `~/.local/share/slooper/effects/` | effect-script folder (created on first run, seeded with `template.rs` + the ABI contract file; the location is user-configurable in the FX window) |

## Windows

**Download (recommended):** grab `slooper-win64.zip` from
[the GitHub Releases page](https://github.com/actually-god/slooper/releases)
— unzip anywhere, run `slooper.exe`. No installer, no admin rights, no
console window; the app icon is embedded in the exe. Windows 10+ x64.
Config in `%APPDATA%\slooper\config.json`, data (effects, synths, cache,
projects) under `%LOCALAPPDATA%\slooper`.

**Scripted effects/synths need a linker:** effects and synth scripts are
compiled at runtime with `rustc`, and on Windows `rustc` needs a linker —
install **MSVC Build Tools** (or use a full Rust GNU toolchain). Without
one slooper still starts and plays audio, samples and plugins; only the
script features fail, with the compile error shown in the FX window.

**Audio is WASAPI** (shared mode, f32): pick the output (and any inputs)
in the Ports window — like JACK, slooper never steals a device, and no
selection means silence.

**Building from source:** releases ship from the MSVC target
(`x86_64-pc-windows-msvc`, the default Rust toolchain on Windows);
`x86_64-pc-windows-gnullvm` (UCRT) additionally cross-builds from Linux
with llvm-mingw — see [WINDOWS-PORT.md](WINDOWS-PORT.md) W1 for the
toolchain recipe.

### On-hardware smoke checklist (report failures as issues)

1. **Boot**: `slooper.exe` starts with no console window; the main window
   paints; nothing appears in Task Manager as a zombie on exit.
2. **Audio**: Ports window lists output devices; selecting one drives the
   master meter (play a sample clip). Event-driven WASAPI: no device, no
   error spam — the app just stays silent, like `NO_START` on Linux.
3. **Scripts**: Settings ▸ Effects compiles the template script (this is
   the MSVC-Build-Tools proof) and a synth loads from the synth folder.
4. **Scan**: scan finds plugins from `%ProgramFiles%\Steinberg\VstPlugins`
   / `Common Files\VST` (VST2 `.dll`), `Common Files\VST3` (bundles), and
   the CLAP folders; a crashing plugin leaves a greyed named row, not a
   dead app.
5. **Editors**: for a VST2 and a VST3 each — the editor opens as its own
   window `slooper — <name>`; knobs click and drag; a fixed-size editor
   (e.g. an old VST2) is unresizable; the window's X closes cleanly and
   the chain survives; a broken editor reports "no editor" and leaves the
   slider panel instead of a blank window.
6. **Crash guard**: after a plugin ban, removing
   `%APPDATA%\slooper\banned_editors.txt` restores probing.
7. **Persistence**: move some knobs inside a plugin GUI, close the editor,
   save + restart the project — values survive.
8. **Embed + detach (W8b)**: with Settings ▸ "PLUGIN EDITOR WINDOWS" on,
   a plugin editor opens INSIDE the slooper window as a dock member
   (resize its slot: the plugin clips, never overpaints a neighbour;
   open any menu: the plugin temporarily vanishes under it and returns).
   Right-click its header ▸ "detach to native window" — it becomes a
   normal titled window that remembers its screen position; the dock slot
   keeps a placeholder. Right-click the DETACHED window's title bar ▸
   "Dock back into slooper" — it lands back in the same slot. Closing the
   detached window (X or its title-bar menu) closes the editor exactly
   like the docked chrome's X would. DPI (W12c): the plugin's own pixels
   are the truth — docked, floated or detached, an editor keeps the size
   it reports 1:1 at 150 %/200 % scaling and across monitors with
   different scaling (the slooper chrome reserves its slot in screen
   points around it). The plugin's drawing is never bitmap-rescaled —
   deliberate: a live-painting child window cannot be rescaled by the
   host, and stretching would only blur it.
9. **MIDI (W9)**: Settings ▸ MIDI INPUTS lists your WinMM devices (tick
   boxes, same shape as the audio port lists). Tick a keyboard, Apply
   Selection — the MIDI MONITOR panel's status line goes live the moment
   you press a key, and `Note On/Off` messages reach synths and bindings.
   Unplug and replug: Refresh re-lists the device and re-Apply re-opens
   it. Quit while a device is open, then open the same device in another
   app — it must NOT report "still held by slooper" (Drop closes the
   WinMM handle). Events use WinMM message timestamps (W12b): the first
   message of each audio block lands at block start and later ones keep
   their measured spacing inside the block, so strums and fast CC runs
   play with their real timing (whole-block-start placement only when a
   driver supplies no timestamps — then everything shifts back to block
   start, nothing else changes).
10. **Stream survival (W12a)**: while audio plays, unplug the output
    device. The RT thread dies with a Notice (no crash, no freeze, no
    error spam); replug the SAME device and within a couple of idle ticks
    a second Notice says "recovering stream after device change" and the
    meter comes back on its own — no Apply click needed (budget: two such
    recoveries per survival streak). Unplug and DON'T replug: you get
    "output device vanished; pick a device and re-Apply" instead, and
    choosing another device in the Ports window works immediately. The
    RT thread runs in the MMCSS "Pro Audio" scheduler class when your
    system has it (visible in Resource Monitor → CPU → Priority Boost
    `Higher` on the audio thread; else the thread is time-critical).
11. **Crash receipts (W12e)**: `set SLOOPER_CRASH_DIR=%TEMP%\slooper-crashes`
    and `set SLOOPER_CRASH_ME=fault`, then start slooper — it dies at once, a
    dialog names the folder, and that folder holds
    `slooper-crash-<epoch>-c0000005.dmp` (first four bytes `MDMP`; WinDbg
    opens it on the faulting frame) plus the matching `.txt`. Repeat with
    `SLOOPER_CRASH_ME=panic`: a `-panic.txt` note with the panic message and a
    backtrace, no `.dmp`, no dialog. `set SLOOPER_CRASH_ME=` when done.

### If slooper crashes (W12e)

`slooper.exe` is a windowed app with no console, so a fatal error used to mean
a silent disappearance. Now it leaves receipts in
`%LOCALAPPDATA%\slooper\crashes\` (set `SLOOPER_CRASH_DIR` to move them):

- **Hard fault** (access violation, or a plugin's C++ throw that escapes every
  guard): `slooper-crash-<epoch>-<ntstatus>.dmp` — a real Windows minidump that
  opens in WinDbg on the faulting frame — plus its `.txt` note (exception code,
  faulting address, whether the dump was written). A dialog names the folder
  before the process exits; `SLOOPER_CRASH_QUIET=1` silences the dialog for
  unattended runs.
- **Rust panic**: a `.txt` note only — panic message, location and a captured
  backtrace — and no dialog (a stray panic inside a probe child or an
  automation run must never pop a modal). The process still aborts by design.

The ten newest events are kept and an event's `.dmp` + `.txt` disappear
together. Nothing leaves your machine unless you attach it to a bug report:
for a crash, include the `.txt` (plus the `.dmp` for hard faults) and the
project file.

To read a dump with function names you need the matching debug file: the
release also ships **`slooper-win64-pdb.zip`** (one file, `slooper.pdb`).
The exe itself carries only a pointer to it (a CODEVIEW record), so unpack
the PDB next to `slooper.exe` — or point WinDbg at the folder
(`.sympath+ C:\path\to\pdb` then `.reload`) — and the dump opens on the
faulting frame BY NAME. The PDB must be the one from the same release: the
exe records its identity (GUID + age) and WinDbg refuses a mismatch. Without
the PDB a dump still shows the module, the RVA and the exception record, and
the `.txt` note already names the code address — usually enough to file a
useful report. Linux is untouched by all of this: the handler is Windows-only
and Linux forensics stay `coredumpctl`.

## Uninstalling

- Package installs (`pacman -R slooper`): removes binary, desktop entry, icon.
- Remove `~/.config/slooper/` and `~/.local/share/slooper/` to discard
  your config and the effects folder.

## License

slooper is licensed under the **MIT license** — see
[LICENSE](../LICENSE). You are free to use, modify, and redistribute it,
including commercially, as long as the copyright notice is kept.
