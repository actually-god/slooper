# Maintainer host notes — NOT user documentation

> This appendix records plugin installs, workarounds and audit verdicts
> verified on the maintainer's own machine (Arch/CachyOS, yabridge-bridged,
> extracted distro packages under `~/AudioFOSS`). Nothing here is user
> documentation: the paths, licenses, distro packages and hardware below are
> specific to that machine, and the facts age with it. User-facing hosting
> behavior lives in [PLUGINS.md](PLUGINS.md) — if a fact here describes what
> **slooper** does (not what this machine holds), it belongs there.
>
> Split out of PLUGINS.md by t_c17239f7. The generic guidance that grew out
> of these sagas (the v19.1 Wine bridge guard, the 32-bit `WINELOADER`
> recipe, legacy VST-API acceptance, the blank-name fallback) stayed in
> PLUGINS.md, worded machine-independently.

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

## Legacy 32-bit Windows VSTs: AudioNerdz **Delay Lama** (the singing monk)

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
   for one-off shells export it before launching slooper). NOTE: a session
   started BEFORE that file existed never sees it — `systemctl --user
   show-environment` then lacks WINELOADER and desktop launches still get
   the system wine. Fix live with `systemctl --user set-environment
   WINELOADER=$HOME/Tools/winege/bin/wine64` (or re-login).
2. **vendored vst2 patch #4**: the crate rejected every plugin reporting
   `GetApiVersion < 2400`, which bricks legacy VST 1.0/2.0 plugins. The gate
   now accepts any plausible version (see `vendor/vst2/VENDOR-NOTES.md`).

## Arturia Analog Lab V (Windows-only) — bridged on this host

Analog Lab Pro has no Linux build. On this machine it runs through
**yabridge + wine-ge** and appears in the synth picker as
`Analog Lab V` (`~/.vst/yabridge/Analog Lab V.so`, 64-bit VST2 bridge).

Recipe (the GUI installer's PureBasic stub deadlocks under wine — skip it):

1. Download the Windows installer from Arturia's downloads page and run it
   under winege just long enough that it unpacks `Analog Lab V Setup.exe`
   + `Setup-1/2.bin` into `Temp/pbt*` — then run
   `innoextract --extract --collisions=overwrite` on that trio.
2. Layout into the prefix: `commonappdata/Arturia/*` →
   `~/.wine/drive_c/ProgramData/Arturia/` (samples, presets, shared data —
   the plugin resolves them via `C:\ProgramData\Arturia`).
3. `winetricks --unattended vcrun2022` (the DLL imports
   `MSVCP140_CODECVT_IDS.dll`).
4. `yabridgectl add ~/AudioFOSS/arturia/win64 && yabridgectl sync`
   (flat directory so slooper's VST2 scan finds the wrapper).
5. `WINELOADER=~ Tools/winege/bin/wine64` must be in the host's environment
   (set via `~/.config/environment.d/yabridge-wine.conf`).

AL V is a synth **with an audio input** — the VST2 probe must read
`effFlagsSynth`, not "no inputs" (v31 fix). Sound stays silent until the
one-time Arturia login. Because the extract route skips the installer's
registry writes, AL V reports "Error 02: Failed to launch Arturia Software
Center client!" until you register the client path and start it first:

```
wine reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\Arturia Software Center.exe" /ve /t REG_SZ /d "C:\Program Files\Arturia Software Center\Arturia Software Center.exe" /f
wine "C:\Program Files\Arturia Software Center\ArturiaSoftwareCenterAgent.exe"   # background
wine "C:\Program Files\Arturia Software Center\Arturia Software Center.exe"     # login window (do the login HERE, not in AL V's modal)
```

then start `Analog Lab V.exe`. AL V's own modal email box crashed while
the client was missing; the ASC client renders its own proper login form.

**Accessibility bridge kill-switch (REQUIRED):** interacting with AL V's
login modal (JUCE a11y init) floods wine's UIA/MSAA stubs and then dies
with a stack-corruption SEH (`call_stack_handlers invalid frame`),
taking the yabridge host — and via chain-loader abort, slooper — with
it. Fix (shipped in `~/.config/environment.d/yabridge-wine.conf`):
`WINEDLLOVERRIDES="oleacc=disabled,uiautomationcore=disabled"`. After
the one-time login the plugin GUI shows no modal and the audio path
measures live (probe peak > 0 where pre-login was exact silence).

**License agent = keep it running.** The plugin mutes itself (exact
silence, no error) whenever `ArturiaSoftwareCenterAgent.exe` is not
alive — it is a license DAEMON, not installer cruft, and it auto-exits
when idle. Supervised here by the systemd user unit
`arturia-asc-agent.service` (`Restart=always`). Killing it silently
re-gates every running/loaded instance.

**Editor crash-guard ban cache:** a probing child that dies (e.g. the
login-modal SEH while the agent was dead) caches a `banned` verdict in
`Config::editor_probe` (keyed by path+mtime) — afterwards the strip
button opens the parameter-sliders window INSTEAD of the plugin GUI,
forever. After fixing a GUI crash, delete that path's key from
`~/.config/slooper/config.json` (app closed) so the next click re-probes.
With the agent up, AL V's editor opens cleanly under RT load (3/3 soak
`examples/alv_editor_live`).

MiniLab 37 hardware needs nothing: class-compliant MIDI, auto-linked to
`slooper:midi_in`. Firmware updates need MCC → Windows or a friend.

## Analog Lab V under yabridge: blank name, and the "ERROR 02 / login" dialog

**Blank name (fixed v32).** yabridge hands back an empty `AEffect->name` for
Analog Lab V when the Wine side has not finished loading at scan time, so the
synth picker showed a nameless row. `probe_vst2`/`probe_vst3` now fall back to
the file stem whenever the reported name is blank — yabridge names the wrapper
after the product, so the row reads "Analog Lab V".

**"ERROR 02: failed to load ASC" + occasional re-login.** This dialog is
Arturia's own copy, not slooper's: Analog Lab asks the Arturia Software Center
catalog (`~/.wine/drive_c/ProgramData/ArturiaSC/.../lic.ascd`) to prove the
login. The license state DOES persist across sessions (that file lives in the
persistent Wine prefix, not in slooper) — but the Arturia cloud token itself
expires on THEIR schedule, and a cold prefix can miss the catalog read during
plugin init. There is no host-side fix: re-login once inside the plugin GUI to
refresh `lic.ascd`. Speed-up that actually helps: after boot, open Analog Lab
once before a session (warms the 11 GB prefix page cache), and do not delete
`~/.wine` — it holds the activation.
