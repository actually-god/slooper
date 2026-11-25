# Plugins: synths and VST hosting

Slooper hosts two kinds of plugin: **scripted synth plugins** (plain Rust
files in `synths/`, compiled on first load into `~/.local/share/slooper/synths`)
and **binary VST2/VST3/CLAP plugins** loaded from the scan folders.

## VST loading

- Scan folders: `vst_dirs` in the config (default `~/.vst`, `~/.vst3`,
  `/usr/lib/vst`, `/usr/lib/vst3`, `/usr/local/lib/vst*`).
- An instrument (a plugin with no audio inputs) can be bound to a track's
  synthesizer slot (`vst2:/path/to/plugin.so`, `vst3:/path/to/plugin.vst3`)
  and is played by that track's MIDI clips; its parameter sliders live in
  the track's synth window.
- VST2 plugins load through the vendored, host-patched `vendor/vst2` crate
  (see `VENDOR-NOTES.md`); VST3 through `vst3-host`.
- Instrument playback keeps the plugin PROCESSING through event-free
  blocks, so sustained notes, long pads and the plugin's own release tails
  sound fully — and StopAll/seek flush both the plugin's staging ring and
  its voices, so nothing leaks into the next run.
- distrho-ports / JUCE-wrapper builds (Vitalium, SwankyAmp, …) advertise
  their audio buses as inactive, which naive hosts honour into permanent
  silence; slooper detects the zero-channel case and activates the output
  buses itself, so these plugins play normally as track instruments.

## Installing plugins: just drop them in

No import step, no rescan button needed: put a plugin file in any scan
folder and it appears in the picker on the next scan (rescan happens on
window open and after every drop). Scan folders: `~/.vst`, `~/.vst3`,
`/usr/lib/vst(3)`, `/usr/local/lib/vst*`, `$VST_PATH`, plus anything in
`vst_dirs` — and Wine prefixes' `Plugin DLLs`/`VSTPlugins` folders too.
Understood layouts: loose `.so`, `.vst`/`.vst3` bundle directories, and
**Windows `.dll` plugins via yabridge** (next section).

### Windows-only VSTs (`.dll`) — automatic Wine bridge

If [yabridge](https://github.com/robbert-vdh/yabridge) is installed
(`yabridgectl` on `$PATH` or `~/.local/bin`), slooper registers every folder
that contains `.dll` plugins with `yabridgectl add` + `sync` automatically
during its scan — no terminal needed. yabridge then generates Linux
sidecars (`foo.dll.so`) that load the Windows plugin under Wine and also
bridge its **GUI** over X11, so Windows synths/effects appear and behave
exactly like native ones: sound, sliders, and their custom editor window.
(Install: AUR `yabridge`, or the release tarball + `./yabridgectl sync`.)

**32-bit (legacy) Windows plugins** need a Wine that can still run 32-bit
code. Arch-based distros ship WoW64-only Wine ≥10 (no `wine64`), which
yabridge's 32-bit bridge cannot use — point `WINELOADER` at a 32-bit-capable
Wine build (e.g. wine-lutris / GE-Proton style packages), preferably in
`~/.config/environment.d/*.conf` so desktop launches inherit it (an
already-running session needs `systemctl --user set-environment` or a
re-login). The Wine bridge guard below turns a wrong loader into a named,
greyed-out scan entry instead of a crash. Legacy plugins reporting a VST
API version below 2.4 (VST 1.0/2.0 era) are accepted by the vendored loader
(vendored vst2 patch #4, see `vendor/vst2/VENDOR-NOTES.md`).

### Windows (native plugins — no yabridge, no Wine)

On Windows the `.dll`s ARE the native format: slooper scans them directly,
yabridge/Wine never enter the picture (and are not used at all). Folders
scanned, in order:

- `%ProgramFiles%\Steinberg\VstPlugins`, `%ProgramFiles%\VSTPlugins`
- `%ProgramFiles%\Common Files\VST`, `...\Common Files\VSTPlugins`,
  `...\Common Files\Steinberg\vstplugins` (VST2 `.dll` loose or in
  per-plugin folders), `%COMMONPROGRAMFILES%\VST3` (VST3; bundle
  directories resolve their `Contents\x86_64-win\*.vst3dll` module),
- CLAP: `%COMMONPROGRAMFILES%\CLAP` (system) and `%LOCALAPPDATA%\CLAP`
  (user) — the SDK conventions.
- Anything you add via `VST_PATH` / `CLAP_PATH` (`;`-separated path
  lists; both variables work on both OSes) or the `vst_dirs` setting.

Editors are real Win32 child/top-level windows — dock-into-slooper,
detach, DPI behaviour and the editor crash guard work as documented in
[INSTALL.md](INSTALL.md)'s Windows smoke checklist (items 5–8).

## Scripted synths

The other half of the plugin system: a synth is one plain Rust file.
On first run slooper seeds `~/.local/share/slooper/synths` with the ABI
contract file (`synth_api.rs`) and the bundled scripts (`piano.rs`,
`drums.rs`, `pad.rs`, `tri_osc.rs`, `fm.rs`, `pluck.rs`, `granular.rs`,
`additive.rs`, `sampler.rs`); stock copies update in place on app
updates (the overwritten copy keeps a `.bak`), files you name yourself
are never touched. Each new or changed file is compiled with rustc on
load — custom synths, like custom effects, need rustc installed.

To write one: copy a bundled script in that folder, rename it, edit.
Your script `include!`s `synth_api.rs` and implements the entry points;
the bundled `piano.rs` / `sampler.rs` are the reference
implementations. The realtime rule is the effect rule — the render
callback must stay allocation-free. Step-by-step: the wiki's
[Writing Plugins](https://github.com/actually-god/slooper/wiki/Writing-Plugins)
page; the effect-script twin of this guide is
[EFFECT-SUITE.md](EFFECT-SUITE.md).

## CLAP plugins

`.clap` files are scanned like everything else — no separate setup.
Linux scan folders: `~/.clap`, `/usr/lib/clap`, `/usr/local/lib/clap`,
plus anything in `$CLAP_PATH` (`:`-separated list). Windows scans the
SDK folders `%COMMONPROGRAMFILES%\CLAP` and `%LOCALAPPDATA%\CLAP`, and
there `CLAP_PATH` is `;`-separated (see the Windows section above). A
`.clap` file may contain several plugins; each is its own picker entry,
and a binding id carries the plugin id: `clap:/path/to/file.clap#com.vendor.plug`.

- **Instrument or effect is the plugin's own declaration**: anything
  advertising `instrument` / `note-effect` can bind to a track's synth
  slot and is played by that track's MIDI clips; the rest load into
  effect chains beside VSTs and scripted effects.
- **The editor is the automatic parameter sliders.** CLAP's GUI
  extension wants a native window per plugin instead of VST's
  reparenting, and slooper does not host those windows — the slider
  window works like the editor of a GUI-less VST3 instrument, live in
  both directions.
- **Presets:** the CLAP v1 ABI has no factory program list. When a
  plugin loads a preset on its own (from its internal browser), slooper
  notices and persists the plugin-side state with the project; the
  parameters window also offers **load preset…** — a file picker that
  commands the plugin to load a preset file.

## Presets

Two preset worlds:

- **Scripted effects** share **one JSON file format** — flat files in
  `<effects folder>/user presets/` named `{plugin stem}-{preset name}.json`,
  values in each parameter's native units. The built-in presets are
  ordinary files in that folder (delete its `.factory-seeded` marker to
  bring back factory presets you deleted); hand-written files appear in
  the plugin's list with no restart. The full format spec, load
  semantics and recovery flow are user reference, so they live in
  [USAGE.md → Presets](USAGE.md#presets).
- **Binary plugins** keep their patches in the project file: VST2/VST3
  program picks re-apply on load (the program picker lists the
  plugin's own factory programs), and CLAP behaves as in the section
  above.

## v19.1 Wine bridge guard: a dead Wine can no longer kill the app

Before this, one unloadable yabridge wrapper was an app-killer: the
chainloader spawns Wine inside the dlopen'd probe, and when Wine cannot run
the host binary (WoW64-only wine + 32-bit plugin → `c000007b`) it prints
"The Wine host process has exited unexpectedly" and calls `abort()` **in
slooper's own process** — a core dump mid-scan, every other plugin lost with
it. Reproduced exactly on a desktop session whose environment lacks
`WINELOADER` (see the 32-bit note above).

Scan and open now run a **functional probe before any dlopen**: the chosen
Wine loader (same discovery yabridge itself uses — `WINELOADER`, else
`wine` on PATH; `SLOOPER_WINE` overrides) is asked to start the real yabridge
host binary once per bitness. The host's banner/usage output (or a host that
stays alive) proves the loader works; `c000007b`, a foreign-version
`wineserver`, or a silent death mark that bitness dead, and every bridged
plugin becomes a greyed, named reason — "this Wine cannot run 32-bit Windows
code (c000007b) … Set WINELOADER to a Wine build that runs 32-bit plugins,
then rescan" — instead of a crash. Mixed Wine versions sharing one prefix
also now explain themselves (the wineserver case). Escape hatches:
`SLOOPER_SKIP_BRIDGED=1` never touches bridged plugins; `SLOOPER_BRIDGE_PROBE=0`
trusts wrappers blindly (power users). Pinned by `v191_*` unit tests.

## v17 host fixes: why plugins could crash or load silent

1. **Scan could kill a live instrument (the Vitalium crash).** The scan
   probe instantiated and *dropped* plugin binaries; dropping the last
   instance fires `terminate()`/`deactivate()` into library-global state
   that every other instance of the same binary is still using — a
   delete-while-active abort, then SEGV on the next audio block. Instantiated
   libraries are now remembered process-wide: probes reuse the live
   instance's identity and never drop what they create, and scanning while
   a synth is bound no longer touches that plugin's library at all.
2. **Fresh opens wrote reported defaults into every parameter.** Many
   synths (Dexed's `Output`, JS80P) carry their init state in patch data
   and advertise bogus `default_value` metadata (0.0); writing defaults at
   load silenced them the instant they opened — invisible "plugin is
   broken" reports. A fresh open now writes **nothing** (the plugin keeps
   its own init state); parameter values are pushed only when restoring
   saved/preset state.
3. **Instruments with an audio input bus were shelved as effects.** The
   instrument test was "has no audio inputs", which misfiled JS80P (a
   synth with a feedback bus). The declared VST3 category
   (`Instrument|Synth`) now decides; the bus shape is only a fallback.

## VST custom GUIs (VST2) — open automatically

Binding a plugin instrument (track synth popup) or adding a plugin effect
to a chain **opens the plugin's own GUI window immediately** — that window
is now the standard editing surface for plugins; the parameter-slider
window only appears when the plugin has no editor or the editor refuses
(see below), and the track synth popup's "open plugin window" button just
reopens whatever surface the plugin uses. The editor runs in its **own OS
window** (slooper's main window is GPU-drawn by egui and has no
child-window support), titled `slooper — <plugin>`.

**Windows.** Everything above holds there too, with native Win32 windows
instead of X11 embeds (W6/W7/W8a, now shipped): the scan reads the folders
listed in the Windows section above, `.dll` files load as VST2 directly,
scan/editor probes re-exec the app as hidden child processes, and editors
open under the same verified-paint rule — no blank panels; refused editors
fall back to sliders. The protocol below describes the Linux/X11 path.

How it works (`src/vstedit/x11.rs`) — the standard Linux VST2 hosting protocol,
cross-checked against the VST2.4 SDK (`aeff.h` opcodes), Ardour
(`linux_vst_gui_support.cc`), Carla, JUCE, LMMS and yabridge:

| Step | Opcode | Notes |
| --- | --- | --- |
| Open | `effEditOpen` (14) | `ptr` = the X11 **XID** of our container window cast to a pointer; the plugin creates/reparents its editor as a child. X object IDs are server-global, so the plugin may use its own X connection. |
| Pump | `effEditIdle` (19) | dispatched ~33×/s from one dedicated GUI thread per editor (FLTK/DPF/Steinberg-SDK editors run their event loops from it). The audio thread keeps processing meanwhile — the normal host split. |
| Size | `effEditGetRect` (13) | returns `ERect**` (16-bit `top/left/bottom/right`); the container is resized to the plugin's rect when it changes. VST2.4 resizable plugins instead call back `audioMasterSizeWindow` (15). |
| Close | `effEditClose` (15) | dispatched on the editor thread when the window is closed (button, window manager, or a plugin swap) **before** the plugin instance can be unloaded. |

Requirements: an X11 display (`$DISPLAY`; XWayland under a Wayland session
counts). Without X11 the button reports it and the slider fallback remains.

Three host-wide rules make the difference between painted windows and
blank sentinels — all verified against JUCE, FLTK and VSTGUI editors on
this project (v10.2):

- **One thread owns every plugin control-plane call.** Instantiation,
  `effEditOpen/Idle/Close`, teardown: all on the shared `vst2-gui`
  thread. JUCE (and FLTK) bind their *global* message loop to the
  thread that first touches the plugin; when instantiation ran
  elsewhere, editors were created, sized — and never mapped.
- **Never call `XInitThreads()`.** With it in effect (it is
  process-global), plugin editors stop mapping their windows — verified
  by bisection, identical open sequence, painted vs. never-mapped.
- **The container window stays bare until the editor proves itself.**
  No `XStoreName`, no `WM_PROTOCOLS`, no `XSelectInput` before
  `effEditOpen`: any of those change how the window manager engages the
  window in time, and JUCE then never maps its child. The title and
  close protocol are added after the first verified paint.
- **The app takes the X11 backend whenever one exists.** Plugin editors
  are X11 clients by ABI; a Wayland main window plus Xwayland plugin
  windows cannot be managed as one world, so `WAYLAND_DISPLAY` is
  dropped at startup when `$DISPLAY` is set (escape hatch:
  `SLOOPER_FORCE_WAYLAND=1`, at the cost of plugin GUIs).

### Diagnostics / limitations

- **No-GUI plugins** (`effFlagsHasEditor` clear) keep the automatic
  parameter-slider panel — that panel is always available, GUI or not.
- **VST3 editor windows (since v0.12)** open the plugin's own GUI, the
  same as VST2 — but behind it sits a deliberate workaround: the sounding
  instance lives on `vst3-host`'s realtime runner, which confines it to
  the processing thread, so the window hosts a **shadow instance** of
  the same plugin. The shadow never processes audio, is primed with the
  live parameter values when it opens, and every edit it reports
  (`IComponentHandler::performEdit`) is routed into the same parameter
  cells the sliders and automation write — knob moves change the sound
  through exactly the slider path. One direction is open-loop: automation
  or slider moves update the SOUND but do not move knobs inside an
  already-open VST3 window (close and reopen to see current values).
  The shadow lives on the shared plugin GUI thread inside the same
  verified-paint X11 container as VST2 editors: refused editors never
  leave a blank panel, WM close always retires the window.
- The `effEditOpen` **return value is advisory**: the VST2 spec says 0
  means success, but many toolkits (VSTGUI-based plugins like Wolf Shaper
  or Dragonfly) return 1 as a "true" success flag. Trusting the number
  refused every working editor; slooper instead **verifies the embed**: the
  plugin must actually create its child window or paint pixels within a
  few seconds. Only when nothing appears is the open refused — reported in
  the status bar, window torn down cleanly, sliders take over.
- The embed probe is exact, never "non-black pixels": the container is
  painted a sentinel background colour and counts as showing live UI only
  when a probe pixel is neither that sentinel nor the all-zero state of a
  not-yet-exposed window. An earlier probe accepted the container's own
  grey background as "painted", which turned broken editors (e.g. an
  amsynth 2.0 VST2 build whose editor dies mid-construction and leaves a
  1×1 child window) into permanent blank panels instead of the slider
  fallback (v0.10.1). The probe rule stays.
- **Blank plugin names**: a bridge can report an empty plugin name while
  its backend is still loading at scan time (yabridge does this for slow
  Wine startups). The picker then shows the file stem, so the row still
  reads like the product (`probe_vst2`/`probe_vst3` fallback, v32).
- The title-bar **close button works**: the editor thread dispatches X
  events instead of discarding them (`XNextEvent` returns 0 on *success*;
  an early pump treated that as an error and dropped every
  `WM_DELETE_WINDOW`, so the window ignored the window manager's close),
  and it also exits when the container is destroyed outright. `cargo run
  --release --example editor_smoke [so]` opens a real editor, verifies it,
  injects the same `WM_DELETE_WINDOW` client message a close button sends,
  and asserts the editor exits.
- Plugins with a known-broken GUI no longer need removing by hand: since
  v23 the crash guard (below) detects crashing editors automatically —
  verdicts are cached per file (path + mtime) and per machine, so a bad
  verdict on one install never follows the plugin to another — and routes
  those plugins to the parameter window. Any editor that merely *fails*
  (refuses to paint) is still refused within ~3 s and its parameter
  sliders drive the plugin instead — a bad GUI can never blank-panel,
  hang, or crash the app.
- Probe and live instances of the same plugin binary intentionally share
  one process-lifetime load (see `vst_loader_lock` / probe-leak comments):
  plugin toolkits carry process-global state that does not survive
  terminate/dlclose cycles inside a running host.

## v23: three window bugs users reported, and what the sweep found

> **v24 correction.** The v23 pass fixed the crashes (#3) and part of the
> rest, but two of its three diagnoses were incomplete and its #2 fix
> introduced a new bug. Read the **v24** section below as authoritative;
> the numbered items here are kept for the history of what the sweep
> actually found.

A 47-plugin sweep (`cargo run --release --example v23_sweep -- jsonl`)
graded every plugin installed on the dev machine on open, parameter count,
chunk round-trip, and audio. Three user reports mapped to three distinct
root causes — all fixed, each pinned by a test:

1. **"Changing values in Overtonium / Surge XT only updates after the
   GUI closes."** The host parameter table was capped at 64 entries;
   these plugins publish ~2.9 k (Surge XT 2855, Overtonium 2931, KR-106
   2138). Every `performEdit` past the cap mapped to no cell and died —
   only the teardown harvest made the move appear. The cap is now
   `VST_MAX_PARAMS = 8192` and compile-checked to stay ahead of reality.
2. **"KR-106 changes live, but closing the GUI loses them."** A rebuild
   replanted only the plugin's state chunk — and many vendors' chunks
   simply do not carry parameter values (KR-106: 336 bytes for 2138
   params; the sweep flagged 15+ local plugins). A close now harvests a
   **patch**: chunk first, the shadow's own model values second, applied
   in that order on reopen. Both ride the v22 real-time tick, so even a
   killed window cannot lose more than ~2 s of editing.
3. **"ADLplug shows a solid blue window, then the whole program dies."**
   ADLplug/OPNplug (JUCE 6) SIGSEGV in XRandR display enumeration under
   Xwayland — inside the plugin's process, which until now WAS slooper's
   process.

### The editor crash guard

`open_plugin_gui` is the single path to any vendor window. A plugin whose
verdict is unknown is opened in a **throwaway child first** (the same
`slooper` binary, `SLOOPER_EDITOR_PROBE=V2|V3` +
`SLOOPER_EDITOR_PROBE_PATH=<file>`; a vendor crash kills only the child).
The verdict is cached in `config.json` under `editor_probe` per file
mtime: safe plugins open their window directly from then on, banned ones
open slooper's **parameter window** instead — sliders, a search box, and
full persistence through the same cells and tick as a real editor.
Nothing is ever banned on a skip (no X11 → no verdict), and Wine-bridged
transients are never cached. Status-line text announces every decision;
`SLOOPER_UNSAFE_EDITOR=1` bypasses the guard for development.

### Huge parameter lists are searchable

A 2.9 k-param plugin is unautomatable in a raw list: every automation
target picker carries a shared search box and renders at most 400 rows;
the parameter window has its own filter. (`filtered_targets` in
`src/ui.rs`.)

## v24: the three reports, actually fixed (and the regressions v23 caused)

After updating to v23 the user found #1 unchanged, #2 now produced **silence**
on reopen, and #3's guard made **every** plugin flash a window. All three
traced to root causes under the editor path and pinned by new tests.

### 1. Live edits still didn't reach the sound (Overtonium / Surge XT)

The 64→8192 param-cap fix was real but not the whole story. These vendors'
GUIs drive their **own** controller/processor pair and never call
`performEdit`, so nothing arrived on the report sink the host drained — the
sound only changed at close, when the teardown harvest read the model. Fix:
the shadow editor now runs a **model-sync** every 150 ms — it diffs the
plugin's own parameter model against its previous beat and pushes what moved
into the live cells (writable params only, so meters / gain-reduction are
never pushed back). The `mutate_shadow_model` test changes the model and
writes nothing else — exactly such a silent vendor — and requires the cell to
move; the old `inject_shadow_edit` wrote the cell itself and could never catch
this. Pinned: `v24_model_only_edit_reaches_cells_live`.

### 2. Reopened synth was silent until swapped out and back (KR-106)

v23's patch rode the shadow's **whole** model — every resting MIDI-CC, bender
and gate param included. Reapplying those normalized resting values on reopen
(over a chunk that already restored them) zeroed volume/expression → silence;
binding another synth and back cleared the stale chunk. Fix: the harvested
patch carries **only the parameters that actually moved** (the model-sync's
edited set). Untouched params are left exactly as the chunk restores them.
Pinned by the same test: after one model edit the patch must equal
`[(edited_id, value)]` — nothing resting.

### 3. Every plugin flashed a window on load (the guard painting itself)

The crash guard opened the plugin in a throwaway child **mapped on-screen**,
then the safe verdict reopened it — the "opens, closes, opens again" flash,
for every plugin. Off-screen placement is not an option: Xwayland recenters
every window on the screen regardless of coordinates or override-redirect
(measured). Fix: the probe creates its container but
**never maps it** — a child of an unmapped parent cannot reach the screen, yet
the plugin still builds its view and runs the display-enumeration / idle
bootstrap the guard exists to catch. Verified: unmapped still reproduces
ADLplug's SIGSEGV (child dies) and wolf-shaper survives. Pinned by
`v24_probe_windows_are_never_viewable` (probe never viewable; the same plugin
does open viewable in normal mode) and the real-binary `v23_probe_child…` e2e.

### Deprecated code removed

`drain_param_edits3` (superseded by the inline drain + model-sync); the v23
whole-model param snapshot. `open_vst3_editor` gained a `writable` cell map;
`harvest3` takes the edited set and filters the patch to it.

---

*Machine-specific install lore (what lives on the maintainer's box and its
workarounds) belongs in [host-notes.md](host-notes.md), not here.*
