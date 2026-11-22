# Changelog

All notable changes to slooper are documented here, newest first.
Versions are the git tags (`v<X.Y.Z>`); builds live on
[the releases page](https://github.com/actually-god/slooper/releases).

## [Unreleased]

### Added

- **Subwindow arrangement over MCP (v111/MCP-03).** Nine agent tools drive
  the window layer through the same UI-thread state the mouse uses:
  `windows.list` enumerates every window by a stable machine id (the
  display title can drift — ids never do) with open flag, dock edge/rank
  and measured geometry; `windows.open/close` cover every drawable kind
  (params — addressed per synth or chain row —, drum machine, master out,
  piano roll, automation, synth pick, chain, fx slot, asset views) plus
  plugin editors (list + close; their placement is WM territory and said
  so in the tool text); `windows.move/resize` float windows only,
  clamped to the viewport, and point at undock first for docked ones —
  exactly what the mouse does; `windows.dock/undock` re-pack the tab
  ranks per edge; and `windows.layout.save/restore` round-trip a whole
  arrangement (dock table, strip widths, float rects, open flags) as
  named templates that survive a restart. Bad ids answer with the live
  id list; the read-only latch blocks all eight writers.
- **MCP mixer, chain and rack control (v110/MCP-04).** Eleven agent
  tools ride the GUI's own setter seams: `mixer.list/get/set` (track
  vol/gain/pan/clip-color, sidechain sends and master volume in the same
  MIDI-range numbers the live mixer uses — no unit fork), `chain.list/
  pick/remove` (plugin selection with the GUI's async load semantics: the
  call returns `loading` and readiness shows up in `chain.list`; loading
  a sample-based plugin onto a track still auto-arms its sampler synth),
  and `assets.list/import/remove/assign` (rack scan, WAV import, deletes
  that stay inside the rack, and the drag-and-drop equivalents: pad,
  sampler, channel and single key-map slot). Every write echoes the
  post-change state back; the read-only latch blocks all of them.
- **Note pressure reaches scripted synths (script ABI v3).** The MPE
  per-note aftertouch lane (and every recorded poly-AT event) now rides a
  new `poly_at(ch, note, val)` callback into scripted synths — the v104
  fix armed hosted plugins; this ships the scripted half. Version-gated
  like v100's bend: a script opts in by exporting `slooper_synth_info_v3`
  (one cumulative table: `{abi, bend, poly_at}`); v1/v2 scripts load and
  behave exactly as before. `val` is the raw MIDI byte 0..=127 — the
  script owns its curve. The seven bundled synths (piano, sampler, pad,
  fm, additive, pluck, tri_osc) raise the pressured voice's level by up
  to +3.5 dB; unpressed renders are byte-for-byte the old sound.

- **Pitch curves, drawn on the notes.** The piano roll now draws and
  edits each note's pitch curve right ON the note body: Z+drag paints a
  freehand curve (release over another note and the slide lands on its
  pitch, 808-style), selected notes reveal draggable control points
  (Shift snaps time, Ctrl snaps 100-cent steps), Alt+Z warps the whole
  curve, and Shift+right-click clears just the curve. Every edit bakes
  and re-auditions immediately — you hear what you drew. The old
  separate glide strip is retired: it stays visible only for legacy
  projects whose bends lived in gaps, while everything migrated onto
  notes paints and edits on the note.
- **Per-note pitch curves (engine).** Sampler pitch bends now live ON
  their note as a control-point curve (up to 16 points, ±2 octaves,
  smooth monotone interpolation — 2-point curves are the old straight
  sweep to the bit). Every consumer derives from one source: the baked
  bend lane feeds VST instruments, MIDI export, and the roll preview
  exactly as before, and scripted instruments pick it up over the same
  channel-bend path. Projects are forward-compatible: glides drawn in
  the old lane migrate automatically to their notes on load (glides in
  gaps keep working as before), files save and reload with their curves
  intact, and every bend-less pattern and old project loads byte-
  identical. The in-roll curve editor for it shipped directly above.
- **slooper can be driven over MCP (optional, off by default).** A new
  Settings ▸ AUTOMATION (MCP) section hosts a built-in MCP server: when
  enabled, an MCP client (Claude Desktop, coding agents) can connect to
  THIS running instance and list/call tools that drive it live. It
  binds 127.0.0.1 only; access needs the auto-generated token written
  to `mcp-endpoint.json` in the data folder; a read-only latch refuses
  every writing tool the moment you tick it; every enable/port/latch
  change applies live — no restart — and echoes into the status line.
  Stdio-only clients use `slooper --mcp-bridge`, which discovers port
  and token from that same file, so no secret ever lands in a client
  config. This is the first installment: the protocol core and a demo
  echo tool are live; settings, window, mixer and plugin control tools
  follow in later updates.

### Fixed

- **Dense sidechain routing ducks every track during playback again.**
  The live engine kept its sidechain routing table at 256 edges while the
  mixer can legally wire every track to every track (up to 16k routes at
  128 tracks): past the 256th route the send simply did nothing — no
  ducking, no warning, no counter. Worse, EXPORTS were computed by a
  separate, uncapped path, so the bounced file ducked correctly while the
  speakers did not, hiding the bug from anyone who only checked the
  render. The routing table now always holds every legal route (grown
  once, no per-block cost, zero cost for projects without routing), so
  truncation is structurally impossible; pins drive the live engine at
  400 and 16,002 edges and demand every target's key arrives.

- **Dense MIDI bursts no longer vanish inside VST2 plugins.** When more
  than 64 channel-voice messages came due in one ~12 ms block aimed at a
  single VST2 plugin — a big chord stack, a dense multi-lane pattern
  summed onto one instrument, controller floods after a seek — every
  message past the 64th was silently lost: the host had already drained
  its whole due batch from the queue but handed the plugin only the
  first 64, and the drop counter never heard about it. A lost note-off
  left a note ringing until the next panic; a lost note-on left a dead
  voice. The single per-block `processEvents` batch now grows to carry
  every due event in one call (VST3 and CLAP hosts were already
  honest), verified with 200-simultaneous-note-on and split-across-
  blocks pins that demand every message, in order, drops still 0.

- **Automation and MIDI knobs now follow the chain when you edit it.**
  Automation curves and MIDI value bindings address plugin parameters by
  chain position, so removing or reordering an effect used to leave them
  silently driving the WRONG plugin — delete one row and every curve below
  it jumped a slot, and the automation manager still showed the old plugin
  name. Chain edits (remove, reorder, dead-script cleanup, and the MCP
  `chain.pick`/`chain.remove` tools alike) now retarget every curve and
  binding with the rows they follow, rewrite the saved curve files so the
  fix survives a restart, move open windows and in-flight plugin loads
  too, and echo what moved ("· 2 curve(s) retargeted, 1 binding(s)
  removed"). A curve aimed at a row that is gone is defused (it can never
  be silently captured by a later-added plugin) and reported; if the
  transport ever hits a curve that cannot reach its target, the status bar
  shows an orange "automation target lost" badge naming the dead targets
  until the chain or the binding is fixed.
- **Installing Rust while slooper is running now just works.** The
  compiler availability check used to ask `rustc` exactly once per
  session and trust that answer forever: install or repair Rust after
  starting slooper and every scripted effect and synth kept showing the
  "install Rust" hint until you restarted the app (and after uninstalling
  Rust, slooper kept promising it was there). The check now refreshes
  itself: a missing compiler is noticed again about once a second, and a
  compile attempt that fails re-checks immediately, so the hint appears
  and disappears as the toolchain truly comes and goes — no restart.


- **Per-note aftertouch now reaches CLAP instruments.** A MPE controller's
  note-by-note pressure (polyphonic aftertouch — the message that bends
  one held note while its neighbours stay still) was forwarded to VST2 and
  VST3 instruments but silently dropped on the way to CLAP ones, so the
  same controller played differently depending on the plugin format. CLAP
  instruments now receive it like every other instrument. Built-in
  scripted instruments still ignore per-note pressure — their script
  interface has no per-note aftertouch input yet (a follow-up will add
  it); channel aftertouch and expression work there today.

- **Pitch bends now bend the same amount on every plugin.** A glide or
  wheel bend drawn in the piano roll is written in semitones, but hosted
  instruments received it raw and interpreted it with whatever bend range
  they happened to default to — often ±2 semitones, so a 7-semitone glide
  sounded small and every plugin disagreed. Each track now has a
  **bend range** setting (PARAMETERS window, instrument tracks; default
  ±2 st, 0 disables bend, saved with the session and project): slooper
  programs the plugin with the standard MIDI pitch-bend-sensitivity
  message (RPN 0) on load, on every change, and just before the first
  bend on each MIDI channel, and rescales the wheel to match — so bends
  inside the range hear exactly their written depth, and deeper bends
  need a wider range. CLAP instruments are covered too — they previously
  received no pitch bend at all.

- **The piano roll's glide lane now glides on the built-in instruments.**
  Pitch bends baked into a clip — drawn with the glide tool or recorded
  from a hardware wheel — were silently discarded for every scripted
  instrument, including the default sampler: playback, audition and
  export all sounded exactly like no bend at all. (VST/CLAP instruments
  always received them, so the lane looked alive in the editor.) Scripted
  instruments now receive bends like MIDI: a full deflection is ±12
  semitones, and held notes shift with the line while it plays.

- **Docked subwindow sizes survive restarts — for real this time.**
  Resizing a docked panel (MASTER OUT was the worst case) and relaunching
  reset it toward a minimum: every restart quietly shaved a frame's worth
  of pixels off the saved size until it stuck at the dock floor, and any
  unequal split between windows sharing an edge flattened to a fair
  half-and-half on the very first frame after boot. The persisted geometry
  now measures what the dock actually reserves (instead of the inner
  content area, which is narrower by the panel margins — the ratchet), is
  only updated by actual divider drags (never by the layout's own
  compromises), and reboots honoring the user's proportions instead of
  equal shares. Float↔dock toggling keeps the chosen thickness, and a
  deliberately-thin dock is no longer yanked back to the built-in width.

- **Sampler presets and key maps survive restarts.** Saving a sampler
  key map (session, project, or named preset) and loading it after
  restarting the app listed every sample as `(missing)` and played
  nothing — the refs were fine, the resolver was not: map slots point
  at their **canonical rack file** (audio is never embedded, same rule
  as clips), but resolution only saw the in-session sample table,
  which a fresh start leaves empty. Every resolve step now re-links a
  slot through the rack folders (exact path, then file-name fallback —
  a moved rack folder no longer loses the samples, the slot is
  rewritten to the new canonical location) and decodes it up front. A
  `(missing)` marker now appears only for a file genuinely gone, and
  the load message names each such slot instead of staying silent.
  Drum-kit pads, which persisted the same pointers, got the same fix.

## [0.23.0] — 2026-11-18

The **workflow release**: every sound gets a preset shelf, hosted
plugins expose their factory programs, the mixer grows a full-page
console, the grid grows color — and nearly every click now answers back.
152 commits since 0.22.0, CI-green on Linux **and** Windows throughout.

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
  every edit is **audible immediately** while you drag. Any audio clip
  on the grid opens it from its right-click menu; edits are per-clip
  and never touch the source file.
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
