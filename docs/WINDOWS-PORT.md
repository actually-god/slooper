# WINDOWS-PORT.md — Windows compatibility program

> Living task ledger, one task per cron session. Same protocol as FL-SUITE.md:
> work one task at a time, tick it, append a one-paragraph DONE note with the
> commit sha, update STATUS. Linux is the PRIMARY OS: no task may regress the
> Linux build, the JACK engine, yabridge/WINE hosting, the X11 editor path, or
> any existing pin. Every Windows change is additive behind `cfg` seams.

STATUS: W1 DONE 2026-11-06 (session 1). NEXT: W2 (`paths` module).

## Ground rules (apply to every task)

- Target triple for development: `x86_64-pc-windows-gnullvm`, cross-built on
  Linux with llvm-mingw (UCRT build) at
  `~/Tools/llvm-mingw-20261104-ucrt-ubuntu-22.04-x86_64/bin` (no root
  needed; linker wired in `.cargo/config.toml`). Plain `-gnu` is NOT usable
  with llvm-mingw (see W1). MSVC target (`x86_64-pc-windows-msvc` via
  `xwin`) is release polish (W11), not required until plugin-compat friction
  appears.
- Linux code paths change ONLY by being moved behind functions/modules, never
  by behavior change. The full lib suite + e2e + clippy-zero stay green on
  Linux for every commit (existing slooper-dev gates).
- Windows-only deps go under `[target.'cfg(windows)'.dependencies]`;
  Linux-only deps (jack, libc) stay default. `cargo check --target
  x86_64-pc-windows-gnullvm` must pass before any Windows task is DONE; Linux
  gates must pass before pushing at all.
- New pins per slooper-dev standing order (`vNN_<area>_<behavior>`); OS-gated
  pins use `#[cfg(windows)]` / `#[cfg(unix)]` so each OS asserts its own truth.

## Why each hard part is shaped the way it is (research, 2026-11-06)

Inventory of Linux-isms (measured, not guessed):

1. `src/audio.rs` (6041 ln) — the whole engine: manager thread owns the
   `jack::Client`; RT core is `Proc` (impl ProcessHandler, `process()` at
   ~L3946); GUI contract is `Cmd` (L330) over mpsc + a state channel; 16
   direct `jack_sys` calls (PipeWire/latency workarounds). The mix/transport
   math inside `Proc::process` is JACK-plumbing-free except port accessors.
2. `src/vstedit.rs` (3106 ln) — direct Xlib (`#[link(name="X11")]`, ~40 X*
   symbols): offscreen child window + XGetImage blit for VST2 editors, WM
   hints, probe windows. Windows has NO equivalent of offscreen-pixmap blit:
   Win32 VST2 editors paint into an HWND parent. Needs an OS seam, biggest UI
   design task.
3. `src/vsthost.rs` (4578 ln) — crash-isolated scan probes via
   fork/pipe/poll/waitpid (`libc`); yabridge/WINE detection + symlink
   machinery; `.so`-vs-`.dll` scan rules already partly `.dll`-aware (built
   for yabridge — reuse them for NATIVE Windows scanning).
4. `src/claphost.rs` — libloading + C ABI, zero X11 references. CLAP is
   cross-platform by design; needs extension/path rules (`.clapplug-in`,
   `%COMMONPROGRAMFILES%\CLAP`) and its GUI dispatch story checked.
5. Paths: `config.rs` XDG_CONFIG_HOME/HOME; `synth.rs`/`fx.rs` XDG_DATA_HOME,
   XDG_CACHE_HOME; `audio.rs` impulses dir. Windows: FOLDERID-based
   (`%LOCALAPPDATA%\slooper`, `%APPDATA%\slooper`) — single `paths` module.
6. Runtime rustc script compilation (effects/synths = core capability):
   `Command::new("rustc")` + hardcoded `.so` output names. rustc on Windows
   needs MSVC Build Tools (or GNU toolchain); output is `.dll`.
7. MIDI: JACK MIDI only (`midi.rs` parses jack_midi streams; `audio.rs`
   registers `midi_in`). Windows needs its own device source.
8. Deps verified: eframe/glow/accesskit, rfd, libloading, symphonia, rubato,
   rusty_flac, image — all cross-platform. `vst3-host 0.10` HAS Windows
   support (winapi, Win32 embed, module loader per-OS). `jack-sys 0.5.1`
   build.rs tolerates non-Linux (only unwraps pkg-config on Linux) and ships
   a `dynamic_loading` mode — so (a) Windows builds can cfg the jack crate
   OUT cleanly, and (b) an OPTIONAL native-JACK-on-Windows mode is possible
   later without linking libjack at build time.
9. `main.rs` Wayland/X11 env dance — Linux-only by nature, inert on Windows.
10. The vendored winit patch is X11-IM-specific; untouched by Windows builds.
11. Tests: e2e.rs NEEDS JACK (gate to linux); lib suite runs headless
    (`SLOOPER_NULL_AUDIO`) — that is the Windows CI vehicle. yabridge/shell
    fixtures already sit under `#[cfg(unix)]` (2 spots) or must join them.

## Task list

Order = dependency order. One task per cron session unless trivial.

### W1 — Cross toolchain + compile triage — **DONE 2026-11-06 (session 1)**
Goal: `cargo check` for a Windows target runs; every error bucketed.
Findings (measured):
- **Target = `x86_64-pc-windows-gnullvm`** (NOT `-gnu`): the rustup prebuilt
  windows-gnu std links `-lgcc/-lgcc_eh/libpthread.a` — GCC-isms llvm-mingw
  does not provide (verified failure). gnullvm + llvm-mingw **UCRT build** is
  the upstream-official pairing; hello-world links (`PE32+ executable for MS
  Windows 6.00`) with `~/Tools/llvm-mingw-20261104-ucrt-ubuntu-22.04-x86_64/bin/
  x86_64-w64-mingw32-gcc` (2026 releases name even UCRT-build shims
  `*-w64-mingw32-*`; trust `ls bin/`, not docs). Linker wired in
  `.cargo/config.toml` under the gnullvm target key — inert for Linux.
  Consequence: UCRT-only (Win10+), which is fine for a DAW.
- **ALL dependencies cross-compile clean** (eframe/glow/accesskit, jack-sys
  build.rs tolerates the target, vst3-host with winapi, rfd, symphonia, …).
- `cargo check --lib` error surface = **19 errors, 3 sites**:
  | bucket | site | count | task |
  |---|---|---|---|
  | probe libc calls (fork/pipe/poll/waitpid/kill/WIF*/read/write size_t) | vsthost.rs 1352–1476 | 16 | W7 |
  | `std::os::unix::fs::symlink` (yabridge) | vsthost.rs:736 | 1 | W6 |
  | `WindowHandle::from_x11` (vst3-host enum) | vstedit.rs:1607 | 1 | W8 |
- **check hides link-time debt**: `#[link(name="X11")]` (vstedit) and
  jack-sys extern blocks compile as bare `extern "C"` but will fail the
  *link* of slooper.exe — the cfg seams of W5 (jack out on Windows) and W8
  (X11 editor behind OS seam) are mandatory, not optional.
- Acceptance met: triage committed; Linux gates green after config edit
  (additive section only); hello.exe link proof above.
- [x] rustup targets added (`windows-gnu` + `windows-gnullvm`)
- [x] llvm-mingw 20261104 (UCRT build) in ~/Tools, linker wired
- [x] triage table above

### W2 — `paths` module (portable base dirs)
All XDG/HOME/dir literals behind `paths::data_dir()/cache_dir()/config_dir()`
(+ impulses, plugin-default). Linux behavior byte-identical (XDG precedence,
documented fallbacks); Windows: `%LOCALAPPDATA%\slooper` / `%APPDATA%\slooper`.
`SLOOPER_DATA_DIR`/`SLOOPER_CONFIG_DIR` env overrides (test/CI convenience).
Acceptance: `v42_paths_*` pins (XDG win-order preserved, override works);
full suite + clippy green.

### W3 — Script loaders portable (rustc effects/synths)
`.so`→exe-suffix helper; fx.rs/synth.rs build paths via W2; `rustc_available`
unchanged on Linux, Windows probe message mentions MSVC Build Tools; loader
looks for `.{so|dll}` by target. Linux pins untouched.
Acceptance: Windows check passes for these modules; Linux suite green.

### W4 — Neutralize the RT core (biggest-risk task, Linux-only refactor)
Extract `Proc::process` body → `Proc::process_block(ins, outs, midi_in,
n_frames)`; the jack ProcessHandler shim becomes ~30 lines calling it. Define
`backend` module describing the manager-thread contract (Cmd + state types
unchanged → ui.rs untouched). NO behavior change; bit-true audio verified via
dsp-audio-probe baseline + full suite + e2e on real JACK.
Acceptance: Linux green incl. e2e; process_block documented as THE engine
entry every backend must call.

### W5 — WASAPI backend (Windows transport)
`audio_backend` split: `audio/jack.rs` (existing) + `audio/wasapi.rs`
(wasapi crate; event-driven render+capture, f32 shared mode, device picked in
the Ports window; NO_START ethos: empty patch = silence; graceful error when
no device — CI has none). jack crate becomes `cfg(not(windows))`.
Acceptance: Linux gates; Windows release build links; CI smoke boots
SLOOPER_NULL_AUDIO lib suite. Functional proof on real hardware = user task
(documented checklist in docs/INSTALL.md Windows section).

### W6 — Native plugin scan on Windows (VST2/VST3/CLAP files)
cfg windows: skip yabridge/wine heuristics; scan `%ProgramFiles%\[Steinberg\
\Common Files\VST], …\Common Files\VST3`, `%COMMONPROGRAMFILES%\CLAP`; reuse
the existing `.dll` awareness as the native path; VST3 via vst3-host Windows
support (already in crate); CLAP extension rules.
Acceptance: synthetic-fixture scan pins run ON WINDOWS CI too (MZ fixture
DLLs compiled by llvm-mingw, exports stubbed).

### W7 — Probe child processes on Windows
fork/pipe/poll/waitpid probes → re-exec self (`--slooper-probe <path>` hidden
flag, stdout JSON line protocol) on BOTH OSes? NO: keep fork on Linux (proven,
loader-lock-aware), add CreateProcess path behind the same `spawn_probe()`
seam on Windows. Exit-status signal checks gated.
Acceptance: Linux probe pins untouched; Windows CI probe smoke with stub DLL.

### W8 — Plugin editor windows on Win32 (hardest UI task)
vstedit OS seam: Linux keeps X11 offscreen blit EXACTLY as is. Windows:
effEditOpen(hwndParent) with a host-created child HWND; two sub-tasks:
W8a top-level Win32 editor window per plugin (message pump on the vst2-gui
thread — never the UI thread), W8b child-HWND under the winit HWND for dock
integration (reuse v36/v41 size-guard designs).
Acceptance: builds + non-X11 codepaths pinned where possible; manual smoke
checklist for user hardware.

### W9 — Windows MIDI
`midir` (WinMM) devices feeding the existing MidiMsg pipeline; picker in the
Ports window (same UI shape as port lists). Linux stays 100% JACK MIDI.
Acceptance: Windows build + API-shape pins; user hardware checklist.

### W10 — CI matrix (compile-guard EARLY: after W3)
.github/workflows: linux (lib suite + clippy), windows-latest
(build --release + `cargo test --no-run` + selected lib tests under
SLOOPER_NULL_AUDIO; plugin-load/JACK pins cfg-skipped). Full matrix after W6.
Acceptance: green CI on main.

### W11 — Releases + packaging + docs
GitHub Release assets: linux tar.zst (existing AUR flow untouched) +
`slooper-win64.zip` (exe w/ embedded icon via winresource, README), MSVC
target evaluated, INSTALL.md/PLUGINS.md Windows sections (native VST dirs,
Build-Tools note, no-yabridge note).

### W12 — Polish battery (each = separate mini-task, pick by pain)
SetThreadPriority(timeCritical) on RT thread; WASAPI device hot-unplug error
path; per-monitor DPI; optional native JACK-on-Windows mode via jack crate
`dynamic_loading` feature (for users who have jackbuilds — zero code on
Linux); settings round-trip test across OSes; minidump/crash reporting story.

## Deferred / explicitly NOT doing
- No PulseAudio/WASAPI backend on Linux (JACK/PipeWire-JACK stays primary).
- No yabridge on Windows (native plugins instead — it IS Windows).
- No rewrite of the X11 editor path "for symmetry" — it ships, it works.
- No AUR/desktop-file changes for Windows; AUR stays Linux-only.

## DONE log
- (none yet)
- **2026-11-06 session 1 — W1**: research inventory (docs/WINDOWS-PORT.md "why" section), 12-task
  breakdown, toolchain (rustup gnullvm std + llvm-mingw 20261104 UCRT in ~/Tools,
  linker in .cargo/config.toml), hello.exe link proof, triage: 19 errors / 3 sites
  (probe libc 16, unix symlink 1, vst3 WindowHandle 1); link-time debt (X11/jack
  externs) documented. No src changes — Linux untouched (native check re-verified).
