# SLOOPER PATCH — vst3-host 0.10.0 (vendored)

Upstream: https://github.com/HelgeSverre/rust-vst3-host/ v0.10.0
Vendored: 2026-11-25, card t_8d4e7e7a (v165), for slooper.
Patched file: `src/internal/plugin_impl.rs` (search `SLOOPER PATCH` — 5 sites).

## Bug (upstream)

`send_midi_event_at` validated host→plugin deliveries of
`PitchBend`/`ControlChange`/`ProgramChange`/`ChannelAftertouch` against
"Legacy MIDI CC events are plugin output only" and dropped them. The
runner swallows the error, so hosted VST3 plugins silently never see
pitch bend, CC (incl. RPN0 bend-range announcements), channel
aftertouch, or program change — only note on/off, poly AT and params.
The raw `kLegacyMIDICCOutEvent` conversion machinery already existed on
the output side; only input validation blocked it.

## Patch (law: raw always; mapped params additionally when advertised)

All four arms now ALWAYS queue the raw legacy-MIDI event into
`input_events` (`controlNumber` 0..=127 = CC, 128 = channel AT,
129 = pitch bend (14-bit, `value`/`value2`), 130 = program change) —
exactly the VST3 `LegacyMidiData` contract — and *additionally* keep
the crate's IMidiMapping mapped-parameter route where a mapping exists.
Same-value double delivery is idempotent and matches mature hosts.

Why "always raw" is load-bearing, not belt-and-suspenders padding:
DPF-wrapped plugins (Nekobi, Vitalium, …) advertise IMidiMapping onto
DISPLAY/automation params whose own wrapper `appendEvent()` rejects
`kLegacyMIDICCOutEvent`-derived… rather: whose parameter decode path is
the plugin's own ABI; a mapping-gated route that *skips* raw delivery
silently drops bend for raw-consuming plugins, while a mapping-gated
route that *only* sends params silently drops it for DPF plugins whose
mapping ids don't land on consumable internal params. Observed on both
this host's DPF (Nekobi) and bespoke-wrapper (js80p) plugins.

## slooper-side re-sync checklist

- Keep the arm comments (they are the proof the fixture pins cite).
- `scripts/porting/v66_vst3_fixture.c` is the acceptance observer for
  this patch: its state stream exposes per-kind raw-event counts.
- Pins: `v165_vst3_*` in `src/vsthost.rs`; audio arbiter:
  `cargo run --release --example v165_bend_probe -- ~/.vst3/"Surge XT.vst3" V3`
  (pre_bend vs pre_bend_neg Hz must straddle center at 2^(2·cents/1200)).
- On rebase to a newer upstream, re-check the validator's
  `validate_midi_event` for LegacyMidiCcOut input policy first; if
  upstream fixed it properly, upstream the raw-always law as a feature
  request instead of patching.
