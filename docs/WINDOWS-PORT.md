# WINDOWS-PORT.md — Windows compatibility program

> Living task ledger, one task per cron session. Same protocol as FL-SUITE.md:
> work one task at a time, tick it, append a one-paragraph DONE note with the
> commit sha, update STATUS. Linux is the PRIMARY OS: no task may regress the
> Linux build, the JACK engine, yabridge/WINE hosting, the X11 editor path, or
> any existing pin. Every Windows change is additive behind `cfg` seams.

STATUS: W1 DONE 2026-11-06 (session 1). W2 DONE 2026-11-08 (session 2).
W3 DONE 2026-11-08 (session 3). W4 DONE 2026-11-09 (session 4).
W5 DONE 2026-11-09 (session 5, a8c878e).
W6 DONE 2026-11-09 (session 6, `w6_*` pins; the five `#[cfg(windows)]`
ones execute at W10 CI).
W7 DONE 2026-11-10 (session 7, `w7_*` pins; Windows probes now re-exec
`slooper --slooper-probe <kind> <path>`; one new `#[cfg(windows)]` pin
joins the W6/W2 set at W10 CI).
W8a DONE 2026-11-10 (session 8, `9642a03`): vstedit is an OS seam
(`src/vstedit/{mod,x11,win32}.rs`), the Win32 top-level editor container
builds, and the gnullvm check is at ZERO errors for lib + bins + tests.
W10 DONE 2026-11-10 (session 9, `.github/workflows/ci.yml` +
`[profile.citest]`): CI matrix live on main — linux (release build proof,
headless lib suite with the known editor-pair abort quarantined,
clippy-zero) / windows-cross (gnullvm `--all-targets`, llvm-mingw 20261104
pinned, linker via env override) / windows-native (MSVC build + release
proof + the w2..w8 port pins incl. all 9 `#[cfg(windows)]` pins + a
report-only full-suite triage pass). Four OS-assumption bugs and two CI
harness traps surfaced and fixed by the first real runs — see DONE log.
W8b-p1 DONE 2026-11-10 (session 10): the embed core — opt-in
`SLOOPER_WIN_EMBED` turns plugin containers into `WS_CHILD`s under the
winit HWND with slooper-drawn chrome; Linux stays top-level (seam inert,
pinned both ways). W8b-p2-a DONE 2026-11-10 (session 11): Config field
`win_editor_embed` (Settings, Windows-gated) is the real switch; the env
var stays a force-override. W8b-p2-b DONE 2026-11-11 (session 12): the
chrome is a full v47 dock member (tab groups, menus, persistence) with a
menu/carry hide policy + parent-client coordinate fix. NEXT: W8b-p2-c
(SetParent native-window detach + DPI check); W9 (Windows MIDI) can
interleave. The report-only Windows suite in every run is the grow-list
for future W-tasks (make Windows pins blocking as they stabilize).
W8b-p2-c DONE 2026-11-11 (session 13, `docs/WINDOWS-PORT.md` DONE log):
SetParent float<->embed detach. NEXT: W9 (Windows MIDI); per-monitor DPI
battery + remaining polish = W12.
W9 DONE 2026-11-11 (session 14): Windows MIDI — WinMM (midir 0.11) devices
feed `Proc::process_block` through the new OS-neutral `src/midiin.rs` bus
(lock-free MPMC ring, RT-safe: producer allocates, RT consumer moves);
`Cmd::SetMidiInputs`/`Evt::MidiInputs`/`Evt::MidiSources(opened)` carry the
picker contract; Settings ▸ MIDI INPUTS (Windows-gated section) round-trips
the selection through `Config::win_midi_inputs`. Linux structurally inert:
JACK never overrides the no-op `ManagerBackend::set_midi_inputs` hook (pinned),
`jack.rs` contains zero midiin references (pinned). NEXT: W12 (per-monitor
DPI battery + polish list).
W12a DONE 2026-11-11 (session 15, see DONE log): Windows RT scheduling
(MMCSS "Pro Audio" → TIME_CRITICAL fallback, Drop-reverted) + device-loss
auto-recovery: the RT thread classifies its death (none/stream/invalidated)
and the idle hook asks the new pure `src/audio/rt_recovery` table —
re-Apply the exact patch while the endpoints are present (budget 2, 10 s
survival reset), Notice on a vanished device or exhausted budget. Replug of
a USB DAC resumes audio with NO click. Also fixed a REAL hot-unplug bug:
the WAIT_FAILED guard was dead code (`WAIT_FAILED - WAIT_OBJECT_0(0)` is
never `None`) — a dead-handle wait would busy-spin the RT loop. NEXT: W12
continues — DPI battery, native-JACK mode, settings round-trip; W11
(releases/packaging) still queued.
W12b DONE 2026-11-11 (session 16, see DONE log): sub-block MIDI timing —
the W9 bus carries WinMM's µs timestamp per event and the WASAPI RT loop
converts it with the new pure `midiin::spread`: first stamp of each drained
batch anchors at frame 0 (zero latency added), later events land at their
measured ms distance × sr, clamped to n-1. Unstamped drivers, sr 0 and
backwards jumps degrade to the old block-start behavior; the u32-ms clock
wrap is arithmetically correct. Linux never builds any of it (jack path
untouched, pinned).
W12c DONE 2026-11-11 (session 17, `1d50e04` + this ledger commit): DPI
battery, chrome side — the plugin's `effEditRect` px are the container's
PHYSICAL size at ANY scale (docked, floated, detached), and the egui chrome
reserves `px / live pixels_per_point` via the new pure
`vstedit::editor_slot_points` (identity at 100 %, honest 1:1 on degenerate
scales). `embed_sync_pass` keeps the LIVE ppp for the pt→physical landing;
the round trip is px-exact (odd px × 6 scales pinned). Plugin-content
bitmap rescale stays explicitly deferred (WS_CHILD live-paint).
W11 DONE 2026-11-11 (session 18, `ede9025` + this ledger commit): release
machine live — `.github/workflows/release.yml` tag-push PUBLISHES
`slooper-win64.zip` (MSVC host build; embedded icon + version info proven
by PE walk of the shipped exe: 5xRT_ICON + RT_GROUP_ICON + RT_VERSION,
GUI subsystem = no console) + `slooper-<ver>-linux-x86_64.tar.zst` (usr/
prefix, AUR flow untouched); workflow_dispatch runs the same pipeline as
a DRAFT — proven live (draft `main` carries both assets). winresource
icon embed works on EVERY Windows target incl. gnullvm cross (cargo
reports target_env=gnu there; windres found next to the cross-linker).
Docs: INSTALL.md Windows section = release front door; PLUGINS.md native
scan dirs + no-yabridge note. `w11_*` pins x4, ci.yml filter += `w11_`.
W12d DONE 2026-11-11 (session 19, see DONE log): settings round-trip
across OSes — the OTHER OS's absolute paths are legal relative junk
here, so `Config::load` now drops those shapes (`paths::path_is_foreign`:
drive/UNC roots on Unix, `/` roots on Windows) from the create-or-seed
fields — `fx_dir` (worst consumer: `prepare_effects_dir` seeds bundled
scripts into whatever the config names), `project_dir`, `rack_folders`,
`vst_dirs`, `dir_memo` — and the project-load dir-memo merge guards the
same way (`.slooper` files travel too). Inert view caches
(`editor_probe`/`rack_collapsed`/`rack_dir_order`) stay verbatim for the
trip back. `w12d_*` pins x4 (bare `w12` CI filter already covers).
W12e DONE 2026-11-12 (session 20, see DONE log): the Windows
fatal-crash story — `src/crash.rs`: the unhandled-exception filter writes a
REAL minidump + a text note + a message box (`SLOOPER_CRASH_QUIET=1`
silences it) into `paths::crash_dir()` (`SLOOPER_CRASH_DIR` override; the
newest 10 EVENTS survive, a dump and its note age on one stem), a Rust panic
gets a note with a backtrace and no box, and `SLOOPER_CRASH_ME=panic|fault`
is the startup self-test switch that two CI CHILD pins drive against the
staged app exe. The shipping Windows exe builds from `[profile.winrel]`
(release + symbols) so dumps resolve to function names — release.yml AND the
ci.yml release-proof step now use that profile. Linux stays fully inert
(bare no-op twins, pinned).
W13a DONE 2026-11-12 (session 21, see DONE log): the linux release asset
now BUILDS on the floor distro (release.yml linux job: ubuntu-22.04, glibc
2.35) and a CI step asserts max GLIBC_*/GLIBCXX_*/CXXABI_* demand of the
BUILT exe against floors 2.35/3.4.30/1.9.15. Measured (local ubuntu:22.04
docker build): floor build demands GLIBC exactly 2.35; C++ demand only
GLIBCXX_3.4/CXXABI_1.3. The Arch binary (GLIBC_2.43) fails the same
assert — sabotage-proved. NEXT = W13b (distro smoke matrix), the last W12
item (native JACK-on-Windows mode), or whatever the Windows report-only
suite surfaces. The next real version tag publishes both assets
automatically.

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

### W2 — `paths` module (portable base dirs) — **DONE 2026-11-08 (session 2)**
All XDG/HOME/dir literals behind `paths::data_dir()/cache_home()/config_dir()`
(+ impulses, default projects dir). Linux behavior byte-identical (XDG precedence,
documented fallbacks); Windows: `%LOCALAPPDATA%\slooper` / `%APPDATA%\slooper`.
`SLOOPER_DATA_DIR`/`SLOOPER_CONFIG_DIR` env overrides (test/CI convenience).
Acceptance: `w2_paths_*` pins (XDG win-order preserved, override works,
live-not-memoized); full suite + clippy green.
- [x] `src/paths.rs` (300 ln) + `pub mod paths` in lib.rs
- [x] call sites migrated: config.rs (`Config::path`, `projects_dir`),
      fx.rs (`default_effects_fallback_dir`), synth.rs (`default_dir`,
      `cache_dir`), samples808.rs (`data_dir`), ui.rs (`projects_dir`)
- [x] effects/convolver.rs standalone resolver gained the cfg(windows)
      branch (it compiles alone — kept-in-sync comment added)
- [x] AGENT-GUIDE.md paths.rs section
- DONE note: one duplicate-resolution audit found the four legacy sites
  DISAGREED on empty-string env vars (spec: empty = unset); the unified
  rule is spec-correct and only changes the degenerate empty-var case.
  vsthost's HOME uses (default VST dirs, wine prefixes, yabridgectl)
  deliberately stay — they are Linux-convention constants, and W6 owns
  the cfg(windows) scan-dir set. Windows check: `paths.rs` error-free;
  lib total stays exactly 19 errors, all in the W6/W7/W8 buckets.
  Pins `w2_paths_*` x8 (one is `#[cfg(windows)]` — fires in Windows CI).
  Gates (Linux): release lib 497/498 + known-HEAD `v36_script_fx_editor_
  resizes` boot abort (rest-run 117/117 isolated), e2e 32/32, clippy-zero.

### W3 — Script loaders portable (rustc effects/synths) — **DONE 2026-11-08 (session 3)**
`.so`→exe-suffix helper; fx.rs/synth.rs build paths via W2; `rustc_available`
unchanged on Linux, Windows probe message mentions MSVC Build Tools; loader
looks for `.{so|dll}` by target. Linux pins untouched.
Acceptance: Windows check passes for these modules; Linux suite green.
- [x] `paths::dylib_ext()` (`so`/`dll`) + `paths::build_sidecar(artifact, tag)`
      (`kick.dll` → `kick.dll.tmp` / `.dll.failed`) in paths.rs
- [x] fx.rs: stamped artifact `.build/<stem>-<stamp>.<ext>`; compile_effect
      staging + poison marker via `build_sidecar`; `rustc_install_hint()`
      (Linux = rustup text, Windows = rustup + MSVC Build Tools, ASCII-only
      for the egui font); synth.rs: artifact `<stem>.<ext>` in the cache
      dir, same sidecars + hint. Two settings/effects-window "rustc is not
      on PATH" labels now embed the same hint (was hardcoded rustup twice).
- [x] pins `w3_*` x7: value pins OS-gated (each OS asserts its own suffix
      and hint), consistency pins prove every call site goes through the
      seam (4/4 went red under a 4-site sabotage, incl. both marker paths
      and both artifact constructions); rustc-dependent pins skip gracefully
      where rustc is absent (CI)
- DONE note: Linux artifact/sidecar NAMES are unchanged (`so` branch of the
  seam), so existing caches and poison markers need no migration. Windows
  runtime nuance documented in-code, not worked around: a loaded dll cannot
  be deleted (prune_stale is best-effort by design) and stamp-per-content
  already avoids any in-place overwrite of a mapped file. Windows gnullvm
  check: still exactly the 19 W1-triaged errors (all vsthost/vstedit), zero
  from fx/synth/paths. `→` glyph caught before shipping in the hint (font
  tofu list). Gates (Linux): see DONE log.

### W4 — Neutralize the RT core (biggest-risk task, Linux-only refactor) — **DONE 2026-11-09 (session 4)**
Extract `Proc::process` body → `Proc::process_block(ins, outs, midi_in,
n_frames)`; the jack ProcessHandler shim becomes ~30 lines calling it. Define
`backend` module describing the manager-thread contract (Cmd + state types
unchanged → ui.rs untouched). NO behavior change; bit-true audio verified via
dsp-audio-probe baseline + full suite + e2e on real JACK.
Acceptance: Linux green incl. e2e; process_block documented as THE engine
entry every backend must call.
- [x] `Proc::process_block(n, midi, io)` — the 1370-line body moved verbatim;
      five touchpoints swapped: `n` param, `midi.drain(..)` (owned events),
      `io.in_buf(s)`/`io.out_buf(s)` (null = slot excluded), `return;`
- [x] `src/backend.rs`: `BlockIo` trait + full contract doc (per-block rules,
      manager-thread `Cmd`/`Evt` invariance, RT rules, NO_START ethos)
- [x] jack shim = 20 lines: collect frames + owned MIDI into a reused
      `midi_ev_scratch` (same one-`Vec`-per-event count as the old inline
      `to_vec`; name avoids the older `midi_scratch` synth buffer) + `JackIo`
      (`RawPorts` is now `Copy`)
- [x] pins `w4_*` x3 (source-consistency, sabotage-verified): RT core free of
      `j::`/`ProcessScope`/`self.ports`/`self.midi_in`/`Control::`/
      `n_frames()`; seam wired (`impl BlockIo for JackIo`, exactly one
      process_block call site); shim stays pure plumbing (no mix vocabulary)
- [x] bit-true gate: `scripts/porting/w4capture.rs` drives the LIVE
      PipeWire-JACK RT path with a deterministic 2-clip project, aligns on a
      correlation marker (corr 1.000000) and hashes 190 000 stereo pairs —
      HEAD twice ⇒ `951dce12…` identical (harness is deterministic), new
      binary twice ⇒ same `951dce12…` (refactor is bit-true), a 1 % master-
      gain sabotage ⇒ different hash (harness is falsifiable)
DONE note: the only behavior delta is UB-cleanup — a slot whose
  `jack_port_get_buffer` would have returned null (OOM) is now skipped
  instead of dereferenced. MIDI-event ordering/late-block consumption and the
  idle-return semantics are preserved exactly (verified by capture + e2e).
  Gates (Linux): e2e 32/32 live; release lib 506/507 (389 monolith + 117
  isolated, the 1 = known-boot `v36_script_fx_editor_resizes` abort class);
  clippy-zero; windows-gnullvm `cargo check --lib` stays at EXACTLY the 19
  W1-triaged errors (zero from `audio.rs`/`backend.rs`). Run the capture
  harness from `scripts/porting/` (copy to `tests/`, `W4_TAG=… cargo test
  --release --test w4capture -- --nocapture`).

### W5 — WASAPI backend (Windows transport) — **DONE 2026-11-09 (session 5)**
`audio_backend` split: `audio/jack.rs` (existing) + `audio/wasapi.rs`
(wasapi crate; event-driven render+capture, f32 shared mode, device picked in
the Ports window; NO_START ethos: empty patch = silence; graceful error when
no device — CI has none). jack crate becomes `cfg(not(windows))`.
Acceptance: Linux gates; Windows release build links; CI smoke boots
SLOOPER_NULL_AUDIO lib suite. Functional proof on real hardware = user task
(documented checklist in docs/INSTALL.md Windows section).

### W6 — Native plugin scan on Windows — **DONE 2026-11-09 (session 6)**

Land the native-plugin truth: Windows `.dll` files ARE plugins; yabridge/
Wine are Linux-only concepts that must go fully inert on Windows.
Research findings (all re-verified against this tree before coding):

- `default_vst_dirs()` is Linux/Wine-only: every hardcoded fallback is a
  Wine prefix path (`.wine/drive_c/…`). W6 = compile-time branch to a
  `windows_vst_dirs()` twin; Linux list byte-identical.
- Windows folder set (SDK + de-facto installer reality): VST3
  `%ProgramFiles%\Common Files\VST3` (always case-insensitive `.vst3`
  suffix — vst3-host does exactly this); VST2 (no official dir — de-facto)
  `%ProgramFiles%\Steinberg\VstPlugins`, `Common Files\VST`,
  `Common Files\VSTPlugins`, `Common Files\Steinberg\vstplugins`,
  `%ProgramFiles%\VSTPlugins` (all `ProgramFiles (x86)` equivalents are the
  SAME dir under WOW64 fs-redir — no code); CLAP `%COMMONPROGRAMFILES%\CLAP`
  + `%LOCALAPPDATA%\CLAP` (clap SDK default). `VST_PATH`/`CLAP_PATH` work
  both OSes; `std::env::split_paths` natively handles the `;` separator —
  zero manual parsing.
- **Case-sensitivity is a real scan bug in disguise**: every VST2 arm in
  `scan_inner` matches lowercase literals, but real Windows plugins end in
  `.dll`/`.DLL`/`.Dll`. Fix: lowercased `match_name` copy used for
  extension tests ONLY under `cfg(windows)`; Linux stays byte-exact (an
  uppercase `.SO` stays invisible — pinned).
- Dispatch arms = cfg seams, not rewrites: native `.dll → VST2` behind
  `cfg!(windows)`; yabridge `bridge_ok` chain gets no Windows twin (its
  companion probe needs `/bin/sh` + POSIX shebangs); `.vst3` entries route
  through new `vst3_probe_target()`: Linux passes the entry through
  UNTOUCHED (the crate resolves `Contents/x86_64-linux/*.so` internally —
  do not pre-resolve), Windows resolves `Contents/<arch>-win/*.vst3dll`
  because the crate's resolver globs `*.vst3` ONLY and misses real-world
  Windows bundles named `.vst3dll`. `build_entry`/cache/error-row shape
  need zero changes.
- Vendored VST2 facts (verified against docs.rs vst-0.4.0 — the vendored
  copy matches): loader = `libloading` → already cross-platform; entry
  prefers `VST2CMD/main` fallback `VSTPluginMain` — **fixtures must export
  `VSTPluginMain`**; `AEffect` is repr(C), field-identical Win/Linux.
  Trap: the crate's *dispatcher* switches on its OWN `OpCode` enum
  (GetEffect=58, GetVendorVersion=35, GetProductName=48, GetVendorName=47),
  NOT the SDK `eff*` numbers — a fixture answering eff24=4 would probe
  nameless on BOTH OSes. Probe path also calls `canRunReplacingLoop()`
  (=58) and reads `numInputs/numOutputs/flags` → instrument iff
  numInputs==0 && (flags&1).
- vst3-host 0.10 Windows support is REAL (`winapi_impl` feature;
  `LoadLibraryExW` + `GetPluginFactory` + SFX) — no crate work needed.
- Isolation split (the only architectural decision): keep fork (v19/v27/v37
  loader-lock discipline is proven) — `probe_isolated` gets
  `#[cfg(unix)]`/`#[cfg(windows)]` twins; W6's Windows twin = inline
  (exactly what `SLOOPER_SCAN_INLINE` selects, ffiguard still covers
  throws), W7 replaces it with re-exec (`--slooper-probe` + CreateProcess,
  NO job objects by default — a crashing probe must not take the editor
  down; pipe EOF + waitpid, W64 exit-code&0xFFFFFFFF == signal mapping).
- yabridge inertness: `is_wine_bridged` + `yabridge_available` +
  `yabridge_sync` early-return under `cfg(windows)` (covers scan skip,
  open guard, repair, sync UI); the lone `std::os::unix::fs::symlink`
  (repair path) gets `#[cfg(unix)]` — unreachable on Windows via the
  heuristics gate; `skip_bridged` block `cfg!(unix)`-gated.
Acceptance (met): Linux suite untouched-green + `w6_*` pins; gnullvm
**lib** check error count 19 → 1 (survivor = `from_x11` = W8's bucket).

Completion log (session 6):
- Seams exactly as designed above; every branch is compile-time so the
  Linux binary's behavior is byte-identical by construction (v10/v191/
  v21/v22/v37 pins + full suite green).
- `scripts/porting/w6_vst2_fixture.c` (+ build script) → committed
  `tests/fixtures/w6/w6-vst2.{dll,so}`: minimal VST2 effect exporting
  `VSTPluginMain`+`main`, static-linked even for libgcov; the DLL export
  table verified via llvm-readobj (`VSTPluginMain ordinal 1`).
  Discovered: the vendored crate never sends `effGetParamLabel` — its
  `get_name()` uses its own opcode numbering (see above); fixture answers
  BOTH truth sets is impossible, so it answers the crate's (ground truth
  = what the probe executes).
- Live Linux pin `w6_linux_fixture_vst2_probes_clean` runs the ENTIRE
  chain today: scan dir → dispatch → fork probe → dlopen → vendored
  dispatcher → `VstEntry{name:"W6 VST Fixture", vendor:"slooper W6",
  instrument:false}`. Second Linux pin: real mingw PE + junk `.DLL` +
  uppercase `.SO` all stay unscanned. Five `#[cfg(windows)]` pins
  (native `.dll` probes incl. uppercase; bundle → resolved `*.vst3dll`
  single greyed row; dir-set shape incl. `;` splitting + no-drive_c;
  yabridge heuristics inert) execute at W10 CI — fixture DLLs are real
  mingw builds so they run unmodified there.
- Source-consistency pin (w5 pattern) proves: default_vst_dirs branch,
  cfg-gated `.dll` arm, both probe_isolated twins, `libc::` ONLY inside
  the unix fork body, symlink behind `#[cfg(unix)]`. Comments stripped
  before asserting so pin text can't self-match (bit during authoring).
- Existing pins gated `#[cfg(unix)]`: 8 yabridge/Wine/fork-fixture tests
  + 3 fixture helpers — they build `/bin/sh` fake wines and `.so`s, pure
  Linux semantics.
- Sabotage-verified (then reverted): dropping the `.dll` arm reds the
  Windows scan pins; renaming the repair symlink call without the gate
  reds the source pin.

### W7 — Probe child processes on Windows
fork/pipe/poll/waitpid probes → re-exec self (`--slooper-probe <path>` hidden
flag, stdout JSON line protocol) on BOTH OSes? NO: keep fork on Linux (proven,
loader-lock-aware), add CreateProcess path behind the same `spawn_probe()`
seam on Windows. Exit-status signal checks gated. W6 left a ready-made seam:
`#[cfg(windows)] fn probe_isolated` currently = `probe_inline` — W7 replaces
ONLY that body. Start from `docs/WINDOWS-PORT.md` W6 completion log.
Acceptance: Linux probe pins untouched; Windows CI probe smoke with stub DLL
(replace the W6 inline twin; `w6_windows_scan_probes_native_dll` must stay
green THROUGH the new isolation).

### W8 — Plugin editor windows on Win32 (hardest UI task)
vstedit OS seam: Linux keeps X11 offscreen blit EXACTLY as is. Windows:
effEditOpen(hwndParent) with a host-created child HWND; two sub-tasks:
W8a top-level Win32 editor window per plugin (message pump on the vst2-gui
thread — never the UI thread), W8b child-HWND under the winit HWND for dock
integration (reuse v36/v41 size-guard designs).
Acceptance: builds + non-X11 codepaths pinned where possible; manual smoke
checklist for user hardware.
- [x] **W8a DONE 2026-11-10 (session 8, `9642a03`)** — top-level container
  per plugin on Win32 (details: DONE log entry below)
- [x] **W8b p1 DONE 2026-11-10 (session 10, see DONE log)** — embed core:
  opt-in (`SLOOPER_WIN_EMBED`) `WS_CHILD` containers under the winit HWND,
  egui chrome + rect sync from ui.rs, twin seam + functional pins
- [x] **W8b p2-a DONE 2026-11-10 (session 11, see DONE log)** — Config
  field `win_editor_embed` (Settings row, Windows-gated) replaces the env
  gate; `SLOOPER_WIN_EMBED` stays as a force-override for CI/power users
- [x] **W8b p2-b DONE 2026-11-11 (session 12, see DONE log)** — v47 dock
  integration: native editors are dock members (`DynKind::Vst`/`VstFx`),
  tab groups + slot menus + persistence ride the generic machinery; hide
  policy blanks containers under open menus/popups and during carries
- [x] **W8b p2-c DONE 2026-11-11 (session 13, see DONE log)** — SetParent
  float<->embed detach: slot-menu "detach to native window" (posts the
  style flip to the vst2-gui thread), the detached window's own-class
  `container_proc` hosts `WM_NCRBUTTONUP`→`TrackPopupMenu` re-dock, the
  dock keeps a placeholder; `WM_DPICHANGED` keeps a fixed-size editor at
  its `effEditRect` size (pure `dpi_target` verdict; full rescale = W12)

### W9 — Windows MIDI — **DONE 2026-11-11 (session 14, see DONE log)**
`midir` (WinMM) devices feeding the existing MidiMsg pipeline; picker in the
Ports window (same UI shape as port lists). Linux stays 100% JACK MIDI.
Acceptance: Windows build + API-shape pins; user hardware checklist.
- [x] `src/midiin.rs`: OS-neutral lock-free MPMC event ring + producer +
      label dedupe/selection math (all `w9_ring_*`/`w9_open_set`/`w9_dedupe`
      pins execute on every OS)
- [x] `Cmd::SetMidiInputs` / `Evt::MidiInputs` / no-op
      `ManagerBackend::set_midi_inputs` default hook; manager_loop routes
      (source-pinned; jack.rs proven free of any override / midiin reference)
- [x] wasapi.rs: `open_midi_devices` (one MidiInput per open — midir's
      `connect` consumes it), publish on startup/Refresh/idle hot-plug,
      teardown closes devices; RT loop drains the bus into `process_block`
      each cycle (frame 0 = block start; sub-block timing deferred W12)
- [x] Settings ▸ MIDI INPUTS picker (Windows-gated `cfg!(windows)` section),
      `Config::win_midi_inputs` persistence + launch re-Apply
- [x] pins `w9_*` x13 Linux + x2 Windows-only functional (WinMM API shape,
      honest degradation at 0 devices on CI runners); sabotage-verified;
      ci.yml port-pin filter grew `w9_` (substring trap)
- [x] docs: INSTALL.md smoke item 9, AGENT-GUIDE `midiin.rs` entry

### W10 — CI matrix (compile-guard EARLY: after W3)
.github/workflows: linux (lib suite + clippy), windows-latest
(build --release + `cargo test --no-run` + selected lib tests under
SLOOPER_NULL_AUDIO; plugin-load/JACK pins cfg-skipped). Full matrix after W6.
Acceptance: green CI on main.

### W11 — Releases + packaging + docs — **DONE 2026-11-11 (session 18, see DONE log)**
GitHub Release assets: linux tar.zst (existing AUR flow untouched) +
`slooper-win64.zip` (exe w/ embedded icon via winresource, README), MSVC
target evaluated, INSTALL.md/PLUGINS.md Windows sections (native VST dirs,
Build-Tools note, no-yabridge note).
- [x] `release.yml`: tag v* → published release (generate_release_notes),
  dispatch → draft; linux tar.zst (usr/ prefix + desktop + icon + docs),
  `slooper-win64.zip` (exe + README + LICENSE)
- [x] icon embed: `assets/icon.ico` (256/64/48/32/16 from icon.png) via
  winresource in build.rs, TARGET-OS gated, catch_unwind best-effort
- [x] MSVC evaluated: SHIPS from CI's MSVC host (W10 job already proved
  it green; rc.exe present; same-OS build). gnullvm stays the cross-
  compile guard only.
- [x] no-console exe: `#![cfg_attr(target_os = "windows", windows_subsystem
  = "windows")]` (subsystem=2 verified in the shipped exe)
- [x] docs: INSTALL.md Windows = release front door; PLUGINS.md Windows
  native-plugin section (scan-dir set from `windows_vst_dirs`, CLAP
  conventions, VST_PATH/CLAP_PATH, no-yabridge note)
- [x] pins `w11_*` x4 (sabotage-proven); ci.yml port filter += `w11_`

### W12 — Polish battery (each = separate mini-task, pick by pain)
SetThreadPriority(timeCritical) on RT thread; WASAPI device hot-unplug error
path; per-monitor DPI; optional native JACK-on-Windows mode via jack crate
`dynamic_loading` feature (for users who have jackbuilds — zero code on
Linux); settings round-trip test across OSes; minidump/crash reporting story.
- [x] **W12a DONE 2026-11-11 (session 15)** — RT scheduling + hot-unplug
  auto-recovery (MMCSS/time-critical, classified fault → `rt_recovery`
  table → bounded auto re-Apply; details + the dead WAIT_FAILED guard fix
  in the DONE log)
- [x] **W12b DONE 2026-11-11 (session 16, see DONE log)** — sub-block MIDI
  timing: µs stamps ride the W9 ring, `midiin::spread` converts them to
  in-block frame offsets (batch-relative anchor = block start, wrap-safe,
  honest W9 fallback for unstamped drivers)
- [x] **W12c DONE 2026-11-11 (session 17, see DONE log)** — per-monitor
  DPI battery (chrome-side): `embed_sync_pass` multiplies by LIVE
  `pixels_per_point` (it always did); the BUG was the chrome treating
  plugin px as logical points — now container physical size = unscaled
  plugin px and chrome slot = px/ppp (`editor_slot_points`). Full
  plugin-content rescale = bitmap-scaling editors — explicitly deferred
  (WS_CHILD live-paint).
- [x] **W12d DONE 2026-11-11 (session 19, see DONE log)** — settings
  round-trip across OSes: foreign-syntax absolute paths dropped at
  `Config::load` (create-or-seed fields only; view caches untouched),
  project dir-memo merge guarded alike
- [x] **W12e DONE 2026-11-12 (session 20, see DONE log)** — minidump /
  crash-reporting story: UEF handler → real `.dmp` + note + box
  (`SLOOPER_CRASH_QUIET`), panic → note only, `paths::crash_dir()`
  (`SLOOPER_CRASH_DIR`), newest-10-EVENT pruning, the `winrel` shipping
  profile keeps symbols, and `SLOOPER_CRASH_ME=panic|fault` as the
  self-test switch two CI child pins drive. Linux inert, pinned both ways.
- [ ] native JACK-on-Windows mode — the last unpicked W12 item.

### W13 — Linux distro portability battery (opened 2026-11-12; same protocol)

The program's goal is Windows **and** all major Linux distros. Measured today
on the Arch dev host (glibc 2.44): `objdump -T target/release/slooper`
requires up to **GLIBC_2.43**, and the exe links `libX11/libxcb/libXau/
libXdmcp` + `libstdc++/libgcc_s` directly — libjack, Wayland, GL and EGL are
dlopen'd, so they are NOT load-time dependencies (that is why the AUR
`depends=(libjack.so)` is complete). Consequence: a binary built on a NEW
distro fails to load on an older one (`version 'GLIBC_2.43' not found`) —
exactly the report a Debian 12 (2.36) or Ubuntu 22.04 (2.35) user would file.
Today the linux tar.zst is built on ubuntu-24.04 (glibc 2.39) and the AUR pkg
on Arch (2.43+): neither runs on the older tier. Fix direction = build the
portable asset on the OLDEST distro we support, never the newest.

- [x] **W13a DONE 2026-11-12 (session 21, see DONE log)** — floor picked
  (glibc ≥ 2.35), release linux asset built on ubuntu-22.04, CI asserts the
  exe's GLIBC/GLIBCXX/CXXABI demand against the floors (sabotage-proved).
- [ ] **W13b — distro smoke matrix.** Containers arch / debian:12 /
  ubuntu:22.04 / fedora:40 install only the RUNTIME libs (libx11-6, libxcb1,
  libjack-jackd2-0 …) and execute the SHIPPED asset (not a rebuild) through
  the one CLI mode that exists: `slooper --slooper-probe v2 /nonexistent`
  prints one JSON line and exits WITHOUT opening a window or touching JACK
  (W7 interception runs before `Backend::start`) = a real loads-and-runs
  proof per distro, including the libjack-dlopen path.
- [ ] **W13c — dependency contract in docs.** INSTALL.md gets the
  links-vs-dlopens table, a per-distro install line, and the rule "the single
  tar.zst runs everywhere only if it was built on the floor distro"; AUR flow
  stays untouched.
- [ ] **W13d — optional `.deb`.** The tar.zst already carries an FHS `usr/`
  layout, so Debian/Ubuntu packaging is mostly a control file + the runtime
  dep list from W13b. Lowest priority.

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
- **2026-11-08 session 2 — W2**: `src/paths.rs` single base-dir seam (live env
  reads, no memoization — the suite swaps HOME/XDG mid-run); migrated config.rs/
  fx.rs/synth.rs/samples808.rs/ui.rs call sites + convolver script windows branch;
  AGENT-GUIDE updated. Gates: w2_paths x8, release lib 497/498 (+1 known-HEAD v36
  abort, rest 117/117 isolated), e2e 32/32, clippy-zero; windows-gnullvm lib check
  stays at the 19 triaged errors (none in paths.rs).
- **2026-11-08 session 3 — W3**: script loaders portable — `paths::dylib_ext()`/
  `build_sidecar()` seam; fx.rs stamped artifacts + staging/poison sidecars and
  synth.rs cache artifacts + sidecars now carry the target suffix (Linux names
  byte-identical, no cache migration); `fx::rustc_install_hint()` drives all four
  user-facing rustc-missing messages (Windows variant names MSVC Build Tools;
  ASCII-only for the egui font — a `→` was caught pre-ship). Gates: w3 pins 6/6
  (7th fires on Windows CI) + 4-site sabotage 4/4 red then reverted clean;
  windows-gnullvm check still exactly the 19 W1 errors (zero from fx/synth/paths);
  Linux release lib 503/504 (only known-boot v36_script_fx_editor_resizes; rest
  34 isolated 33 PASS + that known FAIL), clippy-zero, e2e 32/32.
- **2026-11-09 session 4 — W4**: RT core neutralized — `Proc::process_block(n,
  midi, io)` is THE engine entry (1370-line body verbatim; `j::`/`ps`/
  `self.ports`/`self.midi_in`/`Control::` only in the 20-line jack shim +
  `JackIo: BlockIo`); `src/backend.rs` carries the backend contract; pins
  `w4_*` x3 sabotage-green; bit-true LIVE capture (deterministic 2-clip
  project, corr 1.000000): HEAD×2 == refactor×2 == sha `951dce12…` over
  190 000 stereo pairs, 1 % gain sabotage flips it; harness kept at
  `scripts/porting/w4capture.rs`. Gates: e2e 32/32 live, release lib 506/507
  (known-boot v36 abort only), clippy-zero, gnullvm still exactly the 19 W1
  errors.
- **2026-11-09 session 5 — W5**: WASAPI backend — `src/audio.rs` → `src/audio/`
  (`mod.rs` shared core + `jack.rs` via mechanical split script + new
  `wasapi.rs`, ~1000 lines: event-driven shared-mode f32, multithreaded COM on
  the manager thread with teardown-before-CoUninitialize, `IMMEndpoint`
  flow-based render/capture enumeration, friendly-name labels with ` [2]`
  dedupe round-tripped through `Cmd::Apply`, WAVEFORMATEX/TENSIBLE mix parser,
  engine-capture via device re-open in apply, NO_START ethos, silent-fallback
  frames); `ManagerBackend` seam makes `manager_loop` backend-neutral (no jack
  symbols in the shared module, enforced by 5 source-scan pins `w5_*`,
  sabotage-verified red→green; scan uses comment-stripped, test-tail-cut
  source so pins are not self-referential); jack→`cfg(not(windows))` target
  table, windows-rs 0.62 (`Win32_Media_Audio`, `Win32_Security`, …)
  `cfg(windows)`; `CoCreateInstance::<_, IMMDeviceEnumerator>` + pinned
  `CLSID_MMDeviceEnumerator`/KS GUID consts (windows-rs ships neither).
  windows-gnullvm lib check: wasapi.rs ZERO errors — tree sits at exactly
  the 19 W1 vst-bucket errors (W6/W7 territory). Linux gates: w5 5/5, non-ui
  lib 254/254 serial, e2e 32/32 live, clippy-zero, audio files rustfmt-clean.
  Windows-gnullvm gotchas logged: `CreateEventW` hides behind
  `Win32_Security`; `OpenPropertyStore` behind `Win32_UI_Shell_PropertiesSystem`;
  `WAVEFORMATEX` is `packed(1)` (copy fields to locals before `format!`);
  repo is NOT whole-crate rustfmt-clean (217 diffs at HEAD) — never run bare
  `cargo fmt` here, it buries task diffs; the ui plugin-load test cluster
  (`v20_fx_bridge_*`, `v36_script_fx_*`) heap-aborts under scratch-HOME Geonkick
  config loss — environmental, reproduces at HEAD, skip-list is growing.

- **2026-11-09 session 6 — W6**: native Windows plugin scan — commit
  0990c67 (seams, fixtures, 7 pins; windows-gnullvm lib 19→1). Follow-up
  (this commit): gate battery found clippy-zero broken by 7 pre-existing
  `audio/mod.rs` warnings (v49-merge era) — fixed via `cargo clippy --fix`
  needless-borrow autopsies (auto-deref no-ops) + 3 documented
  `too_many_arguments` allows; the w5 seam-shape pin updated to the new
  call literals (its purpose — guarding the seam CALL — unchanged);
  vsthost.rs brought rustfmt-clean (1 pre-existing v49 wrap included).
  Battery: w6 3/3, w5 5/5, v10/v17/v18/v20/v21/v22/v23/v27/v28/v37/v46 +
  claphost + scan chunks green serial; e2e 32/32 LIVE JACK; clippy-zero;
  windows-gnullvm lib = exactly 1 error (from_x11 = W8).
  `chain_window_adds_scanned_plugin_slot` fails deterministically but
  reproduces IDENTICALLY at baseline 2e16bf6 (worktree build) — the
  recorded environmental editor-wait family, not W6.
  Session lore: `git checkout -- <path>` with uncommitted work in it
  DESTROYED the W6 tree once (recovered by replaying every logged patch);
  NEVER sabotage-test via checkout — commit first or sed-revert exactly.

- **2026-11-10 session 7 — W7**: probe child processes on Windows — the
  `#[cfg(windows)]` `probe_isolated` twin now re-execs the app as
  `slooper --slooper-probe <kind> <path>` instead of probing inline.
  Design: hidden flag intercepted in `main` BEFORE the audio backend
  exists (child = one dlopen + one JSON line on stdout + immediate exit,
  never JACK/GUI/config); parent = std::process::Command (== CreateProcessW;
  CREATE_NO_WINDOW so a scan never flashes consoles) with a reader thread
  on the stdout pipe (dead child = EOF), the SHARED `probe_timeout_secs()`
  deadline covering read AND reap (kill + wait past it — no zombies),
  `SLOOPER_SCAN_INLINE` honored on Windows too, and spawn failure
  degrading to inline exactly like pipe/fork failure on unix. NO job
  objects (documented in the module header). Windows crash exits surface
  as NTSTATUS (top-2 severity bits) -> `exception 0xC0000005` labels;
  ordinary exits -> `code N` (the i32->u32 widening IS the
  exit&0xFFFFFFFF the protocol doc calls for). Error strings byte-mirror
  the fork twin's greyed rows.
  Testability (the key trick): the whole parent half lives in a
  `#[cfg(any(windows, test))] mod probe_child` — Linux compiles it under
  test and the w7 pins drive the EXACT shipped parent code today:
  real-exe round trip against the W6 `.so` fixture (`w7_child_real_exe_
  probes_fixture` reads "W6 VST Fixture" through the intercepted flag),
  dead-child (`/bin/false` -> `exit code 1` reason, prompt), hang helper
  (`exec sleep` script, 1 s deadline, killed+reaped), spawn failure
  (Unspawnable), protocol pure-fns (args shape, labels, reply parsing),
  helper resolution order `SLOOPER_PROBE_EXE` (empty=unset) > build.rs
  baked `SLOOPER_EXE_PATH` > current_exe. The baked path matters: unit
  tests cannot re-exec the libtest harness (unknown flag), so build.rs
  now emits the built exe path (CARGO_TARGET_DIR/PROFILE/target suffix)
  — also what Windows CI's w6/w7 scan pins ride for REAL isolation
  (`w7_windows_native_dll_probe_survives_isolation` asserts the helper
  is NOT the test binary). `w7_fork_path_untouched` (source pin) guards
  the untouched unix fork body and main-before-backend ordering.
  Gate note: raw test-binary chunks MUST re-export RUST_TEST_THREADS=1
  (cargo's [env] is skipped) — a first runner pass produced the classic
  env-race wall of false w2-paths reds; with it, identical chunks green.
  Gates (Linux): w7 8/8 (7 lib + 1 bin), w6+w5+w2..w4 25/25, vsthost
  37/37, claphost 9/9, v10/v19/v27/v37 40/40, release lib 524/525 (2
  ignored diag tests, 1 known-boot `v36_script_fx_editor_resizes` abort
  class), e2e 32/32 LIVE JACK, clippy-zero, windows-gnullvm lib STILL
  exactly 1 error (`from_x11` = W8's bucket). Manual smoke: valid child
  JSON, missing-file error row, and malformed forms falling through to
  the normal GUI launch (pinned).

- **2026-11-10 session 9 — W10**: CI matrix live (`.github/workflows/ci.yml`
  + `[profile.citest]`). FIRST FULLY GREEN RUN: 42912518480 (fd7a8e0) —
  linux + windows-cross + windows-native all success. Jobs: **linux** =
  release build proof + headless lib suite (citest profile: release codegen
  without fat LTO — `cargo test --release` would compile every test through
  profile.bench's LTO inheritance) + clippy-zero; the known FX-editor-pair
  heap abort is quarantined (monolith `--skip ui::tests::v36_script_fx_editor`,
  docks strict in isolation, resizes warning-only until the open
  editor-cache bug lands). **windows-cross** = the gnullvm `--all-targets`
  compile guard with llvm-mingw 20261104 (UCRT) pinned to the dev-host
  build; the `CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER` env var
  overrides the local linker path from .cargo/config.toml. **windows-native**
  (MSVC) = exe build → staged probe helper → port pins `w2_..w8_` → release
  build proof → report-only full lib suite (grow-list; do NOT cfg-skip the
  pins — Windows is the only place they execute).
  Bugs the matrix caught on first contact (each was invisible to cross-
  checks or Linux runs): (1) `--all-targets` compiles EXAMPLES — three JACK
  examples imported the cfg'd-out jack crate (wrapped in `mod app` +
  cfg(windows) stub mains); W8a's "0 errors incl --tests" had only covered
  `--tests`. (2) Windows runners check out CRLF (core.autocrlf) — the raw
  source-scan pin w4_rt_core lost its `\n    }\n` anchor; `.gitattributes`
  now pins eol=lf repo-wide + the pin normalizes CRLF (line-helper pins
  like code_lines were immune — they re-join with \n). (3) Five w2_paths
  XDG/HOME pins + w6_linux_fixture (dlopens the .so) + w8_linux_backend_gate
  (the win32 x11_available stub answers always-on BY DESIGN) asserted Linux
  semantics on Windows — gated `#[cfg(unix)]`. (4) Production nit in a
  Windows-only file: `request_wm_delete(0)` RETURNED TRUE on real Win32 —
  PostMessageW(NULL) is a valid thread-message post — breaking the honest-
  refusal contract of the X11 twin; explicit null guard added. (5) CI
  harness trap: `cargo test` claims target/<p>/slooper.exe for the bin
  libtest harness, so the w7 pin's forced baked-path fallback died on
  "Unrecognized option: 'slooper-probe'"; the workflow stages a copy as
  target/citest/slooper-probe.exe + exports SLOOPER_PROBE_EXE (the pin now
  honors it — and thereby exercises the SHIPPED probe path; baked-only
  fallback keeps live proof on Linux). Verified gnullvm compiles
  cfg(windows) TEST code by sabotage probe (0.5s cached checks lie — inject
  an error first when in doubt).
  Windows full-suite triage baseline (report-only): 393 pass / 103 fail —
  93 in ui::tests, 6 synth, 3 audio, 1 samples808; engine/vsthost/vstedit/
  fx/claphost/paths core fully green natively. Grow the blocking set from
  that list in later sessions.
  Gates (Linux host): w2_..w8_ 35/35, host + gnullvm `--all-targets` clean,
  clippy-zero, e2e untouched.

- **2026-11-10 session 10 — W8b p1**: Windows embed core — opt-in
  `SLOOPER_WIN_EMBED` (live env read, empty=unset, parity with
  SLOOPER_SCAN_INLINE) turns plugin containers into frameless `WS_CHILD`s
  under the winit main HWND; slooper draws the chrome (a non-resizable egui
  window whose content rect IS the container slot) and syncs geometry via
  one embed-aware `create_container` helper shared by BOTH generations
  (VST2 `make_editor` + VST3 `make_editor3`; the old inline
  CreateWindowExW blocks are gone — no second creation path to drift).
  Seam = 4 functions on the twin surface: `embed_requested` (x11: always
  false — Linux editors stay top-level, frozen), `set_embed_parent`
  (process-global by design: `on_gui_thread` blocking gives the
  happens-before edge, the wrap in ui.rs `plugin_window_open` is strictly
  ordered per open), `set_container_rect` (synchronous, callers off the
  GUI thread MUST route via `vsthost::post` — the ui.rs pin enforces it),
  `main_thread_hwnd` (EnumThreadWindows on the UI thread: largest visible,
  titled, unowned, non-tool window = the main window; containers live on
  the vst2-gui thread so they can never be picked). Parent client origin ==
  child window origin (no non-client area: `style_of()` feeds
  AdjustWindowRect WS_CHILD for embed, frame_style() otherwise). Chrome
  close = `request_wm_delete` — ONE teardown protocol for both modes
  (pump + `reap_vst_editors` unchanged). Probe mode still never
  WS_VISIBLE, in either mode. Known p1 limits (p2 scope): chrome is a free
  float not a dock slot, egui popups can open UNDER the native child,
  one app-wide pixels_per_point, env gate (p2: Config field).
  Pins: `w8b_x11_embed_api_inert` + `w8b_embed_wiring_is_seamed` (Linux,
  runs in every CI job) + six `cfg(all(test, windows))` functional pins in
  win32.rs on a real window station (off-screen stand-in parent: child
  style/parentage/origin, rect sync exact incl. move-only semantics, probe
  mode unseen, WM_CLOSE protocol on a child, hwnd-finder honesty);
  `#[cfg(windows)]` gate pin asserts env semantics. CI lore: the
  port-pins filter is substring-matched — `w8_` does NOT catch `w8b_*`,
  workflow extended with `w8b_` (comment added at the filter).
  Gates (Linux host, this session): host check + gnullvm `--all-targets`
  clean (embed_tests module sabotage-proven to compile on the cross
  target); port pins w2..w8b 37/37; vstedit chunk 22/22 (needs
  XAUTHORITY from a DE /proc environ — bare agent shells fail its x11
  tests with "Authorization required"); clippy-zero. Windows functional
  truth = the native CI run (w8b_ now blocking).

- **2026-11-10 session 11 — W8b p2-a**: embed switch moves to Config —
  `Config::win_editor_embed` (serde default, additive-safe for existing
  config files; `w8b_win_editor_embed_defaults_off_and_parses` proves the
  legacy-JSON path), App mirror `win_embed` + `save_config` round trip,
  one predicate `App::embed_wanted() = win_embed || embed_requested()`
  gates BOTH the open-wrap and the per-frame chrome pass. Env stays a
  force-override (CI + power users; live read, empty=unset). Settings row
  "PLUGIN EDITOR WINDOWS" is rendered behind `cfg!(windows)` inside the
  established settings-section rhythm (each section's `}` lives under the
  NEXT section's marker comment — an extra brace there breaks the whole
  panel; the section compiles on Linux but never renders). Linux inertness
  unchanged and structurally stronger: even with the flag ON, embed needs
  `embed_hwnd == Some` and `x11::main_thread_hwnd()` always says None.
  Wiring pin extended (predicate + Config field + persistence assertions).
  Gates (Linux host): host check + gnullvm `--all-targets` clean,
  w8b_+w2_+w4_ 14/14, config chunk 12/12, clippy-zero. Linux suite +
  Windows native truth = CI run at this commit.

- **2026-11-10 session 11b — W8b native first-contact fix**: Windows pins
  run 42944046656 = 36 pass / 2 fail, both "container is shown" asserts.
  MEASURED FACT: on the GitHub Windows runner, `CreateWindowExW` honors
  every requested style bit EXCEPT that the live style never reports
  `WS_VISIBLE` (both top-level and child; the `WS_CHILD`/frameless/
  IsChild/geometry/WM_CLOSE asserts all passed natively). Asserting the
  OS kept WS_VISIBLE tested the runner, not the code — requested style
  extracted to pure `container_style(embedded, probe)` +
  `w8b_container_style_matrix` pins it directly; the two OS-retention
  asserts dropped (probe-unseen assert kept: stays valid on real desktops
  and can never false-pass there). Production style logic UNCHANGED
  (mechanical extract). Runner lore: on native Windows CI, style-presence
  asserts are only legal for OS-retained bits (`WS_CHILD`, frame bits);
  show-state needs a real desktop — never test it in CI.
  CI lore 2 (same push's Linux job, run 42946049338): the KNOWN open
  v20_fx_auto -> v20_fx_bridge heap corruption (skill "FX-script heap
  corruption") is not editor-only — plain paired script-effect loads
  SIGABRT the citest monolith too ("corrupted size vs. prev_size" after
  v20_fx_auto passes, dies AT v20_fx_bridge). Quarantine extended like
  the v36 pair: monolith `--skip ui::tests::v20_fx` + both run strict
  one-per-process (solo citest pass re-verified on host). Windows native
  GREEN at this commit: port pins 40/40, report-only full suite grew
  393p -> 403p / 103f.

- **2026-11-11 session 12 — W8b p2-b**: v47 dock integration for native
    editors + popup hide policy + the p1 screen->client coordinate bug.
    (1) DOCK MEMBERSHIP: two new `DynKind` kinds — `Vst(track)` /
    `VstFx(track, row)` (config keys "vst:<t>"/"vstfx:<t>:<r>", parse
    BEFORE the generic splitn so "fx:" can never swallow them; vstfx uses
    a two-part prefix parse like fx). Registration happens ONLY inside
    `embed_editors_pass` (early-returns on Linux), so Linux never
    instantiates the kinds; `dyn_open` = map-entry-alive AND not in
    `chrome_closing` (close-in-flight suppression — WM_CLOSE teardown is
    async, without the gate a closed chrome flickers itself back before
    `reap_vst_editors` lands); `dyn_set(false)` -> `chrome_closing_close`
    = the SAME request_wm_delete protocol as the chrome X (one teardown
    path). Dock slots, tab groups (drop-on-header merge, tab strip,
    drag-out carry), slot menus, dock/float buttons, Windows menu + Ctrl+K
    entries, and DynDock persistence ALL come free from the generic
    machinery. One body (`embed_body`) draws in BOTH chrome generations
    (float owner = drum_window pattern; docked = `dyn_body` dispatch); the
    slot it allocates = min(plugin size, offered) so a squeezed dock clips
    the plugin INSIDE its container instead of painting over a neighbour.
    (2) SYNC: `embed_sync_pass` runs between `side_frames` and
    `central_panels` (float bodies record in the editors pass, docked
    bodies record inside side_frames — the sync must see both), diffs per
    xid, and posts `set_container_rect`/`set_container_visible` to the
    vst2-gui thread (thread rule unchanged).
    (3) COORDINATE FIX (p1 latent bug — no real desktop in CI ever
    measured it): containers are children, SetWindowPos positions them in
    PARENT CLIENT space, but egui rects are screen points — p1 posted
    screen*ppp directly, right on a (0,0)-maximised window and visibly
    off by the window origin otherwise. New twin seam
    `embed_parent_origin(hwnd)` = ClientToScreen(parent, 0,0) (physical),
    subtracted in the sync; x11 twin answers (0,0) inert. win32 functional
    pin `w8b_embed_parent_origin_is_client_to_screen` proves the
    translation on the off-screen stand-in parent.
    (4) HIDE POLICY (p1 limitation closed): a WS_CHILD paints OVER egui's
    GL surface, so containers hide (`set_container_visible` = SW_HIDE /
    SW_SHOWNA — no activation, no geometry) while ANY menu/popup is open
    OR a window is carried. egui's `any_popup_open` is pub(crate) — the
    public equivalent is `ctx.memory(|m| m.layer_ids().any(|l| l.order ==
    Order::Foreground))` (Popup/Menu containers live on Foreground; plain
    Window-order dialogs/tooltips do NOT count — value_menu-class popups
    may still be over-covered by a container, documented).
    Pins: 2 win32 functional (visibility round trip + origin), seam surface
    list + x11-inert + win-null extensions, wiring pin extended (ordering
    asserted BETWEEN side_frames and central_panels; dispatch asserted via
    the `app.embed_body(...)` CALL, registration via the kind-tuple — a
    bare "DynKind::Vst(t) =>" contains-check was SABOTAGE-PASSED vacuously
    because key() carries the same pattern; pin strings must be
    unique-call-site markers). Sabotage rounds: sync-call drop RED,
    body-dispatch rename RED, twin drift RED — all reverted clean.
    Gates (Linux host): host check --all-targets + gnullvm --all-targets
    clean (1 unused-BOOL fixed with `let _`), clippy-zero, w* 171 green,
    w8 8/8, v47 14/14, v43 9/9, v35 16/16, config w8b pin green. Windows
    functional truth = CI run at this commit (w8b_ filter already carries
    the new pins). Deferred honestly: p2-c = SetParent native-window
    detach toggle (own class = own wndproc can host the re-dock context
    menu); per-monitor DPI stays a W12 item (plugin px still treated as
    logical — exact at 100%).


- **2026-11-11 session 13 — W8b p2-c**: SetParent float<->embed detach +
    the scoped DPI check. (1) TOGGLE DESIGN: NO new state — the LIVE
    window style IS the state (`container_is_detached` = "no `WS_CHILD`",
    a style query dispatches nothing so it is safe from the UI thread),
    both directions are style flips and the chrome pass follows next
    frame; re-dock from the native window needs no channel back. New twin
    seams `detach_to_toplevel` / `redock_to_embed` / `container_is_detached`
    (win32 = the SetParent dance via `container_style()` reuse +
    `SetWindowLongW`/`SetParent`/`SWP_FRAMECHANGED`; x11 = documented
    inert — Linux editors were always native top-levels). The embed
    parent is remembered at creation (`Tables.parent`, cleaned by
    `forget_window`) and revalidated live — a closed main window makes
    re-dock an honest `false`. Detach keeps the on-screen position
    (window rects are screen space for children too); re-dock parks at
    the parent origin and drops the geometry-diff entry
    (`embed_sync.remove`) so the slot rect always re-lands.
    (2) UI WIRING: the shared `window_slot_menu` grows a
    "detach to native window" row gated by `native_member_container`
    (kind filter — `None` for every non-native member, xid 0, already-
    detached, and structurally ALWAYS on Linux where Vst/VstFx members are
    never registered); `embed_body` becomes a placeholder ("dock back
    into slooper" + explanation) while detached and records NO slot;
    `embed_sync_pass` skips detached containers for both geometry and
    visibility (a parent-client coord would teleport a top-level; the
    hide policy must not fight user-minimize). Both window calls post to
    the vst2-gui thread like every other HWND touch.
    (3) NATIVE RE-DOCK MENU: own-class `container_proc` handles
    `WM_NCRBUTTONUP` (HTCAPTION only — children have no non-client area,
    so structurally top-level-only; probe mode refuses) with
    `TrackPopupMenu(TPM_RETURNCMD)`: "Dock back into slooper" (only while
    a live parent is remembered) + "Close editor" (the WM_CLOSE twin).
    Documented trade-off: the modal menu loop keeps DISPATCHING this
    thread's messages (plugin children stay responsive) but `effEditIdle`
    pauses while open — user-bounded, like any Windows context menu.
    (4) DPI CHECK: `WM_DPICHANGED` now routes through the pure
    `dpi_target(pinned, suggested, frame_style)`: a resizable container
    follows the suggested rect, a FIXED-SIZE (v36-pinned) container takes
    the suggested POSITION only — its `effEditRect` did NOT rescale, so
    the scaled size would only expose the sentinel. Per-monitor rescaling
    of plugin content itself stays W12.
    (5) PINS: seam surface list + both OS gates extended (Linux inert ×3,
    null-HWND honest refusals ×3); wiring pin extended — placeholder
    gates on the live style, both new calls ride `vsthost::post`, the
    sync pass skips detached for BOTH passes, geometry-diff drop asserted
    (sabotage rounds: sync filter+drop RED, redock-post-drop RED,
    detach-post-drop RED, x11-inert-flip RED — all reverted clean);
    win32 functional `w8c_*` ×3 (detach round trip incl. position
    preservation + idempotence + never-embedded refusal, DPI pinned/
    unpinned via real `SendMessageW(WM_DPICHANGED)`). CI: `w8c_` added to
    the substring filter (the `w8_` does-not-match-subletter trap again).
    Smoke item 8 in INSTALL.md covers the flow on user hardware.
    Gates (Linux host): host check --all-targets + gnullvm --all-targets
    clean ( sabotage-probe re-verified the gnullvm check actually compiles
    the cfg(windows) test code — and it CAUGHT a `parent_of` field typo
    invisible to the Linux build), w2..w8b 40/40, v47 14/14, v43 9/9,
    v35 16/16, vstedit 22/22, config 12/12, clippy-zero. Windows
    functional truth = the CI run at this commit. Honest residuals:
    detach mid-DPI-change transient (style read can lag one frame; self-
    corrects); the modal menu stalls effEditIdle while open; Windows-menu/
    palette entries for a detached member still toggle "visibility" of
    the placeholder only.
- 2026-11-11 · session 14 · **W9 Windows MIDI**: new `src/midiin.rs` —
  OS-neutral MPMC event bus (8192-slot power-of-2 ring, per-slot
  EMPTY→WRITING→FULL CAS claim, per-producer commit cursor so a second
  WinMM device can't clobber, consumer = the RT drain) + `MidiInProducer`
  (fixed 64B msg slab — the callback allocates, RT only `move`s) +
  `dedupe_labels` / `open_set` picker math. wasapi.rs: `MidiInSlot`
  (leaky `Box::leak` box — Drop would `CoUninitialize` the callback
  thread's COM; devices die via `forget` at teardown), `open_midi_devices`
  (open→publish→connect LAST: no callback can post to a slot nobody
  consumes; one `MidiInput` per open — midir's `connect` consumes it),
  wired into startup / `Cmd::Refresh` / idle hot-plug diff / `teardown`;
  RT loop drains before `read` (`self.midi.drain_into(evs)` →
  `process_block`). Manager side: `Cmd::SetMidiInputs` routes to the new
  no-op-default `ManagerBackend::set_midi_inputs` hook — jack.rs must
  NEVER override (pinned); `Evt::MidiInputs` feeds the picker, apply
  publishes the opened set as `Evt::MidiSources` so the MONITOR panel
  can't lie "nothing is being received" on Windows. UI: `win_midi_inputs`
  config field + Settings ▸ MIDI INPUTS section gated by an inline
  `cfg!(windows)` block — not `ui_win_*`-named (test-filter trap) —
  re-Apply after launch restore; `w9_settings_midi_picker_gated` pins the
  gate shape (source-scan via ui.rs's own include_str!). Pins: 13 Linux
  (`w9_ring_*` incl. 4-thread concurrent stress, `w9_open_set`,
  `w9_dedupe`, `w9_manager_routes_midi_cmd`, `w9_jack_stays_midi_free`,
  `w9_midi_hook_default_is_noop`) + 2 Windows-only functional
  (`w9_winmm_api_lists_devices_cleanly`,
  `w9_open_midi_devices_honest_with_zero_devices` — CI runners have no
  WinMM devices, honest degradation IS the contract). Sabotage-verified:
  dropping the dispatch arm reddens the manager pin (NOTE: global sed
  sabotage reddens the pin's own needle in lockstep = vacuous pass —
  hand-edit ONLY the call site line); breaking a w9_midi_tests body
  reddens gnullvm `--all-targets` (proves cross CI compiles the
  cfg(windows) test code — cargo replays warnings on "fresh" units, so
  touch the file when proving cross builds). midir 0.11 behind
  `[target.'cfg(windows)'.dependencies]` (zero Linux graph impact); CI
  port-pin filter grew `w9_` (substring trap). Gates: host clippy 0,
  gnullvm `--all-targets` 0 warn/err (caught a windows-only `unused_mut`
  Linux can never see), w9 pins 13/13, port-pin set 53/53 citest, e2e 32
  live Jack, debug lib 541/542 (the 1 = chain_window_adds_scanned_plugin_slot,
  KNOWN boot-env class, fails solo on unrelated trees too), `cargo run`
  smoke OK (picker Linux-invisible by construction; JACK active, 0 panic).
- 2026-11-11 · session 15 · **W12a RT scheduling + hot-unplug recovery**:
  (1) RT priority: `boost_rt_thread()` at the RT entry — MMCSS
  `AvSetMmThreadCharacteristicsW("Pro Audio")` (the DAW-correct scheduler
  class: system-wide priority-inversion protection), falling through to
  plain `SetThreadPriority(TIME_CRITICAL)` on hosts without the MMCSS
  service, `Nothing` honestly; the `RtBoost` guard reverts on Drop so no
  exit path skips cleanup. Manager/UI threads stay at normal priority
  (pinned: the boost call-site is exactly once, inside `RtCfg::run`).
  (2) Fault classification: `drain_capture`/`render_cycle` return `RtFault
  {code,msg}` via `rt_fault(e)` — `AUDCLNT_E_DEVICE_INVALIDATED`
  (0x88890004) is THE recoverable code; every error break stores the code
  into a session↔RT `Arc<AtomicU32>` before `Evt::Error`. REAL BUG FOUND
  while wiring this: the old `let Some(k) = got.0.checked_sub(WAIT_OBJECT_0
  .0)` "WAIT_FAILED guard" was dead code — `WAIT_OBJECT_0 == 0` so
  `checked_sub` never returns None; a dead-handle wait (0xFFFFFFFF) fell
  through to the capture arm, matched nothing, and would busy-spin the RT
  loop with no fault and no Notice. Now an explicit `got == WAIT_FAILED`
  check precedes the arithmetic and stores FAULT_DEVICE.
  (3) Recovery: new OS-neutral `src/audio/rt_recovery.rs` (the W7
  `cfg(any(windows, test))` pattern — pure table, pinned entirely on
  Linux): `plan(fault, last_patch, still_present, budget) -> Nothing |
  Reapply | DeviceGone | GaveUp`; policy: budget 2 per streak, a stream
  that survived 10 s resets it, a vanished endpoint is never retried.
  wasapi `idle()`: reap → `fault.swap(NONE)` → survival-reset →
  `patch_present` (the SAME `resolve()` the Apply path uses, so recovery
  can only fire on a patch Apply could bring up) → dispatch. `apply()`
  records `last_patch` after a successful spawn and CLEARS it on an
  explicit silence-Apply; the budget is deliberately NOT refilled by
  apply() (recovery-initiated Applies must not loop). Replugging a USB DAC
  now resumes audio on the next idle tick (250 ms loop) with a "recovering
  stream" Notice — no click.
  (4) PINS: `w12a_plan_*` x6 + policy pin (rt_recovery, every OS) + x5
  wiring scans (boost once, WAIT_FAILED ordering, classifier at every
  exit, idle dispatch, apply record/silence clear) + x2 Windows CI
  functional (`w12a_boost_rt_thread_degrades_honestly` — at least one of
  MMCSS/TIME_CRITICAL must engage, double boost+revert legal; classifier
  HRESULTs). Sabotage battery S1–S4: boost-call drop RED, idle-reapply
  drop RED, classifier neuter RED (after hardening — the bare-substring
  needle passed vacuously under `false &&`, the recorded trap biting
  live), store-count drop RED — all reverted clean. ci.yml port-pin
  filter grew `w12` (covers the whole family — comment updated).
  (5) Gates (Linux host): host check --all-targets + gnullvm
  --all-targets 0 warn/err (the cross run is the FIRST compile of the new
  Windows code — it caught a by-ref `map_err` signature and an i32
  literal), clippy-zero, w12a 12/12, port-pin set w2_..w12 65/65 citest,
  citest monolith (CI quarantines) + e2e live — exact counts in commit
  notes; jack.rs and the shared engine core untouched (recovery is
  WASAPI-side; Linux never builds rt_recovery's only reader).
- 2026-11-11 · session 16 · **W12b sub-block MIDI timing**: WinMM's
  `dwTimestamp` (midir passes it as callback arg 1, µs = ms×1000 — was
  discarded as `_ts`) now rides the W9 bus end to end. `MidiInQueue` slots
  carry `(stamp_us, bytes)`; `drain_into` grows a paired `stamps` scratch
  (frames still land at 0 from the drain — the RT-side conversion is a
  separate pure step); `MidiInProducer::on_bytes` takes the stamp;
  `audio/wasapi.rs` RtCfg gains `sr` (from `rstream.mix.sr`) and a reused
  `midi_ts` scratch, and `render_cycle` calls the new pure
  `midiin::spread(evs, midi_ts, sr, n)` between drain and hand-off.
  Design (the clock-alignment decision): WinMM stamps are a per-driver
  ms clock with no shared origin with the audio clock, so the mapping is
  batch-RELATIVE — the FIRST meaningful stamp of each drain anchors at
  frame 0 (zero latency added vs W9) and later events land at their
  measured ms distance × sr, clamped to n-1. Deltas use u32 ms wrap
  arithmetic (the clock wraps every 49.7 days — a correct small delta
  across the wrap is pinned); deltas > 1 h (backwards jumps, or TWO
  device clocks interleaved into one batch) keep the previous offset and
  do not advance the anchor — one bad stamp can only collapse events onto
  the same frame, never reorder (engine stages in arrival order + sorts);
  all-zero stamps / sr 0 / n 0 degrade to the exact W9 block-start
  behavior. The engine already honored per-event offsets (JACK has since
  v32; `process_block` clamps to n-1, v33 carry-over zeroes stale offsets)
  — ZERO engine changes. Linux: midiin is compiled (OS-neutral) but no
  Linux path pushes; jack.rs untouched (w9 pins hold). Pins: `w12b_*` x8
  (ring carries stamps in order; spread space/zero-stamps+degenerate/wrap/
  backwards/clamp/leading-unstamped; wasapi wiring scan) — wiring pin
  asserts the exact call literals (`|ts, bytes, p| p.on_bytes(bytes, ts)`,
  `drain_into(evs, midi_ts)`, `spread(... self.sr, n as u32)` between drain
  and `process_block`, paired `evs.clear(); midi_ts.clear();`, `sr:
  rstream.mix.sr`). The w9 round-trip pin extended (stamps vector assert),
  its wasapi needle + the mod.rs seam pin follow the new call literal.
  Sabotage battery: drop spread RED, revert `_ts` RED, MAX_SPAN_MS→0
  reddens 5 delta pins (unstamped pin correctly stays green) — all
  reverted clean (one test-side error found BY the pins: expected 576
  frames inside a 256-frame block; widened the block, not the clamp).
  ci.yml filter needs no change (`w12` bare already covers `w12b_`).
  Gates (Linux host): host check --all-targets + clippy-zero 0; w9_+w12
  filters 33/33 debug; port-pin set w2_..w12 **73/73** citest (CI filter);
  citest monolith DEADLOCKED at `v23_patch_reopen_keeps_params_a_chunk_
  omits` (known loader-lock × GUI-thread flake — futex-probed, killed by
  PID) so full lib coverage rode the MANDATORY 3-chunk protocol instead:
  521 (rest, CI quarantines applied) + 37 (vsthost) + 4 (ui v23) all
  `test result: ok` exit 0; quarantined strict solo: v20_fx pair +
  v36_script_fx_editor_docks GREEN, v36_script_fx_editor_resizes = the
  known `corrupted size vs. prev_size` boot abort (warning-only in CI);
  e2e **32/32 LIVE JACK**; gnullvm --all-targets clean AND sabotage-proven
  live (an injected type error in wasapi.rs surfaces in the cross check =
  the Windows code IS recompiled, not cache-replayed). Windows functional
  truth = the CI run at this commit. Honest residuals: absolute sub-block
  placement of the FIRST event is by definition block-start (no host-side
  clock bridge for dwTimestamp — matching every other WinMM host); ms
  granularity (the WinMM clock's own unit); multi-device batches spread by
  a single chain (clock mismatch collapses onto shared frames, never
  reorders).
- 2026-11-11 · session 17 · **W12c per-monitor DPI battery (chrome side)**:
  THE bug the ledger half-described: `embed_sync_pass` already multiplied
  the slot rect by the live ppp, but `embed_body`/`embed_chrome` treated
  `editor_size()` (the plugin's `effEditRect` in PHYSICAL px) as egui
  POINTS — net container physical size = px × ppp, so at 150 % a 900×480
  editor got a 1350×720 child window (sentinel padding / clip against a
  too-small dock), exact only at 100 %. Fix = one pure OS-neutral helper,
  `vstedit::editor_slot_points((w,h)_px, ppp) -> points` (mod.rs): floors
  at 8 px, and a degenerate scale (0/neg/NaN/inf — hand-built contexts
  only) degrades to 1:1 instead of an infinite slot. Both chrome call
  sites pass the LIVE scale (`ui.ctx()` in the body, `ctx` in the float);
  the sync pass is UNCHANGED — the round trip
  `((px / ppp) * ppp).round()` is px-exact, pinned for odd sizes (913×487,
  37×1080…) over six common scales. Dock behavior stays "the plugin owns
  its size, a tight slot clips it": in points the slot is simply smaller
  at high DPI, and the clip lands at the plugin's own px. What did NOT
  change (honest residuals): plugin-content bitmap rescale stays
  explicitly deferred (live-paint child — the host cannot rescale its
  drawing; INSTALL.md now sells that as the deliberate contract); a
  remembered FLOAT rect is stored in points, so a rect remembered at one
  scale sizes the window with stale points after a scale change — the
  body clips correctly (same machinery as a plugin resuming at a new
  effEditRect), first-open default sizes are live-correct; embedded
  top-level DPI behavior remains W8b p2-c's `dpi_target`. PINS `w12c_*`
  x4 (run on EVERY OS / CI job — pure math + source scan, no new
  `#[cfg(windows)]` surface): identity at 1.0 (historical semantics
  preserved), the round-trip contract mirroring the sync pass's exact
  `(r*ppp).round().max(8.0)` landing arithmetic, honest degradation, and
  a source pin with EXACT call literals + conversion-count==2 + the sync
  pass's live-ppp needles. Sabotage battery S1–S3 (hand-edited call sites
  only): embed_body ppp→1.0 RED, embed_chrome ppp→2.0 RED, sync
  round→floor RED — all reverted clean. Docs: INSTALL.md smoke item 8
  rewritten (px-are-truth contract, rescale deferral), AGENT-GUIDE
  vstedit entry carries the rule. ci.yml UNCHANGED (the bare `w12`
  substring already covers `w12c_`). Gates (Linux host): clippy-zero 0;
  host check + gnullvm `--all-targets` exit 0 AND sabotage-proven live
  (a type error injected into a cfg(test) w12c pin reddens the cross run
  = the check compiles the new code, not cache replay); w12c 4/4; the CI
  port-pin filter under citest 77/77 (= W12b's 73 + the 4 new); debug
  chunks vstedit 26/26, v47 14/14, v43 9/9, v35 16/16; release build
  proof green (19.8 MB); e2e LIVE JACK 31/32 with `v23_probe_child_dies`
  PASSING SOLO (the recorded boot-environmental class — failed on HEAD in
  session 16 too, and untouched code here: zero vsthost/probe changes).
  Citest monolith DEADLOCKED again (the session-16 class, killed at its
  1500 s deadline), so full release-mode lib coverage rode the MANDATORY
  3-chunk protocol with the CI quarantines: 525 (rest, 2 ignored) + 37
  (vsthost) + 4 (ui v23) — all `test result: ok`, all exit 0. Windows
  functional truth = the CI run at this commit (Linux-side
  risk surface is the three inert chrome lines + the pure helper).

- 2026-11-11 · session 18 · **W11 releases + packaging + docs**:
  `build.rs` embeds `assets/icon.ico` (ImageMagick-generated from
  icon.png: 256/64/48/32/16) via `winresource = 0.1` (unconditional
  build-dep — a `[target.'cfg(windows)']` build-dep would be gated by
  the TARGET while build-script `cfg!` is the HOST, so the gate is the
  runtime check `CARGO_CFG_TARGET_OS == "windows"`). Best-effort by
  construction: `WindowsResource::new()` probes toolchains and can
  panic → `catch_unwind` + `cargo:warning`, never a red cross-check.
  WINRESOURCE LORE: `compile()` returns io::Result (no catch needed
  there); cargo reports `CARGO_CFG_TARGET_ENV=gnu` EVEN FOR gnullvm,
  so the GNU/windres path runs on Linux cross-builds too, and 0.1.34
  locates windres NEXT TO the configured cross-linker — icon embeds
  proved in the local gnullvm exe AND the CI MSVC exe (PE walk: .rsrc
  = 5xRT_ICON + RT_GROUP_ICON + RT_VERSION; no RT_MANIFEST — 0.1.34
  ships none by default, fine: winit sets DPI awareness at runtime).
  PE-walk offsets if you re-verify: SizeOfOptionalHeader at pe+20,
  VirtualSize at section+8. `main.rs`:
  `#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]`
  — the zip ships console-less (smoke item 1); W7 probe children stay
  capturable because every spawn site uses `Stdio::piped()` (verified
  before flipping; the w7 pins pass natively on CI after the change).
  `release.yml`: three jobs (linux tar.zst usr/-prefix / windows zip /
  publish); tag v* → published + generated notes, workflow_dispatch →
  SAME pipeline as draft — run 43117786833 left the DRAFT release
  `main` with `slooper-win64.zip` (7.1 MB) +
  `slooper-0.21.0-dev.ede9025-linux-x86_64.tar.zst` (8.6 MB); shipped
  zip downloaded and resource-walked (subsystem=2, 5xRT_ICON). NOTE:
  `gh release download` outside the repo dir needs `-R actually-god/
  slooper`. Docs: INSTALL.md Windows section rewritten as the release
  front door (zip download, Build-Tools/linker for scripted FX, WASAPI
  port-picking, MSVC-ships vs gnullvm-guards); PLUGINS.md gains the
  Windows native-plugin section mirroring `windows_vst_dirs()`.
  Gates: Linux release build OK, clippy-zero 0; citest monolith 569
  pass/1 FAIL where the 1 was `chain_window_adds_scanned_plugin_slot`
  WITHOUT DISPLAY — the cron-shell env class (deterministic 43 s probe
  timeout headless, passes in 2 s with the documented
  DISPLAY=:1/WAYLAND_DISPLAY/XAUTHORITY exports; NOT a regression).
  Isolated pairs 3/3; v36_script_fx_editor_resizes = known open heap
  class (SIGABRTs even solo). gnullvm `--all-targets` green WITH the
  new build.rs. CI at `ede9025` (run 43117554371) all three jobs
  success: linux 570/0 (+isolated 3/3), windows port pins 92/0 (w11_
  included in the filter), full report-only triage 458 pass/100 fail
  — baseline was 393/103, grow-list continues. w11_ pins 4/4, each
  sabotage-proven (gate string, attribute, asset name, ico count byte
  — every pin reds on its own sabotage only).

- 2026-11-11 · session 19 · **W12d settings round-trip across OSes**:
  the risk was never the JSON layer (serde escapes backslashes correctly
  — a Windows config parses losslessly anywhere); it was that an
  other-OS absolute path is NOT invalid syntax on the receiving OS:
  `C:\Users\x\fx` is a legal RELATIVE directory name on Linux and
  `/home/x/fx` a drive-rooted one on Windows. Create-or-seed consumers
  would then WRITE junk trees — worst is `fx::prepare_effects_dir`,
  which `create_dir_all`s `fx_dir` AND seeds every bundled effect
  script + factory preset into it; `projects_dir` is created on save;
  rack recording writes under the primary folder. Fix = one pure
  detector + one sanitizer: `paths::path_is_foreign` (Unix: drive-letter
  ROOT `X:`/`X:/`/`X:\` or UNC `\\` — drive-RELATIVE `C:foo` deliberately
  NOT claimed, backslashes inside relative names are legal POSIX
  characters; Windows: `/`-root only, `C:\`/`C:/`/UNC stay native) and
  `Config::drop_foreign_paths` applied UNCONDITIONALLY in `Config::load`
  to the five create-or-seed fields (fx_dir/project_dir blanked ⇒ this
  OS's defaults; rack/vst lists filtered — an emptied rack list falls
  back to the samples root like a first launch; dir_memo forgets). The
  inert view caches (`editor_probe`, `rack_collapsed`, `rack_dir_order`)
  are deliberately KEPT verbatim: their keys can never hit cross-OS,
  and dropping them would break the fidelity of the trip BACK. Save is
  untouched (a same-OS round trip stays byte-familiar); the cleaned set
  persists with the next normal save. Project files travel too, so the
  `finish_load` dir-memo merge skips foreign values (a project from the
  other OS loads with every picker falling back by rfd's own missing-dir
  behavior instead of poisoning the memo → config).
  TRAPS: config tests that swap `SLOOPER_CONFIG_DIR` MUST Drop-guard it
  — a leaked override poisons every later `Config::load` in the same
  1-thread process (`ConfigSandbox` pattern in config.rs tests; temp
  dirs outside the repo per the W6 lesson).
  Pins `w12d_*` x4: shape rules (each OS asserts its own truth — the
  Windows branch gets REAL execution only at CI windows-native, first
  time this series), verbatim other-OS JSON parses + filter counts 6,
  end-to-end `Config::load` through a sandbox (local + relative
  survive, foreign drop, inert caches stay, deterministic across two
  loads), and a ui pin driving the REAL flow (build_project →
  project::save → `do_load` → asserts `proj_name` proves finish_load
  ran before asserting the memo). Sabotage PROOF: three layers dead at
  once (detector → always false, load glue, ui filter) = 4/4 pins RED,
  revert = 4/4 GREEN.
  Gates: debug w12d 4/4; citest monolith 571 pass + the 3 vstedit x11
  fails re-passed solo with a FRESH XAUTHORITY (this boot's file is
  still /run/user/1000/xauth_AvdKci — same continuous DE session);
  isolated quarantines 3/3, v36_script_fx_editor_resizes = known open
  heap class (SIGABRT solo, unchanged); clippy-zero 0; gnullvm
  `--all-targets` ZERO errors (both cfg! branches type-check on both
  targets — cfg! is runtime, no compile-time exclusion to prove); e2e
  LIVE JACK 32/32 incl. `v23_probe_child_dies` (no config/paths change
  touches the RT path — Linux behavior for same-OS configs is
  structurally unchanged: nothing matches, filter returns 0 silently).
  CORRIGENDUM (same session, found by the CI windows lane — the grow-list
  earning its keep): first run surfaced ONE new Windows failure,
  `v34_dir_memos_travel_with_the_project` ("fresh app remembers nothing").
  Root cause is a latent TEST-LANE bug my pin merely exposed:
  `headless_app` sandboxes config via XDG_CONFIG_HOME, but on Windows
  config_dir() is %APPDATA% and XDG is inert — so every test that saves a
  config on the Windows lane has been writing the REAL user config, and
  tests sharing no state silently share %APPDATA%. (My `do_load` pin was
  the first to save a non-empty dir_memo; the next test read it back.)
  Fix = the `paths` module's documented test seam: the three harness
  sites that set XDG_CONFIG_HOME now ALSO set
  `SLOOPER_CONFIG_DIR=<dir>/slooper` — the override wins on every OS and
  resolves to the SAME path XDG would on Linux (zero Linux behavior
  change), while the Windows lane finally sandboxes. Windows-lane data
  dir (%LOCALAPPDATA%, XDG_DATA_HOME-inert) stays a known, benign CI-only
  leak (the 808-pack seed is idempotent) — flagged, not fixed. Re-gates:
  targeted 7/7; citest monolith DEADLOCKED at vsthost again (the known
  class — do NOT trust monolith runs), mandatory 3-chunk protocol green:
  rest 533 + vsthost 37 + ui-v23 4, all exit 0; clippy-zero; gnullvm
  `--all-targets` green. Windows truth = CI re-run at this commit
  (`v34_dir_memos...` expected back in the fixed column).
- **2026-11-12 session 20 — W12e**: the Windows fatal-crash story. New
  `src/crash.rs` (OS seam: `install`/`note_panic`/`note_crash`/`self_test`
  all have BARE no-op Linux twins):
  (a) HARD FAULT — a `SetUnhandledExceptionFilter` filter (last resort: the
  ffiguard firewall and any plugin `__try` keep first refusal) writes
  `MiniDumpWriteDump(MiniDumpNormal | MiniDumpWithIndirectlyReferencedMemory)`
  with the real `EXCEPTION_POINTERS` (WinDbg opens on the faulting frame),
  then a text note (exception-code hex, `ExceptionAddress`,
  `minidump written: …`), prunes, then `MessageBoxW` naming the folder unless
  `SLOOPER_CRASH_QUIET`, and returns `EXCEPTION_EXECUTE_HANDLER` — never
  CONTINUE_SEARCH (that would stack WER's dialog on ours). Re-entrancy = an
  AtomicBool guard: a fault inside the writer falls straight through.
  (b) PANIC — main's v42 hook calls `note_panic` BEFORE its eprintln+abort:
  note ONLY (message + location + `Backtrace::force_capture`), no dump (the
  interesting stack is already gone) and NO box — a panic inside a probe
  child or an automation run must never pop a modal on a runner.
  (c) FILES — `paths::crash_dir()` = `<data_dir>/crashes`
  (`%LOCALAPPDATA%\slooper\crashes`; `SLOOPER_CRASH_DIR` override = the W12d
  bleed rule extended to crash artifacts), created ONLY by a writer. One
  event = one stem `slooper-crash-<fixed-width epoch>-<code>`, so a
  lexicographic walk IS chronological; `prune_plan` keeps the newest
  `CRASH_KEEP`=10 stems and drops older ones WHOLE (`.dmp`+`.txt` share the
  stem) and never touches a foreign name. UTC stamp = days-from-civil
  arithmetic (no chrono, no new API in a crash path).
  (d) SHIPPING SYMBOLS — `[profile.winrel]` (inherits release, strip=false,
  debug=1): release.yml's windows job AND ci.yml's windows-native "release
  build proof" now both build it, so the profile that ships is the profile
  that is compile-proven. Linux keeps `release`/strip=true. SEE THE
  CORRIGENDUM BELOW for what that profile actually buys on MSVC.
  (e) SELF-TEST — `SLOOPER_CRASH_ME=panic|fault`, armed by `install()` AFTER
  the filter is set. A process-level filter cannot be exercised in-process,
  so this switch is the only way to prove the chain end to end; the WORDS are
  parsed OS-neutrally (`crash_me_mode`, pinned on Linux; any other value =
  inert, so a typo cannot kill a user's DAW) and only the Windows `self_test`
  acts (`read_volatile(null)` — volatile so optimization can't fold the
  fault away). Probe children never install the guard (main dispatches them
  first), so a crash-testing probe spams no dumps.
  Pins `w12e_*` x14 (9 Linux-executable + 5 `#[cfg(windows)]` = CI-only).
  THE SABOTAGE PASS CAUGHT A REAL PIN HOLE: `w12e_main_wires_the_guard_early`
  was a plain `contains` — commenting the `note_panic` call out kept it GREEN
  (the needle survives inside the comment). Fixed with an `active_call`
  helper that skips `//`/`///` lines; both sabotages (comment-out, and
  `EXCEPTION_CONTINUE_SEARCH` for `_EXECUTE_HANDLER`) then went RED together
  and GREEN on revert. The Windows-only CHILD pins spawn the CI-STAGED app
  exe (`SLOOPER_PROBE_EXE`) and deliberately NOT the build.rs-baked
  `SLOOPER_EXE_PATH`: under `cargo test` that path holds the libtest harness,
  and execing it would run the suite (recursion) instead of the app.
  `=panic` → exactly one `-panic.txt`, backtrace inside, NO `.dmp`, and the
  child's stderr carries the tripwire message (identity check); `=fault` →
  `-c0000005.dmp` (MDMP magic, >4 KB) + same-stem `.txt` saying
  `minidump written: true`. The wait is BOUNDED (90 s then kill + FAIL): a
  guard that never fires must fail the job, not hang it.
  Gates: citest `crash::` 9/9; clippy-zero 0 (one real `single_match` in
  `prune_plan` fixed); gnullvm `--all-targets` ZERO errors — and PROVEN to
  compile the windows-only TEST fns (a type error planted inside a
  `#[cfg(windows)]` pin surfaced as `error[E0308]` there), which is the only
  reason its 1-second "Finished" cache runs are trustworthy; 3-chunk lib
  protocol rest 540/541 + vsthost 37/37 + ui-v23 4/4, the single fail
  `chain_window_adds_scanned_plugin_slot` RE-PASSED SOLO (8.05 s) with
  `DISPLAY=:1 WAYLAND_DISPLAY=wayland-1
  XAUTHORITY=/run/user/1000/xauth_AvdKci` exported = the boot-known
  agent-shell-has-no-X class, not a regression;
  `cargo check --profile winrel --target x86_64-pc-windows-gnullvm
  --all-targets` green (33 s) = the new shipping profile type-checks
  Windows-side. e2e NOT RUN: no JACK server on this boot and NO_START means
  no server gets started for a test; nothing here touches the RT path
  (`install()` sits before `Backend::start`, Linux twin empty). Windows
  truth = the CI windows-native run at this commit (the 5 new pins get real
  execution there, and `winrel` gets its MSVC build proof).
  VERIFIED BY CI (same day, run `43270734131` on `50a66bf`): all three jobs
  GREEN, and the windows-native port-pin log shows every `w12e_*` pin by
  name — including `w12e_windows_self_test_fault_writes_a_real_minidump` and
  `w12e_windows_self_test_panic_writes_only_a_note`, i.e. a real child
  process really died of an access violation and the handler really produced
  an MDMP-magic dump + same-stem note on Windows.
  CORRIGENDUM — THE SYMBOL CLAIM WAS WRONG AS SHIPPED, caught by walking the
  first winrel ARTIFACT (draft-release exe from run `43271527476`, 16.9 MB):
  subsystem 2 and the icon `.rsrc` (303 KB) are there, and so is a DEBUG
  directory — but its only content entry is CODEVIEW/RSDS naming
  `slooper.pdb`; there is NO COFF symbol table (PointerToSymbolTable = 0) and
  no `.debug$S`/`.debug$T` section, because MSVC `link.exe` moves ALL debug
  info into the PDB. A dump alone therefore yields module + RVA + the
  exception stream, and function names need the PDB of the SAME build (the
  exe records its identity; that artifact's was
  `5B917628-EB9C-487F-87F1-B7C6A4510B71` age 1, read out of the RSDS block).
  gnullvm with `strip = false` behaves differently — GNU ld keeps a COFF
  `.symtab` in the file — which is exactly why "the profile keeps symbols"
  must be tested per shipping target, not per profile. Same-day fix
  (`9de9135`): release.yml asserts `target/winrel/slooper.pdb` exists (a
  profile that ever stops emitting it now FAILS the release job) and
  publishes it as a third asset `slooper-win64-pdb.zip` (the user-facing zip
  stays small); crash.rs's module docs and INSTALL.md's crash section now
  describe what the exe actually contains and how to point WinDbg at the PDB.
  LESSON: any "the binary carries X" claim gets verified by parsing the
  SHIPPED artifact — a profile is a wish, the PE is the fact.
  THE FIX IS ALSO VERIFIED ON THE ARTIFACT (run `43280362741` on `9de9135`,
  the first release with the pdb step: GREEN, `slooper-win64-pdb.zip`
  published = a 38.6 MB `slooper.pdb`): the new exe's RSDS record reads
  `5FFE2E3F-AFDB-4899-9122-B370018E9EFB` age 1 and that exact 16-byte GUID
  appears inside the shipped PDB (two stream offsets) — so the symbols in the
  release really do match the exe in the same release.
  THE WINDOWS LANE THEN FOUND A REAL BUG IN THE HANDLER (run `43280270807`,
  `w12e_windows_self_test_fault_writes_a_real_minidump` FAILED): the dump and
  the note each called `now_secs()`, and `MiniDumpWriteDump` takes ~1 s, so a
  crash that started at x.9 wrote `.dmp` on stem `…-x-` and `.txt` on
  `…-(x+1)-` — one event, two stems, so pruning aged the dump and its note
  separately and the "same stem" assertion the pin added THIS VERY SESSION
  broke it. No Linux run could ever have shown this (the path is
  Windows-only) — the clearest argument yet for the CI child pins. Fix:
  `write_note_at(dir, ts, …)` now TAKES the event's timestamp, the note-only
  entry point (`note_crash`) is the only caller that reads the clock, and
  `uef` passes its `ts` to both writers. The bug is pinned on Linux too:
  `w12e_hard_fault_terminates_after_the_dump` now asserts (on the
  whitespace-collapsed handler body) that BOTH writers get the SAME `ts`, and
  re-sabotaging the call back to `now_secs()` turns that pin RED — so the
  class cannot come back silently. RE-VERIFIED ON WINDOWS: run `43286322759`
  on `a047600` prints all 13 executable `w12e_*` pins `ok`, including
  `w12e_windows_self_test_fault_writes_a_real_minidump` (the one that caught
  it) — dump and note now share one stem on real Windows.
- **2026-11-12 session 21 — W13a** (Linux-distro portability, first W13
  task): floor picked = **glibc ≥ 2.35** (Ubuntu 22.04 / Debian 12 /
  Fedora 38 tier; Debian 12 actually carries 2.36, so 2.35 is a true floor).
  `release.yml` linux job: `runs-on: ubuntu-24.04` → **`ubuntu-22.04`**
  (still a supported GitHub runner label as of today's docs check; plus a
  `rustup update stable` step — the image's preinstalled toolchain can lag)
  and a new **"Portability floor assert"** step right after Build: for each
  family in {GLIBC, GLIBCXX, CXXABI} it takes
  `objdump -T | grep -oE '<fam>_[0-9.]+' | sed | sort -uV | tail -1` off
  the built exe and FAILS the job if the demand exceeds the job-env floors
  2.35 / 3.4.30 / 1.9.15 (floors = what the OLDEST supported distro's
  libc/libstdc++ provides, not what we happen to build), then prints the
  NEEDED list. Measurement BEFORE choosing constants: a real
  `ubuntu:22.04` docker build on this host (`git archive HEAD` → tarball →
  bind-mount, deps = the ci.yml apt list + build-essential, rustup into a
  volume) — result: floor build demands **GLIBC exactly 2.35** (passes the
  2.35 floor, zero headroom needed), and only **GLIBCXX_3.4 / CXXABI_1.3**
  for the C++ side (ffiguard's shim is minimal C++; the 3.4.30/1.9.15
  floors have enormous headroom — the static-libstdc++ fallback was NOT
  needed). NEEDED = libX11, libstdc++, libgcc_s, libm, libc — matches the
  W13 preamble (jack/Wayland/GL stay dlopen'd, so runtime deps = those two
  packages + their chains on every distro). SABOTAGE-PROOF: the same assert
  run against the ARCH-built exe fails (`exe requires GLIBC 2.43` ,
  exit 1) — the compare logic genuinely bites. LESSONS: (1) measure the
  floor in a local `ubuntu:22.04` docker container first — minutes of
  CI-free truth, and the container's rustup/registry in a reused volume
  makes rebuilds cheap; keep the container below host CPUs (`--cpus=6`)
  when the backend is a local LLM. (2) The versioned-symbol demand of a
  C++ shim is NOT the compiler's version — minimal C++ demands only base
  GLIBCXX_3.4/CXXABI_1.3; never size a floor from `gcc --version`.
  NOT pinned with a Rust test (the assert lives in the workflow, and its
  inputs — the runner's libc — are untestable from the lib suite); the
  sabotage run above is the pin-equivalent. W13b (per-distro smoke of the
  shipped artifact) is the runtime-side proof and stays open.
