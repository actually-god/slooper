# SLOOPER PATCHES — vendor/vst3-host (vendored from vst3-host 0.10.0)

Vendored verbatim from crates.io `vst3-host` 0.10.0, then patched in-tree
(same doctrine as `vendor/vst2`). Purpose: fix a silent, user-audible MIDI
drop affecting every raw-MIDI VST3 plugin (kanban card t_8d4e7e7a).

## Patch v1 — `src/internal/plugin_impl.rs`, `send_midi_event_at`

**Bug:** the arms for `ControlChange`, `PitchBend`, `ChannelAftertouch`, and
`ProgramChange` delivered ONLY through the mapped-parameter route
(`queue_mapped_midi_controller` → `send_midi_controller_at`), which queues a
parameter change for each IMidiMapping entry matching the controller. A
plugin that declares no mapping (typical: monosynths that parse raw bytes
from the input EventList — Nekobi, JS80P, ADLplug-class) got `Ok(())` and
NO event: the message never reached `input_events`, no error, no counter.
Hosts therefore receive "worked fine" return values while pitch bend, CC,
RPN0 (bend range), channel AT, and program change are all silently lost.
Measured on slooper's wire: ±1200¢ wheel into Nekobi/JS80P produced
byte-identical audio with and without bend.

**Fix:** keep the mapped route byte-identical when a mapping exists; when
the controller is unmapped, write the raw `kLegacyMIDICCOutEvent` form into
the same `input_events` list used by NoteOn/NoteOff/PolyAT
(`controlNumber` = the crate's/SDK's own controller constants: 128 channel
pressure, 129 pitch bend with LSB in `value` + MSB in `value2`, 130 program
change, else the CC number itself). The VST3 input EventList uses
kLegacyMIDICCOutEvent as the carrier for raw MIDI bytes host→plugin — this
is what raw-MIDI plugins parse (and what this crate itself already PARSES
in the plugin→host direction, `event_to_midi`, and what
`com_implementations::plugin_event_to_raw` already builds).

Deliberately NOT done: double-delivery for mapped controllers (a plugin
that maps keeps the parameter route ONLY — zero behavior change there),
and no relaxation of `validate_plugin_event`'s LegacyMidiCcOut rejection
(that blocks the `send_plugin_event` route; untouched to keep the diff
minimal — noted in the upstream issue).

Upstream: issue filed at https://github.com/HelgeSverre/rust-vst3-host
(v165 lane) with the same analysis. Re-sync by diffing against a fresh
0.10.x unpack before dropping this directory.
