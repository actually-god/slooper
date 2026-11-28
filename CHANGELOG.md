# Changelog

All notable changes to slooper are documented here, newest first.
Versions are the git tags (`v<X.Y.Z>`); builds live on
[the releases page](https://github.com/actually-god/slooper/releases).

## [Unreleased]

### Added

- **Every input device now records as its own clip.** Ticking three
  inputs in the INPUTS list and pressing record used to bounce ONE mixed
  mono take per armed track — and a long-standing flag bug planted that
  same take TWICE, which is why recordings always arrived as a pair.
  Now each ticked input device is captured separately and lands as its
  own clip on the armed track, named
  `project name-input device name-Track number` (e.g.
  `Live set-Motif M2-Track 3`); recording the same device again appends
  `-2`, `-3`, … so no take is ever overwritten. Nothing ticked, or only
  one thing ticked, behaves as before with one clip. Preview bounces,
  loop takes and MIDI recording keep their established behaviour, and
  the live take waveform keeps drawing while the recording rolls.
- **Removing a child track now asks first.** Pressing `−` on a track
  whose last child track still holds clips opens a confirmation that
  names what is lost (the clips on that lane) and what is kept (the
  assets, safe in the asset rack). Cancel — or the Escape key — keeps
  everything; *Never warn again*, checked at confirmation, is remembered
  across restarts. Empty child tracks still remove instantly. This
  dialog is the app's new STANDARD confirmation pattern: future
  "are you sure?" popups share its look, its safe-default Cancel and
  its persisted never-ask checkbox.

- **Drawn pitch curves gained a readable axis.** A bent note's curve
  trail now paints and drags at a constant cents-per-pixel: ±1200 cents
  fills the trail box and a wilder swing EXTENDS it — a ±2-octave curve
  paints fully, edits at the same precision, and the row-zoom wheel is
  the curve-zoom knob. Every aim now reads back: while drawing or
  dragging a curve point, the status line names the live pitch in the
  project's tuning (`E4`, or `D#4+50c` in 24-EDO) plus the cents,
  hovering a control dot reads its pitch on the canvas, and the release
  echoes carry the label too.

- **Microtonal keyboards now read.** With the octave-divisions setting
  off 12-EDO, every note label stays a standard note name on the rows
  that coincide with 12-EDO ("anchor" rows — a 24-EDO C-sharp still
  says `C#4`, and sits exactly where 12-EDO's C#4 sits), while the
  other rows name their nearest standard note plus the signed deviation
  in cents (`C4+50c`) and print dimmed so the two classes are obvious
  at a glance. Piano-roll key rows, the sampler-map keyboard, status
  lines, MIDI-monitor echoes and the root picker all draw from ONE
  label builder, the root choice moves the anchor grid, and 12-EDO
  rendering is byte-for-byte what it always was.

- **The master spectrum can draw as one smooth line.** A
  "smooth spectrum line" toggle in SETTINGS — plus `points` / `line`
  words right above the panel in MASTER OUT — switches the trace from
  separate points to ONE continuous curve (Catmull-Rom through the
  same points, peak hold included; no polyline kinks). Both modes read
  one shared display pipeline; the choice is remembered across
  restarts, and points stay the default.

### Changed

- **Record groups live in the selection now.** The mixer strip's `G`
  checkbox is retired — the track HEAD MENU carries both directions for
  the whole selection at once: *REC group (selection)* joins every
  selected track (unchanged join order and shared loop selector), and a
  new *Leave group* removes the selected tracks that are members while
  the rest of the group stays. Joined tracks still arm and stop
  together from the top-bar **REC GROUP** master.

- **The master spectrum reads evenly now.** The analyzer window doubled
  to 16384 samples (bin spacing ≈2.9 Hz @ 48 kHz, 8192 bins), and the
  displayed trace is projected onto 1024 equal-per-octave points, so
  the points spread uniformly across the sweep instead of crowding into
  the high band and going sparse at the low band. Narrow peaks survive
  the projection at full resolution (per-cell max-hold, nearest-bin
  fallback below 2 kHz), and the added analysis stays off the audio
  thread entirely (measured: 291 µs per UI frame, zero RT-thread change).
- **The whole interface speaks with one voice.** A program-wide
  consistency pass took every surface — mixer strips and master out,
  the arrangement grid, piano roll and MIDI windows, the stock
  editors (drums, sampler map, sample editor), automation, dialogs,
  and the top bar — and gave them one law: every action echoes a
  plain-language verdict to the status line, every hover states what
  the control acts on, and a verb that can't run is *disabled with
  the reason in its hover* instead of clicking silently (an empty
  record group is the example you'll meet first). The top bar now
  folds its grid controls into a **More…** popup as windows shrink —
  nothing clips at any usable width. Status echoes fade after ~6 s
  and hand the line back to the engine summary at ~12 s; the full
  text always survives in the strip's tooltip. Color and geometry
  now live in single frozen homes, so two surfaces can no longer
  drift apart.
- **Confirmations are all the same dialog now.** Loading a project
  asks first and offers a one-step **Save current & load** escape
  hatch; saving under a name that belongs to another project asks
  before overwriting, with a remembered *Never warn again*; Settings
  ▸ Reset to Defaults names everything it clears and confirms first.
  All share the standard look, the safe-default Cancel, and `Esc`.

### Fixed

- **Releasing a sampler key stops only that note.** Held several keys on
  a sampler track and lifting one changed nothing — every note kept
  playing until the LAST key came up, then they all died together. Each
  key now owns its release: the note fades out over a short ~0.3 s
  envelope while the other held notes carry on. Hammering one key
  (retriggering before releasing) keeps the newest note sounding through
  the older key-ups, and the fade is identical live and in exported
  audio.
- **A rubber-band drag now selects across tracks.** Dragging from one
  track's lane down through another used to select only clips in the
  lane where the drag started. The box now collects every lane it
  sweeps through, and a Ctrl/Shift drag keeps the whole pre-drag
  selection — including clips on tracks it never touches. A drag that
  stays inside one lane behaves exactly as before.
- **The MPE settings tooltip now matches what ships.** It told users
  that built-in scripted synths get no pitch bend and no per-note
  aftertouch — true only before script ABI v2/v3. Bend has reached
  scripted synths since v100 (script ABI v2) and per-note pressure
  since v106 (script ABI v3), and all nine bundled scripts export the
  v3 table today. The hover now says both reach hosted plugins **and**
  the  built-in scripted synths.
- **Three clipping bugs, found by the same pass that unified the
  hovers.** Long MIDI-binding labels pushed the **relearn / ×** buttons
  off-screen, leaving bindings you could not edit or remove; the drum
  pad grid silently lost its bottom row of pads in short windows;
  and the kit pad row's tail (mute / unassign) was cut off at the
  synth window's default width. All three now render fully at any
  usable size, with the row layout and window defaults measured to
  prove it.

## [0.24.0] — 2026-11-24

The **expression release**: pitch curves you draw right on the notes,
per-note pressure (poly-AT) reaching every instrument, sidechain
send/receive levels, and an optional MCP server an agent can drive the
app through — on top of a sampler-workflow, recovery and copy-correctness
pass. 349 commits since 0.23.0, CI-green on Linux **and** Windows
throughout.

### Added

- **Draw pitch curves on the notes.** Z+drag paints a pitch curve right
  on a piano-roll note (release over another note for an 808-style
  slide); selected notes expose draggable control points, and every
  edit re-auditions as you draw. The curve is per-note engine data
  (up to 16 points, ±2 octaves) feeding VST, MIDI and scripted
  instruments from one source; glides in old projects migrate onto
  their notes on load, and the separate glide strip is retired.
  Details: [docs/SAMPLER-PITCH.md](docs/SAMPLER-PITCH.md).
- **Note pressure everywhere.** Per-note aftertouch now reaches
  scripted synths (new `poly_at` script-ABI v3 hook), the stock drum
  kit (chokes cymbals, brightens snares, tightens toms), the granular
  script, and CLAP instruments. Unpressed renders are byte-identical
  to before.
- **Optional MCP server (off by default).** Settings ▸ AUTOMATION (MCP)
  hosts a localhost-only server — token in `mcp-endpoint.json`, a
  read-only latch, changes apply live; stdio-only clients use
  `slooper --mcp-bridge`. **45 tools** drive settings, windows and
  layouts, the mixer, chains, rack assets, pattern notes and the
  built-in synths/effects. Tool table: [docs/MCP.md](docs/MCP.md).
- **Sidechain send and receive levels.** Every MIX column grows a
  **send** slider — set the ducking strength once, immune to that
  track's own fader and mute — and every routed "Sidechain in" chip a
  **receive** slider, so one source can key one chain hard and another
  barely. Details: [docs/SIDECHAIN.md](docs/SIDECHAIN.md).
- **JACK server-death auto-reconnect.** When the JACK/PipeWire server
  dies mid-session (DAC unplug, device-profile switch, PipeWire
  restart) the status line says **"JACK server lost —
  reconnecting…"** and audio comes back by itself: your exact ports,
  MIDI links and the live sample rate are re-wired the moment the
  server answers. Windows WASAPI recovery's Linux twin.
- **Multi-track workflow.** Shift-click track headers to build a
  selection and preview several tracks at once — a sidechain source
  and its target now audition as an audible ducking pair. Drag rack
  samples straight onto the sampler key map: drop on a lane for one
  velocity layer, on a key for all of them.
- **The version in plain sight.** The public version shows under the
  project name in the header; Settings ends with the internal build id
  for support requests.

### Changed

- The documentation that seeds into your `synths/` and `effects/`
  folders now matches the code: bundled script headers, the
  `synth_api.rs` key-map contract, and the convolver README's
  normalization model (plus its real Windows folder). The effect
  template now teaches the full plugin-ABI ladder, and
  [docs/PLUGINS.md](docs/PLUGINS.md) gained scripted-synth, CLAP and
  Presets sections. Existing installs pick the corrected files up
  automatically.

### Fixed

- **Sound correctness:** offline bounces no longer turn pitch bends and
  per-note pressure into ghost notes at hosted plugins; Chorus and
  Delay pass a true stereo image; fixed the Delay startup thump,
  chopped cymbal/tom tails, the reversed Echo+Reverb `damp` knob, and
  Auto Pan's dead `hold` shape and non-panning *centre* knob; the drum
  kit and granular scripts now answer velocity, glide and pressure
  lanes; dense sidechain routing and dense MIDI bursts are no longer
  silently truncated; a ducking source's fader and mute no longer
  weaken the duck.
- **Plugins:** a throwing CLAP plugin gets isolated (parked, silent),
  not fatal; per-track bend range programs each hosted instrument so
  drawn bends land at their written depth; automation curves and MIDI
  bindings retarget along with the chain when you remove or reorder
  rows.
- **Persistence:** docked-window sizes survive restarts; sampler key
  maps and drum kits re-link their samples through the rack after a
  restart instead of showing `(missing)`; renaming or moving a sample
  keeps its baked edits; names Windows would refuse now fold safely on
  both OSes.
- **Workflow:** repeated S walks the clip; the EXPORT scope choice is
  one radio again; velocity-lane drags land where you grab; the master
  CLIP LED is clickable to reset and the master meter follows
  sample-rate changes; the sampler key map scrolls vertically and its
  black keys follow the real 2+3 pattern; drum pickers show file names
  instead of Windows paths; "install Rust" notices appear/disappear
  without restarting slooper.
- **Licensing:** every release artifact now carries the GPL-2 license
  text and notice for the embedded piano sample.

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
  stereo spread, vocoder, and a sidechain compressor. Roster with every
  effect's key parameters: [docs/EFFECT-SUITE.md](docs/EFFECT-SUITE.md).
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
