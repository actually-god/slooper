# Plugins: synths and VST hosting

Slooper hosts two kinds of plugin: **scripted synth plugins** (plain Rust
files in `synths/`, compiled on first load into `~/.local/share/slooper/synths`)
and **binary VST2/VST3 plugins** loaded from `vst_dirs`.

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

## The open-source instrument set on this host

Everything here is a **prebuilt Linux x86-64 plugin with a free license** —
drop-in installs from upstream releases, no patches, no config. Each one is
verified by `cargo run --release --example plugin_audit` (held-note render
through the real slooper open path) plus a real-editor probe; instruments
are additionally covered by the `every_local_instrument_makes_sound` test.

| Instrument | Type | License / upstream |
|---|---|---|
| Vitalium | wavetable | GPL-3.0, `matthartung/vitalium` (distrho-ports build) |
| Geonkick | analog drums | GPL-3.0, `raedv/geonkick` |
| Dexed | FM (DX7) | GPL-3.0, `asb2m10/dexed` (v1.0.1 `Dexed-1.0.1-lnx.zip`) |
| JS80P | 8-voice FM/phase | GPL-3.0, `attilammagyar/js80p` (v4.1.1 VST3 bundle) |
| Overtonium | additive | AGPL-3.0, `benjamindehli/overtonium` |
| Virtual FM-1 | FM / organ | GPL-3.0, `jbschooley/Virtual-FM-1` |
| Dragonfly reverbs, Swanky Amp, Wolf Shaper | effects | GPL/MIT, upstreams as packaged by Linux Audio |

amSynth 2.0 is **not** part of the set: its VST2 editor dies mid-build on
this build (`unordered_map::at` + blank panel), and its GUI-less VST3
variant duplicates Vitalium coverage less reliably; the file was removed
from `~/.vst` (v17).

## v19 expansion set (installed under `~/AudioFOSS`, linked into `~/.vst3`/`~/.vst`)

Varied FOSS synths **and** effects, all verified through the real host path
by `examples/plugin_one` per-plugin children (a vendor crash can only take
down its own row). Arch/CachyOS binary packages were extracted into
`~/AudioFOSS/{vst3,vst,share}` (no root needed) and symlinked into the scan
folders; plugin data (Surge, Cardinal) is symlinked under
`~/.local/share/`.

| Plugin | Kind | License / source |
|---|---|---|
| Surge XT + Surge XT Effects | synth / FX | Gpl-3.0, `surge-synthesizer` |
| Odin2 | poly synth | GPL-2.0, `micdhall/Odin2` |
| OB-Xf | synth | MIT, `Tytelus/OB-Xf` |
| JC303 | acid bass | GPL-3.0, `squarewaveaudio/jc303` |
| SG-323 | guitar sim | GPL-3.0, `pusevox/SG-323` |
| SoulForce | organ | MIT, `AzioSoulforce/SoulForce` |
| OneTrick CHONK | bass | GPL-3.0, `OneTrickBass/CHONK` |
| Ultramaster KR-106 | bass/saturn | GPL-3.0, `OlofLindahl/Ultramaster` |
| ADLplug (OPL2/OPL3) / OPNplug (OPNM) | FM synths | MIT + GPL-2.0, `adamgq/*` |
| Kars / Nekobi / MVerb / Ma* / 3Band* / CycleShifter / glBars / PingPongPan / ProM / Amplitude* | DPF demo set + MDA-DM | BSD/GPL, DISTRHO |
| lsp-plugins (198 FX classes) | FX suite | GPL-3.0, `sadko4u/lsp-plugins` |
| RoomReverb | convolution verb | GPL-3.0, `ElephantDashDSP` |
| Cardinal / CardinalSynth / CardinalFX | VCV modular | GPL-3.0, `KXStudio/Cardinal` |
| Stochas | pattern sequencer | GPL-3.0, `SWSoundStudios/Stochas` |

Verified-working notes (audit verdicts):

* **lsp-plugins**: upstream 1.2.35 ships a `moduleinfo.json` whose legacy
  `Classes`/`Compatibility` sections carry duplicate class ids for dev-only
  "A/B Tester" entries; the spec-compliant loader then rejects the WHOLE
  bundle. Fixed locally by dropping those two legacy sections (original kept
  as `moduleinfo.json.orig`); all 198 effect classes load afterwards.
* **ADLplug/OPNplug**: the CachyOS znver4 rebuilds link `libfmt.so.11` but
  the distro moved to fmt 12. Fixed without root: `libfmt.so.11` extracted
  from the Arch archive into `~/AudioFOSS/lib` + `rpath` patched onto the
  plugin `.so`s (patchelf from an extracted package).
* **AmplitudeImposer** (DPF leftovers): crashes on open (no moduleinfo,
  segfault) — quarantined in `~/AudioFOSS/quarantine`, NOT linked in.
* **Cardinal / CardinalSynth / CardinalFX / Stochas**: load and run but are
  intentionally SILENT until a patch/sequence is loaded — the unit tests
  skip them (`local_audio_test_drivable`), as silence is their correct
  default state.

### Legacy 32-bit Windows VSTs: AudioNerdz **Delay Lama** (the singing monk)

The 2001 freeware VST (32-bit Win32, VST ~1.0) has no Linux build. Install
path verified end-to-end on this host — the DLL (from the Internet Archive
mirror of the original distribution, sha256
`469ce87e…`) lives at `~/AudioFOSS/delaylama/win32/Delay Lama.dll`,
registered with `yabridgectl`, and plays through slooper's normal instrument
list. (It deliberately does NOT live under `~/wine*/drive_c/Program Files/…`
— `default_vst_dirs` scans those VSTPlugins paths, and a raw `.dll` found
there would get instantiated by every scan through the bridge.)

Two infrastructure facts this uncovered (both fixed):

1. **Arch wine ≥10 is WoW64-only (no `wine64`), and yabridge's 32-bit
   bitbridge does not work on it** (upstream-documented). A legacy-WoW wine
   must be supplied: `~/Tools/winege` (wine-lutris GE-Proton8-26, contains
   `wine64` + 32-bit libs, no root needed), exported via
   `~/.config/environment.d/yabridge-wine.conf` →
   `WINELOADER=$HOME/Tools/winege/bin/wine64` (takes effect at next login;
   for one-off shells export it before launching slooper).
2. **vendored vst2 patch #4**: the crate rejected every plugin reporting
   `GetApiVersion < 2400`, which bricks legacy VST 1.0/2.0 plugins. The gate
   now accepts any plausible version (see `vendor/vst2/VENDOR-NOTES.md`).

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

How it works (`src/vstedit.rs`) — the standard Linux VST2 hosting protocol,
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
  the processing thread, so the window hosts a **shadow instance** of the
  same plugin. The shadow never processes audio, is primed with the live
  parameter values when it opens, and every edit it reports
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
  grey background as "painted", which turned broken editors (e.g. the
  amsynth 2.0 VST2 GUI, whose editor build dies mid-construction and
  leaves a 1×1 child window) into permanent blank panels instead of the
  slider fallback (v0.10.1). That plugin was later REMOVED from this host
  for exactly that reason; the probe rule stays.
- The title-bar **close button works**: the editor thread dispatches X
  events instead of discarding them (`XNextEvent` returns 0 on *success*;
  an early pump treated that as an error and dropped every
  `WM_DELETE_WINDOW`, so the window ignored the window manager's close),
  and it also exits when the container is destroyed outright. `cargo run
  --release --example editor_smoke [so]` opens a real editor, verifies it,
  injects the same `WM_DELETE_WINDOW` client message a close button sends,
  and asserts the editor exits.
- Plugins with a known-broken GUI are removed from the plugin folders
  rather than shipped broken (amsynth 2.0's VST2 threw `unordered_map::at`
  during editor build and was deleted from `~/.vst`). Any plugin whose
  editor still fails is refused within ~3 s and its parameter sliders
  drive the plugin instead — a bad GUI can never blank-panel or hang.
- Probe and live instances of the same plugin binary intentionally share
  one process-lifetime load (see `vst_loader_lock` / probe-leak comments):
  plugin toolkits carry process-global state that does not survive
  terminate/dlclose cycles inside a running host.
