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
NEXT: W8b (child-HWND docking under the winit HWND — the remaining half
of the W8 task entry); then W9 (Windows MIDI). The report-only Windows
suite in every run is the grow-list for future W-tasks (make Windows pins
blocking as they stabilize).

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
- [ ] W8b: child-HWND embedding under the winit HWND (in-window editor
  dock; today Windows editors are separate top-level windows — the same
  model Linux had before the in-window work, fully usable)

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

