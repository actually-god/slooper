//! Audio engine + manager-thread core for slooper (DAW engine).
//!
//! Backend-neutral since WINDOWS-PORT W5: the OS device shims live in
//! `audio/jack.rs` (Linux, primary) and `audio/wasapi.rs` (Windows);
//! the block-level contract they implement is in `src/backend.rs`, the
//! manager-level one on [`manager_loop`] / [`ManagerBackend`].
//! Everything below — `Cmd`/`Evt`, `Backend`, `Engine`,
//! `Proc::process_block` — is shared by both.
//!
//! A dedicated non-realtime "audio manager" thread owns the `jack::Client`.
//! The GUI talks to it over an mpsc command channel and receives state
//! updates (port lists, MIDI events, finished takes, status) on a second
//! channel.
//!
//! Port layout: the client registers a fixed pool of input/output slots
//! (`in0..`, `out0..`) plus a MIDI input once at startup, and stays
//! activated for its whole lifetime. Applying a selection only connects /
//! disconnects external ports to/from the pool — it never deactivates the
//! client. (Deactivating is unsafe here: the jack crate's `deactivate` only
//! clears the thread-init/process callbacks, leaving notification callbacks
//! registered against handler memory that is then freed — a use-after-free
//! on PipeWire-JACK.) The realtime process handler reads two atomic
//! "connected" masks and mixes only enabled slots, so no locking or stale
//! port state ever enters the audio path.
//!
//! Playback model: ONE global transport clock drives the whole project. The
//! musical content is an [`Arrangement`] (tracks → lanes → clips) plus an
//! asset table, both shared as immutable Arc snapshots the GUI swaps in with
//! [`Cmd::SetProject`]; the realtime path walks the timeline and sums every
//! clip the playhead crosses. There are no per-track clocks and no overdub —
//! recording captures raw input into a take buffer and hands the finished
//! take back to the GUI (as an asset + clip placement) when stopped.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::Arc;

// OS backends (WINDOWS-PORT W5): exactly one compiles per OS.
#[cfg(not(windows))]
mod jack;
#[cfg(windows)]
mod wasapi;
// W12a: pure lost-stream recovery table (pinned on every OS; only the
// WASAPI backend reads it — the W7 `any(windows, test)` pattern keeps it
// compiling for the Linux pins without a dead module in Linux builds).
#[cfg(any(windows, test))]
mod rt_recovery;

use crate::arrange::Arrangement;

/// Fixed pool of mix slots; `Apply` maps the selected ports onto slots 0..n-1.
pub const MAX_SLOTS: usize = 16;

/// 1/sqrt(2) — the equal-power centre gain and the mono-downmix scale.
const F1S2: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Equal-power pan gains `(left, right)` for a position in [0, 1]
/// (0 = hard left, 0.5 = centre, 1 = hard right). At centre both are
/// 1/sqrt(2), so the mono downmix `(l + r)/sqrt(2)` of a centred source
/// is exactly the source — a single (or L+R-routed) output reproduces the
/// old mono mix unchanged at the default pan.
pub fn pan_gains(p: f32) -> (f32, f32) {
    let th = p.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    (th.cos(), th.sin())
}

/// Audition voice list: (voice id, asset id, samples). Starts are additive,
/// so the list is the whole audition state the RT thread mixes.
pub type PreviewVoices = Arc<Vec<(u64, u32, Arc<[f32]>)>>;

/// Number of simultaneous tracks.
/// Track capacity (v0.10: 16 → 128). Fixed-size RT arrays scale with this;
/// the per-sample mix path iterates a compact ACTIVE-track list so an empty
/// 128-track project costs the same CPU as a 16-track one did before.
pub const MAX_LOOPS: usize = 128;

/// Preview voice id for auditions that belong to no rack asset (piano-roll
/// note taps, edit echoes). Stopping by this id never touches asset previews.
pub const PREVIEW_TRANSIENT: u32 = u32::MAX;
/// Per-track recording capacity: ~60 s @ 48 kHz (~30 s @ 96 kHz).
pub const LOOP_CAP: u32 = 2_880_000;

/// v35 R2: how far PAST the project end the playhead may be placed
/// (seek/slack). The grid paints a short tail of empty measures; this cap
/// simply keeps `pos` a sane sample count.
pub const SEEK_SLACK: u64 = 64 * 48_000;

// Per-track recording states (AtomicU8 in the engine).
pub const TR_IDLE: u8 = 0;
pub const TR_REC: u8 = 1;

/// MIDI value at which the per-track gain slider sits at unity (0 dB): below
/// it cuts toward -inf, above it boosts toward [`GAIN_MAX_DB`].
pub const GAIN_UNITY_MIDI: u8 = 80;
/// Maximum boost of the per-track gain slider (MIDI 127), in dB.
pub const GAIN_MAX_DB: f32 = 12.0;
/// Linear amplitude of [`GAIN_MAX_DB`] (10^(12/20)).
const GAIN_MAX_LIN: f32 = 1.584_893_2;

/// Seek sentinel in the engine's seek slot: "no seek pending".
pub const NO_SEEK: u64 = u64::MAX;

/// Per-track gain trim: MIDI 0..=127 → linear amplitude (0 = silence / -inf,
/// [`GAIN_UNITY_MIDI`] = unity / 0 dB, 127 = +12 dB).
#[inline]
pub fn gain_amp(v: u8) -> f32 {
    if v <= GAIN_UNITY_MIDI {
        v as f32 / GAIN_UNITY_MIDI as f32
    } else {
        1.0 + (v - GAIN_UNITY_MIDI) as f32 / (127 - GAIN_UNITY_MIDI) as f32 * (GAIN_MAX_LIN - 1.0)
    }
}

/// A remote audio port the user can select (full name is `client:port`).
#[derive(Clone, Debug, PartialEq)]
pub struct PortInfo {
    pub name: String,
    pub client: String,
}

/// One slot of a track's chain: either a scripted effect or a hosted VST
/// plugin. Both kinds live in the *same* ordered chain, so a plugin can sit
/// anywhere in the effects order (FL Studio's mixer-slot model).
pub enum FxKind {
    /// Bundled/JS-compiled effect script.
    Scripted {
        inst: Arc<crate::fx::Instance>,
        desc: crate::fx::EffectDesc,
    },
    /// In-process hosted plugin (VST2/VST3/CLAP), loaded by `vsthost`
    /// (CLAP's backend is `claphost`, delivered behind the same
    /// `StageEntry` surface).
    Vst(Arc<dyn crate::vsthost::StageEntry>),
}

/// A chain slot; see [`FxKind`].
pub struct FxSlot {
    pub kind: FxKind,
}

impl FxSlot {
    pub fn scripted(inst: Arc<crate::fx::Instance>, desc: crate::fx::EffectDesc) -> Self {
        Self {
            kind: FxKind::Scripted { inst, desc },
        }
    }
    pub fn vst(entry: Arc<dyn crate::vsthost::StageEntry>) -> Self {
        Self {
            kind: FxKind::Vst(entry),
        }
    }
    pub fn is_vst(&self) -> bool {
        matches!(self.kind, FxKind::Vst(_))
    }
    /// Chain-list label (window titles, chips).
    pub fn label(&self) -> String {
        match &self.kind {
            FxKind::Scripted { desc, .. } => desc.name.clone(),
            FxKind::Vst(e) => e.name().to_string(),
        }
    }
}

/// A track's live effect chain (GUI order = audio order). Replaced whole
/// when the GUI edits it; the RT thread only reads slots, and slider moves
/// ride on the instance's atomics — no chain rebuild, no locks per sample.
#[derive(Clone, Default)]
pub struct TrackFx {
    pub slots: Arc<Vec<FxSlot>>,
}

impl TrackFx {
    pub fn slots(&self) -> &[FxSlot] {
        &self.slots
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// True when any slot hosts a plugin: the track's chain must run
    /// block-wise (plugins consume whole buffers), not per sample.
    pub fn has_vst(&self) -> bool {
        self.slots.iter().any(|s| s.is_vst())
    }
}

/// A track's sound source for MIDI clips. Hosted plugins are first-class
/// synthesizers (VSTi), not just effects, so the synth table holds both
/// kinds; every consumer (RT render, offline render, sliders, panic) goes
/// through this enum.
pub enum SynthHandle {
    Script(Arc<crate::synth::Instance>),
    Vst(Arc<dyn crate::vsthost::StageEntry>),
}

impl SynthHandle {
    /// v42: true when the hosted plugin behind this handle threw a C++
    /// exception (dead bridge) and is parked/bypassed.
    pub fn is_dead(&self) -> bool {
        match self {
            SynthHandle::Vst(e) => e.is_dead(),
            SynthHandle::Script(_) => false,
        }
    }
    /// Silence every ringing voice (transport seek/stop).
    pub fn all_off(&self) {
        match self {
            SynthHandle::Script(i) => i.all_off(),
            // A plugin's already-produced samples live in its staging
            // ring: drop them FIRST, then stop the notes — the all-notes-
            // off messages ride in `pending_midi`, and flushing after
            // would drop them too, leaving plugin voices ringing forever.
            SynthHandle::Vst(e) => {
                e.flush();
                e.all_notes_off();
            }
        }
    }
}

/// Raw MIDI message copied out of the realtime thread.
#[derive(Debug)]
pub struct RawMidiMsg {
    pub bytes: Vec<u8>,
}

/// The realtime mirror of "this message is CONTROL TRAFFIC": every bound
/// action binding (Settings ▸ MIDI) and every project value binding. The
/// rule the MIDI recorder has always used — bound messages drive the app,
/// they are never recorded — now also gates the LIVE synth feed (v32):
/// a bound NOTE/CC fires its binding but does not make noise.
///
/// Encodings (kept tiny so the RT thread scans them cache-cheaply):
/// * `exact` — one bound switch: (type 0=NOTE 1=CC 2=PC, channel, d1).
/// * `axes`  — one continuous (knob) binding: it owns the WHOLE type +
///   channel axis, exactly like the binding matcher, so a controller whose
///   CC numbers drift mid-sweep still owns its messages.
#[derive(Clone, Default, Debug)]
pub struct BoundSet {
    pub exact: Vec<u32>,
    pub axes: Vec<u16>,
}

/// Pack an exact binding identity: `(ty << 16) | (ch << 8) | d1`.
pub fn bind_key(ty: u8, ch: u8, d1: u8) -> u32 {
    ((ty as u32) << 16) | ((ch as u32) << 8) | d1 as u32
}

/// Pack an axis identity: `(ty << 8) | ch`.
pub fn axis_key(ty: u8, ch: u8) -> u16 {
    ((ty as u16) << 8) | ch as u16
}

impl BoundSet {
    /// True when a LIVE message is claimed by a binding. `kind` uses the
    /// pattern-event encoding (0 note-off, 1 CC, 2 chan-press, 3 note-on).
    /// NOTE-OFF is never claimed: its note-on went through the same
    /// decision, and dropping the matching key-up is the classic stuck-note
    /// generator when a binding changes while a key is held.
    pub fn claims(&self, kind: u8, ch: u8, d1: u8) -> bool {
        match kind {
            3 => self.exact.contains(&bind_key(0, ch, d1)) || self.axes.contains(&axis_key(0, ch)),
            1 => self.exact.contains(&bind_key(1, ch, d1)) || self.axes.contains(&axis_key(1, ch)),
            _ => false,
        }
    }
}

/// Parse the LIVE-feed subset of a channel voice message into the pattern
/// event encoding (0 note-off, 1 CC, 2 chan-press, 3 note-on) as
/// `(kind, ch, d1, d2)`. Program change, pitch bend, poly aftertouch and
/// system messages return `None` — the synth event vocabulary has no kind
/// for them (pitch bend needs an ABI addition; tracked for later).
/// Allocation-free: safe on the realtime thread.
pub fn live_event_kind(bytes: &[u8]) -> Option<(u8, u8, u8, u8)> {
    let b0 = *bytes.first()?;
    // Channel-voice status bytes only (0x80..0xF0); drop system messages
    // (0xF0..=0xFF) and non-status bytes (<0x80, invalid without running
    // status, which JACK's reader already resolves).
    if !(0x80..0xF0).contains(&b0) {
        return None;
    }
    let ch = b0 & 0x0F;
    match b0 >> 4 {
        0x9 => {
            let n = *bytes.get(1)?;
            let v = bytes.get(2).copied().unwrap_or(0);
            if v == 0 {
                Some((0, ch, n, 0))
            } else {
                Some((3, ch, n, v))
            }
        }
        0x8 => Some((0, ch, *bytes.get(1)?, 0)),
        0xB => Some((1, ch, *bytes.get(1)?, *bytes.get(2)?)),
        0xD => Some((2, ch, 0, *bytes.get(1)?)),
        // v42/MPE: per-note aftertouch and (per-channel) pitch bend. The
        // MPE engine toggle gates these two at the port; classic setups
        // never see them, expressive controllers always send them.
        0xA => Some((4, ch, *bytes.get(1)?, *bytes.get(2)?)),
        0xE => Some((5, ch, *bytes.get(1)?, *bytes.get(2)?)),
        _ => None,
    }
}

/// A selected output port paired with its stereo routing: bit 0 = left
/// bus, bit 1 = right bus (3 = mono downmix, the default).
pub type OutSel = (String, u8);

/// Immutable asset snapshot: sample buffers indexed by asset id, swapped in
/// as one Arc whenever the rack changes. Clip lengths are the asset lengths.
pub type AssetTable = Vec<Arc<[f32]>>;

/// One MIDI-ish event as the RT queues and audition voices carry it:
/// `(sample_offset, status_hi, channel, note, velocity)`.
pub type MidiEv = (u64, u8, u8, u8, u8);

/// Commands from the GUI to the audio manager thread.
/// Scratchpad for one control-rate automation pass (v0.11 R5). `None`
/// means "no clip sounds on this track for this target" — the manual
/// control keeps its value. Plugin-parameter targets do not appear here:
/// they are written straight onto the live instances.
#[derive(Clone, Copy)]
pub struct AutoOut {
    vol: [Option<f32>; MAX_LOOPS],
    gain: [Option<f32>; MAX_LOOPS],
    pan: [Option<f32>; MAX_LOOPS],
    muted: [bool; MAX_LOOPS],
    master_vol: Option<f32>,
    tempo: Option<u32>,
}

impl AutoOut {
    fn new() -> AutoOut {
        AutoOut {
            vol: core::array::from_fn(|_| None),
            gain: core::array::from_fn(|_| None),
            pan: core::array::from_fn(|_| None),
            muted: [false; MAX_LOOPS],
            master_vol: None,
            tempo: None,
        }
    }
}

impl Default for AutoOut {
    fn default() -> AutoOut {
        AutoOut::new()
    }
}

pub enum Cmd {
    /// Re-enumerate remote audio ports and send them back.
    Refresh,
    /// Connect the selected external ports to mix slots 0..n-1 (disconnecting
    /// whatever was there before). Empty lists stop all audio; MIDI monitoring
    /// keeps running. Selections beyond `MAX_SLOTS` are ignored (with an error).
    /// Each output carries its stereo routing as bits: 1 = left bus,
    /// 2 = right bus, 3 = mono downmix `(L + R)/sqrt2` (the default, and the
    /// bit-exact old mono behaviour for centred mixes). A port routed to both
    /// gets the downmix, never two conflicting writes.
    Apply {
        ins: Vec<String>,
        outs: Vec<OutSel>,
    },
    /// Start/stop a raw input capture on track `i`. Stopping sends the take
    /// back as [`Evt::Take`] (armed at the playhead position it started in);
    /// the GUI turns it into an asset + clip. Takes never touch the engine's
    /// playback state.
    ToggleRecord {
        i: usize,
    },
    /// Set / toggle a track's play/stop: while the transport runs, a *stopped*
    /// track contributes nothing (its clips stay on the timeline untouched).
    SetTrackOn {
        i: usize,
        on: bool,
    },
    ToggleTrackOn {
        i: usize,
    },
    /// Start the global transport from the current playhead position.
    PlayAll,
    /// Stop the global transport; the playhead holds its position.
    StopAll,
    /// Move the playhead (samples). Scrubbing sends these continuously.
    Seek {
        pos: u64,
    },
    /// Timeline loop toggle: wrap the playhead at the loop region's end vs.
    /// park at the project end.
    SetLoop(bool),
    /// The active loop region in samples: wrap from `start` to `end`. The
    /// GUI computes `end` from the loop mode (area handles, current-longest
    /// track content, or whole-song end) on every edit; `end = 0` means
    /// Master switch for live input monitoring (Settings checkbox, default
    /// on). Off mutes the live pass-through of the selected track's chain
    /// only — levels, recording, playback and the meter keep working.
    SetMonitor(bool),
    /// "wrap at the project end". Only consulted while [`Cmd::SetLoop`] is
    /// on — with the loop off the transport still parks at the project end.
    SetLoopRegion {
        start: u64,
        end: u64,
    },
    /// Replace the engine's project snapshot (assets + arrangement + total
    /// length). The RT handler picks the new Arcs up on the next cycle.
    SetProject {
        assets: Arc<AssetTable>,
        arr: Arc<Arrangement>,
        len: u64,
    },
    /// Replace the MIDI pattern table (indexed in parallel with `assets`).
    SetMidiAssets(Arc<Vec<Option<Arc<crate::midipat::Pattern>>>>),
    /// v0.11 R5: automation asset table (parallel to assets).
    SetAutoAssets(Arc<Vec<Option<Arc<crate::automation::Automation>>>>),
    /// Bind/unbind one track's synthesizer (scripted synth or VSTi).
    SetTrackSynth {
        i: usize,
        synth: Option<Arc<SynthHandle>>,
    },
    /// Set the fallback synth (bundled piano) used by tracks with no
    /// explicit binding.
    SetDefaultSynth(Option<Arc<SynthHandle>>),
    /// Write a track's rendered contribution (all its lanes, engine-rate
    /// mixdown of the current project) to `path` in format `fmt`. Runs on
    /// the manager thread; a Notice/Error event reports the outcome.
    ExportTrack {
        i: usize,
        path: std::path::PathBuf,
        fmt: crate::audio_io::ExportFormat,
    },
    /// Render and write the whole project (every sounding track, all lanes,
    /// mixer trims, pans and effects) to `path`.
    ExportProject {
        path: std::path::PathBuf,
        fmt: crate::audio_io::ExportFormat,
    },
    /// Render and write ONLY the listed clips — `(track, lane, clip index)`
    /// into the current arrangement snapshot — time-aligned so the first
    /// clip starts at sample 0. Selected tracks need not be sounding.
    ExportSelection {
        path: std::path::PathBuf,
        fmt: crate::audio_io::ExportFormat,
        items: Vec<(usize, usize, usize)>,
    },
    /// The GUI replaced track `i`'s effect chain (order = audio order). The
    /// slots hold refcounted effect instances created on the GUI thread; the
    /// audio thread picks the new table up on the next cycle without locking.
    SetTrackEffects {
        i: usize,
        chain: TrackFx,
    },
    /// The GUI selection moved to track `i` (0-based). Input monitoring
    /// follows the selected track: its gain, volume, effect chain and pan
    /// shape the live pass-through.
    SetSelected(usize),
    /// Latency compensation: every fresh take discards its first `ms`
    /// milliseconds of input, trimming the silence gap between pressing the
    /// record button and that button click reaching the mic. Monitoring and
    /// the output mix are never delayed. Clamped to 0..=1000 ms.
    SetLatency {
        ms: f32,
    },
    /// Metronome: on/off, tempo in BPM, click subdivision in clicks per beat
    /// (1 = quarter, 2 = eighth, 3 = triplet, 4 = sixteenth, ...), and beats
    /// per measure (the time-signature numerator; beat 1 of every measure
    /// gets the downbeat tone). Three distinct tones: downbeat, quarter note
    /// and subdivision. Clicks land on the *project timeline grid*: while the
    /// transport runs they tick at the transport tempo, and scrubbing the
    /// playhead ticks the grid under it. With the transport stopped the
    /// click free-runs from the playhead position — it never drives the
    /// playhead itself. Never recorded into a take.
    SetMetro {
        on: bool,
        bpm: u32,
        sub: u32,
        num: u32,
    },
    /// Audition a sample at the master output (asset-preview click). The
    /// buffer plays once from its start through the output mix only (never
    /// recorded, never loops). v40: a new preview REPLACES whatever was
    /// playing — the old v0.10 additive stacking turned rack browsing into
    /// a wall of up to 12 overlapping voices. `id` tags the source asset
    /// so a rack row re-click can stop just its own voice.
    PreviewPlay {
        id: u32,
        samples: Arc<[f32]>,
    },
    /// Stop audition voices. `None` stops everything (transport stop);
    /// `Some(id)` stops only the voices started from that asset (the rack
    /// row toggle).
    PreviewStop {
        id: Option<u32>,
    },
    /// W9 (Windows): open exactly these MIDI input devices (labels as
    /// reported by `Evt::MidiInputs`), closing anything else slooper had
    /// open. On Linux this is a deliberate no-op — JACK MIDI auto-connects
    /// every remote port (`jack::rescan_midi_sources`) and there is nothing
    /// to select.
    SetMidiInputs(Vec<String>),
    Quit,
}

/// Events from the audio manager thread to the GUI.
#[derive(Debug)]
pub enum Evt {
    /// No JACK/PipeWire-JACK server available.
    Unavailable(String),
    Ready {
        client_name: String,
        sample_rate: u32,
    },
    Ports {
        ins: Vec<PortInfo>,
        outs: Vec<PortInfo>,
    },
    Applied {
        ins: usize,
        outs: usize,
    },
    /// Transport state changed (the engine auto-stops at the project end
    /// when the loop toggle is off; the GUI mirrors the flag).
    Transport {
        on: bool,
    },
    /// A recording take finished: `at` is the playhead position when
    /// recording started, `data` the captured mono samples. The GUI stores
    /// it as an asset and places a clip; the engine keeps nothing.
    /// v42: `preview` marks a "p" take — captured from the track bus while
    /// the transport played; stored under a "preview" asset name.
    Take {
        track: usize,
        at: u64,
        data: Arc<[f32]>,
        preview: bool,
    },
    /// A remote MIDI source ports currently wired into `slooper:midi_in`
    /// (auto-connected; refreshed on port/client registration events).
    Midi(RawMidiMsg),
    MidiSources(Vec<String>),
    /// W9 (Windows): the selectable WinMM MIDI input devices (deduplicated
    /// labels — this exact list is what `Cmd::SetMidiInputs` accepts back).
    /// Never sent on Linux; the JACK path reports `Evt::MidiSources`.
    MidiInputs(Vec<String>),
    /// A non-error status message worth showing in the top bar (e.g. a
    /// finished export).
    Notice(String),
    Xrun,
    Error(String),
}

/// Spawn point of the manager thread — picks the live OS backend
/// (WINDOWS-PORT W5). Everything past this call is backend-side.
#[cfg(not(windows))]
#[allow(clippy::too_many_arguments)] // backend wiring context, passed whole (W5 seam)
fn run_backend(
    cmd_rx: Receiver<Cmd>,
    evt_tx: SyncSender<Evt>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    meter: Arc<crate::meter::MeterRing>,
    xruns: Arc<AtomicU64>,
    midi_drops: Arc<AtomicU64>,
    engine: Arc<Engine>,
) {
    jack::run(cmd_rx, evt_tx, gain, peak, meter, xruns, midi_drops, engine)
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)] // twin of the unix spawn point above
fn run_backend(
    cmd_rx: Receiver<Cmd>,
    evt_tx: SyncSender<Evt>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    meter: Arc<crate::meter::MeterRing>,
    xruns: Arc<AtomicU64>,
    midi_drops: Arc<AtomicU64>,
    engine: Arc<Engine>,
) {
    wasapi::run(cmd_rx, evt_tx, gain, peak, meter, xruns, midi_drops, engine)
}

/// What `manager_loop` asks of the live backend device (WINDOWS-PORT W5).
/// The loop body is OS-neutral; these four methods are the entire per-OS
/// surface. Implementations: `jack::JackSession`, `wasapi::WasapiSession`.
trait ManagerBackend {
    /// Re-publish the selectable port/device list (`Cmd::Refresh`).
    fn refresh(&mut self, tx: &SyncSender<Evt>);
    /// Map the GUI-selected patch onto slots / devices (`Cmd::Apply`).
    fn apply(&mut self, ins: &[String], outs: &[(String, u8)], tx: &SyncSender<Evt>);
    /// Periodic tick (~250 ms idle): hot-plug scans etc.
    fn idle(&mut self, tx: &SyncSender<Evt>);
    /// Tear the live device down after the loop exits (Quit/Disconnect).
    fn teardown(&mut self);
    /// W9: open exactly these WinMM MIDI devices. The JACK backend never
    /// overrides this — auto-connected JACK MIDI is the Linux contract.
    fn set_midi_inputs(&mut self, _sel: &[String], _tx: &SyncSender<Evt>) {}
}

/// Handle kept by the GUI.
pub struct Backend {
    pub cmd_tx: Sender<Cmd>,
    pub evt_rx: Receiver<Evt>,
    /// Master output level (f32 as raw bits) — written by GUI, read in RT thread.
    pub gain: Arc<AtomicU32>,
    /// Peak of the mixed output during the last cycle (f32 bits).
    pub peak: Arc<AtomicU32>,
    /// v39: post-master-gain stereo samples for the MASTER OUT meter panel
    /// (lock-free SPSC ring; RT writes, the GUI drains once per frame).
    pub meter: Arc<crate::meter::MeterRing>,
    /// Total xruns since start.
    pub xruns: Arc<AtomicU64>,
    /// MIDI events dropped because the GUI event queue was full.
    pub midi_drops: Arc<AtomicU64>,
    /// Shared engine (GUI reads atomics for transport display).
    pub engine: Arc<Engine>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Backend {
    /// Spawn the audio manager thread and try to open a JACK client with NO_START.
    pub fn start() -> Backend {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        // Bounded: the realtime thread uses try_send and drops events if the GUI lags.
        let (evt_tx, evt_rx) = mpsc::sync_channel::<Evt>(256);
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let peak = Arc::new(AtomicU32::new(0));
        let meter = Arc::new(crate::meter::MeterRing::new());
        let xruns = Arc::new(AtomicU64::new(0));
        let midi_drops = Arc::new(AtomicU64::new(0));
        let engine = Engine::new();
        let (t_gain, t_peak, t_meter, t_xruns, t_midi_drops, t_engine) = (
            gain.clone(),
            peak.clone(),
            meter.clone(),
            xruns.clone(),
            midi_drops.clone(),
            engine.clone(),
        );

        let thread = std::thread::Builder::new()
            .name("slooper-audio".into())
            .spawn(move || {
                run_backend(
                    cmd_rx,
                    evt_tx,
                    t_gain,
                    t_peak,
                    t_meter,
                    t_xruns,
                    t_midi_drops,
                    t_engine,
                );
            })
            .expect("failed to spawn audio thread");

        Backend {
            cmd_tx,
            evt_rx,
            gain,
            peak,
            meter,
            xruns,
            midi_drops,
            engine,
            thread: Some(thread),
        }
    }

    /// Send a command, ignoring send errors (thread already gone).
    pub fn cmd(&self, c: Cmd) {
        let _ = self.cmd_tx.send(c);
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Ask the audio thread to exit and wait for it. Joining (bounded) keeps
        // JACK client teardown off the process-exit path, where it would race
        // PipeWire's event loop and abort.
        self.cmd(Cmd::Quit);
        if let Some(handle) = self.thread.take() {
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = handle.join();
                let _ = done_tx.send(());
            });
            // The thread exits on Quit; bound the wait so a stuck thread can't
            // hang shutdown forever.
            let _ = done_rx.recv_timeout(std::time::Duration::from_secs(2));
        }
    }
}

/// The transport engine: recording buffers, atomic transport/mixer state,
/// and immutable Arc snapshots of the project (assets + arrangement), shared
/// between the manager thread (commands, take extraction, renders) and the
/// realtime handler (timeline mixing, capture).
///
/// Concurrency discipline:
/// - The audio thread is the only writer of take lengths while recording
///   (`fetch_add` discipline as before) and of the published playhead.
/// - Project snapshots are swapped wholesale as Arcs (try_lock in RT, never
///   blocks); a swap can never tear a clip mid-sample.
/// - The manager mutates nothing the RT path reads except atomics and the
///   quiesced take buffers (stop → one 20 ms sleep → read).
pub struct Engine {
    /// Per-track raw capture buffers (one take in flight per track).
    rec_bufs: [*mut f32; MAX_LOOPS],
    /// v34 R11: the last control-rate automation pass, published for the
    /// GUI (vol/gain/pan in AMPLITUDE space, pan in -1..1, mute as 0/1;
    /// NAN = "no automation driving this"). The mixer paints its faders
    /// from these while a target is automated, so the controls visibly
    /// move with the curve instead of lying still.
    pub auto_disp: [[AtomicU32; MAX_LOOPS]; 3],
    rec_len: [AtomicU32; MAX_LOOPS],
    rec_state: [AtomicU8; MAX_LOOPS],
    /// Playhead position (samples) when the current take started — reported
    /// back with the finished take so the GUI places the clip there.
    rec_at: [AtomicU64; MAX_LOOPS],
    /// v42 "p": preview-take arming — a take armed with this flag captures
    /// the TRACK BUS (post-FX, post-trim, mono fold: exactly what the track
    /// contributes to the master) instead of the raw input, and follows the
    /// transport: capture runs only while the transport plays, and the take
    /// commits itself when the transport stops. Armed via
    /// `set_rec_preview`, snapshotted by `start_rec`, cleared by `copy_take`
    /// (which reports it to the manager so the asset is named "preview").
    rec_preview: [AtomicBool; MAX_LOOPS],
    rec_pv_active: [AtomicBool; MAX_LOOPS],
    /// v42 loop record: armed for the next take / live for the current one.
    rec_loop: [AtomicBool; MAX_LOOPS],
    rec_loop_active: [AtomicBool; MAX_LOOPS],
    /// Latency compensation in samples (0 = off): every fresh take discards
    /// its first `latency` input samples (armed in `start_rec`, consumed in
    /// `rec_sample`) so the click of pressing the record button — caught a
    /// few ms late by the mic — becomes the take's FIRST sample instead of
    /// landing a few ms into it. Monitoring and the output mix are never
    /// delayed.
    latency: AtomicU32,
    /// Per-track count of leading input samples still to discard for the
    /// current take; armed from `latency` by `start_rec`.
    rec_skip: [AtomicU32; MAX_LOOPS],
    /// Bitmask of tracks whose take the RT handler auto-stopped at buffer
    /// capacity. The manager polls + clears it (it owns the event channel)
    /// and ships those takes to the GUI like any other finished take.
    take_pending: [AtomicU64; 2],

    // ---- global transport ------------------------------------------------
    /// 1 = running (timeline advances), 0 = stopped (playhead holds).
    transport_on: AtomicU8,
    /// 1 = wrap the playhead at the project end, 0 = park at the end.
    loop_tl: AtomicU8,
    /// Loop region (samples): wrap from `loop_start` back to it at
    /// `loop_end`; `loop_end = 0` means "the project end". Set by the GUI
    /// from the loop mode (`Cmd::SetLoopRegion`).
    loop_start: AtomicU64,
    loop_end: AtomicU64,
    /// Playhead position in timeline samples (published by RT, seeks written
    /// by the manager through `seek`).
    pos: AtomicU64,
    /// v42/MPE: when set, pitch-bend (0xEx) and polyphonic aftertouch
    /// (0xAx) from the input port reach track instruments (and pattern
    /// playback feeds them too). Off = the classic behavior where those
    /// messages never left the recorder.
    mpe: AtomicBool,
    /// v37 R5: any recorder is armed (audio take / MIDI take / write
    /// automation) — the transport must NOT park or wrap at the project
    /// end while a take runs. Truncating the take at the content length is
    /// what made "record one measure, get one measure" and cut automation
    /// takes short; the recorded window is the take, not the arrangement.
    recording: AtomicBool,
    /// Pending seek target; `NO_SEEK` when none. The RT handler swaps this at
    /// block start, which makes seeks race-free against RT position writes.
    seek: AtomicU64,
    /// Total project length in samples (>= 1 bar; the GUI guarantees it).
    proj_len: AtomicU64,

    // ---- mixer -----------------------------------------------------------
    /// Per-track play/stop flag (mirror — the GUI is the source of truth;
    /// MIDI/keybinds fire through the GUI too).
    track_on: [AtomicU8; MAX_LOOPS],
    /// Per-track playback volume (linear amplitude as f32 bits; 1.0 = 0 dB).
    /// Applied to each clip's playback contribution only — recorded takes
    /// always stay at raw input level.
    vol: [AtomicU32; MAX_LOOPS],
    /// Per-track gain trim (linear amplitude as f32 bits; 1.0 = 0 dB).
    trk_gain: [AtomicU32; MAX_LOOPS],
    /// v32 R5: sidechain SEND mask (bit k = source track, u128 across
    /// `MAX_LOOPS` split into an atomic lo/hi pair — there is no AtomicU128).
    /// The self-bit is always cleared: a track never keys itself.
    sc_send_lo: [AtomicU64; MAX_LOOPS],
    sc_send_hi: [AtomicU64; MAX_LOOPS],
    /// Per-track stereo pan position as f32 in [0, 1] (0 = hard left,
    /// 0.5 = centre). Equal-power split between the L and R mix buses.
    pan: [AtomicU32; MAX_LOOPS],
    /// GUI-selected track (0-based). Live input monitoring rides this
    /// track's gain x volume, effect chain and pan (see `Proc::process`).
    selected: AtomicU8,
    /// Live input monitoring master switch (Settings checkbox; default on).
    /// Off mutes the pass-through only — levels, recording and playback
    /// are untouched. Gate on the RT thread via `monitor_amp`.
    monitor_on: AtomicBool,
    /// v32: live MIDI input feeds the synthesizers (Settings checkbox;
    /// default on). Off means incoming MIDI drives bindings, the recorder
    /// and the monitor ONLY — no live keyboard sound.
    midi_live: AtomicBool,
    /// v32: tracks armed for MIDI pattern recording. Live notes must sound
    /// on these too (the player must hear the take they are laying down),
    /// in addition to the selected track. Set from the UI thread as arming
    /// flips; read per-block on the RT thread.
    midi_rec_arm: [AtomicBool; MAX_LOOPS],
    /// v32: live mirror of the bound-message set (see [`BoundSet`]); the RT
    /// thread refreshes an Arc clone when the generation moves (try_lock —
    /// never blocks), same pattern as the other project snapshots.
    midi_bound: std::sync::Mutex<Arc<BoundSet>>,
    midi_bound_gen: AtomicU64,
    /// Per-track live effect chains. The manager thread swaps the whole
    /// table on `SetTrackEffects`; the RT handler refreshes a cached Arc
    /// clone when `fx_gen` changes (try_lock — never blocks).
    fx: std::sync::Mutex<Arc<[TrackFx; MAX_LOOPS]>>,
    fx_gen: AtomicU64,

    // ---- project snapshots -----------------------------------------------
    assets: std::sync::Mutex<Arc<AssetTable>>,
    assets_gen: AtomicU64,
    arr: std::sync::Mutex<Arc<Arrangement>>,
    arr_gen: AtomicU64,
    // ---- MIDI snapshots ---------------------------------------------------
    /// MIDI pattern table, indexed in parallel with `assets` (audio slots
    /// hold `None`); pushed together with the project by the GUI.
    midi: std::sync::Mutex<Arc<Vec<Option<Arc<crate::midipat::Pattern>>>>>,
    midi_gen: AtomicU64,
    /// v0.11 R5: automation asset table (parallel to `assets`) + gen.
    auto: std::sync::Mutex<Arc<Vec<Option<Arc<crate::automation::Automation>>>>>,
    auto_gen: AtomicU64,
    /// Per-track synth bindings (scripted or VSTi) + the fallback instance
    /// (bundled piano) used by unbound tracks.
    synths: std::sync::Mutex<Arc<[Option<Arc<SynthHandle>>; MAX_LOOPS]>>,
    synths_gen: AtomicU64,
    /// v46: per-track mono/poly synth mode. The copy table IS the flag:
    /// an empty row = monophonic (every child lane merges into the track's
    /// one synth, the historic behaviour); a non-empty row = polyphonic —
    /// child lane `li` renders through copy `li` (lanes past the row share
    /// copy 0), and the copies' outputs sum before the SAME track chain.
    /// Spawned on the UI thread (`spawn_synth_copy`), published gen-swap.
    poly_voices: std::sync::Mutex<PolyTable>,
    poly_gen: AtomicU64,
    track_poly: [AtomicU8; MAX_LOOPS],
    default_synth: std::sync::Mutex<Option<Arc<SynthHandle>>>,
    /// Chain reaper: a chain swap displaces VST slots; the displaced entries
    /// are *held* in `vst_held` instead of dropping where the swap happened:
    /// plugin destructors (and the VST3 COM teardown the runner hands off)
    /// must run on the creating (UI) thread. The UI drains held entries once
    /// the audio chain cache has caught up (`vst_gen` == `vst_refreshed`).
    vsts_gen: AtomicU64,
    vst_refreshed: AtomicU64,
    vst_held: std::sync::Mutex<Vec<Arc<dyn crate::vsthost::StageEntry>>>,
    /// Jack rate, published once at connect (drives tick math + samplers).
    sample_rate: AtomicU32,

    // ---- metronome ---------------------------------------------------------
    /// Metronome: 0 = off, 1 = on; tempo (BPM); clicks per beat; beats per
    /// measure (time-signature numerator, default 4).
    metro_on: AtomicU8,
    metro_bpm: AtomicU32,
    metro_sub: AtomicU32,
    metro_num: AtomicU32,

    // ---- asset preview -----------------------------------------------------
    /// Audition voice pool (v40: SINGLE voice — each voice is
    /// (voice id, asset id, buffer); a new start REPLACES whatever was
    /// playing, see `play_preview`). The old v0.10 additive pool turned
    /// rack browsing into a wall of up to 12 stacked, never-retired
    /// voices; previews now always mean "audition THIS, stop everything
    /// else", capped at min(preview_secs, sample length) by the UI.
    preview_voices: std::sync::Mutex<PreviewVoices>,
    preview_vid: AtomicU64,
    preview_gen: AtomicU64,
    /// 1 = a preview is playing (RT clears it when the buffer runs out).
    preview_on: AtomicU8,
    /// v40: set by the RT thread when the pool drains naturally (the voice
    /// ran out of samples). The UI retires the dead pool on the next frame
    /// (`take_preview_drained`) so stale Arc buffers stop pinning memory
    /// and a freshly built RT handler can never reseed a dead voice at
    /// cursor 0 (the "preview plays forever / replays on its own" report).
    preview_drained: AtomicBool,
    /// Samples consumed by the YOUNGEST preview voice, republished per RT
    /// block — the piano-roll playhead follows its own preview (transport
    /// does not move while auditioning, so reading `pos` left the bar
    /// permanently static, the v10.2 report).
    preview_pos: AtomicU64,
    /// v38 R1: piano-roll AUDITION voice pool (see `play_audition`). Each
    /// voice stages `(relative sample, kind, ch, d1, d2)` events through a
    /// track's LIVE synth + chain — the offline `bounce_track_preview`
    /// path re-instantiated the plugin per click (lag, lost clicks, and
    /// the leaked-instance churn behind "the roll kills the app after a
    /// while"). Interactive auditions NEVER instantiate plugins anymore.
    aud_voices: std::sync::Mutex<Arc<Vec<AudVoiceReq>>>,
    aud_vid: AtomicU64,
    aud_gen: AtomicU64,
    /// true while a voice pool exists / held notes ring (RT clears when
    /// drained; UI sets on every play_audition).
    aud_on: AtomicBool,
    /// Samples consumed by the YOUNGEST audition voice (roll playhead).
    aud_pos: AtomicU64,
    /// v40: RT set-on-drain flag for the audition pool (mirrors
    /// `preview_drained`); the UI retires the engine-side pool when it
    /// takes this, same one-frame-later contract.
    aud_drained: AtomicBool,
}

/// v46: pseudo-track id of the DEFAULT (no track picked) roll audition
/// route. Its voice renders through a dedicated headless instance in the RT
/// and mixes dead-centre into the master, exactly like asset previews —
/// the audition never touches any real track's mixer, synth or bus.
pub const AUD_PV: usize = usize::MAX;

/// v38 R1: one requested audition voice. `ev` MUST be sorted by
/// (sample, kind) — kind order puts offs (0) before ons (3) at equal
/// samples — the RT advance loop relies on it.
/// v46: `inst` = the HEADLESS SYNTH COPY this voice renders through.
/// Roll auditions always carry one (an exact private copy of the track's
/// configured synth, or the bundled piano for the default route) so an
/// audition NEVER pushes MIDI into the live instance the clips and the
/// keyboard play — the "preview feeds the synth / stuck note" family.
/// `None` keeps the legacy behavior (render through the track's live
/// synth) and is only used by pre-v46 callers and pins.
#[derive(Clone)]
pub struct AudVoiceReq {
    pub vid: u64,
    pub track: usize,
    pub ev: std::sync::Arc<[MidiEv]>,
    pub inst: Option<Arc<SynthHandle>>,
}

/// Held (staged-but-not-yet-off) note count of a voice at cursor `cur`.
/// Used on voice replacement/purge so a dying audition never leaves a
/// synth ringing. Sorted-`ev` assumption as in `AudVoiceReq`.
pub fn aud_pending(ev: &[(u64, u8, u8, u8, u8)], cur: u64) -> i32 {
    let mut p = 0i32;
    for &(off, kind, ..) in ev.iter() {
        if off >= cur {
            break;
        }
        match kind {
            3 => p += 1,
            0 => p -= 1,
            _ => {}
        }
    }
    p.max(0)
}

impl Engine {
    pub fn new() -> Arc<Self> {
        let mut rec_bufs = [std::ptr::null_mut::<f32>(); MAX_LOOPS];
        for buf in rec_bufs.iter_mut() {
            let mut v = vec![0f32; LOOP_CAP as usize];
            *buf = v.as_mut_ptr();
            std::mem::forget(v); // ownership transfers to the engine
        }
        Arc::new(Self {
            rec_bufs,
            auto_disp: core::array::from_fn(|_| {
                core::array::from_fn(|_| AtomicU32::new(f32::NAN.to_bits()))
            }),
            rec_len: core::array::from_fn(|_| AtomicU32::new(0)),
            rec_state: core::array::from_fn(|_| AtomicU8::new(TR_IDLE)),
            rec_at: core::array::from_fn(|_| AtomicU64::new(0)),
            rec_preview: core::array::from_fn(|_| AtomicBool::new(false)),
            rec_pv_active: core::array::from_fn(|_| AtomicBool::new(false)),
            rec_loop: core::array::from_fn(|_| AtomicBool::new(false)),
            rec_loop_active: core::array::from_fn(|_| AtomicBool::new(false)),
            latency: AtomicU32::new(0),
            rec_skip: core::array::from_fn(|_| AtomicU32::new(0)),
            take_pending: [AtomicU64::new(0), AtomicU64::new(0)],
            transport_on: AtomicU8::new(0),
            loop_tl: AtomicU8::new(1),
            loop_start: AtomicU64::new(0),
            loop_end: AtomicU64::new(0),
            pos: AtomicU64::new(0),
            recording: AtomicBool::new(false),
            mpe: AtomicBool::new(false),
            seek: AtomicU64::new(NO_SEEK),
            // Empty project: an idle timeline as long as a take buffer, so
            // a transport parked on it never wraps/parks on a 1-sample
            // grid. The GUI replaces this with the real length via
            // Cmd::SetProject on every edit.
            proj_len: AtomicU64::new(LOOP_CAP as u64),
            track_on: core::array::from_fn(|_| AtomicU8::new(1)),
            poly_voices: std::sync::Mutex::new(Arc::new(Vec::new())),
            poly_gen: AtomicU64::new(0),
            track_poly: core::array::from_fn(|_| AtomicU8::new(0)),
            vol: core::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())),
            trk_gain: core::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())),
            sc_send_lo: core::array::from_fn(|_| AtomicU64::new(0)),
            sc_send_hi: core::array::from_fn(|_| AtomicU64::new(0)),
            pan: core::array::from_fn(|_| AtomicU32::new(0.5f32.to_bits())),
            selected: AtomicU8::new(0),
            monitor_on: AtomicBool::new(true),
            midi_live: AtomicBool::new(true),
            midi_rec_arm: core::array::from_fn(|_| AtomicBool::new(false)),
            midi_bound: std::sync::Mutex::new(Arc::new(BoundSet::default())),
            midi_bound_gen: AtomicU64::new(0),
            fx: std::sync::Mutex::new(Arc::new(core::array::from_fn(|_| TrackFx::default()))),
            fx_gen: AtomicU64::new(0),
            assets: std::sync::Mutex::new(Arc::new(Vec::new())),
            assets_gen: AtomicU64::new(0),
            arr: std::sync::Mutex::new(Arc::new(Arrangement::new())),
            arr_gen: AtomicU64::new(0),
            midi: std::sync::Mutex::new(Arc::new(Vec::new())),
            midi_gen: AtomicU64::new(0),
            auto: std::sync::Mutex::new(Arc::new(Vec::new())),
            auto_gen: AtomicU64::new(0),
            synths: std::sync::Mutex::new(Arc::new(core::array::from_fn(|_| None))),
            synths_gen: AtomicU64::new(0),
            default_synth: std::sync::Mutex::new(None),
            vsts_gen: AtomicU64::new(0),
            vst_refreshed: AtomicU64::new(0),
            vst_held: std::sync::Mutex::new(Vec::new()),
            sample_rate: AtomicU32::new(0),
            metro_on: AtomicU8::new(0),
            metro_bpm: AtomicU32::new(120),
            metro_sub: AtomicU32::new(1),
            metro_num: AtomicU32::new(4),
            preview_voices: std::sync::Mutex::new(Arc::new(Vec::new())),
            preview_vid: AtomicU64::new(0),
            preview_gen: AtomicU64::new(0),
            preview_on: AtomicU8::new(0),
            preview_drained: AtomicBool::new(false),
            preview_pos: AtomicU64::new(0),
            aud_voices: std::sync::Mutex::new(Arc::new(Vec::new())),
            aud_vid: AtomicU64::new(0),
            aud_gen: AtomicU64::new(0),
            aud_on: AtomicBool::new(false),
            aud_pos: AtomicU64::new(0),
            aud_drained: AtomicBool::new(false),
        })
    }

    // ---- take capture (audio thread writes, manager reads while quiesced) --

    /// Capture one dry-mixed sample into track i while recording. Returns
    /// true once the buffer is full (caller should auto-stop the take). The
    /// length counter never exceeds LOOP_CAP: the check happens before
    /// advancing, so a take that hits capacity mid-frame can't overshoot.
    #[inline]
    pub fn rec_sample(&self, i: usize, v: f32) -> bool {
        // Latency compensation: a fresh take discards its first `latency`
        // input samples (armed in `start_rec`) so the click of the button
        // that started recording — heard a few ms late through the mic —
        // becomes the take's FIRST sample instead of landing ms into it.
        let skip = self.rec_skip[i].load(Ordering::Relaxed);
        if skip > 0 {
            self.rec_skip[i].store(skip - 1, Ordering::Relaxed);
            return false;
        }
        let off = self.rec_len[i].load(Ordering::Relaxed) as u64;
        if off < LOOP_CAP as u64 {
            self.rec_write(i, off as u32, v);
            self.rec_len[i].store((off + 1) as u32, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    #[inline]
    pub fn rec_write(&self, i: usize, off: u32, v: f32) {
        unsafe { self.rec_bufs[i].add(off as usize).write(v) };
    }

    #[inline]
    pub fn rec_read(&self, i: usize, off: u32) -> f32 {
        unsafe { *self.rec_bufs[i].add(off as usize) }
    }

    pub fn rec_state_of(&self, i: usize) -> u8 {
        self.rec_state[i].load(Ordering::Relaxed)
    }

    /// RT: this track hit buffer capacity — flag the take for the manager
    /// to pick up and ship to the GUI.
    #[inline]
    pub fn mark_take_pending(&self, i: usize) {
        self.take_pending[i / 64].fetch_or(1u64 << (i % 64), Ordering::SeqCst);
    }

    /// Manager: claim every flagged take (returns a bitmask; bit `i` =
    /// track `i` has a capacity-stopped take ready to copy).
    pub fn claim_takes(&self) -> u128 {
        // Two swaps are individually atomic; a bit set between them is simply
        // claimed by the following claim (no lost updates, claim order
        // irrelevant for the take queue).
        let lo = self.take_pending[0].swap(0, Ordering::SeqCst) as u128;
        let hi = self.take_pending[1].swap(0, Ordering::SeqCst) as u128;
        lo | (hi << 64)
    }

    pub fn rec_len_of(&self, i: usize) -> u32 {
        self.rec_len[i].load(Ordering::Relaxed)
    }

    /// Playhead position when track `i`'s current take started.
    pub fn rec_at(&self, i: usize) -> u64 {
        self.rec_at[i].load(Ordering::Relaxed)
    }

    /// v42 "p": arm/disarm preview mode for the NEXT take on track i.
    pub fn set_rec_preview(&self, i: usize, on: bool) {
        if let Some(a) = self.rec_preview.get(i) {
            a.store(on, Ordering::Relaxed);
        }
    }

    /// v42: true while the running take on track i is a preview take
    /// (track-bus source, transport-following).
    pub fn rec_is_preview(&self, i: usize) -> bool {
        self.rec_pv_active
            .get(i)
            .is_some_and(|a| a.load(Ordering::Relaxed))
    }

    /// v42 loop record: arm the NEXT take (and keep the live one flagged)
    /// as a LOOP take: the timeline wrap commits each pass as a take of its
    /// own and RT restarts capture ~50 ms after the wrap, so every pass
    /// stacks as a child clip — additive by playback.
    pub fn set_rec_loop(&self, i: usize, on: bool) {
        if let Some(a) = self.rec_loop.get(i) {
            a.store(on, Ordering::Relaxed);
        }
        if !on {
            self.stop_rec_loop(i);
        }
    }

    /// v42: armed (not yet snapshotted) preview / loop flags — headless
    /// tests and status hints read these; the RT reads only the snapshots.
    pub fn rec_preview_armed(&self, i: usize) -> bool {
        self.rec_preview
            .get(i)
            .is_some_and(|a| a.load(Ordering::Relaxed))
    }

    pub fn rec_loop_armed(&self, i: usize) -> bool {
        self.rec_loop
            .get(i)
            .is_some_and(|a| a.load(Ordering::Relaxed))
    }

    pub fn rec_is_loop(&self, i: usize) -> bool {
        self.rec_loop_active
            .get(i)
            .is_some_and(|a| a.load(Ordering::Relaxed))
    }

    /// End the loop for good (user STOP, take cap, transport stop): clears
    /// armed AND live flags. Pass commits keep both set.
    pub fn stop_rec_loop(&self, i: usize) {
        if let Some(a) = self.rec_loop.get(i) {
            a.store(false, Ordering::Relaxed);
        }
        if let Some(a) = self.rec_loop_active.get(i) {
            a.store(false, Ordering::Relaxed);
        }
    }

    /// v42: the loop pass hand-off must not re-arm the input latency skip
    /// (only the very first take start follows a button press).
    pub fn clear_rec_skip(&self, i: usize) {
        self.rec_skip[i].store(0, Ordering::Relaxed);
    }

    /// Zero the take buffer, arm the latency-compensation skip, remember the
    /// playhead, and start capturing on track i.
    pub fn start_rec(&self, i: usize) {
        self.rec_state[i].store(TR_IDLE, Ordering::Relaxed);
        let buf = unsafe { std::slice::from_raw_parts_mut(self.rec_bufs[i], LOOP_CAP as usize) };
        buf.fill(0.0f32);
        self.rec_len[i].store(0, Ordering::Relaxed);
        // v42: snapshot the preview arming (flipping the checkbox mid-take
        // must never retarget a running take).
        self.rec_pv_active[i].store(
            self.rec_preview[i].load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        // v42: the loop arming ORs with a live loop flag so a mid-loop
        // restart keeps the take flagged without re-arming anything.
        let la = self.rec_loop[i].load(Ordering::Relaxed)
            || self.rec_loop_active[i].load(Ordering::Relaxed);
        self.rec_loop_active[i].store(la, Ordering::Relaxed);
        // Snapshot the compensation at arm time: changing latency mid-take
        // must never retroactively shift a recording in progress.
        self.rec_skip[i].store(self.latency.load(Ordering::Relaxed), Ordering::Relaxed);
        self.rec_at[i].store(self.pos.load(Ordering::Relaxed), Ordering::Relaxed);
        self.rec_state[i].store(TR_REC, Ordering::Relaxed);
    }

    /// Stop capturing on track i (state first, so RT releases the buffer)
    /// and clone the take's content. The caller must let one or two audio
    /// cycles elapse between the stop and this read (same discipline the old
    /// clear/quantize used); `finish_take` on the manager wraps that.
    pub fn copy_take(&self, i: usize) -> (u64, Arc<[f32]>, bool) {
        self.rec_state[i].store(TR_IDLE, Ordering::Relaxed);
        let n = self.rec_len[i].load(Ordering::Relaxed) as usize;
        let data = unsafe { std::slice::from_raw_parts(self.rec_bufs[i], n) };
        let preview = self.rec_pv_active[i].load(Ordering::Relaxed);
        self.rec_preview[i].store(false, Ordering::Relaxed);
        self.rec_pv_active[i].store(false, Ordering::Relaxed);
        (
            self.rec_at[i].load(Ordering::Relaxed),
            Arc::from(data),
            preview,
        )
    }

    // ---- global transport ---------------------------------------------------

    pub fn transport_on(&self) -> bool {
        self.transport_on.load(Ordering::Relaxed) != 0
    }

    pub fn set_transport(&self, on: bool) {
        self.transport_on.store(on as u8, Ordering::Relaxed);
    }

    /// v37 R5: mirror of "some recorder is armed" (UI publishes whenever
    /// any take/MIDI/automation recorder arms or disarms). While set, the
    /// RT transport runs STRAIGHT THROUGH: no park/auto-stop at the project
    /// end, no loop wrap — the take window is whatever the user records.
    pub fn set_recording(&self, on: bool) {
        self.recording.store(on, Ordering::Relaxed);
    }

    pub fn recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    /// v42/MPE: publish the Settings menu's "MPE support" switch.
    pub fn set_mpe(&self, on: bool) {
        self.mpe.store(on, Ordering::Relaxed);
    }

    pub fn mpe(&self) -> bool {
        self.mpe.load(Ordering::Relaxed)
    }

    /// v37 test seam: publish a playhead position exactly the way RT does
    /// (NULL_AUDIO never starts the RT thread, so headless pins need the
    /// same single atomic write the RT publish performs).
    #[cfg(test)]
    pub fn force_pos(&self, p: u64) {
        self.pos.store(p, Ordering::Relaxed);
    }

    pub fn loop_tl(&self) -> bool {
        self.loop_tl.load(Ordering::Relaxed) != 0
    }
    pub fn set_loop_region(&self, start: u64, end: u64) {
        self.loop_start.store(start, Ordering::Relaxed);
        self.loop_end.store(end, Ordering::Relaxed);
    }
    pub fn loop_region(&self) -> (u64, u64) {
        (
            self.loop_start.load(Ordering::Relaxed),
            self.loop_end.load(Ordering::Relaxed),
        )
    }

    /// Live input monitoring master switch (Settings checkbox).
    pub fn set_monitor_on(&self, on: bool) {
        self.monitor_on.store(on, Ordering::Relaxed);
    }
    pub fn monitor_on(&self) -> bool {
        self.monitor_on.load(Ordering::Relaxed)
    }
    /// v32: live MIDI → synth feed (Settings checkbox).
    pub fn set_midi_live(&self, on: bool) {
        self.midi_live.store(on, Ordering::Relaxed);
    }
    pub fn midi_live(&self) -> bool {
        self.midi_live.load(Ordering::Relaxed)
    }
    /// v32: MIDI-rec arming mirror (UI keeps this authoritative; the RT
    /// thread adds armed tracks to the live-feed target set).
    pub fn set_midi_rec_arm(&self, t: usize, on: bool) {
        if let Some(a) = self.midi_rec_arm.get(t) {
            a.store(on, Ordering::Relaxed);
        }
    }
    pub fn midi_rec_arm(&self, t: usize) -> bool {
        self.midi_rec_arm
            .get(t)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(false)
    }
    /// v32: publish a fresh bound-message snapshot (bindings/learn/reset/
    /// project load). The RT thread sees it on the next block via the gen.
    pub fn set_midi_bound(&self, bs: BoundSet) {
        if let Ok(mut b) = self.midi_bound.lock() {
            *b = Arc::new(bs);
        }
        self.midi_bound_gen.fetch_add(1, Ordering::Relaxed);
    }
    pub fn midi_bound(&self) -> Arc<BoundSet> {
        self.midi_bound
            .lock()
            .map(|b| b.clone())
            .unwrap_or_else(|_| Arc::new(BoundSet::default()))
    }
    pub fn midi_bound_gen(&self) -> u64 {
        self.midi_bound_gen.load(Ordering::Relaxed)
    }
    /// The pass-through gain actually applied to live input: the selected
    /// track's amp when monitoring is on, hard zero when off. The RT mix
    /// gates through the same atomic — pinned by
    /// `monitoring_toggle_gates_live_input`.
    pub fn monitor_amp(&self, sel_amp: f32) -> f32 {
        if self.monitor_on.load(Ordering::Relaxed) {
            sel_amp
        } else {
            0.0
        }
    }

    pub fn set_loop_tl(&self, on: bool) {
        self.loop_tl.store(on as u8, Ordering::Relaxed);
    }

    /// Current playhead position (samples).
    pub fn pos(&self) -> u64 {
        self.pos.load(Ordering::Relaxed)
    }

    /// Request a playhead move. RT swaps `seek` at block start, so this is
    /// safe while the transport runs (scrubbing). v35 R2: the playhead is
    /// placeable PAST the project end — up to [`SEEK_SLACK`] samples beyond
    /// it — so the user can park it in the tail grid, paste clips there,
    /// and arm recording beyond the last measure. Playing from past the end
    /// still restarts from the top (PlayAll guard) and a non-looping run
    /// parks at the project end.
    pub fn seek_to(&self, p: u64) {
        let cap = self
            .proj_len
            .load(Ordering::Relaxed)
            .saturating_add(SEEK_SLACK);
        let p = p.min(cap);
        self.pos.store(p, Ordering::Relaxed);
        self.seek.store(p, Ordering::Relaxed);
    }

    pub fn take_seek(&self) -> Option<u64> {
        match self.seek.swap(NO_SEEK, Ordering::Relaxed) {
            NO_SEEK => None,
            p => Some(p),
        }
    }

    pub fn proj_len(&self) -> u64 {
        self.proj_len.load(Ordering::Relaxed)
    }

    // ---- mixer --------------------------------------------------------------
    /// v35 R1: automation drives REAL controls. While a curve owns its
    /// target, the value also lands on the manual control itself (the
    /// engine atomic every fallback / mirror / export reads): pausing
    /// holds it, seeking outside the clip keeps it, and the user grabs
    /// the control FROM the automated value. (Master out is stored by the
    /// caller — the atomic lives on the Backend.) Called once per RT block
    /// after the control-rate pass — RT-safe (atomic stores only).
    pub fn apply_auto_real(&self, auto: &AutoOut) {
        for t in 0..MAX_LOOPS {
            if let Some(v) = auto.vol[t] {
                self.vol[t].store(v.max(0.0).to_bits(), Ordering::Relaxed);
            }
            if let Some(g) = auto.gain[t] {
                self.trk_gain[t].store(g.max(0.0).to_bits(), Ordering::Relaxed);
            }
            if let Some(pn) = auto.pan[t] {
                self.pan[t].store(pn.to_bits(), Ordering::Relaxed);
            }
        }
        if let Some(tb) = auto.tempo {
            self.metro_bpm.store(tb, Ordering::Relaxed);
        }
    }

    /// v34 R11: last automation-driven vol/gain/pan for a track, in
    /// amplitude space (pan in -1..1). NAN = manual control — the mixer
    /// faders display these so they animate with automation.
    /// v35 R1: live metro tempo (the real control automation drives).
    pub fn metro_bpm_now(&self) -> u32 {
        self.metro_bpm.load(Ordering::Relaxed)
    }

    pub fn auto_live(&self, i: usize) -> [f32; 3] {
        core::array::from_fn(|k| f32::from_bits(self.auto_disp[k][i].load(Ordering::Relaxed)))
    }

    pub fn track_on(&self, i: usize) -> bool {
        self.track_on[i].load(Ordering::Relaxed) != 0
    }

    pub fn set_track_on(&self, i: usize, on: bool) {
        self.track_on[i].store(on as u8, Ordering::Relaxed);
    }

    /// Latency compensation in samples (0 = off). Each fresh take discards
    /// this many leading input samples.
    pub fn latency_samples(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    /// Per-track playback volume (linear amplitude; 1.0 = 0 dB).
    pub fn loop_vol(&self, i: usize) -> f32 {
        f32::from_bits(self.vol[i].load(Ordering::Relaxed))
    }

    /// Set track `i`'s playback volume (linear amplitude; 0 = silence).
    pub fn set_loop_vol(&self, i: usize, amp: f32) {
        self.vol[i].store(amp.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Per-track gain trim (linear amplitude; 1.0 = 0 dB).
    pub fn loop_gain(&self, i: usize) -> f32 {
        f32::from_bits(self.trk_gain[i].load(Ordering::Relaxed))
    }

    /// Set track `i`'s gain trim (linear amplitude; 0 = silence).
    pub fn set_loop_gain(&self, i: usize, amp: f32) {
        self.trk_gain[i].store(amp.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// v32 R5: which tracks key track `i`'s chain (bit k = source track).
    pub fn sc_send(&self, i: usize) -> u128 {
        (self.sc_send_hi[i].load(Ordering::Relaxed) as u128) << 64
            | self.sc_send_lo[i].load(Ordering::Relaxed) as u128
    }

    /// Route `mask` as track `i`'s sidechain sources. The self-bit is
    /// cleared on the way in — self-keying is always off (would be
    /// feeding a track's own output back into its own detector).
    pub fn set_sc_send(&self, i: usize, mask: u128) {
        let m = mask & !(1u128 << i);
        self.sc_send_lo[i].store(m as u64, Ordering::Relaxed);
        self.sc_send_hi[i].store((m >> 64) as u64, Ordering::Relaxed);
    }

    /// Per-track pan position ([0, 1]; 0.5 = centre).
    pub fn loop_pan(&self, i: usize) -> f32 {
        f32::from_bits(self.pan[i].load(Ordering::Relaxed))
    }

    /// Set track `i`'s pan position, clamped to [0, 1].
    pub fn set_loop_pan(&self, i: usize, p: f32) {
        self.pan[i].store(p.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Replace track `i`'s live effect chain (manager thread). Any VST slot
    /// displaced by the swap is held for UI-thread teardown (see
    /// [`Self::drain_vst_held`]) so plugin destructors never run where they
    /// could race the audio cache swap.
    pub fn set_track_fx(&self, i: usize, chain: TrackFx) {
        let mut t = self.fx.lock().unwrap();
        if let Some(old) = t.get(i) {
            let mut displaced = false;
            for s in old.slots() {
                if let FxKind::Vst(e) = &s.kind {
                    self.vst_held.lock().unwrap().push(e.clone());
                    displaced = true;
                }
            }
            if displaced {
                // Arms `drain_vst_held`: releasable only once the RT chain
                // cache has picked up this swap and stamped `vst_refreshed`.
                self.vsts_gen.fetch_add(1, Ordering::Relaxed);
            }
        }
        let fresh = core::array::from_fn(|k| if k == i { chain.clone() } else { t[k].clone() });
        *t = Arc::new(fresh);
        drop(t);
        self.fx_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// Current effect-chain table (GUI snapshots; the RT handler picks it up
    /// when `fx_gen` moves — see `Proc::process`).
    pub fn fx_table(&self) -> Arc<[TrackFx; MAX_LOOPS]> {
        self.fx.lock().unwrap().clone()
    }

    pub fn fx_gen(&self) -> u64 {
        self.fx_gen.load(Ordering::Relaxed)
    }

    // ---- project snapshots ----------------------------------------------------

    /// Replace the engine's project snapshot (any thread; RT refreshes when
    /// either generation moves).
    pub fn set_project(&self, assets: Arc<AssetTable>, arr: Arc<Arrangement>, len: u64) {
        *self.assets.lock().unwrap() = assets;
        self.assets_gen.fetch_add(1, Ordering::Relaxed);
        *self.arr.lock().unwrap() = arr;
        self.arr_gen.fetch_add(1, Ordering::Relaxed);
        self.proj_len.store(len.max(1), Ordering::Relaxed);
        let pl = self.proj_len.load(Ordering::Relaxed);
        // A project can shrink under the playhead — keep it in bounds.
        // v37 R5: never while a take runs: the playhead is deliberately
        // beyond the OLD content end during recording.
        if self.pos.load(Ordering::Relaxed) > pl && !self.recording() {
            self.seek_to(pl);
        }
        // Samplers read the rack through the host table; refresh it with
        // every project push (assets can change under the same gen).
        crate::synth::set_host_assets(self.assets(), self.sample_rate.load(Ordering::Relaxed));
    }

    /// Replace the MIDI pattern table (parallel to `assets`).
    pub fn set_midi_assets(&self, midi: Arc<Vec<Option<Arc<crate::midipat::Pattern>>>>) {
        *self.midi.lock().unwrap() = midi;
        self.midi_gen.fetch_add(1, Ordering::Relaxed);
    }
    pub fn midi(&self) -> Arc<Vec<Option<Arc<crate::midipat::Pattern>>>> {
        self.midi.lock().unwrap().clone()
    }
    pub fn midi_gen(&self) -> u64 {
        self.midi_gen.load(Ordering::Relaxed)
    }

    /// v0.11 R5: replace the automation asset table (parallel to `assets`).
    pub fn set_auto_assets(&self, auto: Arc<Vec<Option<Arc<crate::automation::Automation>>>>) {
        *self.auto.lock().unwrap() = auto;
        self.auto_gen.fetch_add(1, Ordering::Relaxed);
    }
    pub fn auto_assets(&self) -> Arc<Vec<Option<Arc<crate::automation::Automation>>>> {
        self.auto.lock().unwrap().clone()
    }
    pub fn auto_gen(&self) -> u64 {
        self.auto_gen.load(Ordering::Relaxed)
    }

    /// Bind/unbind one track's synthesizer (unbound tracks fall back to the
    /// default synth — the bundled piano).
    pub fn set_track_synth(&self, i: usize, synth: Option<Arc<SynthHandle>>) {
        if let Ok(mut g) = self.synths.try_lock() {
            let mut next = g.clone();
            if let Some(slot) = Arc::make_mut(&mut next).get_mut(i) {
                *slot = synth;
            }
            *g = next;
            self.synths_gen.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn synths(&self) -> Arc<[Option<Arc<SynthHandle>>; MAX_LOOPS]> {
        self.synths.lock().unwrap().clone()
    }
    pub fn synths_gen(&self) -> u64 {
        self.synths_gen.load(Ordering::Relaxed)
    }

    /// v46: publish track `i`'s POLY copy row (spawned UI-side; see
    /// [`spawn_synth_copy`]). `on` false (or an empty row) restores the
    /// monophonic merge. The write is `try_lock` like every other table:
    /// the RT thread snapshots it on the gen bump and never blocks.
    pub fn set_track_poly(&self, i: usize, on: bool, voices: PolyRow) {
        if i >= MAX_LOOPS {
            return;
        }
        let row = if on { voices } else { Vec::new() };
        if let Ok(mut g) = self.poly_voices.try_lock() {
            let mut next = g.clone();
            let v = Arc::make_mut(&mut next);
            while v.len() < MAX_LOOPS {
                v.push(Vec::new());
            }
            v[i] = row;
            *g = next;
        }
        self.track_poly[i].store(on as u8, Ordering::Relaxed);
        self.poly_gen.fetch_add(1, Ordering::Relaxed);
    }
    pub fn poly_on(&self, i: usize) -> bool {
        self.track_poly
            .get(i)
            .map(|a| a.load(Ordering::Relaxed) != 0)
            .unwrap_or(false)
    }
    pub fn poly_voices(&self) -> PolyTable {
        self.poly_voices.lock().unwrap().clone()
    }
    pub fn poly_gen(&self) -> u64 {
        self.poly_gen.load(Ordering::Relaxed)
    }
    pub fn set_default_synth(&self, synth: Option<Arc<SynthHandle>>) {
        *self.default_synth.lock().unwrap() = synth;
        // The RT handler snapshots `default_synth` inside its `synths_gen`
        // refresh — without a bump here it would keep rendering with a
        // stale (or missing) fallback until some track binding changed.
        self.synths_gen.fetch_add(1, Ordering::Relaxed);
    }
    pub fn default_synth(&self) -> Option<Arc<SynthHandle>> {
        self.default_synth.lock().unwrap().clone()
    }

    pub fn vst_gen(&self) -> u64 {
        self.vsts_gen.load(Ordering::Relaxed)
    }

    /// Hand chain-displaced plugin entries back to the UI thread — but only
    /// once the audio chain cache has caught up with every swap, so the final
    /// reference of each plugin dies on the caller's (UI) thread rather than
    /// mid-swap on the audio thread.
    pub fn drain_vst_held(&self) -> Vec<Arc<dyn crate::vsthost::StageEntry>> {
        if self.vst_gen() == self.vst_refreshed.load(Ordering::Relaxed) {
            core::mem::take(&mut *self.vst_held.lock().unwrap())
        } else {
            Vec::new()
        }
    }

    /// The synth a track actually renders through (binding, else default).
    fn synth_for(&self, t: usize) -> Option<Arc<SynthHandle>> {
        self.synths()
            .get(t)
            .cloned()
            .flatten()
            .or_else(|| self.default_synth())
    }

    pub fn set_sample_rate(&self, sr: u32) {
        self.sample_rate.store(sr, Ordering::Relaxed);
        let assets = self.assets();
        crate::synth::set_host_assets(assets, sr);
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }

    pub fn assets(&self) -> Arc<AssetTable> {
        self.assets.lock().unwrap().clone()
    }

    pub fn assets_gen(&self) -> u64 {
        self.assets_gen.load(Ordering::Relaxed)
    }

    pub fn arrangement(&self) -> Arc<Arrangement> {
        self.arr.lock().unwrap().clone()
    }

    pub fn arr_gen(&self) -> u64 {
        self.arr_gen.load(Ordering::Relaxed)
    }

    // ---- metronome --------------------------------------------------------------

    /// Metronome state: (on, bpm, clicks per beat, beats per measure).
    pub fn metro(&self) -> (bool, u32, u32, u32) {
        (
            self.metro_on.load(Ordering::Relaxed) != 0,
            self.metro_bpm.load(Ordering::Relaxed),
            self.metro_sub.load(Ordering::Relaxed),
            self.metro_num.load(Ordering::Relaxed),
        )
    }

    // ---- asset preview -----------------------------------------------------------

    /// Start a SINGLE preview voice (v40): a new audition REPLACES
    /// whatever was playing — the v0.10 additive pool stacked up to 12
    /// never-retired voices (rack browsing became a wall of overlapping
    /// audio, and dead voices pinned their sample buffers until
    /// eviction). `id` still tags the source asset so a rack row re-click
    /// can stop just its own voice.
    pub fn play_preview(&self, id: u32, samples: Arc<[f32]>) {
        let vid = self.preview_vid.fetch_add(1, Ordering::Relaxed) + 1;
        *self.preview_voices.lock().unwrap() = Arc::new(vec![(vid, id, samples)]);
        self.preview_gen.fetch_add(1, Ordering::SeqCst);
        self.preview_on.store(1, Ordering::Relaxed);
        // A fresh start arms a fresh drain cycle: never inherit the flag
        // from the voice this one replaced.
        self.preview_drained.store(false, Ordering::Relaxed);
    }

    /// Stop every audition voice (transport stop / global stop).
    pub fn stop_preview(&self) {
        *self.preview_voices.lock().unwrap() = Arc::new(Vec::new());
        self.preview_gen.fetch_add(1, Ordering::SeqCst);
        self.preview_on.store(0, Ordering::Relaxed);
    }

    /// Stop only the voices started from asset `id` (rack row toggle): a
    /// re-click silences that clip's audition while previews of other
    /// assets keep playing (v0.10 parallel preview).
    pub fn stop_preview_id(&self, id: u32) {
        let mut v = (**self.preview_voices.lock().unwrap()).clone();
        v.retain(|(_, vid, _)| *vid != id);
        let empty = v.is_empty();
        *self.preview_voices.lock().unwrap() = Arc::new(v);
        self.preview_gen.fetch_add(1, Ordering::SeqCst);
        if empty {
            self.preview_on.store(0, Ordering::Relaxed);
        }
    }

    /// Snapshot of the voice list for the RT handler (cheap Arc clone).
    pub fn preview_snapshot(&self) -> PreviewVoices {
        self.preview_voices.lock().unwrap().clone()
    }

    pub fn preview_active(&self) -> bool {
        self.preview_on.load(Ordering::Relaxed) != 0
    }

    /// Samples elapsed in the youngest preview voice (only meaningful
    /// while `preview_active()`).
    pub fn preview_pos(&self) -> u64 {
        self.preview_pos.load(Ordering::Relaxed)
    }

    pub fn preview_gen(&self) -> u64 {
        self.preview_gen.load(Ordering::Relaxed)
    }

    /// v40: consume the RT drain flag for the preview pool exactly once
    /// (true = the RT thread just heard the voice run out of samples;
    /// `Proc::process` sets it).
    pub fn take_preview_drained(&self) -> bool {
        self.preview_drained.swap(false, Ordering::Relaxed)
    }

    /// v40: is the preview voice pool empty? (UI retire guard.)
    pub fn preview_pool_empty(&self) -> bool {
        self.preview_voices.lock().unwrap().is_empty()
    }

    /// v40: consume the RT drain flag for the audition pool exactly once.
    pub fn take_aud_drained(&self) -> bool {
        self.aud_drained.swap(false, Ordering::Relaxed)
    }

    /// v40: is the audition voice pool empty? (UI retire guard.)
    pub fn aud_pool_empty(&self) -> bool {
        self.aud_voices.lock().unwrap().is_empty()
    }

    #[cfg(test)]
    /// Test hook: mirror the exact state an RT-thread drain leaves —
    /// run-flags cleared, retire flags armed (see `Proc::process`).
    pub fn debug_mark_auditions_drained(&self) {
        self.preview_on.store(0, Ordering::Relaxed);
        self.preview_drained.store(true, Ordering::Relaxed);
        self.aud_on.store(false, Ordering::Relaxed);
        self.aud_drained.store(true, Ordering::Relaxed);
    }

    // ---- v38 R1 roll audition voices --------------------------------------

    /// Stage `ev` (sorted `(sample, kind, ch, d1, d2)` events relative to
    /// NOW) as an audition voice. v46: `inst` = the HEADLESS synth copy the
    /// voice renders through (see [`AudVoiceReq`]) — the track's live synth
    /// and its MIDI state are untouched, and on `track == AUD_PV` the audio
    /// mixes dead-centre into the master instead of a track bus. `inst:
    /// None` keeps the v38 behavior (render through track `t`'s LIVE synth +
    /// chain); it exists for legacy pins only. A single-event stream with NO
    /// note-off is a HELD note: it sustains until `stop_audition` (that is
    /// how press-and-hold works). Works with the transport stopped and
    /// while the track's play flag is off.
    pub fn play_audition(&self, track: usize, ev: Arc<[MidiEv]>, inst: Option<Arc<SynthHandle>>) {
        const MAX_AUD_VOICES: usize = 12;
        if ev.is_empty() {
            return;
        }
        let mut v = (**self.aud_voices.lock().unwrap()).clone();
        if v.len() >= MAX_AUD_VOICES {
            v.remove(0);
        }
        let vid = self.aud_vid.fetch_add(1, Ordering::Relaxed) + 1;
        v.push(AudVoiceReq {
            vid,
            track,
            ev,
            inst,
        });
        *self.aud_voices.lock().unwrap() = Arc::new(v);
        self.aud_pos.store(0, Ordering::Relaxed);
        // v40: a fresh start never inherits a drained-idle flag from the
        // pool generation this start extends.
        self.aud_drained.store(false, Ordering::Relaxed);
        self.aud_gen.fetch_add(1, Ordering::SeqCst);
        self.aud_on.store(true, Ordering::Relaxed);
    }

    /// Purge every audition voice. The RT thread all-`off`s every track
    /// that still has a staged-but-not-released audition note (it computes
    /// held counts itself on the generation bump).
    pub fn stop_audition(&self) {
        *self.aud_voices.lock().unwrap() = Arc::new(Vec::new());
        self.aud_pos.store(0, Ordering::Relaxed);
        self.aud_gen.fetch_add(1, Ordering::SeqCst);
        self.aud_on.store(false, Ordering::Relaxed);
    }

    pub fn audition_active(&self) -> bool {
        self.aud_on.load(Ordering::Relaxed)
    }

    pub fn audition_pos(&self) -> u64 {
        self.aud_pos.load(Ordering::Relaxed)
    }

    pub fn aud_gen(&self) -> u64 {
        self.aud_gen.load(Ordering::Relaxed)
    }

    pub fn audition_snapshot(&self) -> Arc<Vec<AudVoiceReq>> {
        self.aud_voices.lock().unwrap().clone()
    }

    // ---- offline rendering --------------------------------------------------------

    /// Render the current project to mono f32 samples at the project length:
    /// every clip of every *sounding* track (stopped tracks contribute
    /// nothing — the export is what the mix plays). v48 EXPORT IDENTITY: a
    /// track's contribution is built exactly like the live bus — the clip
    /// sum (per-clip gain x fade, track vol x gain trim) feeds ONE
    /// continuous pass of the track's effect chain, which is then panned
    /// into the mono downmix. Delay/reverb tails therefore ring through
    /// gaps and past the last clip exactly as in the preview, and chain
    /// state carries across clips (the historical per-clip, phase-0 pass
    /// cut tails at clip ends and reset state — exports did not match
    /// what the user hears). Every slot runs on a fresh private instance,
    /// so an export never advances the live chain. `None` when the project
    /// holds no clips.
    pub fn render_project(&self) -> Option<Vec<f32>> {
        self.render_tracks(None, self.proj_len() as usize)
    }

    /// Render one track's contribution (all lanes; the track's play/stop
    /// flag is ignored when a track export is explicitly requested).
    pub fn render_track(&self, t: usize) -> Option<Vec<f32>> {
        self.render_tracks(Some(t), self.proj_len() as usize)
    }

    fn render_tracks(&self, only: Option<usize>, len: usize) -> Option<Vec<f32>> {
        let arr = self.arrangement();
        let assets = self.assets();
        let midi = self.midi();
        if only.is_none() && arr.is_empty() {
            return None;
        }
        let sr = self.sample_rate().max(1);
        let chains = self.fx.lock().unwrap().clone();
        let mut out = vec![0.0f32; len.max(1)];
        let mut any = false;

        // v32 R5 sidechain mirror: the live bus taps every AUDIBLE source
        // track's dry, post-trim sum and hands the routed targets' chains
        // the summed taps. Offline the taps are sample-aligned (the live
        // one-block detector lag cannot be reproduced without knowing the
        // RT block size and is not part of what the user hears "as mixed").
        // A track without a chain of its own can still be a source, so
        // source buses are computed regardless of the export scope.
        let mut send = [0u128; MAX_LOOPS];
        let mut sc_any = false;
        for (tg, s) in send.iter_mut().enumerate() {
            *s = self.sc_send(tg);
            sc_any |= *s != 0;
        }
        let mut dry_hold: Vec<Option<Vec<f32>>> = if sc_any {
            vec![None; MAX_LOOPS]
        } else {
            Vec::new()
        };

        // The track's DRY bus: all lanes' clips summed post-trim — exactly
        // the signal the live chain is fed (`None` = nothing placed).
        let track_dry = |t: usize| -> Option<Vec<f32>> {
            let tamp = self.loop_vol(t) * self.loop_gain(t);
            let has_audio = arr.tracks[t].lanes.iter().any(|l| {
                l.clips
                    .iter()
                    .any(|c| matches!(assets.get(c.asset as usize), Some(d) if !d.is_empty()))
            });
            let has_midi = self.synth_for(t).is_some()
                && arr.tracks[t].lanes.iter().any(|l| {
                    l.clips
                        .iter()
                        .any(|c| matches!(midi.get(c.asset as usize), Some(Some(_))))
                });
            if !has_audio && !has_midi {
                return None;
            }
            let mut trk = vec![0.0f32; len.max(1)];
            // MIDI clips first: the whole track's patterns through one
            // fresh synth, summed dry before the chain pass below.
            if has_midi {
                let h = self.synth_for(t).expect("checked above");
                let mut buf = vec![0.0f32; len.max(1)];
                // v46: POLY mode exports like the mix sounds: every child
                // lane renders through its own copy (holes merge into copy
                // 0 like the live path), sums to one track row.
                let poly_row = self.poly_voices().get(t).cloned().unwrap_or_default();
                if self.poly_on(t) && !poly_row.is_empty() {
                    let n_cp = poly_row.len().min(32);
                    for (li, lane) in arr.tracks[t].lanes.iter().enumerate() {
                        let ci = li.min(n_cp - 1);
                        let hh = match poly_row.get(ci).cloned().flatten() {
                            Some(x) => x,
                            None if ci == 0 => h.clone(),
                            None => continue,
                        };
                        let mut lev: Vec<(u64, u8, u8, u8, u8)> = Vec::new();
                        for cl in &lane.clips {
                            if let Some(Some(pat)) = midi.get(cl.asset as usize) {
                                stage_midi_events(pat, cl, sr, &mut lev);
                            }
                        }
                        if lev.is_empty() {
                            continue;
                        }
                        let mut lbuf = vec![0.0f32; len.max(1)];
                        render_midi_any(&hh, &mut lev, 0, len.max(1), &mut lbuf, sr);
                        for (k, x) in lbuf.into_iter().enumerate() {
                            buf[k] += x;
                        }
                    }
                } else {
                    let mut ev: Vec<(u64, u8, u8, u8, u8)> = Vec::new();
                    for lane in &arr.tracks[t].lanes {
                        for cl in &lane.clips {
                            if let Some(Some(pat)) = midi.get(cl.asset as usize) {
                                stage_midi_events(pat, cl, sr, &mut ev);
                            }
                        }
                    }
                    render_midi_any(&h, &mut ev, 0, len.max(1), &mut buf, sr);
                }
                for (k, x) in buf.into_iter().enumerate() {
                    trk[k] += x * tamp;
                }
            }
            // Audio clips: the clip is pinned at `pos` and the source
            // starts at `cut` (identical to the live callback); `play_len`
            // past the source contributes silence (nothing summed).
            for lane in &arr.tracks[t].lanes {
                for cl in &lane.clips {
                    let data = match assets.get(cl.asset as usize) {
                        Some(d) if !d.is_empty() => d,
                        _ => continue, // empty (MIDI) or dangling asset
                    };
                    let alen = data.len() as u64;
                    let dstart = cl.data_start() as usize;
                    let dend = cl
                        .data_end(alen)
                        .min(cl.end(alen))
                        .min(len as u64)
                        .max(cl.data_start());
                    // v34 R10: past the playable source is SILENCE — the
                    // live path indexes with `rem_euclid(alen)`, and an
                    // export must never read `data[alen]` (the "export
                    // starts at a random point" report).
                    let dend = dend.max(dstart as u64) as usize;
                    for k in dstart..dend {
                        let s = data[(cl.cut + (k as u64 - cl.data_start())) as usize];
                        trk[k] += s * tamp * cl.gain * cl.fade_at(k as u64, alen);
                    }
                }
            }
            Some(trk)
        };

        // Target tracks: the export scope (an explicit track export
        // ignores that track's play/stop flag; a project export mirrors
        // the live mute state).
        let mut targets: Vec<usize> = Vec::new();
        for t in 0..MAX_LOOPS {
            if let Some(w) = only {
                if t != w {
                    continue;
                }
            } else if !self.track_on(t) {
                continue; // exports mirror the live mix
            }
            targets.push(t);
        }
        if sc_any {
            // Hold the dry bus of every AUDIBLE sidechain source — also of
            // sources OUTSIDE the export scope (they only ever feed another
            // track's chain, exactly like the live tap).
            let src_mask = send.iter().fold(0u128, |m, s| m | *s);
            for (src, slot) in dry_hold.iter_mut().enumerate() {
                if (src_mask >> src) & 1 != 0 && (self.track_on(src) || Some(src) == only) {
                    *slot = track_dry(src);
                }
            }
        }
        // Sum the source taps into each target's sidechain row BEFORE the
        // target loop consumes (moves out) any held dry bus.
        let mut sc_rows: Vec<Option<Vec<f32>>> = if sc_any {
            vec![None; MAX_LOOPS]
        } else {
            Vec::new()
        };
        if sc_any {
            for &t in &targets {
                if send[t] != 0 {
                    let mut row = vec![0.0f32; len.max(1)];
                    for (src, d) in dry_hold.iter().enumerate() {
                        if (send[t] >> src) & 1 != 0 {
                            if let Some(d) = d {
                                for (r, x) in row.iter_mut().zip(d.iter()) {
                                    *r += *x;
                                }
                            }
                        }
                    }
                    sc_rows[t] = Some(row);
                }
            }
        }
        for t in targets {
            let (pgl, pgr) = pan_gains(self.loop_pan(t));
            let pdown = (pgl + pgr) * F1S2;
            let mut trk = match dry_hold.get_mut(t).and_then(|o| o.take()) {
                Some(b) => b,
                None => match track_dry(t) {
                    Some(b) => b,
                    None => continue,
                },
            };
            // This target's sidechain row (empty = no send routed — the
            // documented sc = 0.0 rule for scripts).
            let sc_vec: Vec<f32> = if sc_any {
                sc_rows[t].take().unwrap_or_default()
            } else {
                Vec::new()
            };
            // One fresh full-chain pass over the whole span (fresh private
            // copies: the live chain is never advanced by an export).
            run_chain_over(
                &chains[t],
                sr,
                &mut trk,
                &offline_sync(self, sr, 0),
                &sc_vec,
            );
            for (k, s) in trk.into_iter().enumerate() {
                any = true;
                out[k] += s * pdown;
            }
        }
        if any {
            Some(out)
        } else {
            None
        }
    }

    /// Render staged MIDI events through a track's instrument AND its full
    /// mixer chain — the piano roll's "preview through this track" voice,
    /// so auditioning hears the same synth + effects the clip will play
    /// through. Mono; the gain law mirrors the live order (track gain into
    /// the chain; fader/pan belong to the mix, not the audition).
    pub fn bounce_track_preview(
        &self,
        t: usize,
        ev: &mut [(u64, u8, u8, u8, u8)],
        len: usize,
    ) -> Option<Vec<f32>> {
        let h = self.synth_for(t)?;
        let sr = self.sample_rate().max(1);
        let n = len.max(1);
        let mut buf = vec![0.0f32; n];
        render_midi_any(&h, ev, 0, n, &mut buf, sr);
        let tamp = self.loop_vol(t) * self.loop_gain(t);
        if (tamp - 1.0).abs() > f32::EPSILON {
            for x in buf.iter_mut() {
                *x *= tamp;
            }
        }
        let chain = self.fx.lock().unwrap()[t].clone();
        run_chain_over(&chain, sr, &mut buf, &offline_sync(self, sr, 0), &[]);
        Some(buf)
    }

    /// Render ONLY the listed clips (`(track, lane, clip index)` into the
    /// current snapshot), time-aligned so the earliest clip starts at
    /// sample 0 — the export counterpart of multi-clip selection. Track
    /// play/stop flags are ignored (an explicit selection always sounds).
    /// Returns the timeline position the selection started at plus the
    /// rendered mono mix, or `None` when no listed clip resolves to content.
    pub fn render_selection(&self, items: &[(usize, usize, usize)]) -> Option<(u64, Vec<f32>)> {
        let arr = self.arrangement();
        let assets = self.assets();
        let midi = self.midi();
        let sr = self.sample_rate().max(1);
        let clips: Vec<(usize, crate::arrange::Clip)> = items
            .iter()
            .filter_map(|(t, k, c)| {
                let cl = arr.tracks.get(*t)?.lanes.get(*k)?.clips.get(*c).copied()?;
                if assets.get(cl.asset as usize).is_some() {
                    Some((*t, cl))
                } else {
                    None
                }
            })
            .collect();
        let alen = |c: &crate::arrange::Clip| -> u64 {
            match midi.get(c.asset as usize) {
                Some(Some(pat)) => pat.len_samples(sr),
                _ => assets
                    .get(c.asset as usize)
                    .map(|d| d.len() as u64)
                    .unwrap_or(0),
            }
        };
        let from = clips.iter().map(|(_, c)| c.pos).min()?;
        let to = clips.iter().map(|(_, c)| c.end(alen(c))).max()?;
        if to <= from {
            return None;
        }
        let len = (to - from) as usize;
        let mut out = vec![0.0f32; len];
        let chains = self.fx.lock().unwrap().clone();
        // v48 EXPORT IDENTITY applies inside the window too: per track all
        // selected clips sum dry (gain x fade x trim) and ONE fresh
        // full-chain pass runs over the whole selection window — tails flow
        // across clips and ring to the window end (state from before the
        // window is, by definition, not part of a range render; sidechain
        // sends feed the aligned dry taps of the SELECTED clips only).
        let mut bytrk: Vec<(usize, Vec<crate::arrange::Clip>)> = Vec::new();
        for (t, cl) in &clips {
            match bytrk.iter_mut().find(|(k, _)| *k == *t) {
                Some(e) => e.1.push(*cl),
                None => bytrk.push((*t, vec![*cl])),
            }
        }
        for (t, tclips) in bytrk {
            let tamp = self.loop_vol(t) * self.loop_gain(t);
            let (pgl, pgr) = pan_gains(self.loop_pan(t));
            let pdown = (pgl + pgr) * F1S2;
            let mut trk = vec![0.0f32; len];
            // MIDI clips through one fresh synth copy (window-aligned).
            let mut ev: Vec<(u64, u8, u8, u8, u8)> = Vec::new();
            let mut has_midi = false;
            for cl in &tclips {
                if let Some(Some(pat)) = midi.get(cl.asset as usize) {
                    has_midi = true;
                    stage_midi_events(pat, cl, sr, &mut ev);
                }
            }
            if has_midi {
                if let Some(h) = self.synth_for(t) {
                    let mut buf = vec![0.0f32; len];
                    render_midi_any(&h, &mut ev, from, len, &mut buf, sr);
                    for (k, x) in buf.into_iter().enumerate() {
                        trk[k] += x * tamp;
                    }
                }
            }
            // Audio clips: same clip-pinned-at-pos mapping as live playback
            // and render_tracks; the selection window slices the output.
            for cl in &tclips {
                let data = match assets.get(cl.asset as usize) {
                    Some(d) if !d.is_empty() => d,
                    _ => continue,
                };
                let alen_u = data.len() as u64;
                let dstart = cl.data_start() as usize;
                let dend = cl.data_end(alen_u).min(cl.end(alen_u)) as usize;
                for k in dstart..dend {
                    let o = k - from as usize;
                    if o >= len {
                        break;
                    }
                    let s = data[(cl.cut + (k as u64 - cl.data_start())) as usize];
                    trk[o] += s * tamp * cl.gain * cl.fade_at(k as u64, alen_u);
                }
            }
            run_chain_over(
                &chains[t],
                sr,
                &mut trk,
                &offline_sync(self, sr, from as i64),
                &[],
            );
            for (o, s) in trk.into_iter().enumerate() {
                out[o] += s * pdown;
            }
        }
        Some((from, out))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        use std::alloc::{dealloc, Layout};
        let layout = Layout::array::<f32>(LOOP_CAP as usize).expect("valid layout");
        for p in self.rec_bufs.iter() {
            unsafe { dealloc(*p as *mut u8, layout) };
        }
    }
}

// The raw pointers are only ever dereferenced from the audio thread (take
// sample writes) or the manager thread while the take is quiesced; the Arc
// keeps the engine alive for as long as either thread exists.
unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

/// Manager-side record toggle: start a fresh capture, or stop the take and
/// hand it to the GUI. Stopping always leaves the engine ready for the next
/// take; empty stops (button mashing) are silently dropped.
fn toggle_record(engine: &Engine, i: usize, tx: &SyncSender<Evt>) {
    if engine.rec_state_of(i) == TR_REC {
        engine.rec_state[i].store(TR_IDLE, Ordering::Relaxed);
        // The GUI owns this take now; drop any capacity-stop flag so the
        // manager poll doesn't ship the same take twice.
        engine.mark_take_pending(i);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (at, data, preview) = engine.copy_take(i);
        if data.is_empty() {
            let _ = tx.try_send(Evt::Notice(format!(
                "track {}: empty take discarded",
                i + 1
            )));
        } else {
            let _ = tx.try_send(Evt::Take {
                track: i,
                at,
                data,
                preview,
            });
        }
    } else {
        engine.start_rec(i);
    }
}

/// Backend-neutral manager-thread contract (WINDOWS-PORT W5): drains the
/// GUI command queue, dispatches engine state, claims finished takes,
/// serves exports, and polls the backend's idle tick. THE loop body is
/// identical for every OS — anything device-specific lives behind
/// [`ManagerBackend`], never in a copy of this loop. Exits on
/// `Cmd::Quit`/channel loss; the caller then runs backend teardown.
fn manager_loop(
    cmd_rx: &Receiver<Cmd>,
    evt_tx: &SyncSender<Evt>,
    engine: &Arc<Engine>,
    sample_rate: u32,
    session: &mut dyn ManagerBackend,
) {
    loop {
        match cmd_rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(Cmd::Refresh) => session.refresh(evt_tx),
            Ok(Cmd::SetMidiInputs(sel)) => session.set_midi_inputs(&sel, evt_tx),
            Ok(Cmd::Apply { ins, outs }) => {
                session.apply(&ins, &outs, evt_tx);
            }
            Ok(Cmd::ToggleRecord { i }) => {
                toggle_record(engine, i.min(MAX_LOOPS - 1), evt_tx);
            }
            Ok(Cmd::SetTrackOn { i, on }) => {
                engine.set_track_on(i.min(MAX_LOOPS - 1), on);
            }
            Ok(Cmd::ToggleTrackOn { i }) => {
                let i = i.min(MAX_LOOPS - 1);
                let on = !engine.track_on(i);
                engine.set_track_on(i, on);
            }
            Ok(Cmd::PlayAll) => {
                // Playing at (or past) the end starts over from the top.
                if engine.pos() >= engine.proj_len() {
                    engine.seek_to(0);
                }
                // v0.10: starting the transport clears pending auditions.
                // A leftover rack preview kept playing OVER the real
                // playback — the "something else is processing my audio"
                // double-voice. Auditions belong to browsing, not mixing.
                engine.stop_preview();
                // v38 R1: roll auditions follow the same rule —
                // audition + real playback would double the voice.
                engine.stop_audition();
                engine.set_transport(true);
                let _ = evt_tx.try_send(Evt::Transport { on: true });
            }
            Ok(Cmd::StopAll) => {
                engine.stop_preview();
                engine.stop_audition();
                engine.set_transport(false);
                let _ = evt_tx.try_send(Evt::Transport { on: false });
            }
            Ok(Cmd::Seek { pos }) => engine.seek_to(pos),
            Ok(Cmd::SetLoop(on)) => engine.set_loop_tl(on),
            Ok(Cmd::SetMonitor(on)) => engine.set_monitor_on(on),
            Ok(Cmd::SetLoopRegion { start, end }) => engine.set_loop_region(start, end),
            Ok(Cmd::SetProject { assets, arr, len }) => {
                engine.set_project(assets, arr, len);
            }
            Ok(Cmd::SetMidiAssets(midi)) => engine.set_midi_assets(midi),
            Ok(Cmd::SetAutoAssets(auto)) => engine.set_auto_assets(auto),
            Ok(Cmd::SetTrackSynth { i, synth }) => {
                engine.set_track_synth(i.min(MAX_LOOPS - 1), synth)
            }
            Ok(Cmd::SetDefaultSynth(s)) => engine.set_default_synth(s),
            Ok(Cmd::ExportTrack { i, path, fmt }) => {
                let i = i.min(MAX_LOOPS - 1);
                match engine.render_track(i) {
                    Some(samples) => {
                        match crate::audio_io::export(&path, sample_rate, &samples, fmt) {
                            Ok(()) => {
                                let _ = evt_tx.send(Evt::Notice(format!(
                                    "exported track {} ({} samples) → {}",
                                    i + 1,
                                    samples.len(),
                                    path.display()
                                )));
                            }
                            Err(e) => {
                                let _ = evt_tx.send(Evt::Error(format!("export: {e}")));
                            }
                        }
                    }
                    None => {
                        let _ = evt_tx.send(Evt::Error(format!(
                            "export: track {} has no placed clips",
                            i + 1
                        )));
                    }
                }
            }
            Ok(Cmd::ExportProject { path, fmt }) => match engine.render_project() {
                Some(samples) => match crate::audio_io::export(&path, sample_rate, &samples, fmt) {
                    Ok(()) => {
                        let _ = evt_tx.send(Evt::Notice(format!(
                            "exported project ({} samples) → {}",
                            samples.len(),
                            path.display()
                        )));
                    }
                    Err(e) => {
                        let _ = evt_tx.send(Evt::Error(format!("export: {e}")));
                    }
                },
                None => {
                    let _ = evt_tx.send(Evt::Error("export: no clips placed in any track".into()));
                }
            },
            Ok(Cmd::ExportSelection { path, fmt, items }) => {
                match engine.render_selection(&items) {
                    Some((_from, samples)) => {
                        match crate::audio_io::export(&path, sample_rate, &samples, fmt) {
                            Ok(()) => {
                                let _ = evt_tx.send(Evt::Notice(format!(
                                    "exported selection of {} clip(s) ({} samples) → {}",
                                    items.len(),
                                    samples.len(),
                                    path.display()
                                )));
                            }
                            Err(e) => {
                                let _ = evt_tx.send(Evt::Error(format!("export: {e}")));
                            }
                        }
                    }
                    None => {
                        let _ = evt_tx.send(Evt::Error(
                            "export: nothing to export (empty selection?)".into(),
                        ));
                    }
                }
            }
            Ok(Cmd::SetTrackEffects { i, chain }) => {
                engine.set_track_fx(i.min(MAX_LOOPS - 1), chain);
            }
            Ok(Cmd::SetSelected(i)) => {
                engine
                    .selected
                    .store(i.min(MAX_LOOPS - 1) as u8, Ordering::Relaxed);
            }
            Ok(Cmd::SetLatency { ms }) => {
                let ms = ms.clamp(0.0, 1000.0);
                engine.latency.store(
                    (ms * sample_rate as f32 / 1000.0).round() as u32,
                    Ordering::Relaxed,
                );
            }
            Ok(Cmd::SetMetro { on, bpm, sub, num }) => {
                engine.metro_on.store(on as u8, Ordering::Relaxed);
                engine
                    .metro_bpm
                    .store(bpm.clamp(20, 300), Ordering::Relaxed);
                engine.metro_sub.store(sub.clamp(1, 16), Ordering::Relaxed);
                engine.metro_num.store(num.clamp(1, 16), Ordering::Relaxed);
            }
            Ok(Cmd::PreviewPlay { id, samples }) => engine.play_preview(id, samples),
            Ok(Cmd::PreviewStop { id: None }) => engine.stop_preview(),
            Ok(Cmd::PreviewStop { id: Some(id) }) => engine.stop_preview_id(id),
            Ok(Cmd::Quit) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Takes the RT handler auto-stopped at buffer capacity never
                // saw a stop command, so they wait in the engine until the
                // manager claims them here.
                let mut pend = engine.claim_takes();
                while pend != 0 {
                    let i = pend.trailing_zeros() as usize;
                    pend &= !(1u128 << i);
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    let (at, data, preview) = engine.copy_take(i);
                    if !data.is_empty() {
                        let _ = evt_tx.try_send(Evt::Take {
                            track: i,
                            at,
                            data,
                            preview,
                        });
                    }
                }
                // No command pending: re-derive the MIDI source set. This is
                // what makes bindings work when a device (re)appears on a new
                // port name — e.g. hardware that gets a fresh ALSA/JACK port
                // number on every connect instead of a fixed one.
                session.idle(evt_tx);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Clamp a loop-region request to the live project: `ls` never past the
/// end, and a degenerate `le` (at/behind `ls`, past the end, or 0 = "whole
/// song") falls back to the project end. Pure so it is unit-testable.
fn clamp_loop_region(ls: u64, le: u64, proj_len: u64) -> (u64, u64) {
    let ls = ls.min(proj_len.saturating_sub(1));
    let le = if le <= ls || le > proj_len {
        proj_len
    } else {
        le
    };
    (ls, le)
}

/// Region wrap: `Some(ls)` when `pos` reached the region end (bounce back
/// to the start), `None` to keep rolling. Pure so it is unit-testable.
fn wrap_pos(pos: u64, ls: u64, le: u64) -> Option<u64> {
    (pos >= le).then_some(ls)
}

/// One block's worth of live clips: a raw pointer to the asset data offset to
/// the window start, and the window `[i0, i1)` of block-relative sample
/// indices the clip covers. Rebuilt per block (cheap — tens of clips); the
/// sample loop then does zero arithmetic per clip beyond `ptr.add(i - i0)`.
struct ClipEntry {
    track: u8,
    ptr: *const f32,
    i0: usize,
    i1: usize,
    /// Per-clip gain (linear) and a piecewise fade envelope evaluated at
    /// ABSOLUTE timeline samples. No fade: `fi_end = 0` and
    /// `fo_start = u64::MAX` so both window tests miss for every sample.
    gain: f32,
    pos: u64,
    fi_end: u64,
    fi_step: f32,
    fo_start: u64,
    fo_end: u64,
    fo_step: f32,
}

unsafe impl Send for ClipEntry {}

/// Stage one MIDI pattern clip's events into a shared list, keyed by
/// ABSOLUTE timeline sample (kind: 0=note-off 1=CC 2=pressure 3=note-on).
/// Shared by the offline renders; the RT path stages per block inline.
/// Pass a render chunk through a track's private VST copy (offline export);
/// a no-op when the track has no insert. Fresh per render PASS (v48: one
/// continuous chain pass per track over the whole render span; the live
/// chain is never advanced).
fn vst_chunk(e: Option<&dyn crate::vsthost::StageEntry>, chunk: &mut [f32]) {
    if let Some(e) = e {
        let src = chunk.to_vec();
        e.process(&src, chunk);
    }
}

/// Reopen a *private* copy of a loaded plugin for offline rendering: same
/// path/kind, current slider values. The live entry is never touched (the
/// audio thread owns it), so exports and the running mix cannot interfere.
fn spawn_entry_private(
    e: &dyn crate::vsthost::StageEntry,
    sr: u32,
) -> Option<Arc<dyn crate::vsthost::StageEntry>> {
    let (path, kind) = e.spec().clone();
    let params: Vec<f32> = (0..e.params().len()).map(|k| e.params().get(k)).collect();
    crate::vsthost::open_pid(&path, kind, e.plugin_id(), sr.max(1), Some(&params), None)
        .ok()
        .map(|l| l.entry)
}

/// v46: an EXACT headless copy of a track's configured synth — scripts
/// clone through `spawn_fresh` (same script, current slider values), VSTi
/// reload the same plugin at its current parameter values. This is what
/// roll auditions and POLY child-lane synths render through. NEVER call it
/// on the realtime thread: the VST branch loads a plugin (under the
/// process-wide loader lock).
/// v46: a track's poly voice row and the published whole-table snapshot.
pub type PolyRow = Vec<Option<Arc<SynthHandle>>>;
/// The gen-swapped poly table: one [`PolyRow`] per track.
pub type PolyTable = std::sync::Arc<Vec<PolyRow>>;

pub fn spawn_synth_copy(h: &SynthHandle, sr: u32) -> Option<Arc<SynthHandle>> {
    match h {
        SynthHandle::Script(i) => i.spawn_fresh().map(|n| Arc::new(SynthHandle::Script(n))),
        SynthHandle::Vst(e) => spawn_entry_private(&**e, sr).map(|n| Arc::new(SynthHandle::Vst(n))),
    }
}

/// Tier-6: sync snapshot for an offline chain pass — a render pass counts
/// as "transport running"; `pos0` is where `buf[0]` sits on the timeline
/// (per-clip passes are fed the clip's own start, whole-project renders 0).
/// Scripts self-advance between pushes, so one push covers the whole pass.
fn offline_sync(engine: &Engine, sr: u32, pos0: i64) -> crate::fx::SyncInfoV1 {
    crate::fx::SyncInfoV1::block(
        pos0,
        true,
        engine.metro_bpm.load(Ordering::Relaxed) as f64,
        engine.metro_num.load(Ordering::Relaxed).max(1) as f64,
        sr.max(1) as f64,
    )
}

/// Run the whole chain (scripted + plugins, in order) over a full render
/// buffer using fresh private copies. Plugin slots consume the buffer
/// block-wise; scripted slots run sample-wise like the live path, fed the
/// (mid, mid) pair and the sidechain row `sc` (empty = no send routed).
/// v48: ONE pass over the whole render span is the export-identity rule —
/// the same chain the live bus runs continuously.
fn run_chain_over(
    chain: &TrackFx,
    sr: u32,
    buf: &mut [f32],
    sync: &crate::fx::SyncInfoV1,
    sc: &[f32],
) {
    for slot in chain.slots() {
        match &slot.kind {
            FxKind::Scripted { inst, desc } => {
                if let Some(f) = inst.spawn_fresh(desc) {
                    // Tier-6: fresh private copy gets the pass's sync
                    // origin before its first sample (no-op for v1..v3).
                    f.push_sync(sync);
                    // Tier-3: offline renders are mono — a v3 stereo slot
                    // is fed (mid, mid) and keeps its LEFT output (the
                    // documented fold, see effect_api.rs ABI v3). v48: the
                    // single dispatch mirrors the live one (`call_sample_st`
                    // + the routed sc sample), so mono scripts see the
                    // exact same mid — `(x + x) * 0.5 == x` bit-identically
                    // — and v2 sidechain scripts duck like the live mix.
                    for (i, x) in buf.iter_mut().enumerate() {
                        *x = f
                            .process_private_st_sc(*x, *x, *sc.get(i).unwrap_or(&0.0))
                            .0;
                    }
                }
            }
            FxKind::Vst(e) => {
                if let Some(pv) = spawn_entry_private(&**e, sr) {
                    vst_chunk(Some(pv.as_ref()), buf);
                }
            }
        }
    }
}

/// Render staged MIDI events through either synth kind. VSTi timing is
/// sample-exact inside the plugin's internal blocks (v33; the live path
/// shares `process_with_midi`).
fn render_midi_any(
    h: &SynthHandle,
    ev: &mut [(u64, u8, u8, u8, u8)],
    from: u64,
    len: usize,
    out: &mut [f32],
    sr: u32,
) {
    match h {
        SynthHandle::Script(inst) => {
            if let Some(fresh) = inst.spawn_fresh() {
                render_midi_events(&fresh, ev, from, len, out);
            }
        }
        SynthHandle::Vst(e) => {
            if let Some(pv) = spawn_entry_private(&**e, sr) {
                render_midi_vst(&*pv, ev, from, len, out);
            }
        }
    }
}

fn render_midi_vst(
    e: &dyn crate::vsthost::StageEntry,
    ev: &[(u64, u8, u8, u8, u8)],
    from: u64,
    len: usize,
    out: &mut [f32],
) {
    let mut ev: Vec<(usize, u8, u8, u8, u8)> = ev
        .iter()
        .filter(|(at, ..)| *at >= from && *at - from < len as u64)
        .map(|(at, k, c, d1, d2)| ((*at - from) as usize, *k, *c, *d1, *d2))
        .collect();
    ev.sort_unstable();
    ev.dedup();
    let sil = vec![0.0f32; crate::vsthost::VST_BLOCK];
    // v33: render in internal-block chunks, each fed through
    // `process_with_midi` with the chunk's events at their exact sample
    // offsets. (The old split-feed queued everything at delta 0, and its
    // kind matcher had NO case 2 — pattern channel pressure fell into the
    // note_on arm and fired ghost notes at key 0 in every export that
    // used expression.)
    let mut evq: Vec<(usize, [u8; 3])> = Vec::new();
    let mut i = 0usize;
    let mut done = 0usize;
    while done < out.len() {
        let n = (out.len() - done).min(crate::vsthost::VST_BLOCK);
        evq.clear();
        while i < ev.len() && ev[i].0 < done + n {
            if ev[i].0 >= done {
                let (off, kind, ch, d1, d2) = ev[i];
                let m = match kind {
                    0 => [0x80 | ch & 0x0F, d1 & 0x7F, 0],
                    1 => [0xB0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                    2 => [0xD0 | ch & 0x0F, d2 & 0x7F, 0],
                    _ => [0x90 | ch & 0x0F, d1 & 0x7F, d2.clamp(1, 127)],
                };
                evq.push((off - done, m));
            }
            i += 1;
        }
        e.process_with_midi(&sil[..n], &mut out[done..done + n], &evq);
        done += n;
    }
    e.all_notes_off();
}

fn stage_midi_events(
    pat: &crate::midipat::Pattern,
    cl: &crate::arrange::Clip,
    sr: u32,
    ev: &mut Vec<(u64, u8, u8, u8, u8)>,
) {
    let avail = pat.len_samples(sr);
    let span = cl.span(avail);
    let base = cl.pos.saturating_sub(cl.cut);
    for note in pat.notes.iter() {
        let s0 = pat.ticks_to_samples(note.start, sr);
        if s0 < cl.cut || s0 - cl.cut >= span {
            continue;
        }
        let on = base + s0;
        let vel = (note.vel as f32 * cl.gain * cl.fade_at(on, avail))
            .clamp(0.0, 127.0)
            .round() as u8;
        if vel == 0 {
            continue;
        }
        ev.push((on, 3, note.ch, note.note, vel));
        let len_s = pat.ticks_to_samples(note.len, sr).max(1);
        ev.push((on + len_s, 0, note.ch, note.note, 0));
    }
    for m in pat.ccs.iter() {
        let s = pat.ticks_to_samples(m.at, sr);
        if s < cl.cut || s - cl.cut >= span {
            continue;
        }
        ev.push((base + s, 1, m.ch, m.num, m.val));
    }
    for m in pat.press.iter() {
        let s = pat.ticks_to_samples(m.at, sr);
        if s < cl.cut || s - cl.cut >= span {
            continue;
        }
        ev.push((base + s, 2, m.ch, 0, m.val));
    }
    // v42/MPE lanes: bends (kind 5, 14-bit split) and per-note AT (kind 4).
    for m in pat.bends.iter() {
        let s = pat.ticks_to_samples(m.at, sr);
        if s < cl.cut || s - cl.cut >= span {
            continue;
        }
        ev.push((
            base + s,
            5,
            m.ch,
            (m.val & 0x7F) as u8,
            ((m.val >> 7) & 0x7F) as u8,
        ));
    }
    for m in pat.polys.iter() {
        let s = pat.ticks_to_samples(m.at, sr);
        if s < cl.cut || s - cl.cut >= span {
            continue;
        }
        ev.push((base + s, 4, m.ch, m.note, m.val));
    }
}

/// Render staged absolute-sample events through `inst` into `len` samples
/// of `out` (starting at timeline `from`). Events outside the window are
/// ignored; notes ringing at `from` simply start ringing at sample 0 of
/// this bounce (offline bounces are stateless).
pub fn render_midi_events(
    inst: &crate::synth::Instance,
    ev: &mut [(u64, u8, u8, u8, u8)],
    from: u64,
    len: usize,
    out: &mut [f32],
) {
    let mut p: [f32; crate::synth::MAX_PARAMS] = [0.0; crate::synth::MAX_PARAMS];
    inst.read_params(&mut p);
    // Prime before events (see render_midi_block): a note at offset 0 must
    // not be delivered onto a script instrument that has never processed.
    unsafe { inst.process_into(&mut out[..0], &p) };
    let mut ev: Vec<(usize, u8, u8, u8, u8)> = ev
        .iter()
        .filter(|(at, ..)| *at >= from && *at - from < len as u64)
        .map(|(at, k, c, d1, d2)| ((*at - from) as usize, *k, *c, *d1, *d2))
        .collect();
    // Note-offs (kind 0) before note-ons (kind 3) at one sample.
    ev.sort_unstable();
    let mut cur = 0usize;
    let mut i = 0usize;
    while i < ev.len() {
        let off = ev[i].0.min(len);
        if off > cur {
            let end = off.min(out.len());
            if end > cur {
                unsafe { inst.process_into(&mut out[cur..end], &p) };
                cur = end;
            }
        }
        while i < ev.len() && ev[i].0 == off {
            let (_, kind, ch, d1, d2) = ev[i];
            match kind {
                0 => inst.note_off(ch, d1),
                1 => inst.cc(ch, d1, d2),
                2 => inst.press(ch, d2),
                4 | 5 => {} // v42/MPE: scripted ABI v1 has no bend/poly-AT
                _ => inst.note_on(ch, d1, d2),
            }
            i += 1;
        }
    }
    if cur < out.len() {
        unsafe { inst.process_into(&mut out[cur..], &p) };
    }
    inst.all_off();
}

/// Peak level below which a rendered MIDI block counts as silent
/// (~ -80 dBFS): only such blocks advance the synth's stop-the-clock timer.
const MIDI_SILENCE_FLOOR: f32 = 1e-4;
/// Consecutive silent blocks (nothing scheduled) after which a track's
/// synth stops advancing — a few tens of ms, far below any real tail but
/// long enough for the piano's ~70 ms damper release.
const MIDI_QUIET_BLOCKS: u32 = 6;

/// v33: the track-render gate (issue: "knobs and sliders don't update
/// unless a note is actively being played"). TRUE = skip this block.
/// First block is priming (positions only, no audio). Otherwise a block
/// renders when a pattern plays, live MIDI wants through, OR a plugin
/// synth is bound to a live track (`vst_open`) — plugins must run every
/// block or their meters/mod state/VST2-GUI-edited instance freeze
/// between notes. Script-only rigs keep the quiet-tail pause (free).
fn render_gate_blocked(block_n: usize, pattern_run: bool, live_any: bool, vst_open: bool) -> bool {
    block_n == 0 || (!pattern_run && !live_any && !vst_open)
}

/// The synth slot a live track plays: its own binding if any, else the
/// project default. TRUE when that resolves to a plugin instance.
fn slot_is_vst(
    slot: Option<&std::sync::Arc<SynthHandle>>,
    dflt: Option<&std::sync::Arc<SynthHandle>>,
) -> bool {
    slot.or(dflt)
        .is_some_and(|a| matches!(&**a, SynthHandle::Vst(_)))
}

/// Stage ONE track's pattern clips — and, when `live`, the raw keyboard
/// feed — into `ev` as `(block offset, kind, ch, d1, d2)` events, pushing
/// `ring` past every scheduled note-off. v33: factored out of the track
/// loop so the OWNER of a shared synth handle can stage its sharers'
/// events too (one merged stream per plugin instance; see
/// `render_midi_block`). Kind order puts offs (0) before ons (3) at equal
/// offsets so a retrigger never kills its own note.
/// v46: `only_lane = Some(li)` stages that child lane ALONE (poly mode
/// renders every child through its own synth copy; see the poly pass in
/// `render_midi_block`).
#[allow(clippy::too_many_arguments)]
fn stage_track_events(
    t: usize,
    arr: &Arrangement,
    midi_cache: &[Option<Arc<crate::midipat::Pattern>>],
    live_ev: &[(usize, u8, u8, u8, u8)],
    live: bool,
    pattern_run: bool,
    p0: u64,
    span_end: u64,
    block_n: usize,
    sr: u32,
    ev: &mut Vec<(usize, u8, u8, u8, u8)>,
    ring: &mut u64,
    only_lane: Option<usize>,
) {
    for (li, l) in arr.tracks[t].lanes.iter().enumerate() {
        if !pattern_run {
            break;
        }
        if only_lane.is_some_and(|w| w != li) {
            continue;
        }
        for cl in l.clips.iter() {
            let Some(Some(pat)) = midi_cache.get(cl.asset as usize) else {
                continue;
            };
            let avail = pat.len_samples(sr);
            let span = cl.span(avail);
            // Clip timeline window vs this block.
            if cl.end(avail) <= p0 || cl.pos >= span_end {
                continue;
            }
            let base = cl.pos.saturating_sub(cl.cut);
            for note in pat.notes.iter() {
                let s0 = pat.ticks_to_samples(note.start, sr);
                if s0 < cl.cut || s0 - cl.cut >= span {
                    continue; // head trimmed away or past clip end
                }
                let on = base + s0;
                let len_s = pat.ticks_to_samples(note.len, sr).max(1);
                if on >= p0 && on < span_end {
                    let vel = (note.vel as f32 * cl.gain * cl.fade_at(on, avail))
                        .clamp(0.0, 127.0)
                        .round() as u8;
                    if vel > 0 {
                        ev.push(((on - p0) as usize, 3, note.ch, note.note, vel));
                    }
                    let off_tl = on + len_s;
                    if off_tl >= p0 && off_tl < span_end {
                        ev.push(((off_tl - p0) as usize, 0, note.ch, note.note, 0));
                    }
                    *ring = (*ring).max(off_tl);
                } else if on < p0 && on + len_s > p0 && on + len_s <= span_end {
                    // Rang in earlier; its off lands in this block.
                    ev.push(((on + len_s - p0) as usize, 0, note.ch, note.note, 0));
                    *ring = (*ring).max(on + len_s);
                }
            }
            for m in pat.ccs.iter() {
                let s = pat.ticks_to_samples(m.at, sr);
                if s < cl.cut || s - cl.cut >= span {
                    continue;
                }
                let at = base + s;
                if at >= p0 && at < span_end {
                    ev.push(((at - p0) as usize, 1, m.ch, m.num, m.val));
                }
            }
            for m in pat.press.iter() {
                let s = pat.ticks_to_samples(m.at, sr);
                if s < cl.cut || s - cl.cut >= span {
                    continue;
                }
                let at = base + s;
                if at >= p0 && at < span_end {
                    ev.push(((at - p0) as usize, 2, m.ch, 0, m.val));
                }
            }
            // v42/MPE lanes: pattern bends and per-note aftertouch.
            for m in pat.bends.iter() {
                let s = pat.ticks_to_samples(m.at, sr);
                if s < cl.cut || s - cl.cut >= span {
                    continue;
                }
                let at = base + s;
                if at >= p0 && at < span_end {
                    ev.push((
                        (at - p0) as usize,
                        5,
                        m.ch,
                        (m.val & 0x7F) as u8,
                        ((m.val >> 7) & 0x7F) as u8,
                    ));
                }
            }
            for m in pat.polys.iter() {
                let s = pat.ticks_to_samples(m.at, sr);
                if s < cl.cut || s - cl.cut >= span {
                    continue;
                }
                let at = base + s;
                if at >= p0 && at < span_end {
                    ev.push(((at - p0) as usize, 4, m.ch, m.note, m.val));
                }
            }
        }
    }
    // v32 live feed: this track's synth also plays what the player is
    // physically sending RIGHT NOW (selected + MIDI-rec-armed tracks;
    // bound messages were filtered at the port). v33: when the track
    // shares its synth handle, the owner stages this once per INSTANCE —
    // the exact duplicates that used to land here per sharing track are
    // what pushed voice-stealing plugins into dropping held notes.
    if live && !live_ev.is_empty() {
        for &(off, k, ch, d1, d2) in live_ev.iter() {
            ev.push((off.min(block_n), k, ch, d1, d2));
        }
    }
}

impl Proc {
    /// Render every MIDI pattern clip sounding this block AND the live
    /// keyboard feed through each track's synth into `midi_scratch[t]`
    /// (mono). Notes are triggered by the pattern; a note whose head was
    /// trimmed away (`cut`) stays silent,
    /// one already ringing keeps ringing inside the synth until its off
    /// event — contiguous blocks make that seamless, and a transport seek
    /// `all_off`s everything. A track whose schedule is still ringing (or
    /// whose tail has not decayed yet) keeps ADVANCING its synth even on
    /// event-free blocks: the sound lives inside the instrument, and
    /// pausing the synth between events turned every sustained note into a
    /// one-block blip. v32: `live_mask` tracks additionally receive the
    /// raw messages the player is physically sending this block (selected
    /// track + MIDI-rec armed; bound messages were already filtered) — the
    /// live feed works with the transport STOPPED, which is exactly when
    /// auditioning a patch happens. Returns whether any track produced
    /// sound.
    fn render_midi_block(
        &mut self,
        p0: u64,
        block_n: usize,
        live_mask: u128,
        live_on: bool,
    ) -> bool {
        // v38 R1: the audition pool was replaced/stopped — tracks holding
        // staged-but-never-off audition notes must die NOW, exactly like
        // the live-key cleanup below (never dropped silently: a ringing
        // orphan note is the worst possible feedback).
        if self.aud_purge != 0 {
            let mut m = std::mem::take(&mut self.aud_purge);
            while m != 0 {
                let t = m.trailing_zeros() as usize;
                m &= m - 1;
                if t >= MAX_LOOPS {
                    continue;
                }
                match self.synths_cache.get(t).and_then(|o| o.as_ref()) {
                    Some(h) => h.all_off(),
                    None => {
                        if let Some(d) = &self.dflt_synth {
                            d.all_off();
                        }
                    }
                }
            }
        }
        // Clear last block's scratch first (only rows that actually sounded).
        for (t, live) in self.midi_live.iter_mut().enumerate() {
            if *live {
                self.midi_scratch[t].iter_mut().for_each(|x| *x = 0.0);
                *live = false;
            }
        }
        // v32: live gate flipped off while keys were counted: those voices
        // must die NOW — a disabled feed never leaves a synth ringing.
        if !live_on && self.live_notes.iter().any(|x| *x != 0) {
            for (t, h) in self
                .synths_cache
                .iter()
                .enumerate()
                .filter_map(|(t, h)| h.as_ref().map(|h| (t, h)))
            {
                if self.live_notes[t] != 0 {
                    h.all_off();
                }
            }
            if self.live_notes.iter().enumerate().any(|(t, n)| {
                *n != 0
                    && self
                        .synths_cache
                        .get(t)
                        .map(|s| s.is_none())
                        .unwrap_or(true)
            }) {
                if let Some(d) = &self.dflt_synth {
                    d.all_off();
                }
            }
            self.live_notes = [0i32; MAX_LOOPS];
            // v33: a disabled feed never delivers stale carried messages
            // either — whatever was staged belongs to the OFF gate.
            self.live_ev.clear();
        }
        let pattern_run = self.engine.transport_on() && !self.midi_cache.is_empty() && block_n != 0;
        if !pattern_run {
            // Transport stopped (or the pattern table went away) while
            // pattern voices were sounding: silence them once so nothing
            // leaks into the next run, and drop all pattern sustain
            // bookkeeping. v32: a track HOLDING a live key keeps its
            // voices — the keyboard is not the transport, a held key rings
            // through a stopped transport until its key-up arrives.
            if self.midi_sustain.iter().any(|s| *s) || self.midi_ring_to.iter().any(|r| *r != 0) {
                for (t, h) in self
                    .synths_cache
                    .iter()
                    .enumerate()
                    .filter_map(|(t, h)| h.as_ref().map(|h| (t, h)))
                {
                    // v38: a ringing AUDITION note is not the transport's
                    // to kill — the voice owns its note-offs.
                    if self.live_notes[t].max(0) == 0 && self.aud_held[t] <= 0 {
                        h.all_off();
                    }
                }
                if let Some(d) = &self.dflt_synth {
                    let held_on_default = self.live_notes.iter().enumerate().any(|(t, n)| {
                        (*n > 0 || self.aud_held[t] > 0)
                            && self
                                .synths_cache
                                .get(t)
                                .map(|s| s.is_none())
                                .unwrap_or(true)
                    });
                    if !held_on_default {
                        d.all_off();
                    }
                }
            }
            for t in 0..MAX_LOOPS {
                self.midi_sustain[t] = false;
                self.midi_quiet[t] = 0;
                self.midi_ring_to[t] = 0;
            }
        }
        let live_any =
            live_on && (!self.live_ev.is_empty() || self.live_notes.iter().any(|x| *x > 0));
        // v38 R1: an audition voice in flight (or a held audition note)
        // opens the render path exactly like a physically held key. v46:
        // headless-copy rows and the default pseudo-slot drive it the same
        // way (their quiet tails keep the block alive until they decay).
        let aud_any = !self.aud.is_empty()
            || self.aud_held.iter().any(|h| *h > 0)
            || self.aud_iheld.iter().any(|h| *h > 0)
            || self.aud_isus.iter().any(|s| *s)
            || self.aud_pv_held > 0
            || self.aud_pv_sus
            || !self.aud_pblock.is_empty()
            || self.aud_purge != 0;
        // The PV row only means "audio this block" for blocks that
        // actually reach a render pass.
        self.aud_pv_any = false;
        // v33: with a plugin synth bound to any live track the render path
        // stays OPEN even fully idle: plugins run every block ("constantly
        // listening for all inputs") — their meters, mod/LFO displays and
        // pedal state only advance while process() runs, and a VST2
        // instrument GUI edits the LIVE instance, so a skipped block is a
        // frozen knob. Script synths are exempt: pausing them is free and
        // their sliders are host-side.
        let vst_open = (0..MAX_LOOPS).any(|t| {
            (self.engine.track_on(t) || ((self.aud_mask >> t) & 1) != 0 || self.aud_held[t] > 0)
                && (slot_is_vst(
                    self.synths_cache.get(t).and_then(|o| o.as_ref()),
                    self.dflt_synth.as_ref(),
                )
                    // v46: a poly track's live instance may be silent while
                    // its copies (which can be plugins) run every block.
                    || (self.engine.poly_on(t)
                        && self.poly_cache.get(t).is_some_and(|row| {
                            row.iter().any(|c| {
                                matches!(c.as_deref(), Some(SynthHandle::Vst(_)))
                            })
                        })))
        });
        if render_gate_blocked(block_n, pattern_run, live_any || aud_any, vst_open) {
            return false;
        }
        let sr = self.sr.max(1);
        let span_end = p0 + block_n as u64;
        let arr = self.arr.clone();
        let dflt = self.dflt_synth.clone();
        let engine = &self.engine;
        let mut any = false;
        let Proc {
            midi_ev,
            mono,
            midi_scratch,
            midi_live,
            midi_ring_to,
            midi_sustain,
            midi_quiet,
            synth_p,
            synth_pg,
            synths_cache,
            midi_cache,
            sil,
            evq,
            mono2,
            poly_pg,
            poly_cache,
            own,
            owner,
            live_ev,
            live_notes,
            aud_mask,
            aud_held,
            aud_block,
            aud_iblock,
            aud_iinst,
            aud_iheld,
            aud_isus,
            aud_iquiet,
            aud_pblock,
            aud_pv_inst,
            aud_pv_held,
            aud_pv_sus,
            aud_pv_quiet,
            aud_pv_row,
            aud_pv_any,
            ..
        } = self;
        // v33: pass 0 — shared-handle ownership for this cycle. Tracks
        // rendering through the SAME synth instance (unbound tracks on a
        // shared default synth, or one plugin bound to several strips)
        // must not each push the live feed and each sum the plugin's one
        // output: the doubling choked voice-stealing synths (held notes
        // and pedal sustains died under dense chords) and phase-doubled
        // the mix. First sounding track owns the instance; only owners
        // stage (own + sharers' events, deduplicated) and render.
        own.clear();
        owner.fill(0xFF);
        for (t, owner_slot) in owner.iter_mut().enumerate() {
            // v46: a POLY track never feeds its live instance (its copies
            // render below), so it must not join the shared-handle
            // ownership math — otherwise it would claim the shared
            // default piano as "owner" and stage nothing into it.
            if engine.poly_on(t) && matches!(poly_cache.get(t), Some(row) if !row.is_empty()) {
                continue;
            }
            // v38 R1: an audition target renders even with its play flag
            // OFF — auditioning must answer on ANY track, that is the
            // whole point ("place a note, hear the instrument").
            if !engine.track_on(t) && (((*aud_mask >> t) & 1) == 0 && aud_held[t] <= 0) {
                continue;
            }
            let Some(h) = synths_cache
                .get(t)
                .cloned()
                .flatten()
                .or_else(|| dflt.clone())
            else {
                continue;
            };
            let ptr = Arc::as_ptr(&h) as usize;
            *owner_slot = match own.iter().position(|(p, _)| *p == ptr) {
                Some(i) => own[i].1 as u8,
                None => {
                    own.push((ptr, t));
                    t as u8
                }
            };
        }
        for t in 0..MAX_LOOPS {
            if owner[t] != t as u8 {
                continue; // sharer or no synth: the owner renders
            }
            let handle = match synths_cache
                .get(t)
                .cloned()
                .flatten()
                .or_else(|| dflt.clone())
            {
                Some(i) => i,
                None => continue, // no synth loaded at all: silence
            };
            midi_ev.clear();
            // Ring horizon: the latest sample this instance must keep the
            // synth advancing to (updated from every staged note-off).
            let mut ring = midi_ring_to[t];
            // v33: the owner stages EVERY sharer's events. Pattern
            // staging runs only while the transport table is live (v32:
            // the live feed runs regardless of transport) — and exactly
            // once per instance.
            for t2 in (0..MAX_LOOPS).filter(|x| owner[*x] == t as u8) {
                let live_t = live_on && ((live_mask >> t2) & 1) != 0;
                stage_track_events(
                    t2,
                    &arr,
                    midi_cache,
                    live_ev,
                    live_t,
                    pattern_run,
                    p0,
                    span_end,
                    block_n,
                    sr,
                    midi_ev,
                    &mut ring,
                    None,
                );
                // v38 R1: this track's due audition events ride the SAME
                // merged per-instance stream (the sort/dedup below places
                // them at their exact offsets).
                if !aud_block[t2].is_empty() {
                    midi_ev.extend_from_slice(&aud_block[t2]);
                }
            }
            // (offset, kind) ordering puts offs (kind 0) before ons (kind
            // 3) at the same sample, so a retrigger never kills its own
            // note. v33 `dedup` collapses EXACT duplicates (same offset,
            // kind, ch, data — the signature of the live feed staged via
            // two sharing tracks); distinct events (two patterns' notes)
            // always survive.
            midi_ev.sort_unstable();
            midi_ev.dedup();
            let has_ev = !midi_ev.is_empty();
            // Render this block when it carries staged events OR when the
            // track is still sustaining (a scheduled note rings past this
            // block, or the synth's tail has not decayed yet). Pausing the
            // instrument on event-free blocks is what made every note a
            // one-block blip — the sound between events lives INSIDE the
            // synth (script voices / the plugin's own ring). A physically
            // held live key counts as sustaining too (v32) — on ANY track
            // sharing this instance.
            let holding = (live_on
                && (0..MAX_LOOPS).any(|x| owner[x] == t as u8 && live_notes[x] > 0))
                // v38 R1: a held AUDITION note keeps the instance
                // advancing exactly like a physically held key.
                || (0..MAX_LOOPS).any(|x| owner[x] == t as u8 && aud_held[x] > 0);
            // v33: PLUGIN instruments run EVERY block ("plugins should be
            // constantly listening for all inputs"): a plugin's own GUI
            // state, meters and modulation sources advance only while
            // process() runs, so an idle-skipped plugin looked frozen and
            // mod/pedal state lagged until the next note. Script synths
            // keep the quiet-tail gate — their sliders are host-side and
            // pausing them is free.
            let always = matches!(handle.as_ref(), SynthHandle::Vst(_));
            if !has_ev && !midi_sustain[t] && !holding && !always {
                continue;
            }
            let row = &mut midi_scratch[t];
            if row.len() < block_n {
                row.clear();
                row.resize(block_n, 0.0);
            }
            match handle.as_ref() {
                SynthHandle::Script(inst) => {
                    // Sliders: only refetch when the GUI actually moved one.
                    let gen = inst.param_gen();
                    if gen != synth_pg[t] {
                        inst.read_params(synth_p);
                        synth_pg[t] = gen;
                    }
                    // Prime the instrument with a zero-length block before
                    // delivering any event. A sampler syncs its asset buffer
                    // (and root param) inside process(), so a note staged at
                    // sample 0 delivered FIRST would find an empty buffer
                    // and vanish — the sampler "needs a synth re-selection
                    // to work" bug. One call, no audio, exact params.
                    if has_ev {
                        unsafe { inst.process_into(&mut [], synth_p) };
                    }
                    mono.clear();
                    mono.resize(block_n, 0.0);
                    let mut cur = 0usize;
                    let mut i = 0usize;
                    while i < midi_ev.len() {
                        let off = midi_ev[i].0.min(block_n);
                        if off > cur {
                            // SAFETY: single-threaded here; one instance, one call.
                            unsafe { inst.process_into(&mut mono[cur..off], synth_p) };
                            cur = off;
                        }
                        while i < midi_ev.len() && midi_ev[i].0 == off {
                            let (_, kind, ch, d1, d2) = midi_ev[i];
                            match kind {
                                0 => inst.note_off(ch, d1),
                                1 => inst.cc(ch, d1, d2),
                                2 => inst.press(ch, d2),
                                // v42/MPE: scripted synth ABI v1 has no
                                // bend/poly-AT callbacks — drop cleanly.
                                4 | 5 => {}
                                _ => inst.note_on(ch, d1, d2),
                            }
                            i += 1;
                        }
                    }
                    if cur < block_n {
                        unsafe { inst.process_into(&mut mono[cur..block_n], synth_p) };
                    }
                    for (r, m) in row[..block_n].iter_mut().zip(mono[..block_n].iter()) {
                        *r += *m;
                    }
                }
                SynthHandle::Vst(e) => {
                    // v33: ONE block call carrying every event at its
                    // exact sample position (was: process() split at each
                    // event boundary, all deltas 0 — fast retriggers and
                    // dense chords arrived smeared to internal-block
                    // starts). Silence drives generation; the plugin's
                    // ring and tail stay live.
                    sil.clear();
                    sil.resize(block_n, 0.0);
                    evq.clear();
                    for &(_off, kind, ch, d1, d2) in midi_ev.iter() {
                        let m = match kind {
                            0 => [0x80 | ch & 0x0F, d1 & 0x7F, 0],
                            1 => [0xB0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                            2 => [0xD0 | ch & 0x0F, d2 & 0x7F, 0],
                            // v42/MPE: per-note aftertouch and 14-bit bend
                            // (d1 = LSB, d2 = MSB) ride straight into the
                            // entry's raw-message queue.
                            4 => [0xA0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                            5 => [0xE0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                            _ => [0x90 | ch & 0x0F, d1 & 0x7F, d2.clamp(1, 127)],
                        };
                        evq.push(((_off).min(block_n), m));
                    }
                    e.process_with_midi(&sil[..block_n], &mut row[..block_n], evq);
                }
            }
            // Always-running plugins may GENERATE without events (drones,
            // arpeggiators): open the mix slot on audible output only, so
            // silent idling costs the sample loop nothing.
            let sounding = !always
                || has_ev
                || midi_sustain[t]
                || holding
                || row[..block_n].iter().fold(0.0f32, |m, x| m.max(x.abs())) >= MIDI_SILENCE_FLOOR;
            if sounding {
                midi_live[t] = true;
                any = true;
            }
            // Sustain bookkeeping. Keep advancing while the schedule rings
            // past this block; once nothing is scheduled, stop only after a
            // few near-silent rendered blocks, so the instrument's release
            // tail plays out fully but a dead track costs no CPU. (For
            // always-running plugins these flags only steer the mix-slot
            // decision above — rendering never stops.)
            midi_ring_to[t] = ring;
            if has_ev {
                midi_sustain[t] = true;
                midi_quiet[t] = 0;
            }
            // Quiet-tail decay runs only once nothing is held physically:
            // a key-down must never time out mid-press (v32). v38: a held
            // audition note counts the same way.
            if ring <= span_end && live_notes[t].max(0) == 0 && aud_held[t] <= 0 {
                let peak = row[..block_n].iter().fold(0.0f32, |m, x| m.max(x.abs()));
                if peak < MIDI_SILENCE_FLOOR {
                    midi_quiet[t] = midi_quiet[t].saturating_add(1);
                    if midi_quiet[t] >= MIDI_QUIET_BLOCKS {
                        midi_sustain[t] = false;
                        midi_quiet[t] = 0;
                    }
                } else {
                    midi_quiet[t] = 0;
                }
            }
        }
        // =============== v46 POLY: one synth copy per child lane =========
        // A poly track bypasses the shared-instance map above entirely:
        // every child lane renders through its own headless copy of the
        // configured instrument, and the copies' mono outputs sum into
        // the track's scratch row — so the track's chain, gain, pan and
        // metering are untouched. Lanes past the copy row share copy 0
        // (the historic merged stream); the live keyboard feed and legacy
        // audition events land on copy 0. The live instance is never fed.
        for t in 0..MAX_LOOPS {
            if !engine.poly_on(t) {
                continue;
            }
            let mut copies: Vec<Option<Arc<SynthHandle>>> =
                poly_cache.get(t).cloned().unwrap_or_default();
            if copies.is_empty() {
                continue;
            }
            // Keep script copies honest: the LIVE instance owns the
            // sliders, so push its param generation onto every copy
            // (cheap atomic pushes; VST copies resync from their own
            // host-visible mirrors at spawn and on rebinding).
            if let Some(live_h) = synths_cache
                .get(t)
                .cloned()
                .flatten()
                .or_else(|| dflt.clone())
            {
                if let SynthHandle::Script(live_i) = live_h.as_ref() {
                    let g = live_i.param_gen();
                    if g != poly_pg[t] {
                        let mut pv = [0.0f32; crate::synth::MAX_PARAMS];
                        live_i.read_params(&mut pv);
                        for c in copies.iter_mut().flatten() {
                            if let SynthHandle::Script(ci) = c.as_ref() {
                                for (k, v) in pv.iter().enumerate() {
                                    ci.set_param(k, *v);
                                }
                            }
                        }
                        poly_pg[t] = g;
                    }
                }
            }
            let n_cp = copies.len().min(32);
            let n_lanes = arr.tracks[t].lanes.len();
            let live_t = live_on && ((live_mask >> t) & 1) != 0;
            let holding = (live_on && live_notes[t] > 0) || aud_held[t] > 0;
            let always = copies
                .iter()
                .any(|c| matches!(c.as_deref(), Some(SynthHandle::Vst(_))));
            let mut ring = 0u64;
            let mut has_ev = false;
            let row = &mut midi_scratch[t];
            if row.len() < block_n {
                row.clear();
                row.resize(block_n, 0.0);
            }
            for (ci, copy) in copies.iter_mut().take(n_cp).enumerate() {
                let _ = &copy;
                midi_ev.clear();
                let mut cring = 0u64;
                if pattern_run {
                    for li in 0..n_lanes {
                        let target = if li < n_cp { li } else { 0 };
                        if target != ci {
                            continue;
                        }
                        stage_track_events(
                            t,
                            &arr,
                            midi_cache,
                            &[],
                            false,
                            true,
                            p0,
                            span_end,
                            block_n,
                            sr,
                            midi_ev,
                            &mut cring,
                            Some(li),
                        );
                    }
                }
                if ci == 0 {
                    if live_t {
                        stage_track_events(
                            t, &arr, midi_cache, live_ev, true, false, p0, span_end, block_n, sr,
                            midi_ev, &mut cring, None,
                        );
                    }
                    if !aud_block[t].is_empty() {
                        midi_ev.extend_from_slice(&aud_block[t]);
                    }
                }
                let cp_ev = !midi_ev.is_empty();
                has_ev |= cp_ev;
                ring = ring.max(cring);
                if !cp_ev && !always && !midi_sustain[t] && !holding {
                    continue;
                }
                let handle = match copy.clone().or_else(|| {
                    // Only copy 0 may fall back to the live handle (a
                    // spawn failure keeps the lane audible as a merged
                    // stream); a SECOND copy rendering through the live
                    // instance would stage its events twice.
                    if ci == 0 {
                        synths_cache
                            .get(t)
                            .cloned()
                            .flatten()
                            .or_else(|| dflt.clone())
                    } else {
                        None
                    }
                }) {
                    Some(h) => h,
                    None => continue,
                };
                midi_ev.sort_unstable();
                midi_ev.dedup();
                match handle.as_ref() {
                    SynthHandle::Script(inst) => {
                        let gen = inst.param_gen();
                        if gen != synth_pg[t] {
                            inst.read_params(synth_p);
                            synth_pg[t] = gen;
                        }
                        if cp_ev {
                            unsafe { inst.process_into(&mut [], synth_p) };
                        }
                        mono.clear();
                        mono.resize(block_n, 0.0);
                        let mut cur = 0usize;
                        let mut i = 0usize;
                        while i < midi_ev.len() {
                            let off = midi_ev[i].0.min(block_n);
                            if off > cur {
                                // SAFETY: single-threaded here; one instance, one call.
                                unsafe { inst.process_into(&mut mono[cur..off], synth_p) };
                                cur = off;
                            }
                            while i < midi_ev.len() && midi_ev[i].0 == off {
                                let (_, kind, ch, d1, d2) = midi_ev[i];
                                match kind {
                                    0 => inst.note_off(ch, d1),
                                    1 => inst.cc(ch, d1, d2),
                                    2 => inst.press(ch, d2),
                                    4 | 5 => {}
                                    _ => inst.note_on(ch, d1, d2),
                                }
                                i += 1;
                            }
                        }
                        if cur < block_n {
                            unsafe { inst.process_into(&mut mono[cur..block_n], synth_p) };
                        }
                        for (r, m) in row[..block_n].iter_mut().zip(mono[..block_n].iter()) {
                            *r += *m;
                        }
                    }
                    SynthHandle::Vst(e) => {
                        sil.clear();
                        sil.resize(block_n, 0.0);
                        mono2.clear();
                        mono2.resize(block_n, 0.0);
                        evq.clear();
                        for &(_off, kind, ch, d1, d2) in midi_ev.iter() {
                            let m = match kind {
                                0 => [0x80 | ch & 0x0F, d1 & 0x7F, 0],
                                1 => [0xB0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                                2 => [0xD0 | ch & 0x0F, d2 & 0x7F, 0],
                                4 => [0xA0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                                5 => [0xE0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                                _ => [0x90 | ch & 0x0F, d1 & 0x7F, d2.clamp(1, 127)],
                            };
                            evq.push(((_off).min(block_n), m));
                        }
                        e.process_with_midi(&sil[..block_n], &mut mono2[..block_n], evq);
                        for (r, m) in row[..block_n].iter_mut().zip(mono2[..block_n].iter()) {
                            *r += *m;
                        }
                    }
                }
            }
            let peak = row[..block_n].iter().fold(0.0f32, |m, x| m.max(x.abs()));
            if has_ev || midi_sustain[t] || holding || peak >= MIDI_SILENCE_FLOOR {
                midi_live[t] = true;
                any = true;
            }
            midi_ring_to[t] = ring;
            if has_ev {
                midi_sustain[t] = true;
                midi_quiet[t] = 0;
            }
            if ring <= span_end && live_notes[t].max(0) == 0 && aud_held[t] <= 0 {
                if peak < MIDI_SILENCE_FLOOR {
                    midi_quiet[t] = midi_quiet[t].saturating_add(1);
                    if midi_quiet[t] >= MIDI_QUIET_BLOCKS {
                        midi_sustain[t] = false;
                        midi_quiet[t] = 0;
                    }
                } else {
                    midi_quiet[t] = 0;
                }
            }
        }
        // =============== v46 headless audition copies =====================
        // Voices carrying their own synth copy render HERE, never in the
        // owner map above: the copy is fed nothing but that row's events,
        // so an audition can never touch the instance the clips and the
        // live keyboard play. Its mono output sums into the track's scratch
        // row afterwards, so it still rides that track's chain/gain/pan.
        for t in 0..MAX_LOOPS {
            let has_ev = !aud_iblock[t].is_empty();
            let held = aud_iheld[t];
            if !has_ev && held <= 0 && !aud_isus[t] {
                aud_iinst[t] = None; // row finished: release the copy
                continue;
            }
            let Some(handle) = aud_iinst[t].clone() else {
                aud_iinst[t] = None;
                continue;
            };
            let mut ev = std::mem::take(&mut aud_iblock[t]);
            ev.sort_unstable();
            let row = &mut midi_scratch[t];
            if row.len() < block_n {
                row.clear();
                row.resize(block_n, 0.0);
            }
            mono.clear();
            mono.resize(block_n, 0.0);
            match handle.as_ref() {
                SynthHandle::Script(inst) => {
                    inst.read_params(synth_p);
                    if has_ev {
                        // Prime like the live pass (sampler asset sync).
                        unsafe { inst.process_into(&mut [], synth_p) };
                    }
                    let mut cur = 0usize;
                    let mut i = 0usize;
                    while i < ev.len() {
                        let off = ev[i].0.min(block_n);
                        if off > cur {
                            unsafe { inst.process_into(&mut mono[cur..off], synth_p) };
                            cur = off;
                        }
                        while i < ev.len() && ev[i].0 == off {
                            let (_, kind, ch, d1, d2) = ev[i];
                            match kind {
                                0 => inst.note_off(ch, d1),
                                1 => inst.cc(ch, d1, d2),
                                2 => inst.press(ch, d2),
                                4 | 5 => {}
                                _ => inst.note_on(ch, d1, d2),
                            }
                            i += 1;
                        }
                    }
                    if cur < block_n {
                        unsafe { inst.process_into(&mut mono[cur..block_n], synth_p) };
                    }
                }
                SynthHandle::Vst(e) => {
                    sil.clear();
                    sil.resize(block_n, 0.0);
                    evq.clear();
                    for &(off, kind, ch, d1, d2) in ev.iter() {
                        let m = match kind {
                            0 => [0x80 | ch & 0x0F, d1 & 0x7F, 0],
                            1 => [0xB0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                            2 => [0xD0 | ch & 0x0F, d2 & 0x7F, 0],
                            4 => [0xA0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                            5 => [0xE0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                            _ => [0x90 | ch & 0x0F, d1 & 0x7F, d2.clamp(1, 127)],
                        };
                        evq.push((off.min(block_n), m));
                    }
                    e.process_with_midi(&sil[..block_n], &mut mono[..block_n], evq);
                }
            }
            let mut peak = 0.0f32;
            for (r, m) in row[..block_n].iter_mut().zip(mono[..block_n].iter()) {
                *r += *m;
                let a = m.abs();
                if a > peak {
                    peak = a;
                }
            }
            if peak >= MIDI_SILENCE_FLOOR {
                midi_live[t] = true;
                any = true;
            }
            // Copy-row sustain: keep advancing while its own notes are
            // held, and decay out with the same quiet-tail rule the live
            // pass uses (piano damper time, no infinite spin-up).
            if has_ev {
                aud_isus[t] = true;
                aud_iquiet[t] = 0;
            }
            if held <= 0 && !has_ev {
                if peak >= MIDI_SILENCE_FLOOR {
                    aud_iquiet[t] = 0;
                } else {
                    aud_iquiet[t] = aud_iquiet[t].saturating_add(1);
                    if aud_iquiet[t] >= MIDI_QUIET_BLOCKS {
                        aud_isus[t] = false;
                        aud_iquiet[t] = 0;
                        aud_iinst[t] = None;
                    }
                }
            }
        }
        // v46: the DEFAULT (no-track) audition pseudo-slot. Same private-
        // copy rendering, but the mono row mixes dead-centre into the
        // master in the sample loop (like an asset preview) — no track bus,
        // no mixer, no synth of any kind is involved.
        {
            let has_ev = !aud_pblock.is_empty();
            let live_pv = has_ev || *aud_pv_held > 0 || *aud_pv_sus;
            *aud_pv_any = false;
            if live_pv {
                if let Some(handle) = aud_pv_inst.clone() {
                    let mut ev = std::mem::take(aud_pblock);
                    ev.sort_unstable();
                    aud_pv_row.clear();
                    aud_pv_row.resize(block_n, 0.0);
                    match handle.as_ref() {
                        SynthHandle::Script(inst) => {
                            inst.read_params(synth_p);
                            if has_ev {
                                unsafe { inst.process_into(&mut [], synth_p) };
                            }
                            let mut cur = 0usize;
                            let mut i = 0usize;
                            while i < ev.len() {
                                let off = ev[i].0.min(block_n);
                                if off > cur {
                                    unsafe {
                                        inst.process_into(&mut aud_pv_row[cur..off], synth_p)
                                    };
                                    cur = off;
                                }
                                while i < ev.len() && ev[i].0 == off {
                                    let (_, kind, ch, d1, d2) = ev[i];
                                    match kind {
                                        0 => inst.note_off(ch, d1),
                                        1 => inst.cc(ch, d1, d2),
                                        2 => inst.press(ch, d2),
                                        4 | 5 => {}
                                        _ => inst.note_on(ch, d1, d2),
                                    }
                                    i += 1;
                                }
                            }
                            if cur < block_n {
                                unsafe {
                                    inst.process_into(&mut aud_pv_row[cur..block_n], synth_p)
                                };
                            }
                        }
                        SynthHandle::Vst(e) => {
                            sil.clear();
                            sil.resize(block_n, 0.0);
                            evq.clear();
                            for &(off, kind, ch, d1, d2) in ev.iter() {
                                let m = match kind {
                                    0 => [0x80 | ch & 0x0F, d1 & 0x7F, 0],
                                    1 => [0xB0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                                    2 => [0xD0 | ch & 0x0F, d2 & 0x7F, 0],
                                    4 => [0xA0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                                    5 => [0xE0 | ch & 0x0F, d1 & 0x7F, d2 & 0x7F],
                                    _ => [0x90 | ch & 0x0F, d1 & 0x7F, d2.clamp(1, 127)],
                                };
                                evq.push((off.min(block_n), m));
                            }
                            e.process_with_midi(&sil[..block_n], &mut aud_pv_row[..block_n], evq);
                        }
                    }
                    let peak = aud_pv_row[..block_n]
                        .iter()
                        .fold(0.0f32, |m, x| m.max(x.abs()));
                    *aud_pv_any = true;
                    if has_ev {
                        *aud_pv_sus = true;
                        *aud_pv_quiet = 0;
                    }
                    if *aud_pv_held <= 0 && !has_ev {
                        if peak >= MIDI_SILENCE_FLOOR {
                            *aud_pv_quiet = 0;
                        } else {
                            *aud_pv_quiet = aud_pv_quiet.saturating_add(1);
                            if *aud_pv_quiet >= MIDI_QUIET_BLOCKS {
                                *aud_pv_sus = false;
                                *aud_pv_quiet = 0;
                            }
                        }
                    }
                }
            }
        }
        // v33: the live feed is CONSUMED here — an early return above
        // (idle gate, zero-length block) leaves it staged, and
        // `Proc::process` carries it into the next cycle instead of
        // dropping the player's notes.
        live_ev.clear();
        any
    }
}

/// One staged audition row while a block renders:
/// `(sample offset in block, status kind, channel, data1, data2)` — the
/// exact shape `render_midi_block` consumes from `aud_block` (usize
/// offset, not the u64 voice-queue offset of `MidiEv`).
type AudRow = (usize, u8, u8, u8, u8);

/// Realtime process handler. No allocation, no locks, no I/O: the slot pool
/// is fixed at startup and its raw port pointers are cached here; per-cycle
/// work is a handful of atomic loads plus one pass over the arrangement's
/// clips to build this block's entry list. The global transport advances the
/// playhead (or follows seeks), clips are summed where the playhead crosses
/// them, and takes capture raw input inline while armed.
struct Proc {
    /// WINDOWS-PORT W4: scratch the JACK shim materializes this cycle's MIDI
    /// events into (borrowed `RawMidi` → owned) before handing off to the
    /// backend-neutral `process_block`. Kept on `Proc` so the outer buffer
    /// never reallocs block-to-block; the per-event `Vec<u8>` count equals the
    /// old inline `e.bytes.to_vec()` exactly. (Distinct from `midi_scratch`,
    /// the per-track synth sample buffer.)
    mask_in: Arc<AtomicU64>,
    mask_out_l: Arc<AtomicU64>,
    mask_out_r: Arc<AtomicU64>,
    tx: SyncSender<Evt>,
    midi_drops: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    /// v39: master-out tap ring (post-master-gain stereo → GUI meter).
    meter: Arc<crate::meter::MeterRing>,
    /// v39: scratch buffer holding one block's interleaved tap pairs.
    tap_buf: Vec<f32>,
    engine: Arc<Engine>,
    // RT-local take-capture state (audio thread only).
    prev_rec: [u8; MAX_LOOPS],
    /// v42 "p": the transport ran at least once since the current preview
    /// take armed — only then does a stopped transport COMMIT the preview
    /// (a take armed while stopped must not instantly self-commit empty).
    pv_seen: bool,
    /// v42: loop-record pass hand-off: sample position at which track t
    /// resumes capture after a wrap-commit (u64::MAX = nothing pending).
    lr_restart: [u64; MAX_LOOPS],
    /// Tracks that hit LOOP_CAP while recording: suppresses the auto-stop
    /// state store from re-firing on every remaining sample of the block.
    /// Cleared on the track's next state edge.
    rec_full: u128,
    /// RT-local playhead (the engine atomic is the published copy).
    pos: u64,
    /// Last transport flag; the falling edge publishes an auto-stop event.
    prev_transport: bool,
    /// Metronome flag of the previous block (activation edge, below).
    prev_metro: bool,
    /// Engine sample rate (for metronome math).
    sr: u32,
    /// Metronome clock: the grid index (click number on the timeline) last
    /// emitted (-1 = nothing yet, so a transport start on a grid line clicks
    /// immediately), the absolute sample the current click started at
    /// (-1 = idle) and its tone (0 = downbeat, 1 = quarter, 2 = subdivision).
    metro_idx: i64,
    metro_start: i64,
    metro_tone: u8,
    /// Free-running metronome position (samples). While the transport runs
    /// the click ticks off the transport position; with the transport
    /// stopped the click must keep ticking WITHOUT moving the playhead, so
    /// it counts on this clock instead. Re-anchored to the playhead on
    /// seeks/activation, and synced to the playhead on every transport
    /// block, so switching states never bursts or jumps the beat.
    metro_clock: f64,
    /// Absolute sample counter (click envelope clock).
    sample_no: u64,
    /// Cached effect-chain table; refreshed from the engine when fx_gen
    /// moves (rare, user-driven — the only allocation left on this thread).
    fx_cache: Arc<[TrackFx; MAX_LOOPS]>,
    fx_cache_gen: u64,
    /// Tier-6 ABI v4: block-start transport snapshot (refreshed once per
    /// RT block after the automation pass — the automation tempo rides
    /// `metro_bpm`, so read it AFTER that pass baked the value in).
    sync: crate::fx::SyncInfoV1,
    /// Bumped on every `sync` refresh; per-slot `sync_seen` lags it, so
    /// each scripted instance gets exactly one push per block, lazily.
    sync_gen: u64,
    /// Per-track, per-slot cached slider snapshots (+ the instance
    /// generation they were read at), refreshed per block on change.
    fx_rt: Vec<Vec<FxRt>>,
    /// Cached project snapshots; refreshed when either generation moves.
    arr: Arc<Arrangement>,
    arr_cache_gen: u64,
    assets: Arc<AssetTable>,
    assets_cache_gen: u64,
    /// Asset preview: cached audition buffer (refreshed on preview_gen),
    /// its RT read cursor, and whether one plays this block.
    preview: PreviewVoices,
    preview_cache_gen: u64,
    /// RT read cursors, (voice id, sample index), kept across generation
    /// bumps so surviving voices never restart when a new one joins.
    preview_curs: Vec<(u64, usize)>,
    preview_live: bool,
    /// Reusable per-block clip window list (taken out of self during the
    /// block so the sample loop can call `&mut self` effect code freely).
    clip_buf: Vec<ClipEntry>,
    // ---- MIDI playback ------------------------------------------------------
    synths_cache: Arc<[Option<Arc<SynthHandle>>; MAX_LOOPS]>,
    synths_cache_gen: u64,
    dflt_synth: Option<Arc<SynthHandle>>,
    /// Per-block pre-master mix. Tracks whose chain hosts a plugin cannot
    /// use the per-sample path (a chain runs block-wise), so the block mix
    /// lands here and the port writes move to a post-loop pass — for every
    /// block, uniform path.
    mix_l: Vec<f32>,
    mix_r: Vec<f32>,
    /// Held-back track sums (tracks with a plugin in the chain) and monitor
    /// tap; plus the VST scratch buffer for block-wise chain passes.
    trk_bufs: Vec<Vec<f32>>,
    /// Tier-3: right halves of the held-back track sums (and the monitor
    /// tap). Only sized for tracks whose chain ran on this block; mono
    /// chains fill it identically to the left, stereo slots split it.
    trk_bufs2: Vec<Vec<f32>>,
    /// v32 R5: sidechain bus. `sc_apply[t]` is what track `t`'s scripted
    /// effects read THIS block (the previous block's source sum — one
    /// block of detection latency, see docs/SIDECHAIN.md); `sc_build[t]`
    /// accumulates this block's taps for the next one. `sc_active[t]`
    /// precomputes "mask != 0" so the sample loop stays branch-cheap.
    sc_apply: Vec<Vec<f32>>,
    sc_build: Vec<Vec<f32>>,
    sc_active: [bool; MAX_LOOPS],
    mon_buf: Vec<f32>,
    mon_buf2: Vec<f32>,
    vst_tmp: Vec<f32>,
    /// v0.11 R5: automation asset table refresh pair.
    auto_cache: Arc<Vec<Option<Arc<crate::automation::Automation>>>>,
    auto_cache_gen: u64,
    /// v34 R11: last control-rate pass; held while the transport is
    /// stopped so automation does not snap back on pause.
    auto_last: AutoOut,
    /// MIDI pattern table (parallel to `assets`; refreshed on `midi_gen`).
    midi_cache: Arc<Vec<Option<Arc<crate::midipat::Pattern>>>>,
    midi_cache_gen: u64,
    /// Events staged for the current block, sorted by (offset, kind):
    /// (sample offset, kind 0=note-off 1=CC 2=pressure 3=note-on, ch, d1, d2).
    midi_ev: Vec<(usize, u8, u8, u8, u8)>,
    /// Per-track synth render output for this block (mixed into `acc`).
    midi_scratch: Vec<Vec<f32>>,
    midi_live: [bool; MAX_LOOPS],
    /// Latest staged note-off per track (timeline samples): the synth keeps
    /// rendering while the schedule is still ringing. The note sounds
    /// INSIDE the instrument between events, so event-free blocks must not
    /// pause it — skipping them rendered every sustained note as a blip.
    midi_ring_to: [u64; MAX_LOOPS],
    /// Track's synth is still advancing (until its tail falls silent).
    midi_sustain: [bool; MAX_LOOPS],
    /// Consecutive rendered near-silent blocks with nothing scheduled:
    /// this many and the synth stops advancing until the next note.
    midi_quiet: [u32; MAX_LOOPS],
    /// Per-track cached synth param snapshot + change counter.
    synth_p: [f32; crate::synth::MAX_PARAMS],
    synth_pg: [u64; MAX_LOOPS],
    /// Reusable mono render buffer for one track's synth block.
    mono: Vec<f32>,
    /// v46: second mono buffer — the POLY pass renders one synth copy at
    /// a time into it and sums into the track row.
    mono2: Vec<f32>,
    /// v46: per-track snapshot of the LIVE instance's script param
    /// generation; when it moves, every script copy of that track gets
    /// the new values pushed (copies stay honest to the patch you dialed).
    poly_pg: [u64; MAX_LOOPS],
    /// v46: RT snapshot of the engine's per-track poly copy rows
    /// (refreshed on `poly_gen`, gen-swap like every other table).
    poly_cache: PolyTable,
    poly_cache_gen: u64,
    /// Reusable zero input for driving a VSTi with no audio side-chain.
    sil: Vec<f32>,
    /// v33: (block offset, status message) stream encoded for
    /// `RtStageEntry::process_with_midi` — one call per block, events at
    /// their true sample positions.
    evq: Vec<(usize, [u8; 3])>,
    /// v33: shared-synth-handle identity for THIS cycle: `(Arc ptr, owner
    /// track)` pairs, rebuilt from scratch every block (capacity lives on
    /// after the first one). Two tracks rendering through one plugin
    /// instance must feed it ONE merged event stream — the doubling this
    /// replaces is what choked voice-stealing synths under dense chords.
    own: Vec<(usize, usize)>,
    /// v33: per-track owner map for `own` (track index of the handle
    /// owner; 0xFF = track has no synth this cycle).
    owner: [u8; MAX_LOOPS],
    /// v32 live MIDI: this block's UNBOUND feedable messages staged as
    /// `(block offset, kind, ch, d1, d2)` in the pattern event encoding.
    /// Cleared at block entry by the port reader; capacity is reserved at
    /// init, the 1024/block cap silently drops pathological floods only.
    live_ev: Vec<(usize, u8, u8, u8, u8)>,
    /// v32 live MIDI: physically held keys per track (note-on counted when
    /// a message is staged, note-off decrements). While > 0 the synth must
    /// keep advancing event-free blocks — a held key is a ringing key.
    live_notes: [i32; MAX_LOOPS],
    /// v38 R1: live audition voices `(request, cursor-sample)`, refreshed
    /// on `aud_gen` bumps exactly like `preview_curs` does for asset
    /// previews. Cursors advance with the RT block clock, so auditions run
    /// with the transport stopped.
    aud: Vec<(crate::audio::AudVoiceReq, u64)>,
    aud_seen_gen: u64,
    /// Staged-but-not-released AUDITION notes per track. Like
    /// `live_notes` a held audition note keeps the synth advancing and the
    /// transport-stop cleanup from killing it mid-voice.
    aud_held: [i32; MAX_LOOPS],
    /// Tracks with audition events due THIS block (drives owner-map and
    /// per-track event merge; rows are cleared for the tracks it names).
    aud_mask: u128,
    /// Per-track staging rows for `aud_mask` (capacity lives after the
    /// first touch; cleared per block for the rows used).
    aud_block: Vec<Vec<AudRow>>,
    /// Tracks whose held audition notes were purged (pool replaced) —
    /// `render_midi_block` all-offs them once.
    aud_purge: u128,
    /// v46: STAGED-COPY audition rows — voices that carry their own
    /// headless synth copy. Events stage into `aud_iblock` (never merged
    /// into the track's live synth stream), `aud_iinst` names the copy the
    /// row renders through (youngest voice's), `aud_iheld` counts its held
    /// notes, and `aud_isus`/`aud_iquiet` are the copy-row's own sustain
    /// bookkeeping (mirrors `midi_sustain`/`midi_quiet` for the live path).
    /// The rendered mono row sums into `midi_scratch[t]` AFTER the live
    /// synth pass, so the copy's audio still rides the track's chain.
    aud_iblock: Vec<Vec<AudRow>>,
    aud_iinst: Vec<Option<Arc<SynthHandle>>>,
    aud_iheld: [i32; MAX_LOOPS],
    aud_isus: [bool; MAX_LOOPS],
    aud_iquiet: [u32; MAX_LOOPS],
    /// v46: the DEFAULT (no-track) audition pseudo-slot (`AUD_PV`):
    /// renders through `aud_pv_inst`, mixes dead-centre into the master
    /// like an asset preview. `aud_pv_any` mirrors "the row has audio this
    /// block" for the sample loop.
    aud_pblock: Vec<AudRow>,
    aud_pv_inst: Option<Arc<SynthHandle>>,
    aud_pv_held: i32,
    aud_pv_sus: bool,
    aud_pv_quiet: u32,
    aud_pv_row: Vec<f32>,
    aud_pv_any: bool,
    /// v32: last bound-set snapshot + its generation (try_lock refresh).
    bound_cache: Arc<BoundSet>,
    bound_gen: u64,
    /// v32 R7: block-edge gain smoothing. Every gain multiplied per
    /// sample (track amp, monitor amp, master gain) linearly ramps from
    /// this block-start value to the new one across the block; a hard
    /// step is what the user hears as "static" at loop start/stop and on
    /// volume jumps. These hold the value at the START of each block.
    amp_env: [f32; MAX_LOOPS],
    mon_env: f32,
    g_env: f32,
}

/// Build a fresh engine-side realtime handler from the manager-thread
/// handles (WINDOWS-PORT W5 extraction). Backend-neutral by construction:
/// the jack shim wraps the result in its `JackProc` plumbing struct; the
/// WASAPI shim wraps it the same way with device buffers. `sr` is the
/// live device rate (already pushed into the engine via set_sample_rate).
#[allow(clippy::too_many_arguments)] // Proc construction context
fn new_proc(
    evt_tx: SyncSender<Evt>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    meter: Arc<crate::meter::MeterRing>,
    midi_drops: Arc<AtomicU64>,
    mask_in: Arc<AtomicU64>,
    mask_out_l: Arc<AtomicU64>,
    mask_out_r: Arc<AtomicU64>,
    engine: Arc<Engine>,
    sr: u32,
) -> Proc {
    Proc {
        mask_in: mask_in.clone(),
        mask_out_l: mask_out_l.clone(),
        mask_out_r: mask_out_r.clone(),
        tx: evt_tx.clone(),
        midi_drops,
        gain: gain.clone(),
        peak: peak.clone(),
        meter,
        tap_buf: Vec::new(),
        engine: engine.clone(),
        prev_rec: [TR_IDLE; MAX_LOOPS],
        rec_full: 0,
        pv_seen: false,
        lr_restart: [u64::MAX; MAX_LOOPS],
        pos: 0,
        prev_transport: false,
        prev_metro: false,
        sr,
        metro_idx: 0i64,
        metro_start: -1,
        metro_tone: 0,
        metro_clock: 0.0,
        sample_no: 0,
        fx_cache: engine.fx_table(),
        fx_cache_gen: engine.fx_gen(),
        sync: crate::fx::SyncInfoV1::idle(),
        sync_gen: 0,
        // Seed from the engine's current chains: a launch that restores
        // chains from config never bumps fx_gen, so waiting for a refresh
        // here left an empty grid beside a non-empty cache → RT panic.
        fx_rt: seed_fx_rt(&engine.fx_table()[..]),
        arr: engine.arrangement(),
        arr_cache_gen: engine.arr_gen(),
        assets: engine.assets(),
        assets_cache_gen: engine.assets_gen(),
        preview: engine.preview_snapshot(),
        preview_cache_gen: engine.preview_gen(),
        preview_curs: Vec::new(),
        preview_live: false,
        clip_buf: Vec::new(),
        synths_cache: engine.synths(),
        synths_cache_gen: engine.synths_gen(),
        mix_l: Vec::new(),
        mix_r: Vec::new(),
        trk_bufs: vec![Vec::new(); MAX_LOOPS],
        trk_bufs2: vec![Vec::new(); MAX_LOOPS],
        sc_apply: vec![Vec::new(); MAX_LOOPS],
        sc_build: vec![Vec::new(); MAX_LOOPS],
        sc_active: [false; MAX_LOOPS],
        mon_buf: Vec::new(),
        mon_buf2: Vec::new(),
        vst_tmp: Vec::new(),
        dflt_synth: engine.default_synth(),
        midi_cache: engine.midi(),
        midi_cache_gen: engine.midi_gen(),
        auto_cache: engine.auto_assets(),
        auto_last: AutoOut::default(),
        auto_cache_gen: engine.auto_gen(),
        midi_ev: Vec::new(),
        // One row per track, EXACTLY like trk_bufs: `render_midi_block`
        // indexes `midi_scratch[t]` the moment the first note stages, and an
        // empty vector there aborted the RT thread (v0.6/v0.7 MIDI crash).
        midi_scratch: vec![Vec::new(); MAX_LOOPS],
        midi_live: [false; MAX_LOOPS],
        midi_ring_to: [0u64; MAX_LOOPS],
        midi_sustain: [false; MAX_LOOPS],
        midi_quiet: [0u32; MAX_LOOPS],
        synth_p: [0.0f32; crate::synth::MAX_PARAMS],
        synth_pg: [0u64; MAX_LOOPS],
        mono: Vec::new(),
        mono2: Vec::new(),
        poly_pg: [0u64; MAX_LOOPS],
        poly_cache: engine.poly_voices(),
        poly_cache_gen: engine.poly_gen(),
        sil: Vec::new(),
        evq: Vec::new(),
        own: Vec::new(),
        owner: [0xFFu8; MAX_LOOPS],
        // v32: seeded like every other per-block buffer — the port reader
        // pushes into `live_ev` on the FIRST message the app ever sees.
        live_ev: Vec::with_capacity(64),
        live_notes: [0i32; MAX_LOOPS],
        // v38 R1: audition voices start empty; the engine's snapshot is
        // taken on the first generation bump like every other table.
        aud: Vec::new(),
        aud_seen_gen: engine.aud_gen(),
        aud_held: [0i32; MAX_LOOPS],
        aud_mask: 0,
        aud_block: vec![Vec::new(); MAX_LOOPS],
        aud_purge: 0,
        aud_iblock: vec![Vec::new(); MAX_LOOPS],
        aud_iinst: vec![None; MAX_LOOPS],
        aud_iheld: [0i32; MAX_LOOPS],
        aud_isus: [false; MAX_LOOPS],
        aud_iquiet: [0u32; MAX_LOOPS],
        aud_pblock: Vec::new(),
        aud_pv_inst: None,
        aud_pv_held: 0,
        aud_pv_sus: false,
        aud_pv_quiet: 0,
        aud_pv_row: Vec::new(),
        aud_pv_any: false,
        bound_cache: engine.midi_bound(),
        bound_gen: engine.midi_bound_gen(),
        // v32 R7: start at the default unity controls (no boot ramp).
        amp_env: [1.0; MAX_LOOPS],
        mon_env: 1.0,
        g_env: 1.0,
    }
}

/// RT-side snapshot of one effect instance's sliders.
struct FxRt {
    gen: u64,
    p: [f32; crate::fx::MAX_PARAMS],
    /// Tier-6: `Proc.sync_gen` this slot last received a block-sync push
    /// at (ABI v4 scripts are fed once per block, lazily, on first use).
    sync_seen: u64,
}

/// Build the per-slot RT slider grid matching `table` (one row per track,
/// one `FxRt` per chain slot). Proc seeding AND the `process` refresh must
/// both use this: an empty grid next to a non-empty `fx_cache` makes
/// `fx_rt[t]` index out of bounds the first time such a track sounds.
fn seed_fx_rt(table: &[TrackFx]) -> Vec<Vec<FxRt>> {
    table
        .iter()
        .map(|tf| {
            tf.slots()
                .iter()
                .map(|_| FxRt {
                    gen: 0,
                    p: [0.0; crate::fx::MAX_PARAMS],
                    sync_seen: 0,
                })
                .collect()
        })
        .collect()
}

impl Proc {
    /// Tier-6 ABI v4: refresh the block-start transport snapshot and bump
    /// the lazy-push generation. Called ONCE per RT block, AFTER the
    /// automation pass (automation tempo lands in `metro_bpm` there).
    /// Scripts get their push on first use of the block (`sync_seen` vs
    /// `sync_gen`), so tracks that stay silent cost nothing.
    fn refresh_sync(&mut self, p0: u64, playing: bool) {
        let bpm = self.engine.metro_bpm.load(Ordering::Relaxed) as f64;
        let bpb = self.engine.metro_num.load(Ordering::Relaxed).max(1) as f64;
        self.sync =
            crate::fx::SyncInfoV1::block(p0 as i64, playing, bpm, bpb, self.sr.max(1) as f64);
        self.sync_gen = self.sync_gen.wrapping_add(1);
    }

    /// Run one track-output sample through the track's effect chain (the
    /// volume x gain trim has already been multiplied into `x`). Only for
    /// scripted-only chains; plugin chains go through `run_chain_block`.
    #[inline]
    fn fx_mix(&mut self, t: usize, x: f32, sc: f32) -> (f32, f32) {
        let chain = &self.fx_cache[t];
        if chain.is_empty() {
            return (x, x);
        }
        let mut pair = (x, x);
        let sg = self.sync_gen;
        for (slot, rt) in chain.slots().iter().zip(self.fx_rt[t].iter_mut()) {
            let FxKind::Scripted { inst, .. } = &slot.kind else {
                continue;
            };
            let g = inst.param_gen();
            if g != rt.gen {
                inst.read_params(&mut rt.p);
                rt.gen = g;
            }
            if rt.sync_seen != sg {
                inst.push_sync(&self.sync);
                rt.sync_seen = sg;
            }
            // SAFETY: the live chain is advanced by this thread only, and
            // `rt.p` holds the instance's current slider snapshot.
            // Tier-3: mono slots take the mid and return (y, y) — the
            // v3 dispatch itself, so mono chains behave bit-identically.
            pair = unsafe { inst.call_sample_st(pair.0, pair.1, &rt.p, sc) }
        }
        pair
    }

    /// Run a plugin-bearing chain over a whole block (stereo pair in,
    /// pair out — `tmp` is scratch). Scripted slots keep their per-slot
    /// slider snapshots (`fx_rt`); plugins consume/replenish their rings.
    /// Tier-3: scripted slots run the pair path; a plugin slot processes
    /// the LEFT buffer and copies its mono output to both sides.
    /// The dry input is already trim-scaled; pan/master are folded later.
    fn run_chain_block(
        &mut self,
        t: usize,
        buf: &mut [f32],
        buf2: &mut [f32],
        tmp: &mut [f32],
        sc: &[f32],
    ) {
        let n = buf.len().min(tmp.len()).min(buf2.len());
        let Self {
            fx_cache,
            fx_rt,
            sync,
            sync_gen,
            ..
        } = self;
        for (k, slot) in fx_cache[t].slots().iter().enumerate() {
            match &slot.kind {
                FxKind::Scripted { inst, .. } => {
                    let rt = &mut fx_rt[t][k];
                    let g = inst.param_gen();
                    if g != rt.gen {
                        inst.read_params(&mut rt.p);
                        rt.gen = g;
                    }
                    // Tier-6: once-per-block transport push (this whole
                    // call is one block).
                    if rt.sync_seen != *sync_gen {
                        inst.push_sync(sync);
                        rt.sync_seen = *sync_gen;
                    }
                    // SAFETY: audio thread owns the live chain (as fx_mix).
                    for j in 0..n {
                        let (l, r) = unsafe {
                            inst.call_sample_st(buf[j], buf2[j], &rt.p, *sc.get(j).unwrap_or(&0.0))
                        };
                        buf[j] = l;
                        buf2[j] = r;
                    }
                }
                FxKind::Vst(e) => {
                    e.process(&buf[..n], &mut tmp[..n]);
                    buf[..n].copy_from_slice(&tmp[..n]);
                    buf2[..n].copy_from_slice(&tmp[..n]);
                }
            }
        }
    }

    /// Build this block's clip windows for the timeline span `[p0, p0 + n)`.
    /// `entries` is reused across blocks (cleared, never re-allocated).
    /// One control-rate automation pass (v0.11 R5). Returns the per-track
    /// mixer overrides and applies plugin-parameter targets straight to the
    /// live instances (atomic mirrors; `set_param_rt` never blocks).
    /// Later clips win over earlier ones, matching FL's "last voice" rule.
    fn eval_automation(&self, p0: u64) -> AutoOut {
        let mut out = AutoOut::new();
        if self.auto_cache_gen == 0 {
            return out;
        }
        let pos = p0 as i64;
        for (t, track) in self.arr.tracks.iter().enumerate() {
            if !self.engine.track_on(t) {
                continue;
            }
            for lane in &track.lanes {
                for cl in &lane.clips {
                    let Some(Some(a)) = self.auto_cache.get(cl.asset as usize).cloned() else {
                        continue;
                    };
                    // v34 R11: the curve drives ONLY its clip window.
                    //  * before the clip: this clip is silent (another
                    //    clip or the manual control wins);
                    //  * inside: the R4 clip-window wrap (repeat);
                    //  * after the last played sample: HOLD — the curve
                    //    evaluates at its last played source sample, so
                    //    the target stays where the automation left it
                    //    instead of snapping back to the pre-clip knob
                    //    value the moment the clip scrolls past.
                    let Some(rel) = crate::automation::clip_source_pos(
                        a.len_samples,
                        cl.cut,
                        cl.play_len,
                        pos - cl.pos as i64,
                    ) else {
                        continue;
                    };
                    let v = a.eval(rel);
                    match a.target.as_str() {
                        crate::automation::TARGET_TRACK_VOL => out.vol[t] = Some(v),
                        crate::automation::TARGET_TRACK_GAIN => out.gain[t] = Some(v),
                        crate::automation::TARGET_TRACK_PAN => out.pan[t] = Some(v),
                        crate::automation::TARGET_TRACK_MUTE => out.muted[t] = v >= 0.5,
                        crate::automation::TARGET_MASTER_VOL => out.master_vol = Some(v),
                        crate::automation::TARGET_MASTER_TEMPO => {
                            out.tempo = Some((v.round() as i64).clamp(20, 300) as u32)
                        }
                        other => {
                            let _ = self.route_plugin_param(t, other, v);
                        }
                    }
                }
            }
        }
        out
    }

    /// Write one automated value to a `synth:<idx>` / `fx:<row>:<idx>`
    /// target on track `t`. False for unknown or malformed ids.
    fn route_plugin_param(&self, t: usize, target: &str, v: f32) -> bool {
        if let Some(idx) = target.strip_prefix("synth:") {
            let Ok(idx) = idx.parse::<usize>() else {
                return false;
            };
            let Some(Some(handle)) = self.synths_cache.get(t).cloned() else {
                return false;
            };
            match handle.as_ref() {
                SynthHandle::Script(i) => i.set_param(idx, v),
                SynthHandle::Vst(e) => e.set_param_rt(idx, v),
            }
            return true;
        }
        if let Some(rest) = target.strip_prefix("fx:") {
            let Some((row_s, idx_s)) = rest.split_once(':') else {
                return false;
            };
            let (Ok(row), Ok(idx)) = (row_s.parse::<usize>(), idx_s.parse::<usize>()) else {
                return false;
            };
            let Some(tf) = self.fx_cache.get(t) else {
                return false;
            };
            let Some(slot) = tf.slots().get(row) else {
                return false;
            };
            match &slot.kind {
                FxKind::Scripted { inst, .. } => inst.set_param(idx, v),
                FxKind::Vst(e) => e.set_param_rt(idx, v),
            }
            return true;
        }
        false
    }

    fn collect_clips(&self, p0: u64, n: usize, hold_mask: u128, entries: &mut Vec<ClipEntry>) {
        entries.clear();
        let span_end = p0.saturating_add(n as u64);
        for (t, track) in self.arr.tracks.iter().enumerate() {
            // v32 R7: a just-muted track keeps its window while its release
            // ramp is running (hold_mask) — else the fade has nothing to
            // fade and the mute lands as a hard step.
            if !self.engine.track_on(t) && (hold_mask >> t) & 1 == 0 {
                continue;
            }
            for lane in &track.lanes {
                for cl in &lane.clips {
                    let data = match self.assets.get(cl.asset as usize) {
                        Some(d) if !d.is_empty() => d,
                        _ => continue,
                    };
                    let alen = data.len() as u64;
                    // Clip window on the timeline vs. this block.
                    let c0 = cl.pos;
                    let c1 = cl.end(alen);
                    let w0 = c0.max(p0);
                    let w1 = c1.min(span_end);
                    // Only the part backed by SOURCE data contributes; a
                    // trimmed start (`cut`) shifts the read offset, and a
                    // `play_len` past the data adds nothing (silence).
                    let dstart = cl.data_start();
                    let dend = cl.data_end(alen).min(c1);
                    let wd0 = w0.max(dstart);
                    let wd1 = w1.min(dend);
                    if wd0 < wd1 {
                        // The entry's pointer already includes the offset from
                        // the clip start to the window start, so the sample
                        // loop reads `ptr.add(i - i0)` with no per-sample math.
                        let off = (cl.cut + (wd0 - dstart)) as usize;
                        let off = off.min(data.len().saturating_sub(1));
                        let base = unsafe { data.as_ptr().add(off) };
                        let span = cl.span(alen);
                        let fi = cl.fade_in.min(span / 2);
                        let fo = cl.fade_out.min(span / 2);
                        entries.push(ClipEntry {
                            track: t as u8,
                            ptr: base,
                            i0: (wd0 - p0) as usize,
                            i1: (wd1 - p0) as usize,
                            gain: cl.gain,
                            pos: cl.pos,
                            fi_end: if fi > 0 { cl.data_start() + fi } else { 0 },
                            fi_step: if fi > 0 { 1.0 / fi as f32 } else { 0.0 },
                            fo_start: if fo > 0 { cl.end(alen) - fo } else { u64::MAX },
                            fo_end: cl.end(alen),
                            fo_step: if fo > 0 { 1.0 / fo as f32 } else { 0.0 },
                        });
                    }
                }
            }
        }
    }
}

/// Per-block compact track lists for the mix loop (v0.10 / 128 tracks).
///
/// `entry_mask` marks tracks holding clip windows, `has_midi` + `midi_live`
/// the tracks whose synth is sounding, `chain` the tracks with a non-empty
/// FX chain (their tails must keep decaying), and `vst_mask` the tracks
/// whose chain hosts a plugin. Only these tracks touch the per-sample loop,
/// so CPU scales with ACTIVE tracks, not with the 128-track capacity.
#[derive(Clone, Copy)]
struct MixLists {
    mix: [u16; MAX_LOOPS],
    n_mix: usize,
    acc: [u16; MAX_LOOPS],
    n_acc: usize,
}

fn build_mix_lists(
    entry_mask: u128,
    has_midi: bool,
    midi_live: &[bool],
    audible: &[bool],
    vst_mask: &[bool],
    chain: &[bool],
) -> MixLists {
    let mut out = MixLists {
        mix: [0u16; MAX_LOOPS],
        n_mix: 0,
        acc: [0u16; MAX_LOOPS],
        n_acc: 0,
    };
    for t in 0..MAX_LOOPS {
        let is_src = ((entry_mask >> t) & 1) != 0 || (has_midi && midi_live[t]);
        if is_src {
            out.acc[out.n_acc] = t as u16;
            out.n_acc += 1;
        }
        if audible[t] && (vst_mask[t] || chain[t] || is_src) {
            out.mix[out.n_mix] = t as u16;
            out.n_mix += 1;
        }
    }
    out
}

impl Proc {
    /// THE engine entry point EVERY audio backend must call (WINDOWS-PORT
    /// W4; backend contract: `crate::backend`). Processes exactly one RT
    /// block of `n` frames: `midi` carries this block's raw MIDI events
    /// (block-relative sample offset + bytes, arrival order — fully consumed
    /// here), `io` hands out the live input/output slot buffers for this
    /// cycle. Reads the port masks and engine atomics, advances transport,
    /// mixes clips, runs chains, drives recording — all backend-neutral.
    ///
    /// RT contract (unchanged from the old inline `process`): only
    /// `try_lock` table refreshes, no IO, no panics, no allocation beyond
    /// moving each event's bytes into the GUI channel. The backend calls
    /// this from its realtime callback, once per cycle.
    pub fn process_block(
        &mut self,
        n: usize,
        midi: &mut Vec<(u32, Vec<u8>)>,
        io: &dyn crate::backend::BlockIo,
    ) {
        // v32 R7: one block in units of per-sample step.
        let inv_n = if n > 0 { 1.0 / n as f32 } else { 0.0 };

        // --- v32 live MIDI feed state for this block ----------------------
        // The bound-message snapshot refreshes like every other table
        // (try_lock + generation). The target set is the selected track —
        // the same rule the audio monitor follows — PLUS every track
        // armed for MIDI recording: the player must hear the take they are
        // laying down. Raw messages stay on the UI channel regardless:
        // bindings, the recorder and the monitor keep seeing everything.
        let bg = self.engine.midi_bound_gen();
        if bg != self.bound_gen {
            if let Ok(b) = self.engine.midi_bound.try_lock() {
                self.bound_cache = b.clone();
                self.bound_gen = bg;
            }
        }
        let midi_live_on = self.engine.midi_live();
        let mut live_mask: u128 = 0;
        if midi_live_on {
            let sel = (self.engine.selected.load(Ordering::Relaxed) as usize).min(MAX_LOOPS - 1);
            live_mask |= 1u128 << sel;
            for t in 0..MAX_LOOPS {
                if self.engine.midi_rec_arm(t) {
                    live_mask |= 1u128 << t;
                }
            }
        }
        // v33: live messages staged but NOT rendered last cycle (idle
        // early-return, zero-length block, gate flipped off between
        // cycles) carry over instead of evaporating: their samples are
        // already in the past, so they fire at the head of this cycle,
        // in the order they arrived. (Consumption/clear moved into
        // `render_midi_block` — whoever renders the events clears them.)
        for e in self.live_ev.iter_mut() {
            e.0 = 0;
        }
        // MIDI: copy events out (bounded channel; count drops if the GUI is slow).
        let off_cap = n.saturating_sub(1);
        // v42/MPE: read once per block — bend/poly-AT ride the port only
        // when the user opted into MPE (classic single-channel rigs stay
        // byte-for-byte in the legacy behavior).
        let mpe_on = self.engine.mpe();
        // Consume the block's MIDI events in arrival order. The backend shim
        // already materialized them (one owned `Vec<u8>` per event — exactly
        // what the old inline `e.bytes.to_vec()` allocated); here the bytes
        // move on into the GUI channel unchanged.
        for (etime, bytes) in midi.drain(..) {
            // v32: stage the message for the track synths too. Bound
            // messages are CONTROL TRAFFIC (they drive bindings; the
            // recorder has always had the same rule) and never feed a
            // synth; the held-key count keeps a physically held key
            // ringing across event-free blocks.
            if live_mask != 0 {
                if let Some((kind, ch, d1, d2)) = live_event_kind(&bytes) {
                    if (kind < 4 || mpe_on) && !self.bound_cache.claims(kind, ch, d1) {
                        if self.live_ev.len() < 1024 {
                            let off = (etime) as usize;
                            self.live_ev.push((off.min(off_cap), kind, ch, d1, d2));
                        }
                        let mut m = live_mask;
                        while m != 0 {
                            let t = m.trailing_zeros() as usize;
                            m &= m - 1;
                            match kind {
                                3 => self.live_notes[t] += 1,
                                0 => self.live_notes[t] = (self.live_notes[t] - 1).max(0),
                                _ => {}
                            }
                        }
                    }
                }
            }
            if self.tx.try_send(Evt::Midi(RawMidiMsg { bytes })).is_err() {
                self.midi_drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        if n == 0 {
            return;
        }

        let in_mask = self.mask_in.load(Ordering::Relaxed);
        let out_mask_l = self.mask_out_l.load(Ordering::Relaxed);
        let out_mask_r = self.mask_out_r.load(Ordering::Relaxed);

        // --- engine snapshot for this cycle (atomics read once per block) ---
        let transport = self.engine.transport_on();
        let seek = self.engine.take_seek();
        let proj_len = self.engine.proj_len().max(1);
        // v37 R5: while ANY recorder is armed the transport runs straight
        // through: loop wrap AND park/auto-stop at the project end are
        // suspended, so a take records the whole window the user recorded
        // (truncating at content length = "MIDI records only 1 bar").
        let rec_open = self.engine.recording();
        // v42: a LOOP take WANTS the timeline to keep wrapping — each wrap
        // commits one pass, so recording must not suspend the loop.
        let looprec_open = (0..MAX_LOOPS)
            .any(|t| self.engine.rec_state_of(t) == TR_REC && self.engine.rec_is_loop(t));
        let looping = self.engine.loop_tl() && (!rec_open || looprec_open);

        // Project snapshots: refresh Arcs when a generation moved (try_lock —
        // never blocks on the manager thread). Swapping the Arrangement also
        // re-derives nothing else; clip windows are rebuilt per block anyway.
        let ag = self.engine.arr_gen();
        if ag != self.arr_cache_gen {
            if let Ok(a) = self.engine.arr.try_lock() {
                self.arr = a.clone();
                self.arr_cache_gen = ag;
            }
        }
        let asg = self.engine.assets_gen();
        if asg != self.assets_cache_gen {
            if let Ok(a) = self.engine.assets.try_lock() {
                self.assets = a.clone();
                self.assets_cache_gen = asg;
            }
        }
        let sg = self.engine.synths_gen();
        if sg != self.synths_cache_gen {
            if let Ok(s) = self.engine.synths.try_lock() {
                self.synths_cache = s.clone();
                self.synths_cache_gen = sg;
                self.dflt_synth = self
                    .engine
                    .default_synth
                    .try_lock()
                    .ok()
                    .and_then(|d| d.clone());
            }
        }
        // v46: poly copy rows ride the same gen-swap discipline.
        let pg = self.engine.poly_gen();
        if pg != self.poly_cache_gen {
            if let Ok(p) = self.engine.poly_voices.try_lock() {
                self.poly_cache = p.clone();
                self.poly_cache_gen = pg;
            }
        }
        let mg = self.engine.midi_gen();
        if mg != self.midi_cache_gen {
            if let Ok(m) = self.engine.midi.try_lock() {
                self.midi_cache = m.clone();
                self.midi_cache_gen = mg;
            }
        }
        let ag = self.engine.auto_gen();
        if ag != self.auto_cache_gen {
            if let Ok(a) = self.engine.auto.try_lock() {
                self.auto_cache = a.clone();
                self.auto_cache_gen = ag;
            }
        }
        // Pick up edited effect chains (user action only; try_lock so the
        // audio path can never block on the manager thread).
        let fxg = self.engine.fx_gen();
        if fxg != self.fx_cache_gen {
            if let Ok(tb) = self.engine.fx.try_lock() {
                self.fx_cache = tb.clone();
                self.fx_cache_gen = fxg;
                self.fx_rt = seed_fx_rt(&self.fx_cache[..]);
                // Stamp the chain reaper (see `Engine::drain_vst_held`): the
                // displaced chain died right here, but every VST entry it
                // held also lives in `vst_held`, so no plugin destructor ran
                // on this thread — the UI may now collect them.
                let vg = self.engine.vst_gen();
                self.engine.vst_refreshed.store(vg, Ordering::Relaxed);
            }
        }
        // Asset preview: refresh the audition buffer on a generation change
        // (restarts its read cursor) and mirror the play flag every block —
        // the RT side clears the flag itself when a buffer runs out.
        let pvg = self.engine.preview_gen();
        if pvg != self.preview_cache_gen {
            let snap = self.engine.preview_snapshot();
            // Cursors are keyed by voice id: a new voice joins without
            // rewinding the ones already playing, and stopped voices lose
            // their cursor.
            let mut curs = std::mem::take(&mut self.preview_curs);
            curs.retain(|(vid, _)| snap.iter().any(|(v, _, _)| v == vid));
            for (vid, _, _) in snap.iter() {
                if !curs.iter().any(|(v, _)| v == vid) {
                    curs.push((*vid, 0));
                }
            }
            self.preview_curs = curs;
            self.preview = snap;
            self.preview_cache_gen = pvg;
        }
        self.preview_live =
            self.engine.preview_on.load(Ordering::Relaxed) != 0 && !self.preview_curs.is_empty();

        // Per-track output trim (volume x gain, linear) snapshotted once per
        // cycle; applied to each clip's PLAYBACK contribution only, so the
        // mix level rides the slider while recorded takes stay at raw input.
        let mut amp = [1.0f32; MAX_LOOPS];
        // Equal-power pan gains per track, snapshotted with the trim.
        let mut pgl = [F1S2; MAX_LOOPS];
        let mut pgr = [F1S2; MAX_LOOPS];
        // Tracks whose clips can sound at all this cycle: sounding (play)
        // AND not recording... (takes are raw — capture ignores the mix).
        let mut audible = [false; MAX_LOOPS];
        for (t, a) in amp.iter_mut().enumerate() {
            *a = f32::from_bits(self.engine.vol[t].load(Ordering::Relaxed))
                * f32::from_bits(self.engine.trk_gain[t].load(Ordering::Relaxed));
            let (l, r) = pan_gains(f32::from_bits(self.engine.pan[t].load(Ordering::Relaxed)));
            pgl[t] = l;
            pgr[t] = r;
            audible[t] = transport && self.engine.track_on(t);
        }

        // Input monitoring follows the selected track: its gain x volume,
        // effect chain and pan shape the live pass-through (takes capture
        // the raw, unprocessed `src_in`).
        let sel = (self.engine.selected.load(Ordering::Relaxed) as usize).min(MAX_LOOPS - 1);
        let (mut mon_amp, mon_l, mon_r) = (amp[sel], pgl[sel], pgr[sel]);
        if !self.engine.monitor_on.load(Ordering::Relaxed) {
            mon_amp = 0.0; // Settings → live input monitoring off
        }
        // v32 R7: the monitor gain ramps like every other multiplier
        // (selecting another track / toggling monitor must not click).
        let mon_a = self.mon_env;
        let mon_s = (mon_amp - mon_a) * inv_n;
        self.mon_env = mon_amp;

        // Compact list of recording tracks: the sample loop iterates only
        // these instead of all MAX_LOOPS slots per sample.
        let st_now: [u8; MAX_LOOPS] =
            core::array::from_fn(|i| self.engine.rec_state[i].load(Ordering::Relaxed));
        let mut rec_tracks: [u8; MAX_LOOPS] = [0u8; MAX_LOOPS];
        let mut nrec = 0usize;
        // v42 "p": tracks whose running take is a PREVIEW take capture the
        // track bus later (post-FX), not the raw input — so they leave the
        // input-capture list. Preview follows the transport: with the
        // transport stopped it captures nothing, and the moment the
        // transport stops with a preview running, the take commits itself.
        let mut pv_mask: u128 = 0;
        // v42: tracks whose live take is a loop take (pass commits on wrap).
        let mut loop_mask: u128 = 0;
        for (t, &s) in st_now.iter().enumerate() {
            if s != self.prev_rec[t] {
                self.rec_full &= !(1u128 << t);
                self.prev_rec[t] = s;
                // A freshly armed take has not seen the transport run yet.
                if s == TR_REC && self.engine.rec_is_preview(t) {
                    self.pv_seen = false;
                }
            }
            if s == TR_REC {
                if self.engine.rec_is_preview(t) {
                    pv_mask |= 1u128 << t;
                } else {
                    rec_tracks[nrec] = t as u8;
                    nrec += 1;
                    if self.engine.rec_is_loop(t) {
                        loop_mask |= 1u128 << t;
                    }
                }
            }
        }
        let any_rec = nrec > 0;
        // v42: a loop take ends with the transport — the last pass commits
        // like any other take and the loop flags are cleared for good.
        if loop_mask != 0 && !transport {
            for t in 0..MAX_LOOPS {
                if loop_mask & (1u128 << t) != 0 {
                    self.engine.stop_rec_loop(t);
                    self.lr_restart[t] = u64::MAX;
                    if self.engine.rec_state_of(t) == TR_REC {
                        self.engine.rec_state[t].store(TR_IDLE, Ordering::Relaxed);
                        self.engine.mark_take_pending(t);
                    }
                }
            }
            loop_mask = 0;
        }
        if pv_mask != 0 {
            if transport {
                self.pv_seen = true;
            } else if self.pv_seen {
                // Transport actually ran since arming — end every preview
                // take now: TR_IDLE + hand the buffer to the manager.
                for t in 0..MAX_LOOPS {
                    if pv_mask & (1u128 << t) != 0 {
                        self.engine.rec_state[t].store(TR_IDLE, Ordering::Relaxed);
                        self.engine.mark_take_pending(t);
                    }
                }
                self.pv_seen = false;
                pv_mask = 0;
            }
        }
        let pv_mask = if transport { pv_mask } else { 0 };
        let any_pv = pv_mask != 0;

        let clip_sound = transport
            && self.arr.tracks.iter().enumerate().any(|(t, tr)| {
                self.engine.track_on(t) && tr.lanes.iter().any(|l| !l.clips.is_empty())
            });
        // v32 R7: a release ramp from the previous block still running:
        // the mix must keep producing (fading) even though the gates
        // below already closed. The mask doubles as the "collect windows
        // even when muted" override for the fade blocks.
        let env_mask: u128 = (0..MAX_LOOPS).fold(0u128, |m, t| {
            if self.amp_env[t].abs() > 1e-4 {
                m | (1u128 << t)
            } else {
                m
            }
        });
        let env_held = env_mask != 0;
        let metro_on = self.engine.metro().0;
        if in_mask == 0
            && (out_mask_l | out_mask_r) == 0
            && !any_rec
            && !clip_sound
            && !env_held
            && !metro_on
            && !self.preview_live
            && !self.aud_pv_any
        {
            return; // nothing connected, nothing playing: idle
        }

        // --- transport position for this block ------------------------------
        let prev_pos = self.pos;
        if let Some(p) = seek {
            // v35 R2: the SEEK_SLACK cap matches `Engine::seek_to` — RT
            // must not re-clamp a legal past-the-end placement.
            self.pos = p.min(proj_len + SEEK_SLACK);
        }
        if seek.is_some() {
            // A jump in the timeline: nothing that was ringing still applies.
            for inst in self.synths_cache.iter().flatten() {
                inst.all_off();
            }
            if let Some(d) = &self.dflt_synth {
                d.all_off();
            }
            // Sustain bookkeeping dies with the old timeline position.
            for t in 0..MAX_LOOPS {
                self.midi_sustain[t] = false;
                self.midi_quiet[t] = 0;
                self.midi_ring_to[t] = 0;
            }
            // v33: staged live messages belong to the abandoned position.
            self.live_ev.clear();
            // Plugin buffers/rings in every chain: drop stale audio.
            for tf in self.fx_cache.iter() {
                for s in tf.slots() {
                    if let FxKind::Vst(e) = &s.kind {
                        e.flush();
                    }
                }
            }
        }
        let mut p0 = self.pos;
        // Does the clock advance this block? ONLY the transport moves the
        // playhead — a bare metronome run ticks on its own free-running
        // clock (`metro_clock`) and must leave the (always-visible)
        // playhead exactly where it is. Stop means stop.
        let advance = transport;
        // The active loop region (samples): a looping transport wraps from
        // `ls` back at `le`. Degenerate requests fall back to the whole
        // project (`le = 0` means "the project end").
        let (ls, le) = clamp_loop_region(
            self.engine.loop_region().0,
            self.engine.loop_region().1,
            proj_len,
        );
        // End handling: looping clips the block at the region end (wrapped
        // after the sample loop); non-looping clips at the project end and
        // parks there (auto-stop).
        let mut block_n = n;
        let mut auto_stop = false;
        if advance && looping {
            // Playhead outside the region (a handle dragged under it, or a
            // seek beyond it): re-enter at the region start.
            if p0 < ls || p0 >= le {
                p0 = ls;
                self.pos = ls;
            }
            if p0.saturating_add(n as u64) > le {
                block_n = (le - p0) as usize;
            }
        } else if advance && p0.saturating_add(n as u64) > proj_len && !rec_open {
            block_n = (proj_len - p0.min(proj_len)) as usize;
            auto_stop = true;
        }

        // Build this block's clip windows over [p0, p0 + block_n). The list
        // lives in a local (taken out of self) so the sample loop below can
        // call `&mut self` effect code while iterating it.
        let mut entries = std::mem::take(&mut self.clip_buf);
        entries.clear();
        if clip_sound || env_held {
            self.collect_clips(p0, block_n, env_mask, &mut entries);
        }
        // MIDI pattern clips of this block, rendered per track through the
        // track's synth into `midi_scratch` (mixed in the sample loop).
        // v32: ALWAYS called now — even with the transport stopped it
        // renders the live keyboard feed (that call is how playing a synth
        // works while the timeline stands still).
        // --- v38 R1: audition voices --------------------------------------
        // Refresh like every preview table (gen check + snapshot), then
        // advance the pool with the block clock and stage due events.
        // Voices run whether the transport runs or not — the roll
        // auditions a patch precisely while the timeline stands still.
        let ag = self.engine.aud_gen.load(Ordering::Relaxed);
        if ag != self.aud_seen_gen {
            self.aud_seen_gen = ag;
            let snap = self.engine.audition_snapshot();
            // A replaced/stopped pool may leave held notes mid-voice:
            // those tracks get an all-notes-off (queued for the render).
            // v40: only voices actually LEAVING the pool — survivors now
            // keep their cursors (no rewind/replay), and their pending
            // held notes are still live, not orphans.
            for (req, cur) in self.aud.iter() {
                if snap.iter().any(|r| r.vid == req.vid) {
                    continue;
                }
                let pend = aud_pending(req.ev.as_ref(), *cur);
                if pend == 0 {
                    continue;
                }
                match (&req.inst, req.track < MAX_LOOPS) {
                    // v46 copy voice: its OWN instance dies, never a track
                    // synth — all_off kills the held notes on the copy.
                    (Some(h), true) => {
                        h.all_off();
                        let t = req.track;
                        self.aud_iheld[t] -= pend.min(self.aud_iheld[t].max(0));
                    }
                    (Some(h), false) => {
                        h.all_off();
                        self.aud_pv_held -= pend.min(self.aud_pv_held.max(0));
                    }
                    // Legacy live-route voice (v38): the track synth gets
                    // the all-notes-off exactly like before.
                    (None, true) => {
                        self.aud_held[req.track] -= pend.min(self.aud_held[req.track].max(0));
                        self.aud_purge |= 1u128 << req.track;
                    }
                    (None, false) => {
                        if let Some(h) = &self.aud_pv_inst {
                            h.all_off();
                        }
                        self.aud_pv_held -= pend.min(self.aud_pv_held.max(0));
                    }
                }
            }
            // v40: preserve per-voice cursors keyed by vid — a new voice
            // joining used to reseed EVERY voice at 0, replaying still
            // running auditions from the top on every click (the roll
            // preview "wall of sound").
            // v46: THE STUCK-NOTE SWEEP. A held note-outlives its voice
            // object (the stream ends at note-on; the held COUNT carries
            // the sustain), so the leave-the-pool loop above can never see
            // it — on STOP (empty snapshot) every held note of EVERY kind
            // — legacy track synth, headless copy, default-slot piano —
            // must die here, or its instance rings until the next loud
            // event (the reported "stuck on until you preview something
            // else"). All-off, then zero the books.
            if snap.is_empty() {
                for t in 0..MAX_LOOPS {
                    if self.aud_held[t] > 0 {
                        self.aud_purge |= 1u128 << t;
                    }
                    self.aud_held[t] = 0;
                    if self.aud_iheld[t] > 0 || self.aud_isus[t] {
                        if let Some(h) = &self.aud_iinst[t] {
                            h.all_off();
                        }
                        self.aud_iinst[t] = None;
                        self.aud_iheld[t] = 0;
                        self.aud_isus[t] = false;
                        self.aud_iquiet[t] = 0;
                    }
                }
                if self.aud_pv_held > 0 || self.aud_pv_sus {
                    if let Some(h) = &self.aud_pv_inst {
                        h.all_off();
                    }
                }
                self.aud_pv_held = 0;
                self.aud_pv_sus = false;
                self.aud_pv_quiet = 0;
                self.aud_pv_inst = None;
                self.aud_pblock.clear();
                self.aud_mask = 0;
            }
            let old = std::mem::take(&mut self.aud);
            self.aud = snap
                .iter()
                .map(|r| {
                    let cur = old
                        .iter()
                        .find(|(q, _)| q.vid == r.vid)
                        .map(|(_, c)| *c)
                        .unwrap_or(0);
                    (r.clone(), cur)
                })
                .collect();
        }
        {
            // Clear last block's rows first (bounded by aud_mask).
            let mut m = self.aud_mask;
            while m != 0 {
                let t = m.trailing_zeros() as usize;
                m &= m - 1;
                if t < MAX_LOOPS {
                    self.aud_block[t].clear();
                    self.aud_iblock[t].clear();
                }
            }
            self.aud_mask = 0;
            self.aud_pblock.clear();
        }
        let nn = n as u64;
        let off_cap = (n.saturating_sub(1)) as u64;
        let mut i = 0usize;
        while i < self.aud.len() {
            let (req, cur) = &mut self.aud[i];
            let end = cur.saturating_add(nn);
            let mut done = true;
            // v46: route this voice's due events to its row — the default
            // pseudo-slot (`AUD_PV`), a track's headless-COPY row, or the
            // legacy live-track row. Only copy/PV voices re-arm their
            // instance slot (the row renders through whatever it names).
            let pv = req.track >= MAX_LOOPS;
            let copy = !pv && req.inst.is_some();
            if pv {
                if req.inst.is_some() {
                    self.aud_pv_inst = req.inst.clone();
                }
            } else if copy {
                self.aud_iinst[req.track] = req.inst.clone();
            }
            for &(off, kind, ch, d1, d2) in req.ev.iter() {
                if off >= end {
                    done = false;
                    break; // sorted: everything past is future
                }
                if off >= *cur {
                    let o = (off - *cur).min(off_cap) as usize;
                    if pv {
                        if self.aud_pblock.len() < 1024 {
                            self.aud_pblock.push((o, kind, ch, d1, d2));
                            match kind {
                                3 => self.aud_pv_held += 1,
                                0 => self.aud_pv_held = (self.aud_pv_held - 1).max(0),
                                _ => {}
                            }
                        }
                    } else if copy {
                        let t = req.track;
                        if self.aud_iblock[t].len() < 1024 {
                            self.aud_iblock[t].push((o, kind, ch, d1, d2));
                            match kind {
                                3 => self.aud_iheld[t] += 1,
                                0 => self.aud_iheld[t] = (self.aud_iheld[t] - 1).max(0),
                                _ => {}
                            }
                            self.aud_mask |= 1u128 << t;
                        }
                    } else {
                        let t = req.track.min(MAX_LOOPS - 1);
                        if self.aud_block[t].len() < 1024 {
                            self.aud_block[t].push((o, kind, ch, d1, d2));
                            match kind {
                                3 => self.aud_held[t] += 1,
                                0 => self.aud_held[t] = (self.aud_held[t] - 1).max(0),
                                _ => {}
                            }
                            self.aud_mask |= 1u128 << t;
                        }
                    }
                }
            }
            *cur = end;
            // The roll playhead follows the YOUNGEST voice (last added).
            self.engine.aud_pos.store(*cur, Ordering::Relaxed);
            if done {
                self.aud.remove(i);
            } else {
                i += 1;
            }
        }
        if self.aud.is_empty()
            && !self.aud_held.iter().any(|h| *h > 0)
            && !self.aud_iheld.iter().any(|h| *h > 0)
            && self.aud_pv_held <= 0
        {
            // aud_on flips off only after OUR generation actually drained
            // — a voice staged the same frame stays armed (A3's stop
            // races resolved).
            if self.engine.aud_gen.load(Ordering::Relaxed) == self.aud_seen_gen
                && self.engine.aud_on.load(Ordering::Relaxed)
            {
                self.engine.aud_on.store(false, Ordering::Relaxed);
                // v40: the engine-side audition pool still holds retired
                // voices — arm the UI retire pass (mirrors preview_drained).
                self.engine.aud_drained.store(true, Ordering::Relaxed);
            }
        }
        // A voice whose every event is staged can already have retired
        // while its tail decays; aud_on stays true until the pool drains,
        // then the roll's stop button flips back on the next UI frame.
        let has_midi = self.render_midi_block(p0, block_n, live_mask, midi_live_on);
        if has_midi {
            // A track whose synth is sounding is audible even when the
            // transport is stopped: the live feed rides the track's own
            // chain × gain × volume × pan, exactly like its clips.
            for (t, a) in audible.iter_mut().enumerate() {
                *a |= self.midi_live[t];
            }
        }

        // --- automation assets (v0.11 R5) --------------------------------
        // Control-rate pass: every automation clip sounding at this block's
        // start position writes its ABSOLUTE curve value once, overriding
        // the manual control it targets (a curve wins over the knob it
        // rides while it sounds — FL semantics). Plugin-parameter targets
        // land directly on the live instances' atomics.
        // v34 R11: STOPPING keeps the last automation pass — a pause one
        // sample past an automation clip must not snap every automated
        // knob back. A play/seek elsewhere re-evaluates at the new
        // position on the next run block.
        if transport {
            self.auto_last = self.eval_automation(p0);
            // v35 R1: automation drives REAL controls (see apply_auto_real).
            self.engine.apply_auto_real(&self.auto_last);
            if let Some(mv) = self.auto_last.master_vol {
                // (the master atomic lives on the Backend; Proc holds the clone)
                self.gain.store(mv.max(0.0).to_bits(), Ordering::Relaxed);
            }
        }
        // Tier-6 ABI v4: refresh the scripted sync clock AFTER the
        // automation pass, so automation tempo rides this block's push.
        // Runs even while stopped — a seek must re-anchor the position.
        self.refresh_sync(p0, transport);
        let auto = self.auto_last;
        // v34 R11: publish the live automation values for the GUI.
        for t in 0..MAX_LOOPS {
            let nan = f32::NAN.to_bits();
            self.engine.auto_disp[0][t]
                .store(auto.vol[t].map_or(nan, |v| v.to_bits()), Ordering::Relaxed);
            self.engine.auto_disp[1][t]
                .store(auto.gain[t].map_or(nan, |v| v.to_bits()), Ordering::Relaxed);
            self.engine.auto_disp[2][t]
                .store(auto.pan[t].map_or(nan, |v| v.to_bits()), Ordering::Relaxed);
        }
        for t in 0..MAX_LOOPS {
            // v22: vol and gain automation COMPOSE — each curve overrides
            // only its own manual control (a gain curve keeps the fader,
            // a vol curve keeps the trim, both together ride together).
            if auto.vol[t].is_some() || auto.gain[t].is_some() {
                let v = auto.vol[t]
                    .unwrap_or_else(|| f32::from_bits(self.engine.vol[t].load(Ordering::Relaxed)));
                let g = auto.gain[t].unwrap_or_else(|| {
                    f32::from_bits(self.engine.trk_gain[t].load(Ordering::Relaxed))
                });
                amp[t] = v * g;
            }
            if let Some(pn) = auto.pan[t] {
                let (l, r) = pan_gains(pn);
                pgl[t] = l;
                pgr[t] = r;
            }
            if auto.muted[t] {
                amp[t] = 0.0;
            }
        }

        // ---- v32 R7: block-edge gain smoothing ---------------------------
        // Every per-sample multiplier ramps from last block's value to
        // this block's target across the block. Tracks that fell SILENT
        // (mute, transport stop, automation off-clip) keep their mix slot
        // until the release ramp finishes: audio never gets cut mid-
        // waveform at a block edge — the "static" at loop start/stop and
        // volume jumps. Linear across ONE block (~21 ms at 1024/48k),
        // zero-cost when the target did not move.
        let mut amp_a = [0.0f32; MAX_LOOPS];
        let mut amp_s = [0.0f32; MAX_LOOPS];
        for t in 0..MAX_LOOPS {
            let tgt = if audible[t] { amp[t] } else { 0.0 };
            amp_a[t] = self.amp_env[t];
            amp_s[t] = (tgt - self.amp_env[t]) * inv_n;
            if !audible[t] && self.amp_env[t].abs() > 1e-4 {
                audible[t] = true; // hold the slot until the fade ends
            }
            self.amp_env[t] = tgt;
        }

        // Cache this cycle's input buffers (raw pointers are stable). Only
        // the connected slots are kept, compacted to the front, so the sample
        // loop iterates the live ones with no per-sample null checks.
        // v42 MONITOR-STEAL FIX (tier-5b, docs/FL-SUITE.md): with NO input
        // connected, the monitored chain used to run every sample on
        // silence — through the SAME script instances the playback path
        // uses. Narrow band/biquad state (vocoder banks) and envelope
        // followers cannot survive 2x-rate (signal, 0, signal, 0...)
        // interleaving; the vocoder's wet collapsed ~25x on the live bus
        // (first e2e to run a sidechain-consuming script on the SELECTED
        // track). Drive the monitor chain only while an input stream
        // exists — and per sample only while it (or its own tail) rings.
        let mut in_bufs: [*const f32; MAX_SLOTS] = [std::ptr::null(); MAX_SLOTS];
        let mut nin = 0usize;
        for s in 0..MAX_SLOTS {
            if in_mask & (1u64 << s) != 0 {
                // Null = slot has no buffer this cycle (old: null port).
                let bp = io.in_buf(s);
                if !bp.is_null() {
                    in_bufs[nin] = bp;
                    nin += 1;
                }
            }
        }
        let monitor_live = nin > 0;
        // Cache output buffers too: the stereo mix is computed once per
        // sample and written to every enabled slot according to its routing
        // — left bus, right bus, or the mono downmix (both bits).
        let mut out_bufs_l: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        let mut out_bufs_r: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        let mut out_bufs_m: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        let (mut nl, mut nr, mut nm) = (0usize, 0usize, 0usize);
        for s in 0..MAX_SLOTS {
            let l = out_mask_l & (1u64 << s) != 0;
            let r = out_mask_r & (1u64 << s) != 0;
            if !(l || r) {
                continue;
            }
            // Null = slot has no buffer this cycle (old: null port).
            let b = io.out_buf(s);
            if b.is_null() {
                continue;
            }
            match (l, r) {
                (true, false) => {
                    out_bufs_l[nl] = b;
                    nl += 1;
                }
                (false, true) => {
                    out_bufs_r[nr] = b;
                    nr += 1;
                }
                _ => {
                    out_bufs_m[nm] = b;
                    nm += 1;
                }
            }
        }
        let ins = &in_bufs[..nin];

        // v35 R1: master automation now WRITES self.gain (real value),
        // so the multiplier is gone — reading the atomic already includes
        // the curve, and it holds the last automated value when stopped.
        let g = f32::from_bits(self.gain.load(Ordering::Relaxed));
        // v32 R7: the master knob / master automation ramps too.
        let g_a = self.g_env;
        let g_s = (g - g_a) * inv_n;
        self.g_env = g;

        // Metronome grid on the timeline: click k at timeline sample
        // `k * sub_len` (sub = clicks per beat). Track the grid index last
        // emitted so playhead crossings — while playing OR while scrubbing —
        // fire exactly one click each. Re-anchored on transport starts and
        // seeks (the position jump itself must not burst-fire).
        let metro_bpm = auto
            .tempo
            .or_else(|| {
                let b = self.engine.metro_bpm.load(Ordering::Relaxed);
                Some(b)
            })
            .unwrap_or(120)
            .clamp(20, 300)
            .max(1);
        let metro_sub = self.engine.metro_sub.load(Ordering::Relaxed).clamp(1, 16);
        let metro_num = self.engine.metro_num.load(Ordering::Relaxed).clamp(1, 16);
        let sub_len = (60.0 * self.sr as f64 / metro_bpm as f64) / metro_sub as f64;
        // The next click is the first grid line at or after the playhead.
        // Re-anchor on transport starts and backward seeks; a forward seek
        // keeps the old anchor so crossing grid lines while scrubbing still
        // fires one click each. Any seek or start also re-anchors the
        // free-running click clock to the (now stationary) playhead, so a
        // bare metronome run ticks from where the playhead sits.
        if seek.is_some_and(|p| p <= prev_pos)
            || (transport && !self.prev_transport)
            || (metro_on && !self.prev_metro)
        {
            // Activating the click (or starting/seeking back into a run)
            // drops the anchor one grid line behind the playhead, so the
            // very first block fires the click at the current line.
            self.metro_idx = (self.pos as f64 / sub_len).ceil() as i64 - 1;
            self.metro_clock = self.pos as f64;
        } else if seek.is_some() {
            // Forward seek: keep the grid anchor (scrub crossings still
            // tick), but jump the click clock to the new position.
            self.metro_clock = self.pos as f64;
        }
        self.prev_transport = transport;
        self.prev_metro = metro_on;
        let metro_base = self.metro_clock;
        let metro_dur = (self.sr / 40).max(2); // 25 ms click
        let metro_decay = self.sr as f32 * 0.012;

        // Mixer gate: while the transport runs, every sounding track's chain
        // processes each sample (so delay/reverb tails keep decaying across
        // silent gaps instead of freezing). With no clips out and no effect
        // chains there is nothing to run at all. `has_entries` is
        // block-constant: the accumulator only needs refilling when this
        // block actually holds clip windows.
        let mut mixer_live = clip_sound || env_held || has_midi;
        let mut chain_mask = [false; MAX_LOOPS];
        let mut any_chain = false;
        for (t, m) in chain_mask.iter_mut().enumerate() {
            *m = !self.fx_cache[t].is_empty();
            any_chain |= *m;
        }
        if transport && !mixer_live {
            mixer_live = any_chain;
        }
        // Per-track sample accumulators for the clip mix.
        let mut acc = [0.0f32; MAX_LOOPS];

        // Tracks whose chain hosts a plugin: their per-track sums (and the
        // monitor tap) run the chain block-wise after the sample loop.
        let mut vst_mask = [false; MAX_LOOPS];
        for t in 0..MAX_LOOPS {
            vst_mask[t] = audible[t] && self.fx_cache[t].has_vst();
        }
        let sel_vst = audible[sel] && self.fx_cache[sel].has_vst();
        let has_vst = vst_mask.iter().any(|v| *v) || sel_vst;

        // v0.10 (128 tracks): compact per-block lists of the tracks that can
        // actually contribute — clip windows, live MIDI, or a non-empty FX
        // chain (whose tail must keep decaying). The per-sample loop below
        // touches ONLY these, so an empty 128-track project costs exactly
        // what an empty 16-track one did before.
        let mut entry_mask = 0u128;
        for e in entries.iter() {
            entry_mask |= 1u128 << e.track;
        }
        let lists = build_mix_lists(
            entry_mask,
            has_midi,
            &self.midi_live,
            &audible,
            &vst_mask,
            &chain_mask,
        );
        let MixLists {
            mix: mix_list,
            n_mix: nmix,
            acc: acc_list,
            n_acc: nacc,
        } = lists;

        // ---- v32 R5: sidechain bus block setup --------------------------
        // Sends are read every block from the engine atomics (UI writes
        // land next block). Edges expand to (source, target) pairs; the
        // per-sample loop taps ONLY sources that are actually used, and a
        // project without routing costs one AND per mixed track.
        let mut sc_src_any = 0u128;
        let mut n_edge = 0usize;
        let mut e_src = [0u8; 256];
        let mut e_tgt = [0u8; 256];
        let mut sc_any = false;
        for tg in 0..MAX_LOOPS {
            let m: u128 = self.engine.sc_send(tg);
            self.sc_active[tg] = m != 0;
            if m == 0 {
                continue;
            }
            sc_any = true;
            sc_src_any |= m;
            for s in 0..MAX_LOOPS {
                if (m >> s) & 1 != 0 && n_edge < e_src.len() {
                    e_src[n_edge] = s as u8;
                    e_tgt[n_edge] = tg as u8;
                    n_edge += 1;
                }
            }
        }
        let mut sc_apply = std::mem::take(&mut self.sc_apply);
        let mut sc_build = std::mem::take(&mut self.sc_build);
        if sc_any {
            for t in 0..MAX_LOOPS {
                if self.sc_active[t] {
                    if sc_apply[t].len() < n {
                        sc_apply[t].resize(n, 0.0);
                    }
                    if sc_build[t].len() < n {
                        sc_build[t].resize(n, 0.0);
                    }
                    sc_build[t][..n].fill(0.0);
                }
            }
        }
        let sc_empty: [f32; 0] = [];

        // Per-block mix buffers (taken out of self so the sample loop can
        // call `&mut self` effect code freely; `clip_buf` uses the same trick).
        let mut mix_l = std::mem::take(&mut self.mix_l);
        let mut mix_r = std::mem::take(&mut self.mix_r);
        let mut trk_bufs = std::mem::take(&mut self.trk_bufs);
        let mut trk_bufs2 = std::mem::take(&mut self.trk_bufs2);
        let mut mon_buf = std::mem::take(&mut self.mon_buf);
        let mut mon_buf2 = std::mem::take(&mut self.mon_buf2);
        let mut vst_tmp = std::mem::take(&mut self.vst_tmp);
        mix_l.clear();
        mix_l.resize(n, 0.0);
        mix_r.clear();
        mix_r.resize(n, 0.0);
        vst_tmp.clear();
        vst_tmp.resize(n, 0.0);
        for (t, buf) in trk_bufs.iter_mut().enumerate() {
            buf.clear();
            if vst_mask[t] {
                buf.resize(n, 0.0);
            }
        }
        for (t, buf) in trk_bufs2.iter_mut().enumerate() {
            buf.clear();
            if vst_mask[t] {
                buf.resize(n, 0.0);
            }
        }
        if sel_vst {
            mon_buf.clear();
            mon_buf.resize(n, 0.0);
            mon_buf2.clear();
            mon_buf2.resize(n, 0.0);
        }

        // v42: loop pass hand-off — resume capture at the scheduled
        // position (set at the wrap commit; the manager has copied the
        // finished pass out of the buffer long before this fires).
        for t in 0..MAX_LOOPS {
            if self.lr_restart[t] != u64::MAX {
                if self.pos >= self.lr_restart[t] {
                    self.lr_restart[t] = u64::MAX;
                    if self.engine.rec_is_loop(t) && self.engine.rec_state_of(t) == TR_IDLE {
                        self.engine.start_rec(t);
                        self.engine.clear_rec_skip(t);
                    }
                } else if !self.engine.rec_is_loop(t) {
                    self.lr_restart[t] = u64::MAX;
                }
            }
        }

        let mut peak_val: u32 = 0;
        let mut mon_pre = (0.0f32, 0.0f32);
        let mut pos_now = self.pos;
        for i in 0..n {
            // Output buffer writes beyond `block_n` (the auto-stop tail of a
            // non-looping block) still need to emit silence.
            let live = i < block_n;

            // Dry mix of the connected inputs.
            let mut dry = 0.0f32;
            for b in ins {
                dry += unsafe { *b.add(i) };
            }
            // Live input for this sample. Latency compensation is NOT a
            // running input delay: it only discards the first `latency`
            // samples of a fresh take (engine `rec_skip`), so monitoring
            // stays exactly aligned with the timeline grid while the
            // button-press silence at a take's start gets trimmed away.
            let src_in = dry;

            // Take capture (raw input; independent of the mix and of the
            // transport — an armed track records whenever there is input,
            // even while the transport is parked at the project end).
            if any_rec {
                for &ti in &rec_tracks[..nrec] {
                    let t = ti as usize;
                    if self.engine.rec_sample(t, src_in) && self.rec_full & (1u128 << t) == 0 {
                        // Capacity reached: stop capturing into this track;
                        // the manager still hasn't been told, so finish the
                        // take here and hand it over asynchronously.
                        self.rec_full |= 1u128 << t;
                        self.engine.rec_state[t].store(TR_IDLE, Ordering::Relaxed);
                        self.engine.mark_take_pending(t);
                        // v42: the take cap ends a loop for good.
                        if self.engine.rec_is_loop(t) {
                            self.engine.stop_rec_loop(t);
                            self.lr_restart[t] = u64::MAX;
                        }
                    }
                }
            }

            // Clip mix: sum the entries covering this sample into their
            // track accumulators, then run the mixer over every sounding
            // track (chains keep decaying through silent gaps).
            let mut play_l = 0.0f32;
            let mut play_r = 0.0f32;
            if live && mixer_live {
                if nacc != 0 {
                    for k in 0..nacc {
                        acc[acc_list[k] as usize] = 0.0;
                    }
                    for e in entries.iter() {
                        if i >= e.i0 && i < e.i1 {
                            let mut v = unsafe { *e.ptr.add(i - e.i0) } * e.gain;
                            let s = p0 + i as u64;
                            if s < e.fi_end {
                                v *= (s - e.pos) as f32 * e.fi_step;
                            } else if s >= e.fo_start {
                                v *= (e.fo_end - s) as f32 * e.fo_step;
                            }
                            acc[e.track as usize] += v;
                        }
                    }
                    if has_midi {
                        for &tt in &acc_list[..nacc] {
                            let t = tt as usize;
                            if self.midi_live[t] && i < block_n {
                                acc[t] += self.midi_scratch[t][i];
                            }
                        }
                    }
                }
                for &tt in &mix_list[..nmix] {
                    let t = tt as usize;
                    let a = acc[t];
                    // v32 R7: ramped track trim (block-constant `amp` was
                    // the click at mute toggles / volume jumps).
                    let av = amp_a[t] + amp_s[t] * i as f32;
                    // v32 R5: sidechain tap — the track's dry post-trim
                    // sum feeds every target that lists it as a source
                    // (consumed NEXT block: one block of detector lag).
                    if sc_src_any & (1u128 << t) != 0 {
                        let tap = a * av;
                        if tap != 0.0 || n_edge != 0 {
                            for k in 0..n_edge {
                                if e_src[k] as usize == t {
                                    sc_build[e_tgt[k] as usize][i] += tap;
                                }
                            }
                        }
                    }
                    if vst_mask[t] {
                        // Held back: this track's chain (scripts and
                        // plugins, in chain order) runs block-wise over
                        // the pre-pan sum after the sample loop.
                        trk_bufs[t][i] = a * av;
                        trk_bufs2[t][i] = a * av;
                    } else {
                        // v32 R5: this track's scripted chain sees last
                        // block's sidechain sum (0.0 when no send routed).
                        let scv = if self.sc_active[t] {
                            sc_apply[t][i]
                        } else {
                            0.0
                        };
                        // Tier-3: the chain returns a pair; mono-only
                        // chains return (y, y), so the pan fold is the
                        // historical mono law bit-for-bit.
                        let (ml, mr) = self.fx_mix(t, a * av, scv);
                        let (pl, pr) = (ml * pgl[t], mr * pgr[t]);
                        // v42 "p": preview take — tap the track bus right
                        // here (post-FX, post-trim, mono fold): exactly the
                        // contribution this track adds to the master.
                        if any_pv
                            && pv_mask & (1u128 << t) != 0
                            && self.engine.rec_sample(t, (pl + pr) * F1S2)
                            && self.rec_full & (1u128 << t) == 0
                        {
                            self.rec_full |= 1u128 << t;
                            self.engine.rec_state[t].store(TR_IDLE, Ordering::Relaxed);
                            self.engine.mark_take_pending(t);
                        }
                        play_l += pl;
                        play_r += pr;
                    }
                }
            }

            // Metronome click (added to the output mix only — `dry` never
            // sees it, so it can never be recorded into a take). While the
            // transport runs the clicks tick off the transport position;
            // with it stopped they tick off the free-running click clock,
            // which leaves the playhead untouched.
            // v37 R3: while a PREVIEW buffer plays, the free-running click
            // is muted — the piano roll stamps its pattern-locked click
            // grid INTO the preview audio (two phase-unsynced metros at
            // once was unusable for composing).
            let mut click = 0.0f32;
            if live && metro_on && !self.preview_live {
                let p = if advance {
                    (p0 + i as u64) as f64
                } else {
                    metro_base + i as f64
                };
                let idx = (p / sub_len) as i64;
                if idx > self.metro_idx {
                    self.metro_idx = idx;
                    self.metro_start = self.sample_no as i64;
                    // Tone of this click: the first quarter note of the
                    // measure is the downbeat, the other quarter notes get
                    // the plain quarter tone, everything in between gets the
                    // subdivision tone.
                    let beat = (idx / metro_sub as i64) % metro_num as i64;
                    let k = idx % metro_sub as i64;
                    self.metro_tone = if k == 0 {
                        if beat == 0 {
                            0
                        } else {
                            1
                        }
                    } else {
                        2
                    };
                }
            }
            if self.metro_start >= 0 {
                let k = self.sample_no as i64 - self.metro_start;
                if k >= 0 && (k as u32) < metro_dur {
                    let kf = k as f32;
                    // Three distinct tones: downbeat (high, loudest),
                    // quarter note (mid), subdivision (low, softest).
                    let (f, a) = match self.metro_tone {
                        0 => (1760.0f32, 0.5f32),
                        1 => (1175.0, 0.35),
                        _ => (880.0, 0.25),
                    };
                    click = (kf * std::f32::consts::TAU * f / self.sr as f32).sin()
                        * (-kf / metro_decay).exp()
                        * a;
                } else {
                    self.metro_start = -1;
                }
            }
            self.sample_no += 1;

            // Asset audition: next sample of the preview buffer, dead
            // centre like the click, output mix only (never recorded).
            // Auto-stops when the buffer runs out.
            let mut pvc = 0.0f32;
            if self.preview_live {
                let mut any = false;
                for (k, (_, idx)) in self.preview_curs.iter_mut().enumerate() {
                    let buf = &self.preview[k].2;
                    if *idx < buf.len() {
                        pvc += buf[*idx] * F1S2;
                        *idx += 1;
                        any = true;
                    }
                }
                if !any {
                    // All voices finished: auto-stop like a single one-shot.
                    self.preview_live = false;
                    self.engine.preview_on.store(0, Ordering::Relaxed);
                    // v40: arm the UI retire pass (App::logic) — the engine
                    // pool still holds the dead voice and its sample Arc;
                    // leaving it is what made previews replay when a new RT
                    // handler reseeded cursors at 0.
                    self.engine.preview_drained.store(true, Ordering::Relaxed);
                }
            }

            // v46: default-route audition row (headless piano copy) —
            // dead centre like the click, output mix only (never recorded
            // into a take).
            let pvca = if self.aud_pv_any && i < self.aud_pv_row.len() {
                self.aud_pv_row[i] * F1S2
            } else {
                0.0
            };

            // Live input rides the selected track's chain/trim/pan; the click
            // always sits dead centre. At unity trim and centre pan the L/R
            // pair still sums to the historical unity mono mix.
            // The monitored chain still runs every sample (tails decay);
            // with a VST insert on the monitored track its output is held
            // back for the plugin instead of feeding the mix directly.
            let mut mon = (0.0f32, 0.0f32);
            let mon_i = mon_a + mon_s * i as f32;
            if sel_vst {
                if monitor_live {
                    mon_buf[i] = src_in * mon_i;
                    mon_buf2[i] = src_in * mon_i;
                }
            } else if monitor_live
                && (src_in != 0.0 || mon_pre.0 * mon_pre.0 + mon_pre.1 * mon_pre.1 > 1e-12)
            {
                // Runs only while input is present or the monitor's own
                // tail still rings; otherwise the playback path stays the
                // ONLY writer to the chain's script instances.
                mon = self.fx_mix(sel, src_in * mon_i, 0.0);
            }
            mon_pre = mon;
            let clickc = click * F1S2;
            // Pre-master, pre-gain mix; ports get written post-loop (once any
            // VST inserts have run). The output mix is NEVER delayed: what
            // the user hears stays exactly on the timeline grid (latency
            // compensation only trims the start of a fresh take — see
            // `Engine::rec_sample`).
            mix_l[i] = mon.0 * mon_l + clickc + play_l + pvc + pvca;
            mix_r[i] = mon.1 * mon_r + clickc + play_r + pvc + pvca;

            // Advance the transport clock (only while running).
            if live && advance {
                pos_now += 1;
            }
        }

        // Plugin chains: run the held-back track sums (pre-pan) and the
        // monitor tap through their full chains block-wise, folding each
        // result into the mix at the track's pan / the monitor level.
        if has_vst {
            for t in 0..MAX_LOOPS {
                if !vst_mask[t] {
                    continue;
                }
                let (buf, buf2, tmp) = (&mut trk_bufs[t], &mut trk_bufs2[t], &mut vst_tmp);
                // v32 R5: scripted slots inside a plugin-bearing chain get
                // their sidechain row too; the plugin slots ignore it.
                let scrow: &[f32] = if self.sc_active[t] {
                    &sc_apply[t]
                } else {
                    &sc_empty
                };
                self.run_chain_block(t, buf, buf2, tmp, scrow);
                // v42 "p": preview tap for plugin-bearing chains (same
                // post-FX, post-trim mono fold as the scripted path).
                let pvt = any_pv && pv_mask & (1u128 << t) != 0;
                for i in 0..n {
                    let (l, r) = (buf[i] * pgl[t], buf2[i] * pgr[t]);
                    mix_l[i] += l;
                    mix_r[i] += r;
                    if pvt
                        && self.engine.rec_sample(t, (l + r) * F1S2)
                        && self.rec_full & (1u128 << t) == 0
                    {
                        self.rec_full |= 1u128 << t;
                        self.engine.rec_state[t].store(TR_IDLE, Ordering::Relaxed);
                        self.engine.mark_take_pending(t);
                    }
                }
            }
            if sel_vst && monitor_live {
                let (buf, buf2, tmp) = (&mut mon_buf, &mut mon_buf2, &mut vst_tmp);
                self.run_chain_block(sel, buf, buf2, tmp, &sc_empty);
                for i in 0..n {
                    mix_l[i] += buf[i] * mon_l;
                    mix_r[i] += buf2[i] * mon_r;
                }
            }
        }

        // Master gain + port writes (moved out of the sample loop so VST
        // inserts can run before the mix leaves the callback).
        // v39: the master tap doubles the block for the GUI meter ring —
        // SPSC, never blocks; a stalled UI only drops meter resolution.
        let mut tap = std::mem::take(&mut self.tap_buf);
        tap.resize(n * 2, 0.0);
        for i in 0..n {
            // v32 R7: ramped master (g_a → g across the block).
            let gm = g_a + g_s * i as f32;
            let v_l = mix_l[i] * gm;
            let v_r = mix_r[i] * gm;
            tap[2 * i] = v_l;
            tap[2 * i + 1] = v_r;
            let av = v_l.abs().max(v_r.abs());
            if av.to_bits() > peak_val {
                peak_val = av.to_bits();
            }
            if nm > 0 {
                let v_m = (v_l + v_r) * F1S2;
                for b in &out_bufs_m[..nm] {
                    unsafe { *b.add(i) = v_m };
                }
            }
            for b in &out_bufs_l[..nl] {
                unsafe { *b.add(i) = v_l };
            }
            for b in &out_bufs_r[..nr] {
                unsafe { *b.add(i) = v_r };
            }
        }
        self.meter.write(&tap[..n * 2]);
        self.tap_buf = tap;

        // Give the per-block buffers back for the next block.
        self.mix_l = mix_l;
        self.mix_r = mix_r;
        self.trk_bufs = trk_bufs;
        self.trk_bufs2 = trk_bufs2;
        // v32 R5: publish this block's taps as next block's SC input.
        std::mem::swap(&mut sc_apply, &mut sc_build);
        self.sc_apply = sc_apply;
        self.sc_build = sc_build;
        self.mon_buf = mon_buf;
        self.mon_buf2 = mon_buf2;
        self.vst_tmp = vst_tmp;

        // Give the clip window list back for the next block.
        self.clip_buf = entries;

        // Continuous loop: at the loop region end the playhead wraps back to
        // the region start; a non-looping transport parks at the project end
        // and auto-stops (an event wakes the GUI so its button mirrors
        // reality). The block was clipped at `le`, so one wrap per block.
        if advance {
            if looping {
                if let Some(wrapped) = wrap_pos(pos_now, ls, le) {
                    pos_now = wrapped;
                    // Wrap re-anchors the metronome grid without a burst.
                    self.metro_idx = (pos_now as f64 / sub_len).ceil() as i64 - 1;
                    // v42: loop record — every pass commits as its own take
                    // and capture resumes ~50 ms after the wrap; passes
                    // stack as child clips, so the loop ADDS up by playback.
                    if loop_mask != 0 {
                        for t in 0..MAX_LOOPS {
                            if loop_mask & (1u128 << t) != 0
                                && self.engine.rec_state_of(t) == TR_REC
                            {
                                self.engine.rec_state[t].store(TR_IDLE, Ordering::Relaxed);
                                self.engine.mark_take_pending(t);
                                self.lr_restart[t] =
                                    pos_now + (self.engine.sample_rate() as u64) / 20;
                            }
                        }
                    }
                }
                self.pos = pos_now;
            } else {
                // v37 R5: a running take runs the playhead PAST the
                // project end (the arrangement grows when the take
                // bounces); only the parked/idle transport clamps.
                self.pos = if rec_open {
                    pos_now
                } else {
                    pos_now.min(proj_len)
                };
            }
        }
        // Keep the free-running click clock glued to the playhead while the
        // transport advances; with the transport stopped and the click on,
        // it counts on through the live samples on its own (the playhead
        // itself stays put).
        if advance {
            self.metro_clock = self.pos as f64;
        } else if metro_on {
            self.metro_clock = metro_base + block_n as f64;
        }
        if auto_stop && transport {
            self.engine.set_transport(false);
            self.prev_transport = false;
            let _ = self.tx.try_send(Evt::Transport { on: false });
        }
        // Publish the playhead for the GUI (display resolution is far
        // coarser than the block rate).
        self.engine.pos.store(self.pos, Ordering::Relaxed);
        self.engine.preview_pos.store(
            self.preview_curs
                .last()
                .map(|(_, i)| *i as u64)
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        self.peak.store(peak_val, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrange::Clip;

    /// The project render must run each track's effect chain (on private
    /// instances, leaving the live ones untouched) after the vol x gain trim,
    /// and honour per-track play/stop, panning and clip placement.
    #[test]
    fn v34_automation_clip_window_bounds_and_tail_hold() {
        use crate::automation::clip_source_pos as csp;
        // 1000-sample curve, clip plays the whole thing
        assert_eq!(csp(1000, 0, None, -1), None, "before the clip is silent");
        assert_eq!(csp(1000, 0, None, 0), Some(0));
        assert_eq!(csp(1000, 0, None, 400), Some(400));
        // a clip SHORTER than the curve wraps inside its own window
        assert_eq!(csp(1000, 0, Some(2000), 1500), Some(500));
        // past the window: HOLD the last played source sample — never a
        // wrap back to the beginning (that was the "value snaps back" bug)
        assert_eq!(csp(1000, 0, Some(500), 700), Some(499));
        assert_eq!(csp(1000, 0, Some(500), 500), Some(499));
        // a cut start shifts the window; the hold lands on the cut's last
        // sample, not on the curve head
        assert_eq!(csp(1000, 200, None, 2000), Some(999));
        assert_eq!(csp(1000, 200, Some(100), 150), Some(299));
    }

    #[test]
    fn pan_gains_are_equal_power_and_mono_safe() {
        // Hard positions.
        let (l, r) = pan_gains(0.0);
        assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6, "0 = hard left");
        let (l, r) = pan_gains(1.0);
        assert!(l.abs() < 1e-6 && (r - 1.0).abs() < 1e-6, "1 = hard right");
        // Centre: 1/sqrt2 on both buses, so the (L + R)/sqrt2 downmix of a
        // centred source is exactly unity — the historical mono mix intact.
        let (l, r) = pan_gains(0.5);
        assert!((l - F1S2).abs() < 1e-6 && (r - F1S2).abs() < 1e-6);
        assert!(((l + r) * F1S2 - 1.0).abs() < 1e-4);
        // Constant power across the whole field; clamped outside [0, 1].
        for k in 0..=16 {
            let p = k as f32 / 16.0;
            let (l, r) = pan_gains(p);
            assert!((l * l + r * r - 1.0).abs() < 1e-3, "power at p={p}");
        }
        assert_eq!(pan_gains(-2.0), pan_gains(0.0));
        assert_eq!(pan_gains(9.0), pan_gains(1.0));
    }

    #[test]
    fn engine_fresh() {
        let e = Engine::new();
        for i in 0..MAX_LOOPS {
            assert_eq!(e.rec_state_of(i), TR_IDLE);
            assert_eq!(e.rec_len_of(i), 0);
            assert!(e.track_on(i), "tracks start sounding");
        }
        assert!(!e.transport_on());
        assert_eq!(e.pos(), 0);
        assert!(e.loop_tl(), "the timeline loops by default");
        assert_eq!(e.take_seek(), None);
    }

    #[test]
    fn loop_pan_defaults_centre_and_clamps() {
        let e = Engine::new();
        assert!((e.loop_pan(3) - 0.5).abs() < 1e-9, "default is centre");
        e.set_loop_pan(3, 0.25);
        assert!((e.loop_pan(3) - 0.25).abs() < 1e-6);
        e.set_loop_pan(3, 4.0);
        assert!((e.loop_pan(3) - 1.0).abs() < 1e-6, "clamped to full right");
        e.set_loop_pan(3, -1.0);
        assert_eq!(e.loop_pan(3), 0.0, "clamped to full left");
    }

    /// v35 R1 pin: an automation clip drives the REAL control. Rendering
    /// the whole project walks the curve; the engine atomic behind the
    /// manual knob must end AT the curve's tail and HOLD there (nothing
    /// v35 R1 pin: automation writes REAL controls. `apply_auto_real` is
    /// the one function the RT block calls after the control-rate pass;
    /// driving it directly is driving the shipped code path (the offline
    /// render path intentionally uses manual values, so it cannot stand
    /// in for this test). A later pass without a target must not reset
    /// the stored value — pause holds, grab continues from it.
    #[test]
    fn v35_automation_writes_real_controls() {
        let e = Engine::new();
        let mut a = AutoOut::new();
        a.vol[1] = Some(0.3);
        a.gain[2] = Some(0.5);
        a.pan[3] = Some(-0.25);
        a.tempo = Some(96);
        e.apply_auto_real(&a);
        assert!(
            (e.loop_vol(1) - 0.3).abs() < 1e-6,
            "track vol real value: {}",
            e.loop_vol(1)
        );
        assert!((e.loop_gain(2) - 0.5).abs() < 1e-6, "gain real value");
        assert!(
            (e.loop_pan(3) + 0.25).abs() < 1e-6,
            "pan real value: {}",
            e.loop_pan(3)
        );
        assert_eq!(e.metro_bpm_now(), 96, "tempo real value");
        // Untargeted controls keep the manual value.
        assert!((e.loop_vol(0) - 1.0).abs() < 1e-6, "untargeted vol");
        // A block with no active clip does not snap anything back.
        e.apply_auto_real(&AutoOut::new());
        assert!(
            (e.loop_vol(1) - 0.3).abs() < 1e-6,
            "pause holds the real value"
        );
        assert_eq!(e.metro_bpm_now(), 96, "tempo holds");
    }

    #[test]
    fn seek_clamps_and_publishes() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 1000);
        e.seek_to(400);
        assert_eq!(e.pos(), 400);
        assert_eq!(e.take_seek(), Some(400));
        assert_eq!(e.take_seek(), None);
        // v35 R2: past the project end the playhead rides the tail grid —
        // clamped at `SEEK_SLACK` samples BEYOND the end, not at it.
        e.seek_to(5000);
        assert_eq!(e.pos(), 5000, "past-the-end placement is legal");
        e.seek_to(u64::MAX);
        assert_eq!(e.pos(), 1000 + SEEK_SLACK, "clamped at end + slack");
    }

    /// v35 R2 pin: the playhead can be placed beyond the last measure.
    #[test]
    fn v35_seek_placeable_beyond_project_end() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 1000);
        e.seek_to(1000 + 700);
        assert_eq!(e.pos(), 1700);
        // A shrinking project pulls an over-tail playhead back in bounds.
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 900);
        assert_eq!(e.pos(), 900, "project shrink keeps the playhead in bounds");
    }

    #[test]
    fn take_capture_and_finish_roundtrip() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 10_000);
        e.seek_to(500);
        e.start_rec(2);
        assert_eq!(e.rec_state_of(2), TR_REC);
        assert_eq!(
            e.rec_at(2),
            500,
            "the take remembers where the playhead was"
        );
        // Simulate RT capture.
        for k in 0..100u32 {
            e.rec_write(2, k, k as f32 * 0.01);
        }
        e.rec_len[2].store(100, Ordering::Relaxed);
        let (at, data, _pv) = e.copy_take(2);
        assert_eq!(at, 500);
        assert_eq!(data.len(), 100);
        assert!((data[50] - 0.5).abs() < 1e-6);
        assert_eq!(e.rec_state_of(2), TR_IDLE);
    }

    /// v42 "p": the preview arming must snapshot at start_rec, survive the
    /// take, report out of copy_take exactly once, and clear for next time.
    #[test]
    fn v42_preview_take_arming_snapshots_and_clears() {
        let e = Engine::new();
        // plain take: no preview flag anywhere
        e.start_rec(1);
        assert!(!e.rec_is_preview(1), "default take is an audio take");
        let (_, _, pv) = e.copy_take(1);
        assert!(!pv, "plain take reports preview=false");
        // preview take
        e.set_rec_preview(2, true);
        e.start_rec(2);
        assert!(e.rec_is_preview(2));
        e.set_rec_preview(2, false);
        assert!(
            e.rec_is_preview(2),
            "start_rec snapshots — mid-take flips ignored"
        );
        let (_, _, pv) = e.copy_take(2);
        assert!(pv, "the finished take reports preview=true");
        assert!(!e.rec_is_preview(2), "cleared at commit");
        e.start_rec(2);
        assert!(!e.rec_is_preview(2), "the next take starts clean");
        e.copy_take(2);
    }

    /// v42: pass commits must keep the loop flag across start_rec restarts,
    /// while STOP (stop_rec_loop) clears armed AND live.
    #[test]
    fn v42_loop_take_flags_survive_pass_restarts() {
        let e = Engine::new();
        e.set_rec_loop(3, true);
        e.start_rec(3);
        assert!(e.rec_is_loop(3), "the take inherits the loop arming");
        // A pass commit (RT idles + pends, manager copies) must NOT clear
        // the flag — the user has not stopped yet.
        e.copy_take(3);
        assert!(e.rec_is_loop(3), "pass commit keeps the loop alive");
        e.start_rec(3); // next pass
        assert!(e.rec_is_loop(3), "restart re-snapshots from the live flag");
        e.copy_take(3);
        e.stop_rec_loop(3);
        assert!(!e.rec_is_loop(3), "STOP clears armed AND live");
        assert!(!e.rec_loop_armed(3), "STOP clears armed AND live");
    }

    /// Latency compensation is a START-of-take skip: a fresh recording
    /// discards its first `latency` input samples (so the button click that
    /// started it — a few ms late through the mic — lands at content[0]),
    /// while everything after the skip is captured live.
    /// Live input monitoring defaults on and mutes ONLY the pass-through
    /// gain when switched off (regression pin for the Settings toggle).
    #[test]
    fn monitoring_toggle_gates_live_input() {
        let e = Engine::new();
        assert!(e.monitor_on(), "monitoring defaults on (current behavior)");
        assert_eq!(e.monitor_amp(0.75), 0.75);
        e.set_monitor_on(false);
        assert_eq!(e.monitor_amp(0.75), 0.0, "off mutes the pass-through");
        e.set_monitor_on(true);
        assert_eq!(e.monitor_amp(0.75), 0.75, "back on restores it");
    }

    #[test]
    fn latency_skips_silence_at_recording_start() {
        let e = Engine::new();
        e.latency.store(5, Ordering::Relaxed);
        e.start_rec(0);
        assert_eq!(e.rec_state_of(0), TR_REC);
        for k in 1..=10 {
            assert!(!e.rec_sample(0, k as f32));
        }
        // The first 5 samples were discarded; content starts at sample #6 —
        // exactly the latency-compensated moment.
        let (_at, data, _pv) = e.copy_take(0);
        assert_eq!(&data[..], &[6.0, 7.0, 8.0, 9.0, 10.0]);

        // A second take re-arms from the current latency, and a latency of 0
        // records immediately.
        e.latency.store(0, Ordering::Relaxed);
        e.start_rec(0);
        assert!(!e.rec_sample(0, 42.0));
        assert_eq!(e.rec_len_of(0), 1);
        assert_eq!(e.rec_read(0, 0), 42.0);
    }

    /// Changing the latency mid-take must not retroactively shift the
    /// recording in progress: the skip is armed at start_rec time.
    #[test]
    fn latency_change_mid_take_does_not_shift_recording() {
        let e = Engine::new();
        e.latency.store(3, Ordering::Relaxed);
        e.start_rec(0);
        assert!(!e.rec_sample(0, 1.0)); // consumes one armed skip
        e.latency.store(5000, Ordering::Relaxed); // huge change mid-take
        assert!(!e.rec_sample(0, 2.0)); // armed skip has 2 left
        assert!(!e.rec_sample(0, 3.0)); // ...and captures nothing more
        assert!(!e.rec_sample(0, 4.0)); // skip exhausted: capture resumes
        let (_at, data, _pv) = e.copy_take(0);
        assert_eq!(
            &data[..],
            &[4.0],
            "only the samples after the armed 3-sample skip"
        );
    }

    #[test]
    fn rec_sample_never_overshoots_capacity() {
        let e = Engine::new();
        // Fill exactly to capacity, one sample at a time (RT path).
        for _ in 0..LOOP_CAP {
            assert!(!e.rec_sample(0, 1.0));
        }
        assert_eq!(e.rec_len_of(0), LOOP_CAP);
        // Once full, further samples must report "full" and must not push the
        // counter past LOOP_CAP.
        for _ in 0..1024 {
            assert!(e.rec_sample(0, 0.0));
        }
        assert_eq!(e.rec_len_of(0), LOOP_CAP);
    }

    /// v0.9: per-clip gain + fades must be sample-accurate in the mixdown
    /// (this exercises the export path; the live path shares `fade_at`).
    #[test]
    fn v09_clip_gain_and_fade_shape_render() {
        let a: Arc<[f32]> = Arc::from(vec![1.0f32; 4000]);
        let mut arr = Arrangement::new();
        let mut cl = Clip::new(0, 0);
        cl.gain = 0.5;
        cl.fade_in = 1000;
        cl.fade_out = 1000;
        arr.insert(0, 0, cl);
        let e = Engine::new();
        e.set_project(Arc::new(vec![a]), Arc::new(arr), 4000);
        let out = e.render_project().expect("clips exist");
        assert!(out[0].abs() < 1e-4, "fade-in starts silent, got {}", out[0]);
        assert!((out[500] - 0.25).abs() < 1e-3, "mid fade-in ~ half gain");
        assert!((out[2000] - 0.5).abs() < 1e-3, "body carries the gain");
        assert!((out[3500] - 0.25).abs() < 1e-3, "mid fade-out ~ half gain");
        assert!(out[3999].abs() < 1e-3, "fade-out lands on silence");
    }

    /// v0.9: splitting a clip must not change a single sample — the right
    /// half continues the source at the matching offset.
    #[test]
    fn v09_split_renders_identically() {
        let a: Arc<[f32]> = Arc::from(
            (0..4000)
                .map(|k| if k < 2000 { 0.25f32 } else { 0.75 })
                .collect::<Vec<f32>>(),
        );
        let whole = Clip::new(0, 0);
        let mut arr1 = Arrangement::new();
        arr1.insert(0, 0, whole);
        let e1 = Engine::new();
        e1.set_project(Arc::new(vec![a.clone()]), Arc::new(arr1), 4000);
        let before = e1.render_project().unwrap();

        let (l, r) = whole.split_at(2000, a.len() as u64).unwrap();
        let mut arr2 = Arrangement::new();
        arr2.insert(0, 0, l);
        arr2.insert(0, 0, r);
        let e2 = Engine::new();
        e2.set_project(Arc::new(vec![a]), Arc::new(arr2), 4000);
        let after = e2.render_project().unwrap();
        let diff = before
            .iter()
            .zip(after.iter())
            .fold(0f32, |m, (x, y)| m.max((x - y).abs()));
        assert!(diff < 1e-9, "split changed the render by {diff}");
    }

    /// v0.9: MIDI clips carry the same clip properties — gain and the fade
    /// ramp scale NOTE VELOCITY (reaper-style item volume), silent parts
    /// drop the note entirely.
    #[test]
    fn v09_fade_scales_midi_velocity() {
        use crate::midipat::{Pattern, PPQ};
        let mut pat = Pattern::new(120);
        // beat 0 (fade-in, sample 0), beat 1 (24000 samples: past the fade)
        for start in [0u32, PPQ as u32] {
            pat.notes.push(crate::midipat::Note {
                ch: 0,
                note: 60,
                vel: 100,
                press: 0,
                start,
                len: (PPQ / 2) as u32,
            });
        }
        let mut cl = Clip::new(0, 0);
        cl.fade_in = 4800; // 100 ms
        let mut ev = Vec::new();
        stage_midi_events(&pat, &cl, 48_000, &mut ev);
        let ons: Vec<_> = ev.iter().filter(|e| e.1 == 3).cloned().collect();
        assert_eq!(ons.len(), 1, "note inside the silent fade start drops");
        assert_eq!(ons[0].0, 24000);
        assert_eq!(ons[0].4, 100);
        // Pure gain halves the velocity; 4x clamps at 127.
        let mut cl2 = Clip::new(0, 0);
        cl2.gain = 0.5;
        ev.clear();
        stage_midi_events(&pat, &cl2, 48_000, &mut ev);
        let ons: Vec<_> = ev.iter().filter(|e| e.1 == 3).cloned().collect();
        assert_eq!(ons.len(), 2);
        assert!(ons.iter().all(|e| e.4 == 50), "gain 0.5: {ons:?}");
        let mut cl3 = Clip::new(0, 0);
        cl3.gain = 4.0;
        ev.clear();
        stage_midi_events(&pat, &cl3, 48_000, &mut ev);
        let ons: Vec<_> = ev.iter().filter(|e| e.1 == 3).cloned().collect();
        assert!(ons.iter().all(|e| e.4 == 127), "gain clamps: {ons:?}");
    }

    /// Helper: two assets, track 0 clip at 0, track 1 clip offset by 1000.
    fn two_track_project() -> (Arc<Engine>, Arc<[f32]>, Arc<[f32]>) {
        let a0: Arc<[f32]> = Arc::from(vec![0.5f32; 2000]);
        let a1: Arc<[f32]> = Arc::from(vec![0.25f32; 2000]);
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0));
        arr.insert(1, 0, Clip::new(1, 1000));
        let e = Engine::new();
        e.set_project(Arc::new(vec![a0.clone(), a1.clone()]), Arc::new(arr), 4000);
        (e, a0, a1)
    }

    #[test]
    fn render_project_places_mutes_and_mixes() {
        let (e, _a0, _a1) = two_track_project();
        let out = e.render_project().expect("clips exist");
        assert_eq!(out.len(), 4000);
        // [0,1000): only track 0's asset (unity trims, centre pan).
        assert!((out[500] - 0.5).abs() < 1e-6);
        // [1000,2000): both assets summed additively.
        assert!((out[1500] - 0.75).abs() < 1e-6);
        // [2000,3000): track 1 only.
        assert!((out[2500] - 0.25).abs() < 1e-6);
        assert!((out[3999]).abs() < 1e-9, "past the last clip is silence");

        // Stopping a track mutes it in the render (the mix truth).
        e.set_track_on(0, false);
        let out = e.render_project().unwrap();
        assert!(out[500].abs() < 1e-9);
        assert!((out[2500] - 0.25).abs() < 1e-6);

        // Volume trim rides the render.
        e.set_track_on(0, true);
        e.set_loop_vol(1, 0.5);
        let out = e.render_project().unwrap();
        assert!((out[2500] - 0.125).abs() < 1e-5, "half volume");

        // Single-track export ignores the mute flag (explicit request).
        e.set_loop_vol(1, 1.0); // reset the trim so the level reads pure
        e.set_track_on(1, false);
        let one = e.render_track(1).unwrap();
        assert!((one[2500] - 0.25).abs() < 1e-5);
    }

    #[test]
    fn render_child_lanes_stack() {
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 1000]);
        let mut arr = Arrangement::new();
        // Two clips of the same asset overlapping on different lanes.
        arr.insert(4, 0, Clip::new(0, 0));
        arr.insert(4, 1, Clip::new(0, 500));
        let e = Engine::new();
        e.set_project(Arc::new(vec![a]), Arc::new(arr), 2000);
        let out = e.render_project().unwrap();
        assert!((out[250] - 0.5).abs() < 1e-6);
        assert!((out[750] - 1.0).abs() < 1e-6, "lanes sum additively");
    }

    #[test]
    fn render_empty_project_is_none() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 1000);
        assert!(e.render_project().is_none());
        assert!(e.render_track(0).is_none());
    }

    #[test]
    fn render_applies_effect_chain() {
        if !crate::fx::rustc_available() {
            return; // environments without a compiler skip the loader
        }
        let dir = std::env::temp_dir().join(format!("slooper-fxrender-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::fx::TEMPLATE_FILE), crate::fx::TEMPLATE_SRC).unwrap();
        let mut reg = crate::fx::Registry::open(dir.clone());
        reg.rescan();
        assert!(reg.desc("template").is_some(), "template must load");

        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 4000]);
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0));
        e.set_project(Arc::new(vec![a]), Arc::new(arr), 4000);
        let desc = reg.desc("template").unwrap().clone();

        // No chain: the render is the content itself (trim = 1).
        let plain = e.render_project().unwrap();
        assert!(plain.iter().take(4000).all(|&v| (v - 0.5).abs() < 1e-6));

        // Template at depth 0: exact pass-through, per the ABI contract.
        let inst0 = reg
            .create_instance("template", 48000.0, &[0.0, 4.0, 1.0, 1.0])
            .unwrap();
        e.set_track_fx(
            0,
            TrackFx {
                slots: std::sync::Arc::new(vec![FxSlot::scripted(inst0, desc.clone())]),
            },
        );
        assert_eq!(e.fx_gen(), 1);
        let thru = e.render_project().unwrap();
        assert!(
            thru.iter().take(4000).all(|&v| (v - 0.5).abs() < 1e-3),
            "depth 0 must pass through"
        );

        // depth 1 / mix 1: the tremolo must dip the render (a full LFO
        // cycle lands inside the 4000-sample window at 4 Hz).
        let inst1 = reg
            .create_instance("template", 48000.0, &[1.0, 4.0, 1.0, 1.0])
            .unwrap();
        e.set_track_fx(
            0,
            TrackFx {
                slots: std::sync::Arc::new(vec![FxSlot::scripted(inst1.clone(), desc)]),
            },
        );
        assert_eq!(e.fx_gen(), 2);
        let wet = e.render_project().unwrap();
        let lo = wet.iter().take(4000).copied().fold(f32::MAX, f32::min);
        assert!(lo < 0.45, "tremolo must modulate the render (min was {lo})");
        // Rendering ran on private clones: the live instance is untouched.
        assert_eq!(inst1.param(0), 1.0);
        assert_eq!(e.fx_table()[0].slots().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_project_bounds_the_playhead() {
        let e = Engine::new();
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 10_000);
        e.seek_to(9_000);
        // A project can shrink under the playhead — it clamps to the new end.
        e.set_project(Arc::new(vec![]), Arc::new(Arrangement::new()), 1_000);
        assert_eq!(e.pos(), 1_000);
        assert_eq!(e.proj_len(), 1_000);
    }

    #[test]
    fn metro_defaults_roundtrip() {
        let e = Engine::new();
        assert_eq!(e.metro(), (false, 120, 1, 4));
    }

    // ===================== v0.10 acceptance pins (v40 semantics) ============
    //
    // v40 flipped the v0.10 ADDITIVE preview pool (stack up to 12 voices) to
    // a SINGLE replacing voice: rack browsing stacked overlapping previews
    // that never retired and looked like "preview runs constantly". The
    // per-id stop stays (rack row toggle), only the stacking died.

    /// AC8.1/8.2 (v40): a new preview start REPLACES the previous voice —
    /// one preview at a time, with a fresh unique voice id.
    #[test]
    fn v40_preview_new_start_replaces_previous() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 100]);
        let b: Arc<[f32]> = Arc::from(vec![0.25f32; 80]);
        e.play_preview(1, a.clone());
        let vid1 = e.preview_snapshot()[0].0;
        e.play_preview(2, b.clone());
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 1, "a new start retires the previous voice");
        assert_eq!(snap[0].1, 2, "the NEW asset is the one auditioning");
        assert_ne!(snap[0].0, vid1, "voice ids are unique");
        assert!(e.preview_active());
    }

    #[test]
    fn v40_preview_repeat_same_asset_restarts() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 100]);
        e.play_preview(7, a.clone());
        e.play_preview(7, a);
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 1, "a repeat restarts — never stacks");
        assert_ne!(snap[0].0, 0);
        assert!(e.preview_active());
    }

    #[test]
    fn v40_preview_stop_clears() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 40]);
        e.play_preview(3, a.clone());
        assert!(e.preview_active());
        // Rack toggle on a voice that was already replaced: no-op.
        e.stop_preview_id(1);
        assert!(e.preview_active(), "stopping an absent id keeps the voice");
        // Stopping the live id empties the pool and clears the flag.
        e.stop_preview_id(3);
        assert!(e.preview_snapshot().is_empty());
        assert!(!e.preview_active(), "nothing left: auditions fully stopped");
        // Global stop clears everything at once.
        e.play_preview(5, a);
        e.stop_preview();
        assert!(e.preview_snapshot().is_empty());
        assert!(!e.preview_active());
    }

    #[test]
    fn v40_preview_pool_stays_single() {
        let e = Engine::new();
        for i in 0..30u32 {
            let a: Arc<[f32]> = Arc::from(vec![0.05f32; 20]);
            e.play_preview(i, a);
        }
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 1, "single-voice pool: {}", snap.len());
        assert_eq!(snap.last().unwrap().1, 29, "the most recent start survives");
    }

    /// v40: the RT→UI retire handshake. RT drains → flag; UI `take_*`
    /// consumes it EXACTLY once; a fresh start re-arms (clears) it.
    #[test]
    fn v40_drain_flag_roundtrip() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 10]);
        e.play_preview(1, a.clone());
        assert!(!e.take_preview_drained(), "no drain yet");
        assert!(!e.preview_pool_empty(), "voice is in the pool");
        e.debug_mark_auditions_drained(); // simulate the RT drain branch
        assert!(e.take_preview_drained(), "flag consumed exactly once");
        assert!(!e.take_preview_drained(), "flag cleared by the take");
        assert!(e.take_aud_drained(), "audition flag mirrored too");
        // A fresh start must never inherit the drain flag.
        e.debug_mark_auditions_drained();
        e.play_preview(2, a);
        assert!(!e.take_preview_drained(), "new start disarms the old flag");
    }

    /// AC7.2: per-block work scales with ACTIVE tracks, not the 128
    /// capacity — the compact lists must hold exactly the sounding ones.
    #[test]
    fn v10_active_track_list_compact() {
        let mut audible = [true; MAX_LOOPS];
        let no = [false; MAX_LOOPS];
        let mut midi_live = [false; MAX_LOOPS];
        let mut chain = [false; MAX_LOOPS];

        // One clip on track 70 in a 128-track project.
        let l = build_mix_lists(1u128 << 70, false, &midi_live, &audible, &no, &chain);
        assert_eq!(l.n_mix, 1, "only the clip track reaches the mix loop");
        assert_eq!(l.mix[0], 70);
        assert_eq!(l.n_acc, 1);

        // Live MIDI on 9 and an FX chain tail on 5 add exactly two more.
        midi_live[9] = true;
        chain[5] = true;
        let l = build_mix_lists(1u128 << 70, true, &midi_live, &audible, &no, &chain);
        assert_eq!(l.n_mix, 3);
        assert_eq!(l.n_acc, 2, "tail track accumulates nothing");

        // Inaudible (track-off) contributes nothing at all.
        audible = [false; MAX_LOOPS];
        let l = build_mix_lists(1u128 << 70, true, &midi_live, &audible, &no, &chain);
        assert_eq!(l.n_mix, 0);
    }

    /// AC7.1: tracks past the old 16 limit are real: track 64 plays, and
    /// its play flag mutes it like any other track.
    #[test]
    fn v10_track_64_plays_and_mutes() {
        let a: Arc<[f32]> = Arc::from(
            (0..1000)
                .map(|k| ((k % 40) as f32) * 0.01 - 0.2)
                .collect::<Vec<f32>>(),
        );
        let mut arr = Arrangement::new();
        arr.insert(64, 0, Clip::new(0, 0));
        let e = Engine::new();
        e.set_project(Arc::new(vec![a]), Arc::new(arr), 1000);
        let out = e.render_project().expect("render");
        let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.05, "clip on track 64 must sound (peak {peak})");

        e.set_track_on(64, false);
        let out = e.render_project().unwrap_or_default();
        let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak < 1e-9, "stopped track 64 must be silent, got {peak}");
    }

    /// AC6.1 (sample side): a track's audio clip renders EXACTLY its
    /// samples — no synth, no doubling — even when the same engine also
    /// holds a MIDI clip elsewhere.
    #[test]
    fn v10_audio_clip_is_sample_only_midi_is_synth_only() {
        let a: Arc<[f32]> = Arc::from(
            (0..600)
                .map(|k| (k as f32) * 0.001 - 0.3)
                .collect::<Vec<f32>>(),
        );
        // Pattern asset (id 1) on track 2: the SAMPLE path must ignore it
        // entirely — pattern audio comes only through the track's synth,
        // which the head render never runs.
        let pat = crate::midipat::Pattern::new(120);
        let mut arr = Arrangement::new();
        arr.insert(1, 0, Clip::new(0, 0));
        arr.insert(2, 0, Clip::new(1, 0));
        let e = Engine::new();
        e.set_project(
            Arc::new(vec![a.clone(), Arc::from(Vec::<f32>::new())]),
            Arc::new(arr),
            600,
        );
        e.set_midi_assets(Arc::new(vec![None, Some(std::sync::Arc::new(pat))]));
        let out = e.render_project().expect("render");
        assert_eq!(out.len(), 600);
        for (i, want) in a.iter().enumerate() {
            assert!(
                (out[i] - want).abs() < 1e-6,
                "sample {i}: {} vs {want} — audio clip must render exactly its samples",
                out[i]
            );
        }
        // A pattern clip contributes zero samples to the render.
        let mut arr2 = Arrangement::new();
        arr2.insert(2, 0, Clip::new(1, 0));
        let e2 = Engine::new();
        e2.set_project(Arc::new(vec![a]), Arc::new(arr2), 600);
        e2.set_midi_assets(Arc::new(vec![
            None,
            Some(std::sync::Arc::new(crate::midipat::Pattern::new(120))),
        ]));
        let out = e2.render_project().unwrap_or_default();
        let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(
            peak < 1e-9,
            "a MIDI clip never renders raw samples (peak {peak})"
        );
    }

    /// AC1.3: the full user flow with NO re-selection ritual: bind a fresh
    /// sampler to asset id 2, play a note (must sound); then the user drags
    /// a NEW sample in under the same asset id and plays again — the new
    /// content sounds immediately. Both stages use the SAME instance, i.e.
    /// exactly what the track keeps while the user swaps samples.
    #[test]
    fn v10_sampler_binding_needs_no_ritual() {
        let dir = std::env::temp_dir().join(format!("sp-ritual-{}", std::process::id()));
        std::env::set_var("SLOOPER_SYNTHS_DIR", &dir);
        let reg = crate::synth::Registry::open(dir.clone());
        std::env::remove_var("SLOOPER_SYNTHS_DIR");

        let table = |v: f32| {
            let mut t: Vec<crate::assets::AssetData> = (0..3)
                .map(|_| std::sync::Arc::from(Vec::<f32>::new()))
                .collect();
            t[2] = std::sync::Arc::from(vec![v; 1200]);
            std::sync::Arc::new(t)
        };
        crate::synth::set_host_assets(table(0.85), 48_000);
        let inst = reg
            .create_instance("sampler", 48_000.0, &[2.0, 48.0, 1.0])
            .expect("fresh sampler binds without ritual");
        let render = |inst: &crate::synth::Instance| {
            let mut ev = vec![(0u64, 3u8, 0u8, 60u8, 110u8), (30_000, 0, 0, 60, 0)];
            let mut out = vec![0f32; 48_000 / 8];
            render_midi_events(inst, &mut ev, 0, out.len(), &mut out);
            out.iter().fold(0f32, |m, x| m.max(x.abs()))
        };
        let first = render(&inst);
        assert!(first > 0.5, "first bound sample must sound (peak {first})");
        // User drags a new sample in (same id, new contents) — play again.
        crate::synth::set_host_assets(table(0.05), 48_000);
        let second = render(&inst);
        assert!(
            second < first / 8.0 && second > 0.01,
            "new sample must sound without re-selecting the synth: {second} vs {first}"
        );
        crate::synth::set_host_assets(std::sync::Arc::new(Vec::new()), 48_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC6.2 (v40 semantics): preview is a SINGLE replacing voice and the
    /// per-id stop stays a safe no-op for ids the pool no longer holds.
    #[test]
    fn v10_audition_voices_are_additive_and_isolated() {
        let e = Engine::new();
        let a: Arc<[f32]> = Arc::from(vec![0.5f32; 500]);
        let b: Arc<[f32]> = Arc::from(vec![0.5f32; 500]);
        e.play_preview(1, a.clone());
        e.play_preview(1, a.clone()); // v40: a repeat replaces, never stacks
        assert_eq!(e.preview_snapshot().len(), 1, "one voice after repeats");
        e.play_preview(2, b);
        let snap = e.preview_snapshot();
        assert_eq!(snap.len(), 1, "the new start replaced asset 1");
        assert_eq!(snap[0].1, 2);
        // Stopping an id the pool no longer holds: no-op, never panics,
        // and it must not stop the live voice of another asset.
        e.stop_preview_id(1);
        assert_eq!(e.preview_snapshot().len(), 1, "the live voice survives");
        e.stop_preview_id(99); // unknown id: no-op, never panics
        assert_eq!(e.preview_snapshot().len(), 1);
        e.stop_preview_id(2);
        assert!(
            e.preview_snapshot().is_empty(),
            "stopping the live id clears"
        );
    }

    /// v32: the live-MIDI feed subset. The bug: MIDI reached the UI (the
    /// monitor blinked, bindings fired, recording armed) and NOTHING fed
    /// the synths live. The feed speaks exactly the synth's event
    /// vocabulary — note on/off, CC, channel pressure — and nothing else
    /// (pitch bend is recorded per-clip and rewritten at its own grid
    /// resolution; feeding it live would fight that, same reason the
    /// recorder keeps `live_ev` empty for it).
    #[test]
    fn v32_live_event_kind_is_the_feed_vocabulary() {
        assert_eq!(live_event_kind(&[0x90, 60, 100]), Some((3, 0, 60, 100)));
        assert_eq!(live_event_kind(&[0x9F, 127, 1]), Some((3, 15, 127, 1)));
        // note-on vel 0 and 0x8x are the same note-off to the feed.
        assert_eq!(live_event_kind(&[0x90, 60, 0]), Some((0, 0, 60, 0)));
        assert_eq!(live_event_kind(&[0x83, 61, 0]), Some((0, 3, 61, 0)));
        assert_eq!(live_event_kind(&[0xB1, 7, 90]), Some((1, 1, 7, 90)));
        assert_eq!(live_event_kind(&[0xD2, 40]), Some((2, 2, 0, 40)));
        // Outside the vocabulary (the e2e monitor's own message list sends
        // program change / system — they must drop, not wedge anything).
        assert_eq!(live_event_kind(&[0xC2, 11]), None); // program change
                                                        // v42/MPE: bend (kind 5, LSB/MSB) and poly AT (kind 4, note/val)
                                                        // are parsed now — the engine's MPE switch gates whether they ever
                                                        // reach an instrument.
        assert_eq!(live_event_kind(&[0xE0, 0, 64]), Some((5, 0, 0, 64))); // pitch bend
        assert_eq!(live_event_kind(&[0xA3, 60, 9]), Some((4, 3, 60, 9))); // poly AT
        assert_eq!(live_event_kind(&[0xF0, 1, 2]), None); // system
        assert_eq!(live_event_kind(&[0x3C]), None); // non-status
        assert_eq!(live_event_kind(&[]), None);
        // Truncated note-on: velocity 0 reads as a release, never a stuck
        // note.
        assert_eq!(live_event_kind(&[0x90, 60]), Some((0, 0, 60, 0)));
    }

    /// v33: the shared-instance staging contract. One synth handle bound
    /// to (or defaulted onto) several tracks is fed ONE merged stream:
    /// each sharer's events are staged, and EXACT duplicates — the live
    /// keyboard feed seen through every sharing track — collapse to a
    /// single delivery. Without this the plugin's voice allocator saw
    /// doubled note-ons and stole held notes under dense chords.
    #[test]
    fn v33_shared_instance_stages_the_live_feed_once() {
        use crate::midipat::Pattern;
        let mut pat = Pattern::new(120);
        // One short note at tick 0: 10 ticks at 120 BPM = 500 samples
        // (PPQ is 480 — a quarter note is 24000 samples here).
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 10,
        });
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0));
        let cache: Vec<Option<std::sync::Arc<Pattern>>> = vec![Some(std::sync::Arc::new(pat))];
        // The keyboard holds C4 — staged ONCE into live_ev by the port.
        let live_ev = vec![(5usize, 3u8, 0u8, 60u8, 100u8)];

        // Owner staging pass: track 0 (its own clip + live) and track 1
        // (no clip, live) share this instance.
        let mut ev = Vec::new();
        let mut ring = 0u64;
        stage_track_events(
            0, &arr, &cache, &live_ev, true, true, 0, 512, 512, 48_000, &mut ev, &mut ring, None,
        );
        stage_track_events(
            1, &arr, &cache, &live_ev, true, false, 0, 512, 512, 48_000, &mut ev, &mut ring, None,
        );
        ev.sort_unstable();
        let doubled = ev.len();
        ev.dedup();
        assert_eq!(doubled, 4, "pattern on/off + two live copies expected");
        assert_eq!(
            ev,
            vec![(0, 3, 0, 60, 100), (5, 3, 0, 60, 100), (500, 0, 0, 60, 0)],
            "after dedup: pattern note (on+off) once, live note-on ONCE"
        );
        assert_eq!(ring, 500, "ring must track the scheduled note-off");

        // A live note held past the block must survive while `live` is
        // staged, and the ring keeps the engine advancing to its off.
        let held = vec![(9usize, 3u8, 2u8, 70u8, 90u8)];
        let mut ev = Vec::new();
        stage_track_events(
            4, &arr, &cache, &held, true, false, 0, 512, 512, 48_000, &mut ev, &mut ring, None,
        );
        assert_eq!(ev, vec![(9, 3, 2, 70, 90)]);
    }

    /// v33: the "knobs and sliders don't update unless a note is actively
    /// being played" gate. With a plugin synth bound, idle blocks MUST
    /// still render — the plugin's meters/mod/pedal state, and for VST2
    /// the very instance the GUI edits, only advance inside process().
    /// Script-only rigs keep the free quiet-tail pause; block 0 is priming.
    #[test]
    fn v33_render_gate_keeps_plugins_rendering_while_idle() {
        // Script-only idle: gated (pause, silence).
        assert!(render_gate_blocked(3, false, false, false));
        // Plugin synth bound: an idle block is NEVER skipped.
        assert!(!render_gate_blocked(3, false, false, true));
        // Playing or live input: never gated, any handle kind.
        assert!(!render_gate_blocked(3, true, false, false));
        assert!(!render_gate_blocked(3, false, true, false));
        // Priming: block 0 always skips audio.
        assert!(render_gate_blocked(0, true, true, true));
        // The plugin term must be load-bearing (pre-v33 code gated the
        // plugin-idle case exactly like script-idle — this fails on it).
        assert_ne!(
            render_gate_blocked(3, false, false, false),
            render_gate_blocked(3, false, false, true),
            "idle+plugin must differ from idle+script"
        );
    }

    /// v32: the bound-message exclusion for live playing. Rule (the same
    /// one MIDI recording already used): bound = control traffic. Note-off
    /// is NEVER claimed — blocking a bound note-on while its release passes
    /// is how stuck notes are born, and an unbound key must keep working.
    /// A bound CC knob owns its whole axis (continuous actions read CC 7
    /// under a CC 7 label).
    #[test]
    fn v32_boundset_claims_only_control_traffic() {
        let bs = BoundSet {
            exact: vec![bind_key(0, 0, 36), bind_key(1, 2, 7)],
            axes: vec![axis_key(1, 5)],
        };
        assert!(bs.claims(3, 0, 36), "bound note-on");
        assert!(!bs.claims(3, 1, 36), "same note, other channel");
        assert!(!bs.claims(3, 0, 37), "same channel, other note");
        assert!(bs.claims(1, 2, 7), "bound CC");
        assert!(bs.claims(1, 5, 42), "a knob owns its whole axis");
        assert!(!bs.claims(1, 6, 42), "neighbour CC feeds");
        assert!(
            !bs.claims(0, 0, 36),
            "note-off never claimed — no stuck notes"
        );
        assert!(
            !bs.claims(2, 3, 0),
            "channel pressure is unbindable → feeds"
        );
        assert!(!bs.claims(3, 0, 60), "unbound keys play");
        assert!(BoundSet::default().axes.is_empty());
    }

    /// v32: the engine-side live state the UI mirrors on boot and every
    /// change. Default ON (playing a controller should just work), a
    /// generation bump on bound-set refresh (the RT pulls at the next
    /// block), and the per-track recorder mask.
    #[test]
    fn v32_engine_live_midi_state() {
        let e = Engine::new();
        assert!(e.midi_live(), "live MIDI defaults ON");
        e.set_midi_live(false);
        assert!(!e.midi_live());
        assert!(!e.midi_rec_arm(3));
        e.set_midi_rec_arm(3, true);
        assert!(e.midi_rec_arm(3));
        e.set_midi_rec_arm(3, false);
        assert!(!e.midi_rec_arm(3));
        let g0 = e.midi_bound_gen();
        e.set_midi_bound(BoundSet {
            exact: vec![bind_key(0, 0, 60)],
            axes: vec![],
        });
        assert_eq!(e.midi_bound_gen(), g0 + 1, "RT refreshes on the gen bump");
        assert!(e.midi_bound().claims(3, 0, 60));
        drop(e);
    }

    /// v37 R5: the recording flag gates the project-end clamp — while a
    /// take runs the playhead lives PAST the old content end on purpose
    /// (this clamp firing mid-take was one more way the app truncated
    /// recordings at the arrangement length).
    #[test]
    fn v37_recording_flag_gates_the_project_end_clamp() {
        let e = Engine::new();
        let empty_assets: Arc<crate::audio::AssetTable> = Arc::new(Vec::new());
        let set = |e: &Engine| {
            e.set_project(
                empty_assets.clone(),
                Arc::new(crate::arrange::Arrangement::new()),
                2000,
            )
        };
        set(&e);
        assert!(!e.recording(), "not recording by default");
        e.force_pos(5000);
        set(&e);
        assert_eq!(e.pos(), 2000, "idle: playhead clamps back into the project");
        e.set_recording(true);
        assert!(e.recording());
        e.force_pos(5000);
        set(&e);
        assert_eq!(e.pos(), 5000, "recording: playhead runs past the old end");
        e.set_recording(false);
    }

    #[test]
    fn v38_audition_voice_lifecycle() {
        let e = Engine::new();
        assert!(!e.audition_active(), "nothing armed by default");
        let ev: Arc<[MidiEv]> = vec![(0u64, 3u8, 0u8, 60u8, 100u8), (16000, 0, 0, 60, 0)].into();
        e.play_audition(3, ev.clone(), None);
        assert!(e.audition_active(), "armed by a voice");
        assert_eq!(e.audition_snapshot().len(), 1, "one voice in the pool");
        e.play_audition(3, ev, None); // additive taps stack (v0.10 key semantics)
        assert_eq!(e.audition_snapshot().len(), 2);
        e.stop_audition();
        assert!(!e.audition_active(), "stopped");
        assert!(e.audition_snapshot().is_empty(), "pool purged");
        // An empty event list never arms the flag (an empty-pattern
        // preview must not stick the roll in STOP state).
        let empty: Arc<[MidiEv]> = vec![].into();
        e.play_audition(0, empty, None);
        assert!(!e.audition_active(), "empty voice is no voice");
    }

    /// v46: a voice given a headless synth COPY keeps it in the published
    /// snapshot — RT renders that instance, never the track's live synth.
    /// `AUD_PV` (the default no-track route) is a legal pseudo-slot.
    #[test]
    fn v46_audition_voice_carries_its_headless_copy() {
        let e = Engine::new();
        let reg = crate::synth::Registry::open(crate::synth::default_dir());
        let inst = reg
            .create_instance("piano", 44100.0, &[])
            .expect("bundled piano must compile and load");
        let h = std::sync::Arc::new(SynthHandle::Script(inst));
        let ev: Arc<[MidiEv]> = vec![(0u64, 3u8, 0u8, 60u8, 100u8)].into();
        e.play_audition(2, ev.clone(), Some(h.clone()));
        let snap = e.audition_snapshot();
        assert_eq!(snap.len(), 1);
        assert!(
            snap[0]
                .inst
                .as_ref()
                .is_some_and(|c| std::sync::Arc::ptr_eq(c, &h)),
            "the voice renders through the COPY it was given"
        );
        e.play_audition(AUD_PV, ev, Some(h));
        assert_eq!(
            e.audition_snapshot().len(),
            2,
            "PV pseudo-slot accepts voices"
        );
        e.stop_audition();
        assert!(e.audition_snapshot().is_empty(), "stop purges the pool");
    }

    /// Helper for the v46 poly pins: a 2-lane / 2-note arrangement.
    fn v46_poly_arrangement(second_lane_only: Option<bool>) -> crate::arrange::Arrangement {
        use crate::arrange::{Clip, Lane};
        let mut arr = crate::arrange::Arrangement::new();
        let clip = |a: u32| Clip::new(a, 0);
        match second_lane_only {
            Some(true) => {
                arr.tracks[0].lanes = vec![Lane {
                    clips: vec![clip(1)],
                }];
            }
            Some(false) => {
                arr.tracks[0].lanes = vec![Lane {
                    clips: vec![clip(0)],
                }];
            }
            None => {
                arr.tracks[0].lanes = vec![
                    Lane {
                        clips: vec![clip(0)],
                    },
                    Lane {
                        clips: vec![clip(1)],
                    },
                ];
            }
        }
        arr
    }

    /// v46: a fresh master instance of the bundled piano, bound as track
    /// 0's instrument (a bare Engine has no default synth).
    fn v46_piano_master() -> Arc<SynthHandle> {
        let reg = crate::synth::Registry::open(crate::synth::default_dir());
        Arc::new(SynthHandle::Script(
            reg.create_instance("piano", 44100.0, &[])
                .expect("bundled piano must compile and load"),
        ))
    }

    fn poly_row(e: &Engine, t: usize) -> usize {
        e.poly_voices().get(t).map(|r| r.len()).unwrap_or(0)
    }

    fn v46_poly_patterns() -> Arc<Vec<Option<Arc<crate::midipat::Pattern>>>> {
        use crate::midipat::{Note, Pattern};
        let mut pa = Pattern::new(120);
        pa.len_ticks = 480;
        pa.notes.push(Note {
            ch: 0,
            note: 48,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
        });
        let mut pb = Pattern::new(120);
        pb.len_ticks = 480;
        pb.notes.push(Note {
            ch: 0,
            note: 55,
            vel: 90,
            press: 0,
            start: 0,
            len: 240,
        });
        Arc::new(vec![Some(Arc::new(pa)), Some(Arc::new(pb))])
    }

    /// v46: the per-track poly mode flag + copy row publish atomically,
    /// keep published handles, and die with the mode.
    #[test]
    fn v46_poly_row_publishes_and_clears() {
        let e = Engine::new();
        assert!(!e.poly_on(0) && poly_row(&e, 0) == 0, "mono by default");
        let master = v46_piano_master();
        e.set_track_synth(0, Some(master.clone()));
        let copies: Vec<Option<Arc<SynthHandle>>> =
            (0..2).map(|_| spawn_synth_copy(&master, 44100)).collect();
        let keep = copies[0].clone();
        e.set_track_poly(0, true, copies);
        assert!(e.poly_on(0));
        assert_eq!(poly_row(&e, 0), 2, "one copy per child lane");
        e.set_project(
            Arc::new(vec![]),
            Arc::new(v46_poly_arrangement(None)),
            22050,
        );
        assert!(
            std::sync::Arc::ptr_eq(
                e.poly_voices()[0][0].as_ref().unwrap(),
                keep.as_ref().unwrap()
            ),
            "re-publishing the project keeps the copies (no respawn churn)"
        );
        e.set_track_poly(0, false, Vec::new());
        assert!(!e.poly_on(0) && poly_row(&e, 0) == 0, "mono clears the row");
    }

    /// v46 R2: exporting a POLY track must equal the per-lane renders
    /// summed (each lane through its own synth), while the monophonic
    /// export keeps the historic single-instance merged rendering.
    #[test]
    fn v46_poly_export_equals_sum_of_lane_renders() {
        let n = 22050usize;
        let e = Engine::new();
        e.set_midi_assets(v46_poly_patterns());
        e.set_track_on(0, true);
        let master = v46_piano_master();
        e.set_track_synth(0, Some(master.clone()));
        // Per-lane references (monophonic renders).
        e.set_project(
            Arc::new(vec![]),
            Arc::new(v46_poly_arrangement(Some(false))),
            n as u64,
        );
        let a = e.render_tracks(Some(0), n).expect("lane A renders");
        e.set_project(
            Arc::new(vec![]),
            Arc::new(v46_poly_arrangement(Some(true))),
            n as u64,
        );
        let b = e.render_tracks(Some(0), n).expect("lane B renders");
        // Both lanes, monophonic: one shared instance (historic).
        e.set_project(
            Arc::new(vec![]),
            Arc::new(v46_poly_arrangement(None)),
            n as u64,
        );
        let mono = e.render_tracks(Some(0), n).expect("mono renders");
        // POLY: two fresh copies render one lane each; the sum must match
        // A + B (the synth is linear, so lane renders add exactly).
        let master = v46_piano_master();
        e.set_track_synth(0, Some(master.clone()));
        let copies: Vec<Option<Arc<SynthHandle>>> =
            (0..2).map(|_| spawn_synth_copy(&master, 44100)).collect();
        e.set_track_poly(0, true, copies);
        let poly = e.render_tracks(Some(0), n).expect("poly renders");
        assert_eq!(poly.len(), mono.len());
        let mut max = 0.0f32;
        let mut drift = 0.0f32;
        for i in 0..n {
            max = max.max(mono[i].abs()).max(poly[i].abs());
            drift = drift.max((poly[i] - (a[i] + b[i])).abs());
        }
        assert!(
            drift < 1e-4 * (1.0 + max),
            "poly export drift {drift} (max {max})"
        );
        assert!(max > 0.01, "the renders must actually sound");
    }

    #[test]
    fn v38_audition_pending_counts_staged_notes() {
        // Sorted pattern encoding: 3 = note-on, 0 = note-off.
        let ev = [
            (0u64, 3u8, 0u8, 60u8, 100u8),
            (100, 0, 0, 60, 0),
            (200, 3, 0, 61, 90),
        ];
        assert_eq!(aud_pending(&ev, 0), 0, "nothing staged yet");
        assert_eq!(aud_pending(&ev, 50), 1, "first note ringing");
        assert_eq!(aud_pending(&ev, 150), 0, "its off arrived");
        assert_eq!(aud_pending(&ev, 250), 1, "second note held");
    }

    // ---- v48: export identity — one continuous chain pass per track -----
    //
    // Live bus rules the export must reproduce: the track chain is fed the
    // clip sum post-trim and runs CONTINUOUSLY — delay/reverb tails ring
    // across clip boundaries and past the last clip, chain state never
    // resets between clips, and routed sidechain sends reach the target's
    // chain (muted sources contribute no tap, mirroring the audible gate).

    /// Deterministic feedback comb (a delay/reverb stand-in): taps every
    /// `time` seconds, decaying by `fb`. Params: time (s), fb, wet mix.
    const V48_ECHO_SRC: &str = r#"include!("effect_api.rs");

struct Echo { sr: f64, buf: Vec<f32>, w: usize }

const P_TIME: usize = 0;
const P_FB: usize = 1;
const P_MIX: usize = 2;

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = sample_rate.max(1.0);
    let n = (sr * 0.125) as usize + 3;
    Box::into_raw(Box::new(Echo { sr, buf: vec![0.0f32; n], w: 0 })) as *mut core::ffi::c_void
}
extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() { drop(unsafe { Box::from_raw(state as *mut Echo) }); }
}
extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() { return input; }
    let st = unsafe { &mut *(state as *mut Echo) };
    let time = unsafe { *params.add(P_TIME) } as f64;
    let fb = unsafe { *params.add(P_FB) };
    let mix = unsafe { *params.add(P_MIX) };
    let n = st.buf.len();
    let d = ((time * st.sr).round() as usize).clamp(1, n - 1);
    let wet = st.buf[(st.w + n - d) % n];
    st.buf[st.w] = input + fb * wet;
    st.w = (st.w + 1) % n;
    input + mix * wet
}

static PARAMS: [EffectParamV1; 3] = [
    EffectParamV1 { label: c"time".as_ptr(), min: 0.001, max: 0.1, default: 0.015625 },
    EffectParamV1 { label: c"fb".as_ptr(), min: 0.0, max: 0.95, default: 0.5 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];
pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"V48 Comb".as_ptr(),
    author: core::ptr::null(),
    description: core::ptr::null(),
    n_params: 3,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};
#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 { &DESC }
"#;

    /// v2 sidechain duck: output x (1 - 0.9 * min(|sc|, 1)).
    const V48_SC_SRC: &str = r#"include!("effect_api.rs");

extern "C" fn create(_sr: f64) -> *mut core::ffi::c_void {
    Box::into_raw(Box::new(0usize)) as *mut core::ffi::c_void
}
extern "C" fn destroy(_s: *mut core::ffi::c_void) {
    if !_s.is_null() { drop(unsafe { Box::from_raw(_s as *mut usize) }); }
}
extern "C" fn process(_s: *mut core::ffi::c_void, input: f32, _p: *const f32) -> f32 { input }
extern "C" fn process_sc(_s: *mut core::ffi::c_void, input: f32, _p: *const f32, sc: f32) -> f32 {
    input * (1.0 - 0.9 * sc.abs().min(1.0))
}

static PARAMS: [EffectParamV1; 1] = [
    EffectParamV1 { label: c"none".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
];
pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"V48 Duck".as_ptr(),
    author: core::ptr::null(),
    description: core::ptr::null(),
    n_params: 1,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};
pub static DESC2: EffectInfoV2 = EffectInfoV2 {
    abi: SLOOPER_EFFECT_ABI_V2,
    name: c"V48 Duck".as_ptr(),
    author: core::ptr::null(),
    description: core::ptr::null(),
    n_params: 1,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
    process_sc,
};
#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 { &DESC }
#[no_mangle]
pub extern "C" fn slooper_effect_info_v2() -> *const EffectInfoV2 { &DESC2 }
"#;

    /// Load a test effect script into a private registry (temp dir).
    fn v48_reg(tag: &str, file: &str, src: &str) -> (crate::fx::Registry, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("slooper-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{file}.rs")), src).unwrap();
        let mut reg = crate::fx::Registry::open(dir.clone());
        reg.rescan();
        assert!(reg.desc(file).is_some(), "test effect {file} must load");
        (reg, dir)
    }

    fn v48_click(one: usize) -> Arc<[f32]> {
        let mut d = vec![0.0f32; one];
        d[0] = 1.0;
        Arc::from(d)
    }

    /// Tap assert: powers of two on both sides, 1e-6 only absorbs the pan
    /// downmix's last bit.
    fn v48_tap(out: &[f32], i: usize, want: f32) {
        assert!(
            (out[i] - want).abs() < 1e-6,
            "out[{i}] = {} want {want}",
            out[i]
        );
    }

    fn v48_comb(e: &Engine, reg: &crate::fx::Registry, t: usize) {
        let inst = reg
            .create_instance("v48echo", 48000.0, &[0.015625, 0.5, 1.0])
            .unwrap();
        let desc = reg.desc("v48echo").unwrap().clone();
        e.set_track_fx(
            t,
            TrackFx {
                slots: Arc::new(vec![FxSlot::scripted(inst, desc)]),
            },
        );
    }

    /// R1: an echo tail RINGS PAST the clip end. d = 750, fb 0.5, mix 1: a
    /// 1000-sample click clip in a 4000-project must place comb taps at
    /// 750, 1500, 2250, 3000, 3750 — everything past sample 1000 was hard
    /// silence in the historical per-clip phase-0 render.
    #[test]
    fn v48_export_tail_rings_past_clip_end() {
        if !crate::fx::rustc_available() {
            return;
        }
        let (reg, dir) = v48_reg("v48echo1", "v48echo", V48_ECHO_SRC);
        let e = Engine::new();
        e.set_sample_rate(48000);
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0));
        e.set_project(Arc::new(vec![v48_click(1000)]), Arc::new(arr), 4000);
        v48_comb(&e, &reg, 0);
        let out = e.render_project().expect("render");
        v48_tap(&out, 0, 1.0); // dry click
        v48_tap(&out, 750, 1.0); // tap 1 — inside the clip
        v48_tap(&out, 1500, 0.5); // past the source: the reported bug
        v48_tap(&out, 2250, 0.25);
        v48_tap(&out, 3000, 0.125);
        v48_tap(&out, 3750, 0.0625);
        assert!(out[1200].abs() < 1e-9, "gap between taps stays silent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R2: chain state SURVIVES the gap between clips. Click A at 0, click
    /// B at 1200 (both 1000 long): ONE continuous chain hears both — A's
    /// later taps ring inside and after B's window while B builds its own
    /// echo train. Fresh per-clip instances could not produce tap(1500) =
    /// 0.5 (inside B's span, from A).
    #[test]
    fn v48_export_chain_state_survives_gap() {
        if !crate::fx::rustc_available() {
            return;
        }
        let (reg, dir) = v48_reg("v48echo2", "v48echo", V48_ECHO_SRC);
        let e = Engine::new();
        e.set_sample_rate(48000);
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0));
        arr.insert(0, 0, Clip::new(1, 1200));
        e.set_project(
            Arc::new(vec![v48_click(1000), v48_click(1000)]),
            Arc::new(arr),
            4000,
        );
        v48_comb(&e, &reg, 0);
        let out = e.render_project().expect("render");
        v48_tap(&out, 1200, 1.0); // B's dry click
        v48_tap(&out, 1500, 0.5); // A's tap ringing INSIDE B's old window
        v48_tap(&out, 1950, 1.0); // B's tap 1 (1200 + 750)
        v48_tap(&out, 2250, 0.25); // A continues
        v48_tap(&out, 2700, 0.5); // B continues
        v48_tap(&out, 3000, 0.125);
        v48_tap(&out, 3450, 0.25);
        assert!(out[2400].abs() < 1e-9, "gap between taps stays silent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R3: a selection export runs the chain over the WHOLE window, so
    /// clip A's tail flows into clip B's part of the window; the window
    /// still ends at the selection end (taps beyond it are truncated).
    #[test]
    fn v48_export_selection_window_chain() {
        if !crate::fx::rustc_available() {
            return;
        }
        let (reg, dir) = v48_reg("v48echo3", "v48echo", V48_ECHO_SRC);
        let e = Engine::new();
        e.set_sample_rate(48000);
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0));
        arr.insert(0, 0, Clip::new(1, 1200));
        e.set_project(
            Arc::new(vec![v48_click(1000), v48_click(1000)]),
            Arc::new(arr),
            4000,
        );
        v48_comb(&e, &reg, 0);
        let (from, out) = e
            .render_selection(&[(0, 0, 0), (0, 0, 1)])
            .expect("selection render");
        assert_eq!(from, 0);
        assert_eq!(out.len(), 2200, "window = first start .. last end");
        v48_tap(&out, 1500, 0.5); // A's tail across the gap, inside window
        v48_tap(&out, 1950, 1.0); // B's tap 1
        assert!(out[2199].abs() < 1e-9, "window end is a cut, not a fade");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R4: a routed sidechain send reaches the target's chain in the
    /// export (v2 scripts duck under the source's post-trim dry taps), and
    /// muting the source removes the tap — the live audible gate mirrored.
    #[test]
    fn v48_export_sidechain_ducks_target() {
        if !crate::fx::rustc_available() {
            return;
        }
        let (reg, dir) = v48_reg("v48sc", "v48sc", V48_SC_SRC);
        let e = Engine::new();
        e.set_sample_rate(48000);
        let src: Arc<[f32]> = Arc::from(vec![1.0f32; 1500]);
        let mut arr = Arrangement::new();
        arr.insert(0, 0, Clip::new(0, 0)); // track 0: constant source
        arr.insert(1, 0, Clip::new(1, 500)); // track 1: click at 500
        e.set_project(Arc::new(vec![src, v48_click(1000)]), Arc::new(arr), 1500);
        let inst = reg.create_instance("v48sc", 48000.0, &[0.0]).unwrap();
        let desc = reg.desc("v48sc").unwrap().clone();
        e.set_track_fx(
            1,
            TrackFx {
                slots: Arc::new(vec![FxSlot::scripted(inst, desc)]),
            },
        );
        e.set_sc_send(1, 1 << 0);
        let out = e.render_project().expect("render");
        v48_tap(&out, 0, 1.0); // source is dry at unity
        assert!(
            (out[500] - 1.1).abs() < 1e-6,
            "target click must duck to 0.1 under the tap, got {}",
            out[500]
        );
        // Mute the source: no audible tap -> the duck disappears.
        e.set_track_on(0, false);
        let out2 = e.render_project().expect("render");
        assert!(out2[0].abs() < 1e-9, "muted source contributes nothing");
        v48_tap(&out2, 500, 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// WINDOWS-PORT W4 contract pins: the RT core (`Proc::process_block`) must
/// stay backend-neutral, the jack shim must stay ~pure plumbing, and the
/// seam (`crate::backend::BlockIo`) must stay wired. These are source-text
/// consistency pins (the W3 pattern): they run on every OS including
/// Windows CI, and they went red under a sabotage of each seam.
#[cfg(test)]
mod w4_tests {
    /// Extract one indented `fn` body: from `needle` to the first line that
    /// is exactly `    }` (the fn's closing brace — body lines are >= 8).
    fn extract_fn(src: &str, needle: &str) -> String {
        let start = src.find(needle).expect("fn not found");
        let rest = &src[start..];
        let end = rest
            .find("\n    }\n")
            .expect("closing brace not found — body layout changed?");
        rest[..end + "\n    }\n".len()].to_string()
    }

    #[test]
    fn w4_rt_core_is_backend_neutral() {
        // W5 split: the engine core lives in mod.rs; the jack shim in jack.rs.
        // CRLF-normalized: Windows CI checkouts may carry CRLF (git
        // core.autocrlf), which would hide the "\n    }\n" closing-brace
        // anchor. .gitattributes pins eol=lf; this is defense in depth.
        let src = include_str!("mod.rs").replace("\r\n", "\n");
        let body = extract_fn(&src, "pub fn process_block(");
        // Full-body sanity: the playhead/peak publish must be inside.
        assert!(body.contains("self.peak.store(peak_val"), "body truncated");
        // No JACK-specific identifier may appear in the engine core again.
        for banned in [
            "j::",
            "jack_port",
            "ProcessScope",
            "self.ports",
            "self.midi_in",
            "Control::",
            "n_frames()",
        ] {
            assert!(
                !body.contains(banned),
                "process_block must be backend-neutral, found `{banned}`"
            );
        }
    }

    #[test]
    fn w4_backend_contract_wired() {
        let be = include_str!("../backend.rs");
        for shape in [
            "pub trait BlockIo",
            "fn in_buf(&self, slot: usize) -> *const f32;",
            "fn out_buf(&self, slot: usize) -> *mut f32;",
        ] {
            assert!(be.contains(shape), "backend.rs lost `{shape}`");
        }
        assert!(
            include_str!("../lib.rs").contains("pub mod backend;"),
            "backend module not declared"
        );
        let src = include_str!("jack.rs");
        assert!(
            src.contains("impl crate::backend::BlockIo for JackIo"),
            "jack BlockIo impl missing"
        );
        // (needle split so this pin's own source does not count as a hit)
        let needle = concat!("self.proc.process_", "block(n, &mut evs, &io);");
        assert_eq!(
            src.matches(needle).count(),
            1,
            "exactly one process_block call site (the shim)"
        );
    }

    #[test]
    fn w4_jack_shim_is_pure_plumbing() {
        let src = include_str!("jack.rs");
        let start = src
            .find("impl ProcessHandler for JackProc {")
            .expect("shim missing");
        let end = src[start..].find("struct JackIo").expect("JackIo missing");
        let shim = &src[start..start + end];
        // The shim owns mix/transport state: none of its vocabulary.
        for banned in ["clip", "transport", "peak", "meter", "engine.", "gain"] {
            assert!(
                !shim.contains(banned),
                "jack shim must be pure plumbing, found `{banned}`"
            );
        }
        // The event scratch round-trip (take -> fill -> hand off -> store).
        for shape in [
            "std::mem::take(&mut self.midi_ev_scratch)",
            "evs.clear();",
            "self.midi_ev_scratch = evs;",
        ] {
            assert!(shim.contains(shape), "shim lost `{shape}`");
        }
    }
}

/// W5 structural pins (WINDOWS-PORT): the split must STAY split. Source
/// scans like the w4 family — they assert the seam shapes that let the two
/// backends compile independently, and each fails if code migrates back
/// into the shared module or across the cfg boundary.
#[cfg(test)]
mod w5_tests {
    fn cargo_toml() -> &'static str {
        include_str!("../../Cargo.toml")
    }

    /// Source for scanning: line comments stripped (prose may legitimately
    /// name the symbols the pins ban) and the test tail dropped (the pins
    /// themselves contain the literals — counting them would be self-referential).
    fn code(src: &str) -> String {
        let cut = src.find("\n#[cfg(test)]").unwrap_or(src.len());
        src[..cut]
            .lines()
            .map(|l| l.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn w5_backend_deps_are_cfg_gated() {
        let t = cargo_toml();
        let lin = t
            .split("[target.'cfg(windows)'.dependencies]")
            .next()
            .unwrap();
        assert!(
            lin.contains("[target.'cfg(not(windows))'.dependencies]") && lin.contains("jack = "),
            "jack must live under the cfg(not(windows)) target table only"
        );
        assert!(
            !lin.contains("windows = {"),
            "windows crate leaked into the non-windows table"
        );
        let win = t
            .split_once("[target.'cfg(windows)'.dependencies]")
            .unwrap()
            .1;
        assert!(
            win.contains("windows = {"),
            "windows crate missing from cfg(windows) table"
        );
        assert!(win
            .split("[build-dependencies]")
            .next()
            .unwrap()
            .contains("Win32_Media_Audio"));
        // shared [dependencies] must not re-add either crate
        let shared = t.split_once("[target.").unwrap_or((t, "")).0;
        assert!(
            !shared.contains("\njack = ") && !shared.contains("\nwindows = "),
            "backend dep left the target table"
        );
    }

    #[test]
    fn w5_shared_module_has_no_backend_vocabulary() {
        let src = code(include_str!("mod.rs"));
        // `jack::run` appears ONLY inside the cfg(not(windows)) run_backend.
        assert_eq!(
            src.matches("jack::run").count(),
            1,
            "exactly one jack:: entry call (run_backend)"
        );
        assert_eq!(src.matches("wasapi::run").count(), 1);
        for banned in ["use jack::", "jack_sys", "ProcessScope", "j::"] {
            assert!(
                !src.contains(banned),
                "backend vocabulary `{banned}` migrated into mod.rs"
            );
        }
        // and the OS modules are cfg-declared
        assert!(
            src.contains("#[cfg(not(windows))]\nmod jack;"),
            "jack mod not cfg-gated"
        );
        assert!(
            src.contains("#[cfg(windows)]\nmod wasapi;"),
            "wasapi mod not cfg-gated"
        );
    }

    #[test]
    fn w5_manager_loop_is_backend_neutral() {
        let src = code(include_str!("mod.rs"));
        let start = src.find("fn manager_loop(").expect("manager_loop missing");
        let end = src[start..]
            .find("fn clamp_loop_region(")
            .expect("manager_loop region end missing");
        let body = &src[start..start + end];
        for banned in [
            "send_ports",
            "apply_layout",
            "rescan_midi",
            "active.",
            "j::",
            "jack",
        ] {
            assert!(
                !body.contains(banned),
                "manager_loop must be backend-neutral, found `{banned}`"
            );
        }
        for shape in [
            "session.refresh(evt_tx)",
            "session.apply(&ins, &outs, evt_tx)",
            "session.idle(evt_tx)",
        ] {
            assert!(
                body.contains(shape),
                "manager_loop lost seam call `{shape}`"
            );
        }
    }

    // --- W9: the Windows MIDI seam (docs/WINDOWS-PORT.md) ------------------
    use super::{Evt, ManagerBackend};
    use std::sync::mpsc::SyncSender;

    #[test]
    fn w9_manager_routes_midi_cmd_to_the_session_hook() {
        let src = code(include_str!("mod.rs"));
        // Assert the CALL (unique-call-site marker, W8b pin lore).
        assert!(
            src.contains("Ok(Cmd::SetMidiInputs(sel)) => session.set_midi_inputs(&sel, evt_tx),"),
            "manager_loop does not route SetMidiInputs to the backend hook"
        );
        // The trait default is an empty no-op...
        assert!(
            src.contains("fn set_midi_inputs(&mut self, _sel: &[String], _tx: &SyncSender<Evt>) {}"),
            "the ManagerBackend default hook changed shape"
        );
        // ...and JACK does NOT override it: Linux keeps its auto-connect
        // contract structurally, not just behaviorally.
        let jack = include_str!("jack.rs");
        assert!(
            !jack.contains("fn set_midi_inputs"),
            "JackSession must not override the no-op MIDI hook (Linux = JACK MIDI only)"
        );
        assert!(
            jack.contains("fn rescan_midi_sources"),
            "Linux auto-connect machinery vanished"
        );
        assert!(!jack.contains("midiin"), "the W9 bus must not touch the JACK path");
    }

    /// Compile-time proof of the trait default: a session that does not
    /// implement `set_midi_inputs` must still be constructible — this test
    /// only fails to COMPILE if the default ever disappears.
    struct DefaultOnly;
    impl ManagerBackend for DefaultOnly {
        fn refresh(&mut self, _tx: &SyncSender<Evt>) {}
        fn apply(&mut self, _ins: &[String], _outs: &[(String, u8)], _tx: &SyncSender<Evt>) {}
        fn idle(&mut self, _tx: &SyncSender<Evt>) {}
        fn teardown(&mut self) {}
    }

    #[test]
    fn w9_midi_hook_default_compiles_without_override() {
        let mut s = DefaultOnly;
        let (tx, _rx) = std::sync::mpsc::sync_channel::<Evt>(1);
        s.set_midi_inputs(&["anything".into()], &tx); // no-op on every OS
    }

    #[test]
    fn w9_wasapi_rt_feeds_midi_before_the_handoff() {
        let wasapi = include_str!("wasapi.rs");
        let feed = wasapi
            .find("self.midi.drain_into(evs, midi_ts);")
            .expect("RT loop must drain the WinMM bus");
        let hand = wasapi
            .find("self.proc.process_block(n, evs, &io);")
            .expect("RT loop must hand off to the engine");
        assert!(feed < hand, "the bus must be drained BEFORE process_block");
        // Shared bus, not owned: RtCfg gets an Arc clone of the session's.
        assert!(wasapi.contains("midi: self.midi_q.clone(),"));
        assert!(wasapi.contains("midi_q: MidiInQueue::new(),"));
        // Windows overrides the hook; teardown closes devices before COM.
        assert!(wasapi.contains("fn set_midi_inputs(&mut self, sel: &[String], tx: &SyncSender<Evt>) {"));
        let td = wasapi
            .find("fn teardown(&mut self) {")
            .expect("teardown missing");
        assert!(
            wasapi[td..].contains("self.midi_conns.clear()"),
            "WinMM devices must be closed on teardown"
        );
    }

    /// W12b: the stamps must reach the queue AND be converted before the
    /// engine hand-off (needles are the exact call literals — unique call
    /// sites, sabotage-verifiable).
    #[test]
    fn w12b_wasapi_spreads_stamps_into_the_block() {
        let wasapi = include_str!("wasapi.rs");
        // The midir callback forwards its µs timestamp (no more `_ts`).
        assert!(
            wasapi.contains("|ts, bytes, p| p.on_bytes(bytes, ts)"),
            "the WinMM timestamp must ride into the queue"
        );
        // Spread runs on the drained batch AFTER the drain and BEFORE the
        // hand-off, with the STREAM's clock.
        let feed = wasapi
            .find("self.midi.drain_into(evs, midi_ts);")
            .expect("drain call literal moved");
        let spread = wasapi
            .find("midiin::spread(evs, midi_ts, self.sr, n as u32);")
            .expect("RT loop must convert stamps to frame offsets");
        let hand = wasapi
            .find("self.proc.process_block(n, evs, &io);")
            .expect("RT loop must hand off to the engine");
        assert!(feed < spread && spread < hand, "spread sits between drain and hand-off");
        // The scratch pair is cleared together — positional pairing by construction.
        assert!(wasapi.contains("evs.clear();\n        midi_ts.clear();"));
        // RtCfg gets the stream clock at construction (unique: `Mix.sr` is
        // passed positionally to Proc, only the RtCfg literal carries the
        // `sr:` label).
        assert!(wasapi.contains("sr: rstream.mix.sr,"));
    }

    #[test]
    fn w5_each_backend_builds_proc_once_and_shims_once() {
        // (backend files need no filter: their literals ARE the pinned shapes)
        let jack = include_str!("jack.rs");
        let wasapi = include_str!("wasapi.rs");
        assert_eq!(
            jack.matches("proc: new_proc(").count(),
            1,
            "jack builds new_proc once"
        );
        assert_eq!(
            wasapi.matches("proc: new_proc(").count(),
            1,
            "wasapi builds new_proc once"
        );
        // one process_block call site per shim (w4 pins guard the jack one)
        assert_eq!(
            wasapi
                .matches("self.proc.process_block(n, evs, &io);")
                .count(),
            1,
            "wasapi shim must hand off exactly once"
        );
        // each backend implements BlockIo
        assert!(jack.contains("impl crate::backend::BlockIo for JackIo"));
        assert!(wasapi.contains("impl crate::backend::BlockIo for WasapiIo"));
        // engine-side init exists ONLY in the shared builder
        let src = code(include_str!("mod.rs"));
        assert_eq!(
            src.matches("mix_l: Vec::new(),").count(),
            1,
            "Proc init duplicated"
        );
    }

    #[test]
    fn w5_e2e_suite_is_cfg_unix() {
        let e2e = include_str!("../../tests/e2e.rs");
        assert!(
            e2e.contains("#![cfg(not(windows))]"),
            "the live-JACK e2e suite must be cfg'd out on Windows"
        );
    }
}

/// W12a pins (WINDOWS-PORT): RT scheduling + device-loss auto-recovery in
/// the Windows backend. The pure decision table is pinned in `rt_recovery`
/// itself; these are source-wiring scans (the w5 pattern — every OS runs
/// them, red under sabotage of each seam).
#[cfg(test)]
mod w12a_tests {
    fn wasapi() -> String {
        include_str!("wasapi.rs").replace("\r\n", "\n")
    }

    #[test]
    fn w12a_rt_thread_boost_is_wired_once() {
        let src = wasapi();
        // Unique call-site marker (W8b pin lore): exactly one boost, at the
        // RT entry; both scheduling classes and the revert must be present.
        assert_eq!(src.matches("let _boost = boost_rt_thread();").count(), 1);
        for shape in [
            "AvSetMmThreadCharacteristicsW(w!(\"Pro Audio\")",
            "SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)",
            "AvRevertMmThreadCharacteristics(*h)",
        ] {
            assert!(src.contains(shape), "boost machinery lost `{shape}`");
        }
    }

    #[test]
    fn w12a_wait_failed_guard_precedes_index_math() {
        // The pre-W12a bug: WAIT_FAILED - WAIT_OBJECT_0(0) is never None,
        // so a dead-handle wait busy-spun the RT loop and stored no fault.
        let src = wasapi();
        let wait = src
            .find("WaitForMultipleObjects(&handles")
            .expect("wait loop missing");
        let guard = src
            .find("if got == WAIT_FAILED")
            .expect("W12a: WAIT_FAILED guard missing");
        let math = src
            .find("let k = got.0 - WAIT_OBJECT_0.0;")
            .expect("index math missing");
        assert!(
            wait < guard && guard < math,
            "the dead-handle wait must be checked BEFORE the index arithmetic"
        );
        // the guard stores a fault and breaks — never falls through
        assert!(src[guard..guard + 400].contains("self.fault.store(rt_recovery::FAULT_DEVICE"));
    }

    #[test]
    fn w12a_fault_is_classified_at_every_error_exit() {
        let src = wasapi();
        // render + capture arms each store the CLASSIFIED code...
        assert_eq!(
            src.matches("self.fault.store(f.code, Ordering::SeqCst);").count(),
            2
        );
        // ...from the one-HRESULT classifier (exact call-site shape: a bare
        // substring check sabotage-PASSES vacuously — proven this session).
        assert!(src.contains("let code = if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {"));
        // The RT config receives the shared fault slot (Arc clone, w9-style).
        assert_eq!(src.matches("fault: self.fault.clone(),").count(), 1);
    }

    #[test]
    fn w12a_idle_asks_the_recovery_table() {
        let src = wasapi();
        let start = src
            .find("fn idle(&mut self, tx: &SyncSender<Evt>) {")
            .expect("idle lost its recovery-aware signature");
        let end = start + src[start..].find("fn set_midi_inputs").expect("idle region end");
        let body = &src[start..end];
        for shape in [
            "self.fault.swap(rt_recovery::FAULT_NONE, Ordering::SeqCst)",
            "rt_recovery::plan(fault, self.last_patch.as_ref(), present, self.retry_budget)",
            "self.retry_budget = rt_recovery::RETRY_BUDGET",
            "self.retry_budget = self.retry_budget.saturating_sub(1)",
            "self.apply(&patch.0, &patch.1, tx)",
        ] {
            assert!(body.contains(shape), "idle recovery lost `{shape}`");
        }
    }

    #[test]
    fn w12a_apply_records_the_patch_silence_forgets_it() {
        let src = wasapi();
        assert_eq!(
            src.matches("self.last_patch = Some((ins.to_vec(), outs.to_vec()));")
                .count(),
            1
        );
        assert_eq!(src.matches("self.rt_spawn = Some(std::time::Instant::now());").count(), 1);
        // The clear lives in the ok_out == 0 branch (before the render
        // endpoint is resolved), not somewhere the recovery could hit.
        let silence = src[src.find("fn apply(&mut self").expect("apply missing")..]
            .split_once("let rid = out_map")
            .expect("silent path region")
            .0
            .to_string();
        assert!(silence.contains("self.last_patch = None;"));
        // the module itself is declared (needle split: this pin's text must
        // not self-match)
        assert!(include_str!("mod.rs").contains(concat!("mod rt_rec", "overy;")));
    }
}
