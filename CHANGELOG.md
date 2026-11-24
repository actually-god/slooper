# Changelog

All notable changes to slooper are documented here, newest first.
Versions are the git tags (`v<X.Y.Z>`); builds live on
[the releases page](https://github.com/actually-god/slooper/releases).

## [0.24.0] — unreleased

The **expression release**: pitch curves you draw right on the notes,
per-note pressure (poly-AT) reaching every instrument, sidechain
send/receive levels, and an optional MCP server an agent can drive the
app through — on top of a sampler-workflow, recovery and copy-correctness
pass. 329 commits since 0.23.0, CI-green on Linux **and** Windows
throughout.

### Added

- **JACK server-death detection + auto-reconnect (v118).** When the
  JACK/PipeWire-JACK server dies mid-session — the routine desktop events
  (USB-DAC unplug, device-profile switch, PipeWire restart) that used to
  leave Linux users in permanent silence until they restarted slooper —
  the status line now says **"JACK server lost — reconnecting…"** and the
  app rebuilds its client by itself: the server's own shutdown notification
  plus a process-block liveness backstop feed a bounded reconnect loop
  that re-wires your exact port selection (and the MIDI auto-links) the
  moment the server answers, re-announces the possibly-new sample rate,
  and echoes **"JACK server recovered — audio restored"**. A server that
  is simply *down* is retried patiently (no attempt budget — audio comes
  back by itself); a server that is *refusing* gets three rebuilds before
  the app stops and says so, and re-picking your ports in Settings retries
  immediately. This is the Linux twin of the Windows WASAPI recovery
  contract (W12a); the decision table ships pinned in
  `src/audio/rt_recovery.rs` on both platforms.
- **Native effect & instrument control over MCP (v128/MCP-05).**
  Twenty-one tools for the built-in sound layer: `plugins.list` /
  `plugins.instances` (the catalog of every bundled effect and synth,
  with parameter schemas *derived* from the same metadata the knobs
  show, plus everything currently loaded), `plugin.describe/params/
  param.get/param.set` (read or write any script effect/synth parameter
  through the GUI's own apply seam, enum-snapping and clamping
  included), `plugin.preset.list/save/load` (the real preset-bar
  storages, sanitizers honored), `sampler.map.get/set` +
  `sampler.layers.add/remove` (key maps as decomposed {asset, root
  note, velocity layer} slots), `drum.kit.get`, `drum.pad.set`,
  `drum.preset.*`, and `midi.get/note.set/note.remove` (edit pattern
  notes through the roll's own commit path). Deliberately native-only:
  hosted VST/CLAP rows answer `wrong_kind` — their editors own their
  parameters. The read-only latch blocks every writer (pinned by
  `v128_*`).
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
  sampler, channel and single key-map slot). Every write echoes the post-change state
  back; the read-only latch blocks all of them.
- **Settings read/write over MCP (v108/MCP-02).** Three more tools:
  `settings.list` answers with every persisted setting — JSON type,
  live value, requires-restart flag and unit/enum meta — auto-derived
  from the config schema, so it can never go stale or incomplete;
  `settings.get` reads one; `settings.set` writes one, validated
  through serde plus the GUI's own setter seams: hot-appliable fields
  take effect live, the rest persist for the next start and say so,
  and unknown fields or bad values are refused with nothing changed
  (pinned by `v108_*`).
- **Note pressure reaches scripted synths (script ABI v3).** The
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
  config. That first installment (the protocol core plus a demo echo
  tool) has since been filled out: the settings, windows,
  mixer/chain/rack and native-plugin families listed above all ride
  the same spine — **45 tools live today**, each one read-only or
  latch-gated.
- **Per-track sidechain send level on the MIX page (v99).** Every MIX
  channel column grew a **send** row: one slider scales the key bus
  this track feeds to *every* chain that routes it — set the ducking
  strength once instead of per receiver — and because the send is its
  own control, the track's fader and mute never touch the key. It
  rides the project (only non-default senders are stored) and exports
  exactly as it plays (pinned by `v99_*`).
- **Per-route receive level for sidechain sends (v97).** In a track's
  chain window, every routed "Sidechain in" source chip carries a
  receive slider: one source can key one chain hard and another
  barely, independent of how loudly it is sent. Live and export
  alike, saved with the project (pinned by `v97_*`).
- **Preview several tracks at once (v96).** The track-pick menu
  toggles: pick a SET of tracks — via that menu or by Shift-clicking
  their headers — and the preview routes to all of them, staging one
  audition voice per track on its own bus. A sidechain source and
  its target become audible as a pair, ducking and all (pinned by
  `v96_*`).
- **Multi-track selection (v94).** Shift-click a track header (grid
  or mixer strip) to add or remove it from the selection; a plain
  click replaces it as before. The selection prunes correctly when
  tracks are removed, and the record-group "grp" join now works from
  it. Its first payoff is the multi-track preview above (pinned by
  `v94_*`).
- **Drag samples straight onto the sampler key map (v90).** Drag an
  audio asset from the rack: drop it on a lane row to replace just
  that velocity layer, on a key (piano strip or lane) to replace
  every layer of that key — or add the first one if it was empty —
  and on an empty map to create the first layer at C3. Every zone
  explains its drop on hover; non-audio or stale drags are refused
  with a status echo and zero state change, and a good drop lands in
  the live map so the key auditions the new sample immediately
  (pinned by `v90_*`).
- **The version is in plain sight (v91).** The public version now
  shows under the project name in the header — taken from the same
  build metadata a hosted plugin reports to its host, so it can
  never drift — and the Settings panel ends with an "internal
  version: vNN" row (hover explains it) for support requests
  (pinned by `v91_*`).

### Changed

- **The documentation that ships into your folders now tells the truth.**
  Bundled script headers are seeded straight into every user's
  `synths/`/`effects/` folders, so a header claim is user documentation —
  and three of them described DSP the code does not do: the drums
  soft-clip doc promised bus-sum headroom the per-voice curve cannot
  give (the engine's output clipper bounds the mix), the drums tune
  comment claimed the metal hats ignore the tune multiplier (they follow
  it; only crash and ride are pitch-immune), and the pluck's GLIDE
  header promised live re-pitch of held notes (a Karplus-Strong string's
  loop length is frozen at strike, so a bend lands on the *next* note —
  delay-loop models now say so in the bend contract). Every header claim
  is pinned against the code it documents. The preset-file marker also
  sent users to a "Presets" section of `docs/PLUGINS.md` that does not
  exist; it now names `docs/USAGE.md`, where the real Presets section
  lives. Existing installs pick the corrected text up through the
  regular content-diff seeding (pins `v151_*`, card t_0c4df02a).
- **The synth-script contract now documents the real key-map slots.**
  The `synth_api.rs` reference slooper writes into every synths folder
  still described the pre-v52 slot packing `(asset_id << 8) | root_note`
  for the `load_map` host call, while the live ABI packs
  `(asset_id << 16) | (velocity << 8) | root_note` — a third-party
  sampler written to the shipped text reads the velocity byte as the
  asset id and silently plays the wrong sample. The contract now states
  the live packing, the unpack recipe and the velocity-layer semantics
  (a 127 layer answers every note, so maps made before velocity layers
  behave exactly as before). Existing installs pick the corrected file
  up through the regular content-diff seeding (pins `v148_*`).
- **The ABI contract files can no longer drift.** The two contract
  headers every script synths/effects include (`synth_api.rs`,
  `effect_api.rs`) each existed twice in the source tree as
  byte-identical twins — and only the effect twin was guarded by a
  test, so a future edit could have shipped a stale contract into
  users' folders (the v0.12 seeding-class bug, one layer up). The tree
  now holds exactly one copy of each (in `synths/` and `effects/`),
  which the binary embeds directly, and both are pinned. No behavior
  changes; existing installs get the one corrected header line through
  the regular content-diff seeding (pin `v149_synth_api_*`).
- **The convolver's folder documentation now tells the truth.** The
  README the effect generates next to its impulse files promised files
  are “peak-normalized”, while the convolver deliberately
  ENERGY-normalizes them — that is what makes a short and a long IR sit
  at the same loudness, and matching two IRs by peak level produced the
  opposite of what the folder’s own instructions said. The README now
  explains the real model, and the folder hint in the effect list
  follows the OS: Windows users are no longer sent to
  `~/.local/share/slooper/impulses` (a path that does not exist there)
  but shown the real `%LOCALAPPDATA%` location. Existing installs pick
  both up through the regular content-diff seeding (pins `v146_*`).
- **The effect template now teaches the whole plugin ABI.** The starter
  file every user copies (`effects/template.rs`) still taught the v1-era
  contract: an “up to 8 params” limit (the real cap is
  `SLOOPER_MAX_PARAMS` = 32 — the stock parametric EQ ships 26) and no
  mention of the sidechain, stereo, transport-sync, param-metadata or
  custom-GUI capabilities the effect ABI has grown since. It now carries
  an “OPTIONAL LADDER” section naming every rung and a full commented
  ABI-v4 + custom-GUI version of its own tremolo — proven Rust: a pin
  (`v144_*`) un-comments the skeleton, compiles it and loads it through
  the real effect loader, so the skeleton cannot quietly drift stale the
  way the v1-only docs did.

### Fixed

- **A hosted CLAP instrument no longer mallocs on every audio block.** The
  CLAP backend destroyed its event-staging buffer at the end of every block
  and grew a fresh one the next — one malloc/free pair roughly every 12 ms
  on the audio thread, every time a note or knob moved — because the staging
  loop “took” the buffer out of the live state to satisfy the borrow checker.
  Staging now iterates the persistent buffer in place, the pattern the VST2
  backend has always used: the queue keeps its capacity between blocks and
  is emptied, never freed. Allocator jitter is exactly the class of glitch
  that turns into dropouts under load, and this was the last violation of
  the CLAP module’s “the audio thread allocates nothing” rule.
- **Exporting no longer spams debug chatter to the terminal.** The
  Export button still carried two leftover debug lines from its
  authoring session — every export printed `PP exporting` and
  `PP export queued` to stderr, log noise that names nothing. The
  status line in the app is the real progress echo (pinned by
  `v140_*`).
- **Windows: the MIDI "Refresh" button now says what it re-scans.** In
  Settings → MIDI INPUTS, the Refresh button hovered “re-scan the JACK
  port list” — copy-pasted from the audio ports section above. On
  Windows the audio list is WASAPI endpoints (JACK is an opt-in
  backend), and this control re-scans the MIDI devices: the one button
  you reach for after hot-plugging a MIDI keyboard named the wrong
  subsystem. It now reads “re-scan the MIDI device list”.
- **Cymbals and toms now decay to silence instead of being chopped.** The
  stock kit retired its voices long before their tails were inaudible — at
  the top of the decay slider a crash was hard-silenced barely halfway
  through its own decay time constant (a −13 dB instant zero, an audible
  cut and a DC step in mid-ring), and open hats fared barely better. Every
  voice now rings for seven time constants of its slowest decay component
  (about −61 dBFS, provably below hearing) with an 8 ms linear fade to
  exact zero, so retirement itself is inaudible on every pad (pinned by
  `v137_*`).
- **Project-asset context menus now say FILE where they act on files.**
  The rename and move entries on a “project assets” row hovered
  “rename/move this folder …” while the buttons rename or move the
  sample FILE — hover copy is the affordance contract, and users were
  avoiding (or fearing) a folder operation these rows never perform.
  The wording now matches the rack-row twins: “rename this file on
  disk”, “move this file into the library root”, “move this file into
  this folder”.
- **Drum tracks finally obey the GLIDE lane and per-note aftertouch.** The
  stock kit was the last bundled instrument still on the v1 script ABI: a
  baked GLIDE ramp on a pitched-tom row and MPE note pressure (choke or
  brighten a cymbal) were delivered to its track and silently dropped while
  every melodic sibling answered them. The kit now speaks ABI v3: a bend
  ramp bends held kicks, snares and toms (and the hat fundamentals) exactly
  like the tune knob turns, and aftertouch is per-voice and class-based —
  pressed open hats, crash and ride CHOKES with up to 4x faster decay,
  pressed closed hats, snares and claps brighten (HF pre-emphasis and a
  climbing noise band), pressed kicks and toms raise their head tension
  by up to a major third — a pressure of 0 reads as "released", never
  silence
  (pinned by `v136_*`; the last bundled v1 script is gone, so the v1 drop
  guard now rides a dedicated v1-only probe).
- **Deleting a sampler-map layer no longer flickers wrong values into the
  rows below it for one frame.** Clicking a lane's `×` removed the slot
  in the middle of the row-paint loop, so the lanes still being painted
  that frame showed the *shifted* slot's data — a velocity suddenly
  reading another layer's value (or the maximum fallback) — and an edit
  landing in that same frame could write to the wrong layer. The removal
  now applies after the row loop: the click frame paints one consistent
  snapshot, same-frame edits land on the row they were shown on, and the
  next frame renumbers as before (pinned by `v135_smap_delete_*`).
- **Granular tracks finally obey velocity, GLIDE and per-note aftertouch.**
  The granular script was the last bundled instrument that read none of the
  piano-roll expression lanes: `note_on` dropped the velocity byte on the
  floor, and because the script never left ABI v1, the GLIDE lane (v100)
  and the MPE pressure lane (v106) silently discarded those events too.
  Velocity now scales grain gain by (vel/127)^1.3 like every sibling
  (renders at vel=127 stay bit-identical — the law multiplies the grain
  draw, never the RNG stream), a baked bend ramp glides the whole cloud,
  and pressure thickens *and* brightens exactly the pressed voice: the
  grain spawn rate doubles at 127 and each grain gets a brightness
  pre-emphasis, while a val of 0 reads as pressure released, never silence
  (pinned by `v134_*`; granular now joins the shared `v106_*` poly-AT list).
- **Echo + Reverb's `damp` knob now does what its label says.** The knob
  is documented (and displayed) as a damping corner in Hz — higher =
  brighter tail, exactly like the sibling `tone` knob — but the comb
  loop received `1 − exp(−2π·damp/sr)` instead of the filter coefficient
  itself, so dragging `damp` up DARKENED the room. The wiring now mirrors
  the tone path, and the factory rooms were rebalanced to their named
  intent (measured mid-tail ladder): *Practice Hall* opens to 0.82/9000,
  *Opera House* settles at 0.86/2800, *Cathedral* deepens to 0.94/1200 —
  bright hall → auditorium → opera → dark cathedral now descends in
  exactly that tonal order (pinned by `v133_space_echo_*`; the older
  `v14_*` forensic flavors were retargeted to the same effective loop
  coefficients across the law change).
- **A preset name that sanitizes to nothing is now refused instead of
  written as an invisible file.** Typing pure punctuation ("!!!") into a
  hosted plugin's parameter-preset box reported "saved" while writing a
  hidden `.json` that no picker could ever list. The shared preset store
  now refuses any name that empties under sanitizing at the write
  primitive itself, and the plugin-parameter save path echoes the same
  "pick a preset name" refusal as the other preset surfaces before any
  file is touched (v132, pinned by `v132_*`).
- **Auto Pan's "hold" volume shape modulates again.** The fifth tremolo
  shape (`4 hold`) was a dead branch: its latch compared the volume LFO
  phase against a re-arm threshold that starts at zero while the phase
  itself always sits above zero, so it could never fire — the latched
  gain stayed at unity and the volume wheel went *dead* at every rate
  and depth (only the pan wheel kept working). `hold` is now a real
  sample & hold: a fresh deterministic stepped gain latches exactly
  once per LFO cycle, both channels stepping together (phase offset is
  deliberately ignored, as the header and picker now document).
  (v130, pinned by `v130_autopan_hold_latches`.)
- **The master meter now follows a live sample-rate change.** The
  meter's LUFS windows and spectrum axis are baked from the sample rate
  it was built with, but creation used to be a once-ever guard — after
  reconnecting to a server running at another rate (44.1↔48k, hot-swap
  or JACK-server rebuild) the master readouts kept integrating wrong
  window lengths and labeling wrong frequencies until restart. The pump
  now replaces the meter whenever the live rate differs from the one it
  was built for (pinned by `v131_master_meter_*`).
- **Drum-kit pickers stopped showing Windows paths (v129).** Pad sample
  labels were split on forward slash only — and Windows rack-scan paths
  carry backslashes — so the shipping Windows build showed each pad's
  FULL path in its picker rows, the dropdown and the import echo. One
  separator-agnostic filename helper now feeds every such label site,
  with a source-scan pin so a new slash-only site cannot slip in
  (pinned by `v129_*`).
- **Velocity-lane drags now land where you grab (v127).** The bars are
  painted over a shorter travel, with a different floor inset, than the
  drag math assumed: grabbing the painted top of a full-velocity bar
  read ~8 % low, and the error grew as bars got shorter. Paint and drag
  now share one law — the drag mapping is the paint mapping's exact
  inverse — so the pixels are byte-identical and a drag reads exactly
  the velocity it shows (pinned by `v127_*`).
- **Auto Pan's *centre* parameter pans (v126).** It used to scale the
  pair's gain instead: full left muted the pair, full right hot-summed
  it, and only the wheel ever moved the image. Centre now biases the
  shared wheel angle through the app's equal-power pan law — a full
  image shift at constant loudness at either extreme (pinned by
  `v126_*`).
- **Cancel-a-knob-learn no longer wipes an unrelated key binding.**
  Pressing **Delete** while binding a MIDI knob to a project value
  (volume, pan, an FX parameter) used to silently clear the **action-0**
  MIDI binding — "record (selected)" — because the value wizard shares
  the action wizard's slot field and used it as its index; the value
  binding you actually meant to cancel stayed bound, and the status line
  (and the wizard header) named the wrong thing. Delete now mirrors the
  bindings rack: on a re-learn it unbinds exactly that value binding, on
  a fresh one it just cancels, and the action-slot wizard is untouched
  (v124).
- **Every release artifact now carries the piano sample's GPL-2 notice
  and license text.** The embedded piano blob derives from the GPL-2
  TimGM6mb soundfont, but the disclosure lived only inside the repo tree
  — the shipped tarball, deb, rpm, win64 zip and installer carried just
  the MIT `LICENSE`. Now `LICENSE-GPL-2.0.txt` + `THIRD-PARTY-NOTICES.md`
  stage into all five artifact lists (deb/rpm under the doc directory),
  and the packaging scripts assert the docs inside the *built payload*
  (pinned by `gpl2_*`).
- **Chorus and delay now pass a true stereo image through.** Both v3
  dry paths rebuilt the mid/side pair wrong: the chorus dropped the side
  from the left and *added* it to the right (the dry right channel
  literally carried the left input), and the delay added its side term
  unscaled, so dry content leaked into the right even with mix at 100%.
  A hard-panned source through either flagship effect vanished from its
  own channel and collapsed toward centre; mono was unaffected, which is
  why every existing pin kept passing. Both now reconstruct L=mid+side,
  R=mid-side scaled by (1-mix) — mix 0 is a true pass-through and mix 1
  a true wet (v125, pinned by `v125_st_dry_passthrough`).
- **Auto-placement now dodges long clips (and stops dodging ghosts).**
  The lane-occupancy test built every existing clip's right edge from the
  incoming item's own length, so a short take landing on the tail of a
  longer clip stacked onto lane 0 (additive double playback over the
  overlap) while a short existing clip pushed any longer take to a
  needless child lane. Both `Lane::occupied` and `place_auto` now resolve
  existing spans through the asset table — the module doc's promise
  finally holds for asymmetric lengths (v112).
- **The master clip LED can be turned off now.** A single sample that
  ever hit full scale lit the red CLIP indicator in MASTER OUT for the
  rest of the session — nothing could clear it short of restarting
  slooper, so the panel kept accusing the mix long after the one
  transient that set it off. Click the red LED (or the meter column that
  paints it) to reset the latch, or let the "reset I" button do it along
  with the integrated-loudness reset; every affordance explains itself
  on hover and answers in the status line.
- **The EXPORT dialog's scope choice is one radio again.** The three
  scope options shared two state flags, so "Track N (selected)" lit up
  **at the same time** as "Current selection" — and clicking it there did
  nothing: single-track export became unreachable once you had picked
  "Current selection" (the only way back was a detour through "Whole
  project"). Each option now shows its own checked state — exactly one is
  ever filled — and clicking any of them selects exactly that scope.
- **Renaming or moving a sample keeps its baked edits.** Renaming a rack
  file (or moving it into another folder — or moving whole folders) used
  to orphan the sample editor's stored edit and its pristine source: the
  settings were keyed by the file's old path, so the next project load
  silently dropped every fade, gain, reverse and normalize baked onto
  that sample — and a "restore" source copy was left stranded. The edit
  now follows the file wherever the rack takes it (an open editor window
  rides along too), the project records it under the new path, and
  deleting a file forgets its edit instead of letting a later file at the
  same path inherit it.
- **Names Windows would refuse now fold, on both OSes (v121).** The
  NTFS-reserved characters (`< > : " | ? *`) sailed through name boxes
  on Linux and failed the write on Windows. They now fold to a safe
  character on BOTH platforms — a file or preset named on one OS
  always lands on the other — across the sample save-as, rack folder
  adds and every sanitizer-backed name field; and creating a rack
  folder can no longer climb out of the rack with `..` (pinned by
  `v121_*`).
- **Successive splits walk the clip again (S).** The docs promised that
  after a split the right half keeps the selection, so pressing S again
  at a further playhead chops a long clip into pieces — but the
  selection silently stayed on the left half, and the second S answered
  "the playhead is not inside any selected clip". Every split now moves
  the selection onto the right halves (keeping any other selected clips,
  re-pointed at their new indices), so the documented repeated-S
  workflow works end to end on one clip, across several clips, and
  across tracks.
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
- **A misbehaving CLAP plugin gets isolated, not fatal (v116, v117).**
  A C++ exception thrown inside a CLAP plugin — or a panic from
  slooper's own host code at a plugin callback — reached the process
  boundary and killed the whole app (`std::terminate`). Every crossing
  of that boundary now runs behind the firewall the VST side has long
  used: probe and open handshake, the per-block process loop, state
  save, preset load. The offending plugin is parked (shown as dead,
  treated as silent) while the session and every other plugin keep
  running. A companion guard makes the factory-program index mapping
  total, so a bogus value relayed by a flaky plugin clamps to program
  0 instead of corrupting the picker (v119; pinned by `v116_*`,
  `v117_*`, `v119_*`).
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
  instruments now receive it like every other instrument. Built-in scripted
  instruments got their own per-note pressure hook later in this same
  release (script ABI v3 — see *Note pressure reaches scripted synths*);
  channel aftertouch and expression worked there already.
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
- **Muting or fading a ducking source no longer kills the duck
  (v95).** The sidechain key used to be tapped behind the sender's
  volume fader and mute, so mixing a loud source down — or muting it
  to silence it — silently weakened or killed the ducking it drove.
  The tap now sits after pan/gain and ignores the fader and mute
  entirely: only the dedicated send controls (v99/v97) scale the key
  (pinned by `v95_*`).
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
- **The sampler key map's black keys follow the real 2+3 pattern
  (v92).** From F# up, the black keys painted one white-boundary
  early: an octave could show five black keys in a row, one floated
  into the E–F gap, and the A–B and B–C gaps came up empty — and
  because the click hit-test derived its notes from the same broken
  table, those keys played the wrong note. One shared helper now
  feeds the keyboard, lane rows, zone bars and drop targets, with a
  128-key click round-trip pin keeping it true (pinned by `v92_*`).
- **The sampler key map scrolls both ways (v89).** With more mapped
  samples than the window shows, the lane rows were unreachable: the
  scroll area enabled the horizontal axis only — no vertical bar, no
  mouse wheel. Both axes scroll now, with solid bars that appear
  exactly when content overflows, and rows still click through
  correctly inside a scrolled view (pinned by `v89_*`).

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
