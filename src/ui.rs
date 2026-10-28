//! egui front-end for slooper: a small DAW — sixteen tracks with child
//! lanes, a global draggable playhead, a global loop region, an asset rack,
//! clip copy/paste, clip-based arrangement, metronome-anchored quantize and
//! `.slooper` projects.
//!
//! Layout:
//! - top bar: hamburger (settings), status, transport, position readout,
//!   tempo + metronome, Save / Save as / Load / Export
//! - mixer dock header (above Track 1, level with the grid header): peak
//!   meter + master volume row (dB scale, −∞…+6)
//! - left panel: ASSET RACK — folder list (primary = recordings land there;
//!   defaults to the seeded [`crate::samples808`] pack), one row per audio
//!   file, project assets below
//! - central: grid toolbar (loop toggle + mode, auto-quantize,
//!   quantize/snap subdivisions, zoom — also Ctrl+wheel; Shift+wheel pans
//!   horizontally) above the timeline: a ruler pinned above the track
//!   scroll (scrub + draggable loop handles) and one seamless block per
//!   track (flush lane strips, hairline separators, one continuous playhead
//!   line). Track geometry is shared with the mixer via [`App::track_rows`].
//! - right panel: MIXER dock — one strip per track, vertically aligned with
//!   its timeline block: name/REC/play-gate/child buttons, vol/gain/pan
//!   sliders with dB text boxes, FX button; scrolls in lockstep
//! - the settings panel docks to the LEFT of the asset rack when open
//! - bottom: MIDI monitor
//! - windows: settings, effects, effect editor, knob learner, export,
//!   save-project prompt, load-project browser
//!
//! Clipboard: one clip lives in `clip_clipboard`; paste targets the track
//! nearest the last grid click (`paste_track`), at the playhead.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::Frame;
use egui::{Color32, RichText, Sense, Slider};

use crate::arrange::{Arrangement, Clip};
use crate::assets::{Asset, RackFolder};
use crate::audio::{
    Backend, Cmd, Evt, FxSlot, OutSel, PortInfo, SynthHandle, TrackFx, GAIN_UNITY_MIDI, MAX_LOOPS,
    TR_REC,
};
use crate::config::{ChainSave, Config, MidiAxisCal, MidiBinding, TrackFxSave};
use crate::fx;
use crate::midi::{hex, note_name, scale_axis, MidiAction, MidiBind, MidiMsg};
use crate::midipat::{Pattern, PPQ};
use crate::project::{self, ProjAsset, ProjClip, ProjMetro, ProjQuantize, ProjTrack, Project};
use crate::quantize::{self, snap_start};
use crate::synth;

/// Keys a user may plausibly bind to a shortcut.
const KEY_CANDIDATES: [egui::Key; 51] = [
    egui::Key::Num0,
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
    egui::Key::Space,
    egui::Key::Enter,
    egui::Key::Tab,
    // Delete is deliberately NOT a candidate: it is reserved for clearing
    // bindings (it unbinds the cell being edited in both columns) and can
    // never be bound to an action. Configs persisted by builds where it was
    // assignable drop the binding to unbound on load.
    egui::Key::A,
    egui::Key::B,
    egui::Key::C,
    egui::Key::D,
    egui::Key::E,
    egui::Key::F,
    egui::Key::G,
    egui::Key::H,
    egui::Key::I,
    egui::Key::J,
    egui::Key::K,
    egui::Key::L,
    egui::Key::M,
    egui::Key::N,
    egui::Key::O,
    egui::Key::P,
    egui::Key::Q,
    egui::Key::R,
    egui::Key::S,
    egui::Key::T,
    egui::Key::U,
    egui::Key::V,
    egui::Key::W,
    egui::Key::X,
    egui::Key::Y,
    egui::Key::Z,
    egui::Key::F1,
    egui::Key::F2,
    egui::Key::F3,
    egui::Key::F4,
    egui::Key::F5,
    egui::Key::F6,
    egui::Key::F7,
    egui::Key::F8,
    egui::Key::F9,
    egui::Key::F10,
    egui::Key::F11,
    egui::Key::F12,
];

/// One row in the MIDI monitor log.
#[derive(Clone)]
struct MidiLine {
    ts: String,
    type_name: &'static str,
    describe: String,
    raw_hex: String,
}

fn key_debug(k: egui::Key) -> String {
    format!("{k:?}")
}

/// Short display form of a key for the shortcut rows.
fn key_display(k: egui::Key) -> String {
    match k {
        egui::Key::Num0 => "0".into(),
        egui::Key::Num1 => "1".into(),
        egui::Key::Num2 => "2".into(),
        egui::Key::Num3 => "3".into(),
        egui::Key::Num4 => "4".into(),
        egui::Key::Num5 => "5".into(),
        egui::Key::Num6 => "6".into(),
        egui::Key::Num7 => "7".into(),
        egui::Key::Num8 => "8".into(),
        egui::Key::Num9 => "9".into(),
        egui::Key::Space => "space".into(),
        other => format!("{other:?}"),
    }
}

/// Parse a persisted key name back into an `egui::Key` (None if unknown).
fn key_from_name(s: &str) -> Option<egui::Key> {
    KEY_CANDIDATES
        .iter()
        .find(|k| format!("{k:?}") == s)
        .copied()
}

// --- per-loop level controls ---------------------------------------------
// MIDI knobs/sliders send 0..=127. Volume runs -inf dB (silence) at 0 up to
// **+6 dB** at 127; the sliders show the resulting dB.

/// MIDI value of 0 dB on the volume sliders (-inf..=+6 dB range).
pub const VOL_UNITY_MIDI: u8 = 115;

/// Volume mapping: 0 = silence (-inf), 1 = -60 dB, [`VOL_UNITY_MIDI`] =
/// exactly 0 dB, 127 = +6 dB — linear in dB on each leg so the slider feels
/// even, with the 0 dB point sitting where the track/master defaults are.
pub fn vol_amp(v: u8) -> f32 {
    let db = if v <= VOL_UNITY_MIDI {
        (v as f32 - 1.0) * (60.0 / (VOL_UNITY_MIDI as f32 - 1.0)) - 60.0
    } else {
        (v as f32 - VOL_UNITY_MIDI as f32) * (6.0 / (127.0 - VOL_UNITY_MIDI as f32))
    };
    if v == 0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// dB -> volume MIDI value for the -inf..=+6 dB range (inverse of
/// [`vol_amp`]; finite values floor at 1, "-inf" is 0).
pub fn vol_midi(db: f32) -> u8 {
    if !db.is_finite() || db <= -60.0 {
        return if db == f32::NEG_INFINITY { 0 } else { 1 };
    }
    if db >= 6.0 {
        return 127;
    }
    if db <= 0.0 {
        (1.0 + (db + 60.0) * ((VOL_UNITY_MIDI - 1) as f32 / 60.0))
            .round()
            .clamp(1.0, VOL_UNITY_MIDI as f32) as u8
    } else {
        (VOL_UNITY_MIDI as f32 + db * ((127 - VOL_UNITY_MIDI) as f32 / 6.0))
            .round()
            .clamp(VOL_UNITY_MIDI as f32, 127.0) as u8
    }
}

/// Gain-trim mapping: 0 = silence, 80 = 0 dB, 127 = +10 dB
/// (~0.215 dB per step; below 80 it attenuates toward -inf at 1).
pub fn gain_amp(v: u8) -> f32 {
    if v == 0 {
        0.0
    } else {
        let db = (v as i32 - GAIN_UNITY_MIDI as i32) as f32 * 0.215;
        10f32.powf(db / 20.0)
    }
}

/// Pan MIDI value (0..=127) -> the -100..=100 position shown in the text
/// box: -100 = hard left, 0 = centre, +100 = hard right. (The old scale was
/// 0..=100 with 50 centre; this is that position doubled and shifted, so
/// every old position maps 1:1 — 0 -> -100, 50 -> 0, 100 -> 100.)
pub fn pan_text(m: u8) -> i32 {
    (pan_text_old(m) as i32) * 2 - 100
}

/// The legacy 0..=100 position (50 = centre), kept as the intermediate so
/// centre lands exactly on 0 for the MIDI centre value.
fn pan_text_old(m: u8) -> u32 {
    (m as f32 * 100.0 / 127.0).round() as u32
}

/// Engine pan position [0, 1] for a MIDI value (hard right at 127).
pub fn pan01(m: u8) -> f32 {
    m as f32 / 127.0
}

/// L/R routing byte of output `i`, defaulting to the mono downmix.
fn cfg_lr(v: &[u8], i: usize) -> u8 {
    *v.get(i).unwrap_or(&3)
}

/// Live MIDI-pattern recording state for one track. Events land in `evs`
/// tagged with the ABSOLUTE engine position they arrived at; disarming
/// rebases them into a pattern (kind: 0=note-off 1=CC 2=pressure 3=note-on).
#[derive(Clone, Default)]
struct MidiRecState {
    armed: bool,
    start: u64,
    evs: Vec<(u64, u8, u8, u8, u8)>,
}

/// v0.11 R5: per-track write-automation recorder (FL's "write" mode).
/// Arming one starts the transport; from then on, every move of a control
/// is captured as curve nodes — the FIRST control moved owns the take, so
/// one asset always automates exactly one target.
#[derive(Clone, Default)]
struct AutoRecState {
    armed: bool,
    target: Option<String>,
    start: u64,
    nodes: Vec<crate::automation::Node>,
}

/// v0.11 R5: the automation manager window's editing state.
struct AutoEdit {
    /// Existing asset being edited in place (None = unsaved new curve).
    asset: Option<usize>,
    name: String,
    target: String,
    len_samples: i64,
    nodes: Vec<crate::automation::Node>,
    sel: usize,
    /// Node index being dragged on the canvas (gesture armed on press).
    drag: Option<usize>,
    changed: bool,
    /// v0.15: MIDI manager recording — raw moves of the picked bindings
    /// draw the curve (as % of the target's range) instead of acting.
    rec: bool,
    /// Engine position when REC was armed (nodes stamp relative to it).
    rec_start: i64,
}

/// Floating piano-roll editor (the top-bar "Piano roll" window). Notes are
/// drawn on the metronome grid exactly like the timeline: drag on empty
/// space draws a note, drag on a note moves its whole selection, drag a
/// note's edges resizes it (loop-area-handle style), right-click deletes,
/// Ctrl/Shift+drag on empty space rubber-band-selects. The window owns the
/// full note-keyboard range through two scrollbars and closes with its X.
struct PianoRoll {
    pat: Pattern,
    /// Horizontal zoom (px per beat) and row height (px per semitone).
    px_per_beat: f32,
    row_h: f32,
    /// Pixels between the bottom of the canvas and the bottom of note 0 —
    /// the vertical scroll position (driven by the vertical scrollbar and
    /// the wheel). The lowest visible piano key is derived from it every
    /// frame.
    vscroll_px: f32,
    /// Horizontal scroll in seconds (driven by the horizontal scrollbar).
    scroll_sec: f64,
    changed: bool,
    /// In-flight canvas gesture (move / resize / marquee / fresh draw).
    gesture: RollGesture,
    /// Canvas rect painted last frame (screen space) — the wheel router
    /// claims ctrl/alt-wheel gestures only while the pointer is over it.
    canvas_rect: Option<egui::Rect>,
    /// Velocity the next drawn note gets (FL carries a velocity knob in
    /// the roll toolbar instead of hard-coding one).
    new_vel: u8,
    /// Selected note indices into `pat.notes`. The vec is only ever
    /// REBUILT on selection gestures — `pat.sort()` runs at save only, so
    /// indices stay valid while the window is open.
    sel_notes: Vec<usize>,
    /// Shift-range anchor note.
    sel_anchor: Option<usize>,
    /// Snapshot of the selected notes when a move-drag grabs: the group
    /// moves rigidly from these bases regardless of what the selection
    /// did mid-drag.
    drag_notes: Vec<(usize, crate::midipat::Note)>,
    /// True while the roll owns the keyboard: the roll then handles
    /// Ctrl+C/V/A and Delete, and NO main-GUI shortcut fires (space must
    /// not start the transport behind the roll).
    focused: bool,
    /// Full window frame (INCLUDING the title bar) from the last painted
    /// frame. `WindowResponse.response.rect` excludes the title bar, so
    /// anything that locates the title bar (tests, close logic) needs this.
    win_rect: Option<egui::Rect>,
    /// When set, the roll is EDITING this asset (opened from a selected
    /// MIDI clip or rack asset): **save** overwrites the asset in place
    /// instead of minting a new one.
    edit_asset: Option<u32>,
}

/// One in-flight piano-roll canvas gesture.
#[derive(Clone, Copy)]
enum RollGesture {
    None,
    /// Drag note bodies. `fresh` = the press drew a brand-new note (its
    /// tail follows the pointer until release). `off` = seconds between
    /// the grabbed point and the note start; `base` = that note's start
    /// tick at grab time (the group moves by the pointer's snapped delta).
    Move {
        idx: usize,
        off: f64,
        base: u32,
        fresh: bool,
    },
    /// Drag one note's edge: `edge` 0 = head (moves start, end pinned),
    /// 1 = tail (end follows the pointer). `t0`/`end0` = note boundaries
    /// in seconds at grab time.
    Resize {
        idx: usize,
        edge: u8,
        t0: f64,
        end0: f64,
    },
    /// Rubber-band box from `from` to the pointer. `add` keeps the
    /// previous selection (Ctrl/Shift held at press).
    Marquee {
        from: egui::Pos2,
        add: bool,
    },
}

/// In-flight three-step calibration for one continuous (knob) MIDI binding.
///
/// Bind-wizard state for one continuous (knob) binding slot.
/// Step 1 captures the AXIS — message TYPE + CHANNEL, never the data byte.
/// While the axis is unlocked every message re-picks it, so moving a
/// different control changes the knob being bound; **Lock axis** freezes it.
/// Step 2 then listens on that axis for ANY 0..=127 data value until
/// **Lock min**, and step 3 listens again until **Finish**. `lo`/`hi` track
/// the latest value seen on each end so a noisy first touch is not fatal.
/// `lo > hi` (a control wired backwards) is stored as captured —
/// `scale_axis` flips it so the translation stays continuous and monotonic.
struct KnobLearn {
    slot: usize,
    /// Axis (type + channel); `d1` is the last full id seen, shown for
    /// orientation ("which physical knob am I looking at?") only.
    bind: MidiBind,
    has_axis: bool,
    axis_locked: bool,
    /// Data byte of the most recent message (value/velocity/program), used
    /// to seed `lo` the moment the axis locks.
    last_val: Option<u8>,
    lo: u8,
    hi: u8,
    /// min locked (step 2 done) — collecting the maximum.
    locked: bool,
}

/// Number of bindable actions — one keyboard slot per entry of
/// [`MidiAction::ALL`].
pub const N_ACTIONS: usize = MidiAction::ALL.len();

/// Default keyboard bindings, one per entry of `MidiAction::ALL` (same
/// order); `None` = no default. Layout matches `MidiAction::ALL`: selected
/// actions (0–3), select 1–16 (4–19), per-track rec/clear/play × 16
/// (20–67, MIDI-bound), metronome + selection + transport (68–72), then the
/// value-driven knob ranges (never key-bound).
const SHORTCUT_DEFAULTS: [Option<egui::Key>; N_ACTIONS] = {
    let mut a = [None; N_ACTIONS];
    a[0] = Some(egui::Key::R); // record (selected)
    a[1] = Some(egui::Key::C); // clear (selected)
    a[3] = Some(egui::Key::E); // export prompt
    a[4] = Some(egui::Key::Num1);
    a[5] = Some(egui::Key::Num2);
    a[6] = Some(egui::Key::Num3);
    a[7] = Some(egui::Key::Num4);
    a[68] = Some(egui::Key::M); // metronome on / off
    a[71] = Some(egui::Key::Space); // play / stop all (transport toggle)
    a
};

/// Grid subdivisions, shared by the metronome click, the quantize grid and
/// the clip snap grid: (steps per beat, display name).
const SUBDIVISIONS: [(u32, &str); 6] = [
    (1, "quarter note"),
    (2, "eighth note"),
    (3, "triplet"),
    (4, "sixteenth note"),
    (6, "triplet eighths"),
    (8, "thirty-second note"),
];

/// Min/max columns for one asset's waveform thumbnail (cached per asset
/// index + column count; `peak` normalizes quiet takes to full height).
struct AssetWave {
    cols: usize,
    data: Vec<[f32; 2]>,
}

/// A clip being dragged on the timeline. Dragging a clip that is part of a
/// multi-selection drags the whole group: `items` lists every member
/// (`(track, lane, clip index, timeline position at drag start)`, sorted,
/// the earliest-position one first) and `base0` is the group's start
/// position — the point every member moves relative to.
#[derive(Clone)]
struct ClipDrag {
    items: Vec<(usize, usize, usize, u64)>,
    base0: u64,
    /// Samples between the group's start and the grab point, so the group
    /// does not jump when the drag begins.
    grab: u64,
    /// Some(lane) for a SINGLE-clip drag: the pointer may then also move
    /// the clip vertically between the track's lanes. Multi-clip groups
    /// (any group, including several clips on one track) move horizontally
    /// only.
    lane0: Option<usize>,
}

/// One clip in the copy buffer: its offset from the copied group's
/// topmost track / left edge, plus the asset and trim state — so a whole
/// multi-track selection pastes as a group at the playhead.
#[derive(Clone, Copy)]
struct ClipboardItem {
    t_off: usize,
    asset: u32,
    cut: u64,
    play_len: Option<u64>,
    pos_off: u64,
    gain: f32,
    fade_in: u64,
    fade_out: u64,
}

/// The clipboard: one copied group (pasted at the playhead on the target
/// track; place_auto picks free lanes so overlaps never clobber).
#[derive(Clone)]
struct Clipboard {
    items: Vec<ClipboardItem>,
}

/// An in-progress clip edge drag (trim/extend). `edge` 0 = left (moves the
/// start, adjusting the source `cut`), 1 = right (moves the end, adjusting
/// `play_len`). The asset itself is never touched.
#[derive(Clone, Copy)]
struct ClipResize {
    t: usize,
    lane: usize,
    c: usize,
    edge: u8,
    /// Clip state at drag start.
    pos0: u64,
    cut0: u64,
    end0: u64,
}

const COL_PLAYHEAD: Color32 = Color32::from_rgb(255, 95, 86);
const COL_CLIP: Color32 = Color32::from_rgb(72, 118, 196);
const COL_CLIP_SEL: Color32 = Color32::from_rgb(128, 200, 255);
const COL_GHOST: Color32 = Color32::from_rgb(120, 160, 120);
const COL_BAR_LINE: Color32 = Color32::from_gray(135);
const COL_BEAT_LINE: Color32 = Color32::from_gray(78);
/// Sub-beat snap lines: a third, dimmer tier drawn at the snap
/// subdivision (grid-toolbar snap combo), so clip placement has a visible
/// target without cluttering the bar/beat grid. MUST stay the darkest tier
/// (brightness order bar > beat > snap — pinned by
/// `grid_line_brightness_is_bar_beat_snap`). Deliberately an opaque dark
/// gray: an alpha-blended stroke painted *opaque gray(120)* in the live
/// glow build and out-shone the beat lines (the user's "swap downbeat and
/// subdivision" report).
const COL_SNAP_LINE: Color32 = Color32::from_gray(44);
/// Nominal lane-strip height in the grid. A track's block is
/// `lanes × LANE_H`, never shorter than `TRACK_MIN_H` (the height its
/// mixer strip needs), so clips get tall waveforms and the mixer stays
/// aligned strip-to-track.
const LANE_H: f32 = 48.0;
const TRACK_MIN_H: f32 = 96.0;
/// Smallest height a lane may be squeezed to when the user resizes a track.
const LANE_MIN_H: f32 = 36.0;
/// Separation bar painted between adjacent tracks (the only gap left).
const TRACK_SEP: f32 = 3.0;
/// Hit height around the separation bar — it doubles as the resize handle.
const TRACK_SEP_HIT: f32 = 7.0;
/// Loop region modes (see [`App::loop_mode`]); index = config value.
const LOOP_MODES: [&str; 3] = ["area", "current longest", "whole song"];

/// Timeline zoom range in px/second. The floor is tuned so max zoom-out
/// shows ~100 measures in the ~940 pt viewport of the default 1440×853
/// window at 80 BPM 4/4 (3 s per bar → 300 s → 940/300 ≈ 3.1 px/s), and
/// the ceiling gives deep zoom-in for sample-level editing. The default
/// stays at 160 px/s; the toolbar slider maps this range logarithmically,
/// which puts the default near the slider's centre (≈60 %).
const ZOOM_MIN: f32 = 3.0;
const ZOOM_MAX: f32 = 2400.0;
const ZOOM_DEFAULT: f32 = 160.0;
/// Ctrl+wheel zoom sensitivity: 12 % per wheel line (trackpad pixel deltas
/// use 1 % per px so a fast flick doesn't warp the timeline).
const ZOOM_WHEEL_LINE: f32 = 0.12;
const ZOOM_WHEEL_PX: f32 = 0.01;
/// Shift+wheel horizontal-pan sensitivity in px per wheel-line unit. A
/// notch reports ~3 line units, so one click pans ~240 px — a visible
/// chunk of the view. (Was `delta * 2.0` = ~6 px per notch: read as
/// "horizontal scrolling is way too slow".)
const PAN_WHEEL_LINE: f32 = 80.0;

/// The user's home folder (projects default to `~/slooper-projects`).
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// One row of a track's mixer chain (ordered audio path). Scripted effects
/// and hosted plugins are the same kind of slot — FL Studio's mixer model —
/// so a plugin can sit anywhere in the chain and a track can host as many
/// as the user likes.
pub enum ChainRow {
    /// Bundled/user effect script.
    Script { stem: String, params: Vec<f32> },
    /// Hosted plugin. `entry` is None until it is opened at the engine
    /// sample rate (startup/Ready) or until an async load started by
    /// `add_fx_plugin` lands; `params` carries the latest snapshot
    /// for persistence and reopen-after-failure. `open_rx` (v16) is the
    /// completion channel of a load running on the plugin GUI thread —
    /// the UI thread never blocks on instantiation.
    Vst {
        path: String,
        kind: crate::vsthost::VstKind,
        name: String,
        params: Vec<f32>,
        entry: Option<Arc<crate::vsthost::RtStageEntry>>,
        open_rx:
            Option<std::sync::mpsc::Receiver<Result<Arc<crate::vsthost::RtStageEntry>, String>>>,
        /// v20: latest state chunk harvested from this plugin's VST3
        /// shadow editor (see `vstedit::Shared::state`). Session-only —
        /// like VSTi params it never enters config or project saves.
        state: Option<Vec<u8>>,
    },
}

/// Hand-written `Clone` (v16): `Receiver` is not `Clone`, and the only
/// clone sites are per-frame read snapshots of the row list for drawing.
/// The snapshot never needs the completion channel — the real rows in
/// `App::fx_rows` keep it and `poll_vst_opens` drains it.
impl Clone for ChainRow {
    fn clone(&self) -> Self {
        match self {
            ChainRow::Script { stem, params } => ChainRow::Script {
                stem: stem.clone(),
                params: params.clone(),
            },
            ChainRow::Vst {
                path,
                kind,
                name,
                params,
                entry,
                state,
                ..
            } => ChainRow::Vst {
                path: path.clone(),
                kind: *kind,
                name: name.clone(),
                params: params.clone(),
                entry: entry.clone(),
                state: state.clone(),
                open_rx: None,
            },
        }
    }
}

/// Plugin-instrument synth bindings reuse the `track_synth` stem table with
/// a scheme prefix: `"vst3:/abs/x.vst3"` / `"vst2:/abs/x.so"` — absolute
/// paths can never collide with a script stem name.
fn vst_synth_encode(kind: crate::vsthost::VstKind, path: &str) -> String {
    format!("{}:{path}", kind.tag().to_lowercase())
}

fn vst_synth_decode(s: &str) -> Option<(crate::vsthost::VstKind, &str)> {
    if let Some(p) = s.strip_prefix("vst3:") {
        Some((crate::vsthost::VstKind::V3, p))
    } else if let Some(p) = s.strip_prefix("vst2:") {
        Some((crate::vsthost::VstKind::V2, p))
    } else {
        None
    }
}

/// A selected clip: (track, lane, index into the lane's clip list).
type ClipId = (usize, usize, usize);

/// The whole application: engine backend handle, persistent UI state, and
/// the per-frame layout cache (`row_h` → [`App::track_rows`]) that keeps the
/// timeline blocks and the Mixer dock strip-per-track aligned.
pub struct App {
    backend: Backend,

    // --- audio / device state ---
    status: String,
    client_name: Option<String>,
    sample_rate: u32,
    avail_in: Vec<PortInfo>,
    avail_out: Vec<PortInfo>,
    sel_in: HashSet<String>,
    /// Selected outputs with their stereo routing (bit 0 = L, bit 1 = R);
    /// absent / 0 = not selected.
    sel_out: HashMap<String, u8>,
    applied: Option<(usize, usize)>,
    applied_ports: Option<(Vec<String>, Vec<OutSel>)>,
    /// Master output volume, MIDI-range (-inf..=+6 dB via [`vol_amp`]).
    master_midi: u8,
    /// GUI zoom factor (Settings → GUI); 1.0 = default UI size.
    gui_scale: f32,
    show_settings: bool,

    // --- mixer (per track) ---
    trk_vol: [u8; MAX_LOOPS],
    trk_gain: [u8; MAX_LOOPS],
    trk_pan: [u8; MAX_LOOPS],
    /// Last track selection propagated to the audio engine (input
    /// monitoring follows the selected track's chain/gain/pan).
    sel_sent: usize,
    levels_dirty: bool,

    // --- DAW state ---
    /// Track targeted by keyboard/MIDI "selected" actions.
    selected: usize,
    /// Tracks the program EXPOSES (Settings → DAW, 1..=16). Levels and
    /// bindings of hidden tracks survive: raising the count back restores
    /// them, so lowering tracks can never destroy a binding.
    n_tracks: usize,
    /// Timeline loop: wrap the playhead at the loop region's end (mode
    /// below) vs. park at the project end.
    loop_tl: bool,
    /// Loop region mode: 0 = area (dragged handles), 1 = current longest
    /// (0 → last clip end), 2 = whole song (0 → project end).
    loop_mode: usize,
    /// Area loop bounds in samples (mode 0; end 0 = "not set yet" → 2 bars
    /// once the sample rate is known).
    loop_area: (u64, u64),
    /// Which area-handle is being dragged on the ruler (0 = start, 1 = end).
    drag_loop_handle: Option<u8>,
    metro_on: bool,
    metro_bpm: String,
    metro_num: String,
    /// Index into [`SUBDIVISIONS`] (click subdivision).
    metro_sub: usize,
    /// Index into [`SUBDIVISIONS`] for content quantize / clip snap.
    q_sub: usize,
    snap_sub: usize,
    /// Quantize every fresh recording's content onto the grid.
    auto_quantize: bool,
    latency_ms: String,

    // --- project content (GUI master copies; engine holds Arc snapshots) ---
    arr: Arrangement,
    assets: Vec<Asset>,
    /// Waveform thumbnails by asset index (cleared on structural edits).
    wave: HashMap<u32, AssetWave>,
    /// Timeline zoom.
    px_per_sec: f32,
    drag_clip: Option<ClipDrag>,
    /// A press on empty grid owns the pointer until release: dragging it
    /// scrubs the global playhead (aligned with the lane grid exactly).
    scrub_drag: bool,
    /// Horizontal view offset of the timeline in px (shared by the pinned
    /// ruler and every lane strip — the playhead stays one straight line).
    hscroll: f32,
    /// Width of the visible grid strip in px (measured each frame by the
    /// central panel); lets logic-side code scroll pasted clips into view.
    grid_view_w: f32,
    /// Vertical scroll offset of the track stack, shared with the mixer
    /// panel so each mixer strip stays aligned with its track.
    vscroll: f32,
    /// Per-track grid-block height in points (0 = automatic:
    /// `lanes × LANE_H`). The user drags the separation bar under a track
    /// to resize it; the mixer strip mirrors the same height.
    trk_height: Vec<f32>,
    /// Active separation-bar drag: (track, height at drag start, pointer y
    /// at drag start). Growing a track never resizes its neighbours.
    sep_drag: Option<(usize, f32, f32)>,
    /// Absolute y of the timeline track stack's top edge, measured each
    /// frame by the central panel; the mixer aligns its strips to it (one
    /// frame late at most) so strip ↔ track can never drift apart.
    grid_anchor: f32,
    /// Selected clips: `(track, lane, clip index)` triplets, kept sorted
    /// and unique. Click selects one, Ctrl+click toggles, Shift+click
    /// extends within a lane; dragging one member drags the whole group.
    sel_clips: Vec<(usize, usize, usize)>,
    /// Last single-selected clip — the anchor for Shift+click extension.
    sel_anchor: Option<(usize, usize, usize)>,
    /// Track owning the last primary press inside the grid — clipboard
    /// paste targets this track (falls back to `selected`).
    last_click_track: Option<usize>,
    /// Clip copy buffer: a group of clips (offsets from the group's top
    /// track / left edge); paste re-places it at the playhead on the
    /// last-clicked track, opening a child lane when busy.
    clipboard: Option<Clipboard>,
    /// Active clip-edge drag (trim / extend, non-destructive).
    clip_resize: Option<ClipResize>,
    /// Rubber-band selection box in flight: (track, lane, x at press,
    /// Ctrl/Shift held → keep the previous selection).
    marquee: Option<(usize, usize, f32, bool)>,
    /// Empty-grid press that may still become a marquee (before the drag
    /// threshold): + selection snapshot taken at press.
    marquee_pending: Option<(usize, usize, f32, bool, Vec<ClipId>)>,
    /// The selection a marquee adds to (Ctrl/Shift box drag).
    marquee_base: Vec<ClipId>,

    // --- asset rack ---
    rack: Vec<RackFolder>,
    sel_asset: Option<usize>,
    /// Live input monitoring on/off (Settings → MONITORING, default on).
    monitor_enabled: bool,
    /// Audition rack entries on click (Settings → ASSET PREVIEW).
    preview_enabled: bool,
    /// Text box value for the preview length in seconds.
    preview_secs: String,
    /// Track whose instrument + mixer chain auditions MIDI (piano-roll
    /// "preview through track" picker); `None` = the default synth script.
    preview_track: Option<usize>,
    /// Piano-roll note clipboard (Ctrl+C/V while the roll has focus).
    roll_clip: Option<Vec<crate::midipat::Note>>,
    /// Grid snap enable (main header checkbox). OFF = placement/resize
    /// snaps to ONE SAMPLE — the program's minimum resolution.
    snap_on: bool,
    /// Asset name currently auditioning (clicking it again toggles stop).
    preview_playing: Option<String>,

    // --- project file ---
    proj_name: String,
    /// Text override for the projects folder ("" = `~/slooper-projects`).
    project_dir: String,
    show_save_prompt: bool,
    save_draft: String,
    show_load: bool,

    // --- MIDI subsystem (patterns, synths, recording, piano roll) ---
    synth_registry: synth::Registry,
    /// Per-track bound synth stem (None/"" = default piano fallback).
    track_synth: Vec<Option<String>>,
    track_synth_params: Vec<Vec<f32>>,
    /// Live handles (mirrors `track_synth`; also pushed into the engine).
    /// VST instruments are first-class synths, so this holds both kinds.
    track_synth_inst: Vec<Option<Arc<crate::audio::SynthHandle>>>,
    /// Track whose synth sliders window is open.
    synth_win: Option<usize>,
    /// v18: files dropped from the OS file manager into the window,
    /// drained from raw input at the top of every frame (routing lives in
    /// `ui_root`): sampler window (while open it owns drops, as ever) →
    /// lane strips (place clips at the pointer) → rack panel (import).
    /// Directories become rack folders immediately. Previously only the
    /// sampler drop zone listened, so dropping samples from the file
    /// manager did nothing anywhere else in the app.
    os_drops: Vec<std::path::PathBuf>,
    /// v17: track whose SYNTHS picker window is open (the ♪ chip).
    synth_pick: Option<usize>,
    /// Open VST2 custom editor windows, keyed by track. Closing (button,
    /// window manager, or a plugin swap) dispatches `effEditClose` on the
    /// editor thread before the plugin may be unloaded (`src/vstedit.rs`).
    vst_editors: std::collections::HashMap<usize, crate::vstedit::EditorWindow>,
    midi_rec: Vec<MidiRecState>,
    /// v0.11 R5: write-automation recorders + open automation manager.
    auto_rec: Vec<AutoRecState>,
    auto_edit: Option<Box<AutoEdit>>,
    /// v0.15: bindings picked as automation-manager record sources
    /// (action slot + bind). While the manager is open these are MUTED in
    /// the main window; with REC armed their raw values draw the curve.
    auto_mgr_rec: Vec<(usize, MidiBind)>,
    /// v0.15: the "select value" bind popup is open (auto_pick_window).
    auto_pick_open: bool,
    piano_roll: Option<Box<PianoRoll>>,
    /// v0.15: desktop-style right-click popup for automatable values whose
    /// widget cannot host an egui context menu (FX-script bridge sliders,
    /// the bpm text box): (track, target, current value, screen pos).
    value_menu: Option<(usize, String, f32, egui::Pos2)>,
    /// v0.15: "Set value" dialog (track, target, input text in USER units).
    value_dialog: Option<(usize, String, String)>,
    /// v0.15: focus + arm the "Set value" text box once, when it opens.
    value_dialog_focus: bool,
    /// v0.15: the mini menu only closes on clicks after its opening frame.
    value_menu_armed: bool,
    /// v0.15: last-frame rect of the bpm text box — the ui_root input pass
    /// swallows right-presses inside it so the automation popup, not the
    /// text-edit cut/copy/paste menu, answers.
    bpm_rect: Option<egui::Rect>,

    // --- per-track effects framework ---
    fx_registry: fx::Registry,
    /// Per-track mixer chains (ordered): scripted effects and VST plugins
    /// as one slot list — plugins sit anywhere in the chain.
    fx_rows: Vec<Vec<ChainRow>>,
    /// Script instances backing `fx_rows` (None on plugin rows; same order).
    fx_live: Vec<Vec<Option<std::sync::Arc<fx::Instance>>>>,
    fx_window: Option<usize>,
    /// Live X11 editor windows for CHAIN plugins, keyed (track, row).
    /// v0.10: adding a plugin effect opens the plugin's own GUI straight
    /// away; this map keeps those windows alive and reaped like the synth
    /// editors. v0.11 R3: hosted plugins have NO slider-window fallback --
    /// their own window is the only parameter surface.
    fx_vst_editors: std::collections::HashMap<(usize, usize), crate::vstedit::EditorWindow>,
    /// v20: last state chunk harvested from each track's VST3 synth
    /// editor; re-applied when the binding rebuilds (session-only, like
    /// `track_synth_params`).
    track_synth_state: Vec<Option<Vec<u8>>>,
    /// Scan of all VST folders (lazy on first window open, manual Rescan).
    vst_scan: Vec<crate::vsthost::VstEntry>,
    vst_scanned: bool,
    /// User-set extra VST folders (config); the system defaults are always
    /// scanned on top of these.
    vst_dirs: Vec<String>,
    /// One-line feedback under the folder row.
    vst_note: String,
    /// v16: completion channel of a folder scan running on the plugin GUI
    /// thread (the first chain-window open must not freeze the UI).
    vst_scan_rx: Option<std::sync::mpsc::Receiver<Vec<crate::vsthost::VstEntry>>>,
    /// v16: chain rows (track, row) awaiting their load completion, then
    /// the v0.10 auto-open of the plugin's own GUI.
    vst_pending_gui: Vec<(usize, usize)>,
    fx_editor: Option<Box<crate::fxgui::FxEditor>>,
    fx_dir_edit: String,
    fx_note: String,
    fx_save_pending: bool,

    // --- MIDI bindings: one slot per entry of MidiAction::ALL ---
    midi_binds: [Option<MidiBind>; N_ACTIONS],
    short_keys: [Option<egui::Key>; N_ACTIONS],
    learning_slot: Option<usize>,
    midi_learning_slot: Option<usize>,
    knob_learn: Option<KnobLearn>,
    knob_cal: Vec<Option<MidiAxisCal>>,
    midi_sources: Vec<String>,
    bind_loop_sel: usize,

    // --- export dialog ---
    show_export: bool,
    export_whole: bool,
    /// Export only the current multi-clip selection.
    export_sel: bool,
    export_path: String,
    export_fmt: usize,

    // --- MIDI monitor ---
    last_midi: Option<MidiLine>,
    midi_log: VecDeque<MidiLine>,
    peak_disp: f32,
}

impl App {
    pub fn new(backend: Backend) -> Self {
        let cfg = Config::load();
        // Effects framework: open (creating/seeding when needed) the folder
        // and compile+load every script; chains rebuild once the engine
        // sample rate is known (Evt::Ready).
        let fx_dir = fx::prepare_effects_dir(if cfg.fx_dir.trim().is_empty() {
            None
        } else {
            Some(cfg.fx_dir.trim())
        });
        let mut fx_registry = fx::Registry::open(fx_dir.clone());
        fx_registry.rescan();
        // Mixer chain rows from the saved config (scripts materialize at
        // rebuild; plugins open later, once the engine sample rate is known).
        let chain_cfg = cfg.chain();
        let fx_rows: Vec<Vec<ChainRow>> = (0..MAX_LOOPS)
            .map(|i| {
                chain_cfg
                    .get(i)
                    .map(|v| {
                        v.iter()
                            .filter_map(|s| match s {
                                ChainSave::Fx { stem, params } => Some(ChainRow::Script {
                                    stem: stem.clone(),
                                    params: params.clone(),
                                }),
                                ChainSave::Vst {
                                    path,
                                    kind,
                                    name,
                                    params,
                                } => {
                                    let kind = crate::vsthost::VstKind::parse(kind)?;
                                    Some(ChainRow::Vst {
                                        path: path.clone(),
                                        kind,
                                        name: name.clone(),
                                        params: params.clone(),
                                        entry: None,
                                        open_rx: None,
                                        state: None,
                                    })
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        // Keyboard table: this layout is trusted verbatim; older persisted
        // layouts are remapped, then defaults fill the gaps.
        let migrated = crate::midi::migrate_shortcuts(
            &cfg.shortcuts
                .iter()
                .map(|s| s.as_deref().and_then(key_from_name))
                .collect::<Vec<_>>(),
        );
        let trusted = cfg.shortcuts.len() == N_ACTIONS;
        let short_keys = core::array::from_fn(|i| match migrated.get(i).copied().flatten() {
            Some(k) => Some(k),
            None if trusted => None,
            None => SHORTCUT_DEFAULTS[i],
        });
        let mut midi_binds = [None; N_ACTIONS];
        for b in &cfg.midi {
            let i = MidiAction::idx(b.action);
            if i < N_ACTIONS {
                if let Some(bd) = b.bind {
                    midi_binds[i] = Some(bd);
                }
            }
        }
        let mut knob_cal: Vec<Option<MidiAxisCal>> = vec![None; N_ACTIONS];
        for c in &cfg.midi_axis {
            if let Some(s) = knob_cal.get_mut(usize::from(c.slot)) {
                *s = Some(*c);
            }
        }
        // Per-track levels: pad persisted vectors and push into the engine
        // atomics — those are the live source of truth for the RT thread.
        let trk_vol =
            core::array::from_fn(|i| cfg.trk_vol.get(i).copied().unwrap_or(VOL_UNITY_MIDI));
        let trk_gain =
            core::array::from_fn(|i| cfg.trk_gain.get(i).copied().unwrap_or(GAIN_UNITY_MIDI));
        let trk_pan = core::array::from_fn(|i| cfg.trk_pan.get(i).copied().unwrap_or(64));
        for t in 0..MAX_LOOPS {
            backend.engine.set_loop_vol(t, vol_amp(trk_vol[t]));
            backend.engine.set_loop_gain(t, gain_amp(trk_gain[t]));
            backend.engine.set_loop_pan(t, pan01(trk_pan[t]));
        }
        let sel_in: HashSet<String> = cfg.ports_in.iter().cloned().collect();
        let sel_out: HashMap<String, u8> = cfg
            .ports_out
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), cfg_lr(&cfg.ports_out_lr, i)))
            .collect();
        let applied_ports = if cfg.ports_in.is_empty() && cfg.ports_out.is_empty() {
            None
        } else {
            Some((
                cfg.ports_in.clone(),
                cfg.ports_out
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.clone(), cfg_lr(&cfg.ports_out_lr, i)))
                    .collect(),
            ))
        };
        // Bundled 808 pack: seed it into the app data dir; it becomes the
        // default rack folder when the user has configured none — takes then
        // record into it and every 808 voice is one drag away.
        let seeded = crate::samples808::seed_pack();
        let mut rack: Vec<RackFolder> = cfg
            .rack_folders
            .iter()
            .map(|p| RackFolder::new(p.clone()))
            .collect();
        if rack.is_empty() {
            rack.push(RackFolder::new(seeded.to_string_lossy().into_owned()));
        }
        for f in rack.iter_mut() {
            f.rescan();
        }
        let sub_idx = |v: u32| SUBDIVISIONS.iter().position(|(c, _)| *c == v).unwrap_or(3);
        let synth_registry = synth::Registry::open(synth::default_dir());
        let mut track_synth: Vec<Option<String>> = Vec::with_capacity(MAX_LOOPS);
        let mut track_synth_params: Vec<Vec<f32>> = Vec::with_capacity(MAX_LOOPS);
        let mut track_synth_inst: Vec<Option<Arc<SynthHandle>>> = Vec::with_capacity(MAX_LOOPS);
        for t in 0..MAX_LOOPS {
            let stem = cfg
                .track_synth
                .get(t)
                .cloned()
                .flatten()
                .filter(|s| !s.is_empty());
            track_synth.push(stem);
            track_synth_params.push(cfg.track_synth_params.get(t).cloned().unwrap_or_default());
            track_synth_inst.push(None);
        }
        Self {
            backend,
            status: "starting…".into(),
            client_name: None,
            sample_rate: 0,
            avail_in: Vec::new(),
            avail_out: Vec::new(),
            sel_in,
            sel_out,
            applied: None,
            applied_ports,
            master_midi: cfg.master_vol.min(127),
            gui_scale: cfg.gui_scale.clamp(0.7, 1.6),
            show_settings: false,
            trk_vol,
            trk_gain,
            trk_pan,
            sel_sent: usize::MAX,
            levels_dirty: false,
            selected: 0,
            n_tracks: cfg.n_tracks.clamp(1, MAX_LOOPS),
            loop_tl: cfg.loop_tl,
            loop_mode: cfg.loop_mode.min(2),
            loop_area: (cfg.loop_area_start, cfg.loop_area_end),
            drag_loop_handle: None,
            metro_on: false,
            metro_bpm: cfg.metro_bpm.to_string(),
            metro_num: cfg.metro_num.clamp(1, 16).to_string(),
            metro_sub: SUBDIVISIONS
                .iter()
                .position(|(c, _)| *c == cfg.metro_sub)
                .unwrap_or(0),
            q_sub: sub_idx(cfg.q_sub),
            snap_sub: sub_idx(cfg.snap_sub),
            auto_quantize: cfg.auto_quantize,
            latency_ms: if cfg.latency_ms == 0.0 {
                "0".into()
            } else {
                format!("{}", cfg.latency_ms)
            },
            arr: Arrangement::new(),
            assets: Vec::new(),
            wave: HashMap::new(),
            px_per_sec: ZOOM_DEFAULT,
            drag_clip: None,
            scrub_drag: false,
            hscroll: 0.0,
            grid_view_w: 900.0,
            vscroll: 0.0,
            trk_height: (0..MAX_LOOPS)
                .map(|i| cfg.trk_h.get(i).copied().unwrap_or(0.0))
                .collect(),
            sep_drag: None,
            grid_anchor: 0.0,
            sel_clips: Vec::new(),
            sel_anchor: None,
            last_click_track: None,
            clipboard: None,
            clip_resize: None,
            marquee: None,
            marquee_pending: None,
            marquee_base: Vec::new(),
            rack,
            sel_asset: None,
            monitor_enabled: cfg.monitor_on,
            preview_enabled: cfg.preview_on,
            preview_secs: cfg.preview_secs.max(0.0).to_string(),
            preview_track: cfg.preview_track.map(|t| t as usize),
            roll_clip: None,
            snap_on: cfg.snap_on,
            preview_playing: None,
            proj_name: String::new(),
            project_dir: cfg.project_dir.clone(),
            show_save_prompt: false,
            save_draft: String::new(),
            show_load: false,
            synth_registry,
            track_synth,
            track_synth_params,
            track_synth_inst,
            synth_win: None,
            os_drops: Vec::new(),
            synth_pick: None,
            vst_editors: std::collections::HashMap::new(),
            midi_rec: vec![MidiRecState::default(); MAX_LOOPS],
            auto_rec: vec![AutoRecState::default(); MAX_LOOPS],
            auto_edit: None,
            auto_mgr_rec: Vec::new(),
            auto_pick_open: false,
            value_menu: None,
            value_dialog: None,
            value_dialog_focus: false,
            value_menu_armed: false,
            bpm_rect: None,
            piano_roll: None,
            fx_registry,
            fx_rows,
            fx_live: vec![Vec::new(); MAX_LOOPS],
            fx_window: None,
            fx_vst_editors: std::collections::HashMap::new(),
            track_synth_state: vec![None; MAX_LOOPS],
            vst_scan: Vec::new(),
            vst_scan_rx: None,
            vst_pending_gui: Vec::new(),
            vst_scanned: false,
            vst_dirs: cfg.vst_dirs.clone(),
            vst_note: String::new(),
            fx_editor: None,
            fx_dir_edit: fx_dir.display().to_string(),
            fx_note: String::new(),
            fx_save_pending: false,
            midi_binds,
            short_keys,
            learning_slot: None,
            midi_learning_slot: None,
            knob_learn: None,
            knob_cal,
            midi_sources: Vec::new(),
            bind_loop_sel: 0,
            show_export: false,
            export_whole: false,
            export_sel: false,
            export_path: String::new(),
            export_fmt: 0,
            last_midi: None,
            midi_log: VecDeque::with_capacity(300),
            peak_disp: 0.0,
        }
    }

    fn now_stamp() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() % 86_400)
            .unwrap_or(0);
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60
        )
    }

    /// File-name suffix that keeps fresh recordings unique.
    fn fstamp() -> String {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        format!("{}{:03}", d.as_secs() % 100_000, d.subsec_millis())
    }

    /// Process all pending backend events right now (UI loop + tests).
    pub fn drain_events(&mut self) {
        while let Ok(evt) = self.backend.evt_rx.try_recv() {
            self.handle_evt(evt);
        }
    }

    /// Arm the MIDI "listening" slot for `idx` — what clicking a binding cell
    /// does; the next bindable MIDI message becomes that slot's binding.
    /// Arming cancels any armed keyboard cell: only one cell may listen at a
    /// time, otherwise the key-learn branch would swallow Delete/keys first.
    pub fn arm_midi_slot(&mut self, idx: usize) {
        self.midi_learning_slot = Some(idx);
        self.learning_slot = None;
    }

    /// Start the three-step knob learn for continuous slot `idx`: opens a
    /// sub-window that walks the user through locking the axis (type +
    /// channel), then capturing the raw minimum and maximum (see
    /// [`knob_learn_window`]).
    pub fn arm_knob_learn(&mut self, idx: usize) {
        self.midi_learning_slot = None;
        self.learning_slot = None;
        self.knob_learn = Some(KnobLearn {
            slot: idx,
            bind: MidiBind {
                ty: 0xff,
                ch: 0,
                d1: 0,
            },
            has_axis: false,
            axis_locked: false,
            last_val: None,
            lo: 0,
            hi: 127,
            locked: false,
        });
        self.status = format!(
            "knob learn: {} — move the knob to define its axis",
            MidiAction::ALL[idx].label()
        );
    }

    /// Step 1 done: freeze the axis (type + channel) found so far and start
    /// collecting the minimum. While the axis is unlocked, every message
    /// re-picks it (moving a different knob changes the binding); after
    /// Lock axis, messages on other axes are rejected. Restart unlocks it
    /// again.
    pub fn knob_lock_axis(&mut self) {
        if let Some(kl) = self.knob_learn.as_mut() {
            if !kl.has_axis {
                return;
            }
            kl.axis_locked = true;
            // Seed the min readout with the value seen at lock time, then
            // let step 2 follow every message from here on.
            kl.lo = kl.last_val.unwrap_or(kl.lo);
            self.status = format!(
                "axis {} locked — sweep the control to its MINIMUM, then Lock min",
                kl.bind.axis_label()
            );
        }
    }

    /// Step 2 done: freeze the minimum and start collecting the maximum.
    pub fn knob_lock_min(&mut self) {
        if let Some(kl) = self.knob_learn.as_mut() {
            kl.locked = true;
            kl.hi = kl.lo;
            self.status = "sweep the control to its MAXIMUM, then Finish".into();
        }
    }

    /// Step 3 done: commit the axis and the raw lo/hi range. lo > hi (an
    /// inverted control) is stored exactly as captured — `scale_axis` flips
    /// it so the knob still reads low→high continuously.
    pub fn knob_finish(&mut self) {
        let Some(kl) = self.knob_learn.take() else {
            return;
        };
        if kl.hi == kl.lo {
            // Same raw value at both ends: there is nothing to scale against.
            self.knob_learn = Some(kl);
            self.status = "finish needs a maximum different from the minimum — sweep the control and Finish again".into();
            return;
        }
        let (slot, lo, hi, bind) = (kl.slot, kl.lo, kl.hi, kl.bind);
        self.midi_binds[slot] = Some(bind);
        // 0..=127 is the program's native scale: no factor needed.
        self.knob_cal[slot] = if lo == 0 && hi == 127 {
            None
        } else {
            Some(MidiAxisCal {
                slot: slot as u16,
                lo,
                hi,
            })
        };
        self.save_config();
        self.status = format!(
            "bound {} → axis {} (raw {}–{}{})",
            MidiAction::ALL[slot].label(),
            bind.axis_label(),
            lo,
            hi,
            if lo > hi {
                " — INVERTED control: flipped so the knob still reads low→high"
            } else if lo == 0 && hi == 127 {
                ""
            } else {
                ", rescaled to full range"
            }
        );
    }

    /// Arm the keyboard "press a key" slot for `idx` (cancels any MIDI cell
    /// that was listening — see [`Self::arm_midi_slot`]).
    pub fn arm_key_slot(&mut self, idx: usize) {
        self.learning_slot = Some(idx);
        self.midi_learning_slot = None;
    }

    /// The currently captured MIDI binding for slot `idx`, if any.
    pub fn midi_bind_at(&self, idx: usize) -> Option<MidiBind> {
        self.midi_binds[idx]
    }

    /// Read-only backend access (tests / diagnostics).
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Currently selected loop index (test access).
    pub fn selected_loop(&self) -> usize {
        self.selected
    }

    /// Global keyboard shortcuts (IDEA.md: "simple action mapping system with
    /// midi input and keyboard support"). Every action in MidiAction::ALL has
    /// a rebindable key slot (Settings → MIDI + Keyboard Bindings); keys are
    /// persisted. Press edges are read from the per-frame event list so key
    /// auto-repeat doesn't re-fire an action, and the keys are ignored while
    /// ANY text box has focus.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Ignore keys while a text box anywhere in the program has focus —
        // the egui focus registry, so it covers every TextEdit (quantize
        // fields, FX preset name, export path, level nudge, …) including
        // ones added later, instead of a hand-maintained per-widget flag.
        // Ctrl/Cmd+S saves the project (with Shift: always re-ask the
        // name). Checked before the text-focus guard: saving while a field
        // has focus is always meant as save.
        let (save, shift) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(egui::Key::S),
                i.modifiers.shift,
            )
        });
        if save {
            self.request_save(shift);
        }

        // Clip clipboard: Ctrl/Cmd+C copies the whole selection (a group of
        // clips spanning any number of tracks/lanes), Ctrl/Cmd+V places it
        // at the playhead on the last grid-clicked track (busy lanes open a
        // child lane automatically). Handled before action firing so a bare
        // C/V binding never double-fires — AND before the text-focus guard:
        // egui keeps a TextEdit focused until another FOCUSABLE widget is
        // clicked, and grid clips are plain painted rects, so the old
        // "return early while any box has focus" meant Ctrl+C/V silently
        // died the moment the user had ever touched the bpm/quantize/name
        // boxes (the reported "copy/paste does nothing").
        let (ccopy, ccut, cpaste) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(egui::Key::C),
                i.modifiers.command && i.key_pressed(egui::Key::X),
                i.modifiers.command && i.key_pressed(egui::Key::V),
            )
        });
        if ccopy || ccut || cpaste {
            // While the piano roll owns the keyboard these keys belong to
            // the ROLL (handled in `piano_roll_window`) — never clips.
            if !self.piano_roll.as_ref().is_some_and(|r| r.focused) {
                if ccopy {
                    self.clipboard_copy();
                }
                if ccut {
                    // v0.15: Ctrl+X — the cut that was always missing.
                    if self.sel_clips.is_empty() {
                        self.status = "cut: select clips first".into();
                    } else {
                        self.clipboard_copy();
                        self.delete_selection();
                        self.status = format!(
                            "cut {} clip(s) \u{b7} Ctrl+V pastes at the playhead",
                            self.clipboard.as_ref().map(|c| c.items.len()).unwrap_or(0)
                        );
                    }
                }
                if cpaste {
                    self.clipboard_paste();
                }
                return; // swallow the key
            }
        }

        if ctx.text_edit_focused() {
            return;
        }

        // The piano roll owns the keyboard while it has focus: no
        // transport, no clip keys, no select-all behind it (space starting
        // the transport mid-edit was the user complaint). The roll reads
        // its own Ctrl+C/V/A/Delete inside its window code.
        if self.piano_roll.as_ref().is_some_and(|r| r.focused) {
            return;
        }

        let call = ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::A));
        if call {
            self.select_all_clips();
            return;
        }

        // Ctrl+D duplicates the selection at the playhead (documented).
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::D)) {
            self.duplicate_selection();
            return;
        }
        // Bare S splits the selection at the playhead (plain keyboard only:
        // Ctrl+S saved long before this point, and text fields returned).
        if ctx.input(|i| {
            i.key_pressed(egui::Key::S)
                && !i.modifiers.command
                && !i.modifiers.alt
                && !i.modifiers.ctrl
                && !i.modifiers.shift
        }) {
            self.split_selection();
            return;
        }

        // The knob wizard is its own learn mode: it does NOT set
        // `midi_learning_slot` (arm_knob_learn clears it), so it needs its
        // own key branch. Escape cancels; Delete clears the slot's binding
        // and calibration — same vocabulary as the key/MIDI cells. All keys
        // are swallowed so nothing fires behind the wizard.
        if self.knob_learn.is_some() {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = key {
                let kl = match k {
                    egui::Key::Escape => Some(("cancelled knob learn", false)),
                    egui::Key::Delete => Some(("cleared knob binding", true)),
                    _ => None,
                };
                if let Some((verb, clear)) = kl {
                    if let Some(kl) = self.knob_learn.take() {
                        if clear {
                            self.midi_binds[kl.slot] = None;
                            if let Some(c) = self.knob_cal.get_mut(kl.slot) {
                                *c = None;
                            }
                            self.save_config();
                        }
                        self.status = format!("{verb}: {}", MidiAction::ALL[kl.slot].label());
                    }
                }
            }
            return; // swallow all keys while the knob wizard is open
        }

        // Learn mode: the next key press becomes the slot's new binding.
        if let Some(slot) = self.learning_slot {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = key {
                if k == egui::Key::Escape {
                    self.learning_slot = None; // cancel
                    self.midi_learning_slot = None;
                } else if k == egui::Key::Delete {
                    // Delete is reserved: it can never be bound. Pressing it
                    // while a key cell listens unbinds that slot (blank),
                    // mirroring the MIDI column's Delete-to-clear.
                    self.short_keys[slot] = None;
                    self.learning_slot = None;
                    self.save_config();
                    self.status = format!("cleared binding: {}", MidiAction::ALL[slot].label());
                } else {
                    match self.short_keys.iter().position(|s| *s == Some(k)) {
                        Some(s) if s != slot => {
                            self.status =
                                format!("{} is already bound — try another key", key_display(k));
                        }
                        _ => {
                            self.short_keys[slot] = Some(k);
                            self.learning_slot = None;
                            self.save_config();
                        }
                    }
                }
            }
            return; // swallow all keys while learning
        }

        // While a MIDI binding cell is listening, only Delete (clear the
        // binding) and Escape (cancel) are valid — every other key is
        // swallowed so it can neither fire an action nor rebind anything.
        if let Some(slot) = self.midi_learning_slot {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = key {
                if k == egui::Key::Escape {
                    self.midi_learning_slot = None;
                } else if k == egui::Key::Delete {
                    // Delete clears this MIDI cell's binding (the knob
                    // wizard is handled in its own branch above).
                    self.midi_binds[slot] = None;
                    self.midi_learning_slot = None;
                    self.save_config();
                    self.status =
                        format!("cleared MIDI binding: {}", MidiAction::ALL[slot].label());
                }
            }
            return; // swallow all keys while a MIDI cell is listening
        }

        // Collect this frame's key presses in ONE input pass (the old
        // per-slot closure locked the input state up to N_ACTIONS times per
        // frame). Press edges come from the event list so auto-repeat never
        // re-fires an action.
        let pressed_keys: Vec<egui::Key> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
                .collect()
        });

        // Selected clips delete with Delete/Backspace — same vocabulary as
        // clearing bindings (learn modes above already swallowed the key if
        // they were open, and typing focus returned earlier). The assets
        // themselves stay in the rack; only the placements disappear.
        if !self.sel_clips.is_empty()
            && pressed_keys
                .iter()
                .any(|k| matches!(k, egui::Key::Delete | egui::Key::Backspace))
        {
            self.delete_selection();
            return; // swallow the key
        }

        // Slot order matters: selection slots fire first so transport actions
        // in the same frame see the updated selection. Keys are unique across
        // slots (learn mode rejects duplicates), so at most one slot matches a
        // physical key.
        for i in 0..N_ACTIONS {
            if let Some(k) = self.short_keys[i] {
                if pressed_keys.contains(&k) {
                    self.fire_midi_action(MidiAction::ALL[i]);
                }
            }
        }
    }

    /// Set loop `i`'s playback volume from a MIDI-range value (0..=127).
    fn set_trk_vol(&mut self, i: usize, v: u8) {
        self.trk_vol[i] = v;
        self.backend.engine.set_loop_vol(i, vol_amp(v));
        self.note_auto(i, crate::automation::TARGET_TRACK_VOL, vol_amp(v));
    }

    /// Set loop `i`'s gain trim from a MIDI-range value (0..=127).
    fn set_trk_gain(&mut self, i: usize, v: u8) {
        self.trk_gain[i] = v;
        self.backend.engine.set_loop_gain(i, gain_amp(v));
    }

    /// Set loop `i`'s stereo pan from a MIDI-range value (0 = hard left,
    /// 64 = centre, 127 = hard right).
    fn set_trk_pan(&mut self, i: usize, v: u8) {
        self.trk_pan[i] = v;
        self.backend.engine.set_loop_pan(i, pan01(v));
        self.note_auto(i, crate::automation::TARGET_TRACK_PAN, pan01(v));
    }

    /// Rebuild loop `i`'s live effect chain from its window rows and hand it
    /// to the audio engine. Also seeds each row's slider values from the
    /// descriptors (stored values win, clamped to range).
    fn rebuild_fx(&mut self, i: usize) {
        // The editor window edits one specific live instance; a chain
        // rebuild replaces that instance, so the window must not outlive it.
        if self.fx_editor.as_ref().is_some_and(|ed| ed.loop_i == i) {
            self.fx_editor = None;
        }
        let sr = self.sample_rate.max(48_000);
        let mut live: Vec<Option<std::sync::Arc<fx::Instance>>> = Vec::new();
        let mut slots: Vec<FxSlot> = Vec::new();
        for row in self.fx_rows[i].iter_mut() {
            match row {
                ChainRow::Script { stem, params } => {
                    if slots.len() >= fx::MAX_FX_PER_TRACK {
                        continue; // scripted slots share the historic cap; plugins are uncapped
                    }
                    let Some(desc) = self.fx_registry.desc(stem).cloned() else {
                        continue;
                    };
                    let vals = fx::seed_values(params, &desc);
                    *params = vals.clone();
                    if let Some(inst) =
                        self.fx_registry
                            .create_instance(stem, self.sample_rate as f64, &vals)
                    {
                        slots.push(FxSlot::scripted(inst.clone(), desc));
                        live.push(Some(inst));
                    } else {
                        live.push(None);
                    }
                }
                ChainRow::Vst {
                    path,
                    kind,
                    name,
                    params,
                    entry,
                    open_rx,
                    state,
                } => {
                    // Open lazily (first rebuild after load, or after the
                    // folder rescan dropped a failed load). v16: rows with
                    // a live `open_rx` are loading on the plugin thread —
                    // never block the UI thread on them.
                    if entry.is_none() && open_rx.is_none() {
                        let p = std::path::Path::new(path);
                        if p.is_file() || p.is_dir() {
                            match crate::vsthost::open_with_state(
                                p,
                                *kind,
                                sr,
                                Some(params),
                                state.as_deref(),
                            ) {
                                Ok(load) => *entry = Some(load.entry),
                                Err(err) => {
                                    self.vst_note = format!("{name}: {err}");
                                }
                            }
                        }
                    }
                    match entry {
                        Some(e) => {
                            slots.push(FxSlot::vst(e.clone()));
                            live.push(None);
                        }
                        None => live.push(None), // keep row/instance alignment
                    }
                }
            }
        }
        self.fx_live[i] = live;
        self.backend.cmd(Cmd::SetTrackEffects {
            i,
            chain: TrackFx {
                slots: std::sync::Arc::new(slots),
            },
        });
    }

    /// Append a script effect to track `i`'s chain (duplicates allowed —
    /// FL lets the same effect sit in two slots).
    fn add_fx_script(&mut self, i: usize, stem: &str) {
        self.fx_rows[i].push(ChainRow::Script {
            stem: stem.to_string(),
            params: Vec::new(),
        });
        self.rebuild_fx(i);
        self.fx_save_pending = true;
        self.save_config();
    }

    /// v0.12: open a hosted plugin's own editor window, generation-aware.
    /// VST2 embeds the LIVE instance (dispatcher handle); VST3 hosts a
    /// shadow editor instance on the same control thread — see
    /// `RtStageEntry::open_vst3_editor`. Both return the same handle type,
    /// so reaping and toggling never care which generation is behind.
    fn plugin_window_open(
        &mut self,
        entry: &std::sync::Arc<crate::vsthost::RtStageEntry>,
    ) -> Result<crate::vstedit::EditorWindow, String> {
        if let Some(h) = entry.vst2_handle() {
            let pth = entry.path().to_string_lossy().into_owned();
            crate::vstedit::open_editor(&h, &pth, entry.name())
        } else {
            entry.open_vst3_editor(self.sample_rate.max(48_000))
        }
    }

    /// Open a plugin from the scan list and append it to track `i`'s
    /// chain. v16: instantiation runs on the plugin GUI thread — the click
    /// returns immediately, the row shows "loading", and `poll_vst_opens`
    /// lands the entry (then the v0.10 rule kicks in: the plugin's own
    /// GUI opens when the load finishes).
    fn add_fx_plugin(&mut self, i: usize, e: &crate::vsthost::VstEntry) {
        // v0.19 belt-and-braces: instruments never enter a mixer chain
        // (their entry point is the ♪ synth picker on the track header).
        if e.instrument {
            self.vst_note = format!("{} is an instrument — bind it with ♪", e.name);
            return;
        }
        let sr = self.sample_rate.max(48_000);
        let rx = crate::vsthost::open_async(&e.path, e.kind, sr, None);
        self.vst_note = format!("{}: loading…", e.name);
        self.fx_rows[i].push(ChainRow::Vst {
            state: None,
            path: e.path.to_string_lossy().into_owned(),
            kind: e.kind,
            name: e.name.clone(),
            params: Vec::new(),
            entry: None,
            open_rx: Some(rx),
        });
        let row = self.fx_rows[i].len() - 1;
        self.vst_pending_gui.push((i, row));
        self.rebuild_fx(i);
        self.save_config();
    }

    /// v16: drain the completion channels of async plugin loads (started
    /// by [`Self::add_fx_plugin`]) and of the folder scan. Called from the
    /// per-tick `logic`, never blocking: every channel is `try_recv`.
    /// Returns true when anything landed (forces a repaint).
    fn poll_vst_opens(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = self.vst_scan_rx.as_ref() {
            if let Ok(entries) = rx.try_recv() {
                let n = entries.iter().filter(|e| e.error.is_none()).count();
                self.vst_scan = entries;
                self.vst_scanned = true;
                self.vst_scan_rx = None;
                self.vst_note = format!("{n} plugin(s) ready");
                changed = true;
            }
        }
        let mut finished: Vec<usize> = Vec::new();
        for i in 0..MAX_LOOPS {
            let mut opened = false;
            for row in self.fx_rows[i].iter_mut() {
                if let ChainRow::Vst {
                    name,
                    entry,
                    open_rx,
                    ..
                } = row
                {
                    if let Some(rx) = open_rx.as_ref() {
                        match rx.try_recv() {
                            Ok(Ok(e)) => {
                                *entry = Some(e);
                                self.vst_note = format!("{name}: loaded");
                                opened = true;
                            }
                            Ok(Err(err)) => {
                                self.vst_note = format!("{name}: {err}");
                                opened = true;
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                // Loader job vanished (row removed while
                                // loading) — treat as not loaded.
                                self.vst_note = format!("{name}: loader gone");
                                opened = true;
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        }
                        if opened {
                            *open_rx = None;
                        }
                    }
                }
            }
            if opened {
                finished.push(i);
            }
        }
        for i in finished {
            self.rebuild_fx(i);
            self.save_config();
            changed = true;
            // v0.10 standard, now applied on completion: a freshly added
            // plugin opens its own GUI. Rows the user moved/removed, rows
            // whose plugin has no editor, and rows whose instance already
            // has a window (the v0.11 double-open guard) are skipped.
            let mut keep: Vec<(usize, usize)> = Vec::new();
            while let Some((t, row)) = self.vst_pending_gui.pop() {
                if t != i {
                    keep.push((t, row));
                    continue;
                }
                let plan = match self.fx_rows.get(t).and_then(|rows| rows.get(row)) {
                    Some(ChainRow::Vst {
                        path,
                        entry: Some(e),
                        ..
                    }) if e.has_editor() => {
                        let already = self
                            .fx_vst_editors
                            .values()
                            .chain(self.vst_editors.values())
                            .any(|w| w.path() == path.as_str());
                        (!already).then(|| (e.clone(), path.clone()))
                    }
                    _ => None,
                };
                if let Some((entry, _)) = plan {
                    match self.plugin_window_open(&entry) {
                        Ok(w) => {
                            self.fx_vst_editors.insert((t, row), w);
                            self.status = format!("{}: plugin window opened", entry.name());
                        }
                        Err(err) => self.status = format!("plugin GUI: {err} \u{2014} closed"),
                    }
                }
            }
            self.vst_pending_gui = keep;
        }
        changed
    }

    fn remove_fx_row(&mut self, i: usize, k: usize) {
        if k < self.fx_rows[i].len() {
            self.fx_rows[i].remove(k);
            self.rebuild_fx(i);
            self.save_config();
        }
    }

    /// Swap chain slot `k` with its neighbour (rebuild re-pushes the order).
    fn move_fx_row(&mut self, i: usize, k: usize, dir: i32) {
        let n = self.fx_rows[i].len();
        if k >= n {
            return;
        }
        let j = if dir < 0 { k.wrapping_sub(1) } else { k + 1 };
        if j >= n {
            return;
        }
        self.fx_rows[i].swap(k, j);
        self.rebuild_fx(i);
        self.save_config();
    }

    /// Open (or reopen) the child editor window for loop `i`'s chain slot
    /// `row`. Effects that export a custom GUI get their own canvas;
    /// (v0.11 R3: plugin rows are handled by vst_row_gui; hosted
    /// plugins never receive an auto slider window.)
    fn open_fx_editor(&mut self, i: usize, row: usize) {
        self.fx_editor = None; // closing an old window frees its GUI state
        let Some(stem) = (match self.fx_rows[i].get(row) {
            Some(ChainRow::Script { stem, .. }) => Some(stem.clone()),
            _ => None,
        }) else {
            return;
        };
        let inst = self.fx_live[i].get(row).cloned().flatten();
        let desc = self.fx_registry.desc(&stem).cloned();
        let (Some(inst), Some(desc)) = (inst, desc) else {
            return;
        };
        let gui = self.fx_registry.gui(&stem);
        self.fx_editor = Some(Box::new(crate::fxgui::FxEditor::new(
            i,
            row,
            stem,
            desc,
            inst,
            gui,
            self.sample_rate as f64,
            // The ACTIVE effects folder (not the possibly-unapplied edit
            // box): that's where `user presets/` lives for these scripts.
            self.fx_registry.dir().to_path_buf(),
        )));
    }

    /// Draw the editor window and flush its param rides into the persisted
    /// chain rows (config save goes through `fx_save_pending`).
    fn fx_editor_window(&mut self, ui: &mut egui::Ui) {
        let mut closed = false;
        if let Some(ed) = self.fx_editor.as_mut() {
            closed = !crate::fxgui::window(ui, ed);
            if let Some((k, v, pos)) = ed.ctx_menu.take() {
                self.value_menu = Some((ed.loop_i, format!("fx:{}:{}", ed.slot, k), v, pos));
            }
            let pend = std::mem::take(&mut ed.pending);
            if !pend.is_empty() {
                if let Some(ChainRow::Script { params, .. }) =
                    self.fx_rows[ed.loop_i].get_mut(ed.slot)
                {
                    for (k, v) in pend {
                        if let Some(p) = params.get_mut(k) {
                            *p = v;
                        }
                    }
                    self.fx_save_pending = true;
                }
            }
        }
        if closed {
            self.fx_editor = None;
        }
    }

    /// Resolve the "effects location" text box, rescan, and re-point every
    /// loop's chain at the new folder (chains whose scripts vanished simply
    /// drop those slots; slider values of survivors — and all plugin rows —
    /// are kept).
    fn apply_fx_dir(&mut self) {
        let dir = fx::prepare_effects_dir(Some(self.fx_dir_edit.trim()));
        self.fx_registry.set_dir(dir.clone());
        self.fx_dir_edit = dir.display().to_string();
        self.fx_registry.rescan();
        self.prune_dead_script_rows();
        self.save_config();
    }

    /// Drop chain rows pointing at scripts the registry no longer loads.
    fn prune_dead_script_rows(&mut self) {
        for i in 0..MAX_LOOPS {
            self.fx_rows[i].retain(|r| match r {
                ChainRow::Script { stem, .. } => self.fx_registry.desc(stem).is_some(),
                ChainRow::Vst { .. } => true,
            });
            self.rebuild_fx(i);
        }
    }

    /// Compile/load every script in the folder again and refresh all rows.
    fn rescan_fx(&mut self) {
        self.fx_registry.rescan();
        self.prune_dead_script_rows();
        let ok = self.fx_registry.stems().len();
        let bad = self
            .fx_registry
            .items()
            .iter()
            .filter(|e| e.error.is_some())
            .count();
        self.fx_note = format!("{ok} valid effect(s), {bad} rejected");
    }

    // ---- grid maths ------------------------------------------------------
    fn bpm(&self) -> u32 {
        self.metro_bpm
            .trim()
            .parse::<u32>()
            .unwrap_or(120)
            .clamp(20, 300)
    }

    fn beats(&self) -> u32 {
        self.metro_num
            .trim()
            .parse::<u32>()
            .unwrap_or(4)
            .clamp(1, 16)
    }

    /// One measure of the metronome grid, in engine samples: the timeline's
    /// bar length and the project's quantization unit.
    fn bar_samples(&self) -> u64 {
        (quantize::beat_samples(self.sample_rate, self.bpm()) * self.beats() as f64)
            .round()
            .max(1.0) as u64
    }

    fn q_unit(&self) -> f64 {
        quantize::unit_samples(self.sample_rate, self.bpm(), SUBDIVISIONS[self.q_sub].0)
    }

    fn snap_unit(&self) -> f64 {
        // Snap OFF (main header checkbox) = the program's minimum
        // resolution: one sample. Placement, drags and clip resize then
        // land on any sample boundary, never the subdivision grid.
        if self.snap_on {
            quantize::unit_samples(self.sample_rate, self.bpm(), SUBDIVISIONS[self.snap_sub].0)
        } else {
            1.0
        }
    }

    fn asset_len(&self, id: u32) -> u64 {
        self.assets
            .get(id as usize)
            .map(|a| match (&a.pattern, &a.automation) {
                (Some(pat), _) => pat.len_samples(self.sample_rate.max(1)),
                (None, Some(auto)) => auto.len_samples.max(0) as u64,
                (None, None) => a.data.len() as u64,
            })
            .unwrap_or(0)
    }

    /// Preview length in seconds (Settings text box, clamped 0.25…600 s;
    /// unparsable input falls back to the 5 s default).
    fn preview_secs_f(&self) -> f32 {
        self.preview_secs
            .trim()
            .parse::<f32>()
            .unwrap_or(5.0)
            .clamp(0.25, 600.0)
    }

    /// Audition asset `k` at the master output (asset-rack preview): an
    /// asset no longer than the preview length plays in full, a longer one
    /// plays its first `preview_secs` seconds. Clicking the asset that is
    /// already auditioning stops it. Engine-side the buffer auto-stops at
    /// its end, so `preview_playing` is only ever a hair stale.
    fn preview_asset(&mut self, k: usize) {
        // MIDI patterns audition through the default synth instead.
        if self.assets.get(k).is_some_and(|a| a.is_midi()) {
            self.audition_pattern(k);
            return;
        }
        // v0.11 R5: an automation curve is data, not sound — open its
        // editor instead of pretending to play it.
        if self.assets.get(k).is_some_and(|a| a.is_auto()) {
            self.open_auto_editor(Some(k));
            return;
        }
        if !self.preview_enabled {
            return;
        }
        let Some(a) = self.assets.get(k) else { return };
        let key = a.path.to_string_lossy().into_owned();
        // Toggle: clicking the entry that is auditioning stops the click.
        if self.preview_playing.as_deref() == Some(key.as_str()) {
            self.preview_playing = None;
            // v0.10 parallel preview: stop ONLY this asset's voices;
            // previews of other assets keep playing.
            self.backend.cmd(Cmd::PreviewStop { id: Some(k as u32) });
            self.status = "preview stopped".into();
            return;
        }
        if self.sample_rate == 0 {
            self.status = "preview: wait for the audio engine".into();
            return;
        }
        let cap = (self.preview_secs_f() as f64 * self.sample_rate as f64) as usize;
        let full = a.data.len();
        let end = full.min(cap.max(1));
        let samples: Arc<[f32]> = if end >= full {
            a.data.clone()
        } else {
            Arc::from(&a.data[..end])
        };
        let secs = end as f64 / self.sample_rate as f64;
        let nm = a.name.clone();
        self.preview_playing = Some(key);
        self.backend.cmd(Cmd::PreviewPlay {
            id: k as u32,
            samples,
        });
        self.status = format!("previewing {nm} ({secs:.1} s)");
    }

    /// Total timeline length: last clip end, rounded up to whole bars.
    fn total_len(&self) -> u64 {
        let bar = self.bar_samples();
        self.arr.total_len(&|id| self.asset_len(id), bar)
    }

    /// Hand the current project (asset table + arrangement + length) to the
    /// engine as one immutable snapshot.
    /// End of the last clip on any track/lane (0 when the grid is empty) —
    /// the "current longest" loop region end.
    fn last_clip_end(&self) -> u64 {
        let mut e = 0u64;
        for tr in self.arr.tracks.iter() {
            for l in tr.lanes.iter() {
                for c in l.clips.iter() {
                    e = e.max(c.pos.saturating_add(self.asset_len(c.asset)));
                }
            }
        }
        e
    }

    /// The loop region the engine should use right now (samples):
    /// *area* = the dragged ruler handles (unset end = 2 bars from 0),
    /// *current longest* = 0 → last clip end, *whole song* = 0 → project
    /// end. Degenerate areas collapse back to the 2-bar default.
    fn loop_region_eff(&self) -> (u64, u64) {
        if self.sample_rate == 0 {
            return (0, 0);
        }
        match self.loop_mode {
            0 => {
                let (mut s, mut e) = self.loop_area;
                if e == 0 {
                    e = 2 * self.bar_samples();
                }
                if e <= s {
                    s = 0;
                    e = 2 * self.bar_samples();
                }
                (s, e)
            }
            1 => (0, self.last_clip_end()),
            _ => (0, self.total_len()),
        }
    }

    /// Push the effective loop region to the audio engine.
    fn send_loop_region(&mut self) {
        let (s, e) = self.loop_region_eff();
        self.backend.cmd(Cmd::SetLoopRegion { start: s, end: e });
    }

    /// Height of one track's grid block — and of the mixer strip aligned
    /// with it (both read this, so they can never drift apart). User-resized
    /// via the separation bar under each track; lanes inside share it equally.
    fn track_h(&self, t: usize) -> f32 {
        let lanes = self.arr.tracks[t].lanes.len().max(1) as f32;
        let auto = (lanes * LANE_H).max(TRACK_MIN_H);
        let floor = (lanes * LANE_MIN_H).max(TRACK_MIN_H);
        match self.trk_height.get(t).copied().unwrap_or(0.0) {
            0.0 => auto,
            h => h.max(floor),
        }
    }

    /// User height for track `t` (clamped to the lane floor and a sane max).
    fn set_track_h(&mut self, t: usize, h: f32) {
        let floor = (self.arr.tracks[t].lanes.len().max(1) as f32 * LANE_MIN_H).max(TRACK_MIN_H);
        if let Some(s) = self.trk_height.get_mut(t) {
            *s = h.clamp(floor, 1600.0);
        }
    }

    /// A new child lane must NOT shrink the lanes above it: grow the track
    /// by one old-lane height so every lane keeps the height it had.
    fn grow_for_lanes(&mut self, t: usize, old_lanes: usize) {
        let old_lanes = old_lanes.max(1);
        let new = self.arr.tracks[t].lanes.len().max(1);
        if new > old_lanes {
            let per = self.track_h(t) / old_lanes as f32;
            let want = new as f32 * per;
            let cur = self.trk_height.get(t).copied().unwrap_or(0.0);
            self.set_track_h(t, cur.max(want));
        }
    }

    fn push_project(&mut self) {
        let table: Arc<Vec<Arc<[f32]>>> =
            Arc::new(self.assets.iter().map(|a| a.data.clone()).collect());
        let arr = Arc::new(self.arr.clone());
        let len = self.total_len();
        self.backend.cmd(Cmd::SetProject {
            assets: table,
            arr,
            len,
        });
        let midi: Arc<Vec<Option<Arc<Pattern>>>> =
            Arc::new(self.assets.iter().map(|a| a.pattern.clone()).collect());
        self.backend.cmd(Cmd::SetMidiAssets(midi));
        // v0.11 R5: automation curves ride the same parallel-indexed table.
        let auto: Arc<Vec<Option<Arc<crate::automation::Automation>>>> =
            Arc::new(self.assets.iter().map(|a| a.automation.clone()).collect());
        self.backend.cmd(Cmd::SetAutoAssets(auto));
        // v0.10: a MIDI clip landing on an unbound track gives that track
        // its own piano instance (dedicated, never the shared fallback).
        for t in 0..MAX_LOOPS {
            if self.track_synth_inst[t].is_none() && self.track_has_midi(t) {
                self.rebuild_track_synth(t);
            }
        }
        // "current longest" / "whole song" regions follow the arrangement.
        self.send_loop_region();
    }

    // ---- selection + clipboard ------------------------------------------------

    /// Ctrl+A: select every placed clip on every configured track.
    fn select_all_clips(&mut self) {
        let mut ids = Vec::new();
        for t in 0..self.n_tracks {
            for k in 0..self.arr.tracks[t].lanes.len() {
                for c in 0..self.arr.tracks[t].lanes[k].clips.len() {
                    ids.push((t, k, c));
                }
            }
        }
        if ids.is_empty() {
            self.status = "no clips to select".into();
            return;
        }
        self.status = format!("selected {} clip(s)", ids.len());
        self.sel_anchor = ids.first().copied();
        self.sel_clips = ids;
    }

    /// True when the `(track, lane, clip)` id is in the selection.
    fn is_sel(&self, id: (usize, usize, usize)) -> bool {
        self.sel_clips.contains(&id)
    }

    /// Replace the selection with a single clip (moves the Shift anchor).
    fn select_one(&mut self, id: (usize, usize, usize)) {
        self.sel_clips = vec![id];
        self.sel_anchor = Some(id);
    }

    /// Ctrl+click: add/remove a clip from the selection.
    fn select_toggle(&mut self, id: (usize, usize, usize)) {
        match self.sel_clips.iter().position(|x| *x == id) {
            Some(i) => {
                self.sel_clips.remove(i);
            }
            None => {
                self.sel_clips.push(id);
                self.sel_clips.sort();
            }
        }
        self.sel_anchor = Some(id);
    }

    /// Shift+click result: replace with the given range (sorted).
    fn select_range(&mut self, ids: Vec<(usize, usize, usize)>) {
        let mut ids = ids;
        ids.sort();
        self.sel_clips = ids;
    }

    /// Remove every selected clip (assets stay in the rack).
    /// v18: every grid gesture (clip drag, edge resize, marquee) is
    /// index-based, so anything that REMOVES clips must disarm them — a
    /// resize/move holding a dead index panicked on the next pointer move
    /// (cut or Delete pressed while an edge drag was still armed).
    fn cancel_grid_gestures(&mut self) {
        self.drag_clip = None;
        self.clip_resize = None;
        self.marquee = None;
        self.marquee_pending = None;
    }

    fn delete_selection(&mut self) {
        if self.sel_clips.is_empty() {
            return;
        }
        self.cancel_grid_gestures();
        let n = self.sel_clips.len();
        let mut ids = self.sel_clips.clone();
        ids.sort();
        for (t, k, c) in ids.into_iter().rev() {
            self.arr.remove_clip(t, k, c);
        }
        self.sel_clips.clear();
        self.sel_anchor = None;
        self.push_project();
        self.status = format!("{n} clip(s) removed (assets stay in the rack)");
    }

    /// S / toolbar: split every selected clip at the playhead. Clips the
    /// playhead does not cross stay untouched; the right halves keep the
    /// selection, so repeating S walks a long clip into pieces.
    fn split_selection(&mut self) {
        if self.sel_clips.is_empty() {
            self.status = "split: select a clip first (S splits at the playhead)".into();
            return;
        }
        let ph = self.backend.engine.pos();
        let mut ids = self.sel_clips.clone();
        ids.sort();
        let mut n = 0usize;
        // Reverse order: inserting at c+1 never disturbs earlier indices.
        for (t, k, c) in ids.into_iter().rev() {
            let Some(cl) = self.arr.tracks[t].lanes[k].clips.get(c) else {
                continue;
            };
            let alen = self.asset_len(cl.asset);
            let Some((l, r)) = cl.split_at(ph, alen) else {
                continue;
            };
            let lane = &mut self.arr.tracks[t].lanes[k];
            lane.clips[c] = l;
            lane.clips.insert(c + 1, r);
            n += 1;
        }
        if n == 0 {
            self.status = "split: the playhead is not inside any selected clip".into();
            return;
        }
        self.sel_anchor = self.sel_clips.first().copied();
        self.push_project();
        self.status = format!("split {n} clip(s) at the playhead");
    }

    /// Ctrl+D (documented): copy + paste the selection at the playhead.
    fn duplicate_selection(&mut self) {
        if self.sel_clips.is_empty() {
            self.status = "duplicate: select clips first".into();
            return;
        }
        self.clipboard_copy();
        self.clipboard_paste();
    }

    /// Ctrl+C: copy the whole selection as a group (track offsets from the
    /// topmost selected track, position offsets from the leftmost clip).
    /// With no clip selected, falls back to the selected rack asset.
    fn clipboard_copy(&mut self) {
        if self.sel_clips.is_empty() {
            if let Some(k) = self.sel_asset {
                if let Some(a) = self.assets.get(k) {
                    self.clipboard = Some(Clipboard {
                        items: vec![ClipboardItem {
                            t_off: 0,
                            asset: k as u32,
                            cut: 0,
                            play_len: None,
                            pos_off: 0,
                            gain: 1.0,
                            fade_in: 0,
                            fade_out: 0,
                        }],
                    });
                    let nm = a.name.clone();
                    self.status =
                        format!("copied \u{201c}{nm}\u{201d} \u{b7} Ctrl+V pastes at the playhead");
                }
            } else {
                self.status = "nothing to copy \u{b7} select a clip first".into();
            }
            return;
        }
        let mut ids = self.sel_clips.clone();
        ids.sort();
        let t0 = ids.iter().map(|x| x.0).min().unwrap_or(0);
        let p0 = ids
            .iter()
            .filter_map(|(t, k, c)| self.arr.tracks[*t].lanes[*k].clips.get(*c))
            .map(|c| c.pos)
            .min()
            .unwrap_or(0);
        let items: Vec<ClipboardItem> = ids
            .iter()
            .filter_map(|(t, k, c)| {
                let cl = self.arr.tracks[*t].lanes[*k].clips.get(*c)?;
                Some(ClipboardItem {
                    t_off: *t - t0,
                    asset: cl.asset,
                    cut: cl.cut,
                    play_len: cl.play_len,
                    pos_off: cl.pos - p0,
                    gain: cl.gain,
                    fade_in: cl.fade_in,
                    fade_out: cl.fade_out,
                })
            })
            .collect();
        let n = items.len();
        self.clipboard = Some(Clipboard { items });
        self.status = format!("copied {n} clip(s) \u{b7} Ctrl+V pastes the group at the playhead");
    }

    /// Ctrl+V: paste the clipboard group at the playhead on the target
    /// track (busy spans open child lanes). The view scrolls to the pasted
    /// clips — the fix for "paste does nothing" when the playhead sits
    /// off-screen.
    fn clipboard_paste(&mut self) {
        let Some(cb) = self.clipboard.clone() else {
            return;
        };
        let t_target = self
            .last_click_track
            .unwrap_or(self.selected)
            .min(self.n_tracks - 1);
        let pos = self.backend.engine.pos();
        let mut sel: Vec<(usize, usize, usize)> = Vec::new();
        let mut base_lanes: HashMap<usize, usize> = HashMap::new();
        // v0.15 bug hunt: asset ids inside the clipboard are kept in sync
        // with the table by remove_asset_at's remap; paste trusts it.
        for it in &cb.items {
            let t = (t_target + it.t_off).min(self.n_tracks - 1);
            let alen = self
                .assets
                .get(it.asset as usize)
                .map(|a| a.samples())
                .unwrap_or(0);
            let span = it.play_len.unwrap_or(alen.saturating_sub(it.cut)).max(1);
            let nl0 = *base_lanes
                .entry(t)
                .or_insert(self.arr.tracks[t].lanes.len());
            let lane = self.arr.place_auto(
                t,
                Clip {
                    asset: it.asset,
                    pos: pos + it.pos_off,
                    cut: it.cut,
                    play_len: it.play_len,
                    gain: it.gain,
                    fade_in: it.fade_in,
                    fade_out: it.fade_out,
                },
                span,
            );
            self.grow_for_lanes(t, nl0);
            let li = self.arr.tracks[t].lanes[lane].clips.len() - 1;
            sel.push((t, lane, li));
        }
        if sel.is_empty() {
            self.status = "paste: nothing to paste (assets were deleted)".into();
            return;
        }
        sel.sort();
        let npaste = sel.len();
        self.sel_clips = sel;
        self.sel_anchor = self.sel_clips.first().copied();
        self.push_project();
        self.scroll_view_to(pos);
        self.status = format!(
            "pasted {} clip(s) at the playhead \u{b7} track {}",
            npaste,
            t_target + 1
        );
    }

    /// Pan the horizontal view so a timeline position is visible.
    fn scroll_view_to(&mut self, pos: u64) {
        let sr = self.sample_rate.max(1) as f64;
        let x = (pos as f64 / sr * self.px_per_sec as f64) as f32;
        let w = self.grid_view_w.max(200.0);
        if x < self.hscroll {
            self.hscroll = (x - 40.0).max(0.0);
        } else if x > self.hscroll + w {
            self.hscroll = (x - w + 40.0).max(0.0);
        }
    }

    /// True when a timeline position falls inside the horizontal view.
    pub fn hscroll_visible(&self, pos: u64) -> bool {
        let sr = self.sample_rate.max(1) as f64;
        let x = (pos as f64 / sr * self.px_per_sec as f64) as f32;
        x >= self.hscroll - 1.0 && x <= self.hscroll + self.grid_view_w + 1.0
    }

    fn send_metro(&mut self) {
        let bpm = self
            .metro_bpm
            .trim()
            .parse::<u32>()
            .unwrap_or(120)
            .clamp(20, 300);
        // v0.11 R5: tempo is automatable (curve wins while it sounds).
        self.note_auto(0, crate::automation::TARGET_MASTER_TEMPO, bpm as f32);
        self.backend.cmd(Cmd::SetMetro {
            on: self.metro_on,
            bpm,
            sub: SUBDIVISIONS[self.metro_sub.min(SUBDIVISIONS.len() - 1)].0,
            num: self
                .metro_num
                .trim()
                .parse::<u32>()
                .unwrap_or(4)
                .clamp(1, 16),
        });
        self.save_config();
    }

    /// Rescan the system folders plus the user's extra folders. Loads every
    /// plugin once (metadata), so it runs on explicit click / first window
    /// open, never per frame. v16: the loading itself runs on the plugin
    /// GUI thread (it dlopens and instantiates EVERY plugin — seconds on
    /// the old UI-thread path, and it also bound plugin toolkit globals to
    /// the wrong thread); completion lands in `poll_vst_opens`.
    fn vst_rescan(&mut self) {
        let mut dirs = crate::vsthost::default_vst_dirs();
        for d in &self.vst_dirs {
            let p = std::path::PathBuf::from(d);
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
        // v0.10 drop-in Windows plugins: if yabridge is installed and any
        // scan folder contains .dll files, wrap them into native .so
        // wrappers first — then the normal scan below picks them up like
        // any other plugin (docs/PLUGINS.md). Fast no-op without .dlls.
        if crate::vsthost::yabridge_available() {
            if let Some(msg) = crate::vsthost::yabridge_sync(&dirs) {
                self.status = msg.clone();
                eprintln!("[slooper] {msg}");
            }
        }
        self.vst_note = format!("scanning {} folder(s)…", dirs.len());
        self.vst_scan_rx = Some(crate::vsthost::scan_async(dirs));
    }

    /// Persist every user-facing setting (bindings, mixer, grid, rack,
    /// ports, effects chains) to disk.
    fn save_config(&self) {
        let cfg = Config {
            n_tracks: self.n_tracks,
            shortcuts: (0..N_ACTIONS)
                .map(|i| self.short_keys[i].map(key_debug))
                .collect(),
            midi: MidiAction::ALL
                .iter()
                .zip(self.midi_binds.iter())
                .filter_map(|(a, b)| {
                    b.map(|bind| MidiBinding {
                        action: *a,
                        bind: Some(bind),
                    })
                })
                .collect(),
            midi_axis: self
                .knob_cal
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    c.map(|mut m| {
                        m.slot = i as u16;
                        m
                    })
                })
                .collect(),
            ports_in: self
                .applied_ports
                .as_ref()
                .map(|p| p.0.clone())
                .unwrap_or_default(),
            ports_out: self
                .applied_ports
                .as_ref()
                .map(|p| p.1.iter().map(|(n, _)| n.clone()).collect())
                .unwrap_or_default(),
            ports_out_lr: self
                .applied_ports
                .as_ref()
                .map(|p| p.1.iter().map(|(_, lr)| *lr).collect())
                .unwrap_or_default(),
            gui_scale: self.gui_scale,
            latency_ms: self
                .latency_ms
                .trim()
                .parse::<f32>()
                .unwrap_or(0.0)
                .clamp(0.0, 1000.0),
            trk_vol: self.trk_vol.to_vec(),
            trk_gain: self.trk_gain.to_vec(),
            trk_pan: self.trk_pan.to_vec(),
            trk_h: self.trk_height.clone(),
            metro_bpm: self.bpm(),
            metro_sub: SUBDIVISIONS[self.metro_sub.min(SUBDIVISIONS.len() - 1)].0,
            metro_num: self.beats(),
            q_sub: SUBDIVISIONS[self.q_sub].0,
            snap_sub: SUBDIVISIONS[self.snap_sub].0,
            auto_quantize: self.auto_quantize,
            loop_tl: self.loop_tl,
            loop_mode: self.loop_mode,
            loop_area_start: self.loop_area.0,
            loop_area_end: self.loop_area.1,
            master_vol: self.master_midi,
            monitor_on: self.monitor_enabled,
            preview_on: self.preview_enabled,
            preview_secs: self.preview_secs_f(),
            preview_track: self.preview_track.map(|t| t as u32),
            snap_on: self.snap_on,
            vol_scale_v2: true,
            project_dir: self.project_dir.clone(),
            rack_folders: self.rack.iter().map(|f| f.path.clone()).collect(),
            fx_dir: self.fx_dir_edit.trim().to_string(),
            vst_dirs: self.vst_dirs.clone(),
            track_vst: Vec::new(),
            track_chain: self
                .fx_rows
                .iter()
                .map(|rows| {
                    rows.iter()
                        .map(|r| match r {
                            ChainRow::Script { stem, params } => ChainSave::Fx {
                                stem: stem.clone(),
                                params: params.clone(),
                            },
                            ChainRow::Vst {
                                path,
                                kind,
                                name,
                                params,
                                entry: _,
                                open_rx: _,
                                state: _,
                            } => ChainSave::Vst {
                                path: path.clone(),
                                kind: kind.tag().to_string(),
                                name: name.clone(),
                                params: params.clone(),
                            },
                        })
                        .collect()
                })
                .collect(),
            track_fx: Vec::new(),
            track_synth: self.track_synth.clone(),
            track_synth_params: self.track_synth_params.clone(),
        };
        if let Err(e) = cfg.save() {
            eprintln!("[slooper] could not save config: {e}");
        }
    }

    // ---- DAW actions -----------------------------------------------------

    /// Primary rack folder (where recordings are written); "" when none.
    fn primary_folder(&self) -> PathBuf {
        PathBuf::from(
            self.rack
                .first()
                .map(|f| f.path.trim())
                .unwrap_or("")
                .to_string(),
        )
    }

    /// (Re)create track `t`'s synth instance from its binding and push it
    /// into the engine. Empty binding = "no synth" (engine plays through
    /// the default piano fallback). Returns success.
    fn rebuild_track_synth(&mut self, t: usize) -> bool {
        let inst = match self.track_synth[t].as_deref().filter(|s| !s.is_empty()) {
            Some(stem) if vst_synth_decode(stem).is_some() => {
                let (kind, path) = vst_synth_decode(stem).unwrap();
                // v20: re-apply a preset the user picked in this synth's
                // own GUI (harvested when that editor last closed).
                match crate::vsthost::open_with_state(
                    std::path::Path::new(path),
                    kind,
                    self.sample_rate.max(1),
                    None,
                    self.track_synth_state[t].as_deref(),
                ) {
                    Ok(load) => Some(std::sync::Arc::new(SynthHandle::Vst(load.entry))),
                    Err(err) => {
                        self.status = format!("synth: {err}");
                        return false;
                    }
                }
            }
            Some(stem) => {
                let desc = match self.synth_registry.desc(stem) {
                    Some(d) => d.clone(),
                    None => {
                        self.status = format!("synth: “{stem}” not loaded (see synths folder)");
                        return false;
                    }
                };
                let vals = synth::seed_values(&self.track_synth_params[t], &desc);
                self.track_synth_params[t] = vals.clone();
                self.synth_registry
                    .create_instance(stem, self.sample_rate.max(1) as f64, &vals)
                    .map(|i| std::sync::Arc::new(SynthHandle::Script(i)))
            }
            None => {
                // v0.10: an UNBOUND track with MIDI clips renders through
                // its OWN default piano instance, never the shared fallback.
                // The shared instance rang with every other track's held
                // notes — the "synth doubles my audio" bleed. Tracks with
                // no MIDI clips keep `None` (zero instances, zero memory).
                if self.track_has_midi(t) {
                    self.synth_registry
                        .create_instance("piano", self.sample_rate.max(1) as f64, &[])
                        .map(|i| std::sync::Arc::new(SynthHandle::Script(i)))
                } else {
                    None
                }
            }
        };
        // A synth swap must close this track's editor synchronously: the
        // engine may drop the old plugin the moment the command lands, and
        // the editor thread must not outlive the AEffect it drives.
        if let Some(mut w) = self.vst_editors.remove(&t) {
            w.close();
        }
        self.backend.cmd(Cmd::SetTrackSynth {
            i: t,
            synth: inst.clone(),
        });
        self.track_synth_inst[t] = inst;
        true
    }

    /// True when any clip on track `t` is a MIDI pattern asset.
    fn track_has_midi(&self, t: usize) -> bool {
        self.arr.tracks[t]
            .lanes
            .iter()
            .flat_map(|l| l.clips.iter())
            .any(|c| matches!(self.assets.get(c.asset as usize), Some(a) if a.pattern.is_some()))
    }

    /// Bind a synth stem (or "" = default piano) to a track; persistent.
    fn set_track_synth_stem(&mut self, t: usize, stem: &str) {
        self.track_synth[t] = if stem.is_empty() {
            None
        } else {
            Some(stem.to_string())
        };
        self.track_synth_params[t].clear();
        self.track_synth_state[t] = None;
        self.rebuild_track_synth(t);
        // v0.10 STANDARD: a plugin instrument opens its own GUI the moment
        // it is bound. v0.11 R3: hosted plugins never fall back to a slider
        // window -- the plugin's own GUI is the only parameter surface.
        let gui = if !stem.is_empty() && vst_synth_decode(stem).is_some() {
            Some(self.auto_open_synth_gui(t))
        } else {
            None
        };
        self.save_config();
        // auto_open_synth_gui records why it failed in the status line; keep
        // that visible alongside the binding message.
        let gui_note = if gui == Some(false) {
            std::mem::take(&mut self.status)
        } else {
            String::new()
        };
        let bind_msg = format!(
            "track {}: synth {}",
            t + 1,
            if stem.is_empty() {
                "default (piano)"
            } else {
                stem
            }
        );
        self.status = if gui_note.is_empty() {
            bind_msg
        } else {
            format!("{bind_msg} \u{2014} {gui_note}")
        };
    }

    /// Recreate every bound synth plus the shared default piano instance
    /// (called once the sample rate is known, and after project loads).
    fn rebuild_all_synths(&mut self) {
        if self.sample_rate == 0 {
            return; // instances are created at Evt::Ready
        }
        let dflt = self
            .synth_registry
            .create_instance("piano", self.sample_rate as f64, &[])
            .map(|i| std::sync::Arc::new(SynthHandle::Script(i)));
        self.backend.cmd(Cmd::SetDefaultSynth(dflt));
        for t in 0..MAX_LOOPS {
            self.rebuild_track_synth(t);
        }
    }

    /// v0.11 R5: capture one control move for an armed write-automation
    /// take. Global (`master.*`) controls record into the first armed
    /// track; track controls into their own track's armed recorder. The
    /// first control moved claims the take; moving a different control
    /// afterwards is refused (one asset = one target).
    fn note_auto(&mut self, i: usize, target: &str, v: f32) {
        let ti = if target.starts_with("master.") {
            self.auto_rec.iter().position(|r| r.armed)
        } else if self.auto_rec.get(i).is_some_and(|r| r.armed) {
            Some(i)
        } else {
            None
        };
        let Some(ti) = ti else { return };
        let r = &mut self.auto_rec[ti];
        match &r.target {
            None => r.target = Some(target.to_string()),
            Some(t) if t == target => {}
            Some(t) => {
                self.status =
                    format!("automation: already recording \u{201c}{t}\u{201d} on track {} \u{2014} stop first", ti + 1);
                return;
            }
        }
        let trel = self.backend.engine.pos().saturating_sub(r.start) as i64;
        // Throttle: skip a sample of an unchanged value (a slider sitting
        // still must not burn a node every frame).
        if let Some(last) = r.nodes.last() {
            if (last.v - v).abs() < 1e-4 && (trel - last.t) < (self.sample_rate.max(1) as i64 / 50)
            {
                return;
            }
        }
        r.nodes.push(crate::automation::Node {
            t: trel,
            v,
            s: crate::automation::Shape::Linear,
        });
    }

    /// Arm / stop-and-bounce write automation on track `i` (mirrors
    /// [`App::toggle_midi_rec`]).
    fn toggle_auto_rec(&mut self, i: usize) {
        if self.sample_rate == 0 {
            self.status = "automation rec: wait for the audio engine".into();
            return;
        }
        if self.auto_rec[i].armed {
            self.commit_auto_rec(i);
        } else {
            for r in self.auto_rec.iter_mut() {
                r.armed = false;
            }
            self.auto_rec[i] = AutoRecState {
                armed: true,
                target: None,
                start: self.backend.engine.pos(),
                nodes: Vec::new(),
            };
            if !self.backend.engine.transport_on() {
                self.backend.cmd(Cmd::PlayAll);
            }
            self.status = format!(
                "track {}: automation recording \u{2014} MOVE the control you want to capture",
                i + 1
            );
        }
    }

    /// Turn an armed take into a saved `.slauto` asset + clip (like the
    /// MIDI take bounce).
    fn commit_auto_rec(&mut self, i: usize) {
        let st = std::mem::take(&mut self.auto_rec[i]);
        if st.target.is_none() || st.nodes.len() < 2 {
            self.status = "automation rec: no control moved (nothing captured)".into();
            return;
        }
        let target = st.target.clone().unwrap_or_default();
        let sr = self.sample_rate.max(1) as i64;
        let last = st.nodes.last().map(|n| n.t).unwrap_or(1);
        let len = last.max(sr / 4);
        let mut auto = crate::automation::Automation {
            target: target.clone(),
            len_samples: len,
            nodes: st.nodes.clone(),
        };
        auto.normalize(0, f32::INFINITY);
        // Unique file name in the primary rack folder.
        let dir = self.primary_folder();
        let base = format!("auto-t{}", i + 1);
        let mut n = 1u32;
        let path = loop {
            let name = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let p = dir.join(format!("{name}.slauto"));
            if !p.exists() {
                break p;
            }
            n += 1;
        };
        match std::fs::write(&path, auto.to_json().unwrap_or_default()) {
            Ok(()) => {}
            Err(e) => {
                self.status = format!("automation rec: {e}");
                return;
            }
        }
        let id = self.assets.len() as u32;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.assets.push(Asset::automation_asset(
            id,
            name.clone(),
            path,
            Arc::new(auto),
            false,
        ));
        let pos = snap_start(st.start, self.snap_unit());
        let nl0 = self.arr.tracks[i].lanes.len();
        let lane = self.arr.place_auto(i, Clip::new(id, pos), len as u64);
        self.grow_for_lanes(i, nl0);
        if let Some(f) = self.rack.first_mut() {
            f.rescan();
        }
        self.push_project();
        self.save_config();
        self.status = format!(
            "track {}: automation take on {} \u{2192} clip \u{201c}{name}\u{201d} (lane {})",
            i + 1,
            crate::automation::describe_target(&target),
            lane + 1
        );
    }

    /// Instant constant-value automation asset placed at the playhead
    /// (the "flat" branch of the right-click menu).
    fn create_flat_auto_asset(&mut self, track: usize, target: &str, v: f32) {
        if self.sample_rate == 0 {
            self.status = "automation: wait for the audio engine".into();
            return;
        }
        let len = (self.sample_rate * 2) as i64;
        let auto = crate::automation::Automation::flat(target, v, len);
        let dir = self.primary_folder();
        let mut n = 1u32;
        let path = loop {
            let p = dir.join(format!("auto-{n}.slauto"));
            if !p.exists() {
                break p;
            }
            n += 1;
        };
        if let Err(e) = std::fs::write(&path, auto.to_json().unwrap_or_default()) {
            self.status = format!("automation: {e}");
            return;
        }
        let id = self.assets.len() as u32;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.assets.push(Asset::automation_asset(
            id,
            name.clone(),
            path,
            Arc::new(auto),
            false,
        ));
        let pos = snap_start(self.backend.engine.pos(), self.snap_unit());
        let nl0 = self.arr.tracks[track].lanes.len();
        let lane = self.arr.place_auto(track, Clip::new(id, pos), len as u64);
        self.grow_for_lanes(track, nl0);
        if let Some(f) = self.rack.first_mut() {
            f.rescan();
        }
        self.push_project();
        self.status = format!(
            "\u{201c}{name}\u{201d} = flat {} at {v:.3} \u{2192} track {} lane {} \u{b7} edit it from the rack or the automation button",
            crate::automation::describe_target(target),
            track + 1,
            lane + 1
        );
    }

    /// v0.15: apply a "Set value" dialog entry to the control it belongs
    /// to. `txt` is USER units (dB for volume/gain, -100..100 for pan, BPM,
    /// 0..100 % for plugin params) — the 1:1 semantics the requirement
    /// demands (typed -6 = -6 dB, NOT a MIDI position).
    fn apply_user_value(&mut self, track: usize, target: &str, txt: &str) -> bool {
        use crate::automation::*;
        let Some(v) = value_user_parse(target, txt) else {
            self.status = format!("set value: \u{201c}{txt}\u{201d} is not a number");
            return false;
        };
        let track = track.min(self.n_tracks - 1);
        match target {
            TARGET_TRACK_VOL => {
                // v came from value_user_parse: dB -> linear. Back to dB
                // for the fader: typed -12 = -12 dB (0 = +0 dB).
                let m = vol_midi(if v <= 0.0 {
                    f32::NEG_INFINITY
                } else {
                    20.0 * v.log10()
                });
                self.set_trk_vol(track, m);
                self.levels_dirty = true;
            }
            TARGET_PSEUDO_GAIN => {
                // v is linear (parse mapped dB -> linear); back to dB.
                let db = if v <= 0.0 { -80.0 } else { 20.0 * v.log10() };
                let m = (GAIN_UNITY_MIDI as f32 + db / 0.215)
                    .round()
                    .clamp(0.0, 127.0) as u8;
                self.set_trk_gain(track, m);
                self.levels_dirty = true;
            }
            TARGET_TRACK_PAN => {
                self.set_trk_pan(track, (v * 127.0).round() as u8);
                self.levels_dirty = true;
            }
            TARGET_TRACK_MUTE => {
                let want_on = v < 0.5;
                if self.backend.engine.track_on(track) != want_on {
                    self.backend.cmd(Cmd::ToggleTrackOn { i: track });
                }
            }
            TARGET_MASTER_VOL => {
                self.master_midi = vol_midi(if v <= 0.0 {
                    f32::NEG_INFINITY
                } else {
                    20.0 * v.log10()
                });
                self.levels_dirty = true;
            }
            TARGET_MASTER_TEMPO => {
                self.metro_bpm = format!("{:.0}", v.clamp(20.0, 300.0));
                self.send_metro();
                self.save_config();
            }
            t if t.starts_with("synth:") => {
                let k: usize = t["synth:".len()..].parse().unwrap_or(0);
                let Some(Some(h)) = self.track_synth_inst.get(track).cloned() else {
                    self.status = "set value: this track has no instrument".into();
                    return false;
                };
                match h.as_ref() {
                    SynthHandle::Script(i) => i.set_param(k, v),
                    SynthHandle::Vst(e) => e.set_param(k, v),
                }
                self.track_synth_params[track] = match h.as_ref() {
                    SynthHandle::Script(i) => i.values(),
                    SynthHandle::Vst(_) => self.track_synth_params[track].clone(),
                };
                self.save_config();
            }
            t if t.starts_with("fx:") => {
                let rest = &t["fx:".len()..];
                let Some((r, i)) = rest.split_once(':') else {
                    return false;
                };
                let (Ok(r), Ok(i)) = (r.parse::<usize>(), i.parse::<usize>()) else {
                    return false;
                };
                let applied = if let Some(ChainRow::Script { params, stem, .. }) =
                    self.fx_rows[track].get_mut(r)
                {
                    if let Some(p) = params.get_mut(i) {
                        *p = v;
                        if let Some(Some(inst)) = self.fx_live[track].get(r).cloned() {
                            inst.set_param(i, v);
                        }
                        let _ = stem;
                        self.fx_save_pending = true;
                        true
                    } else {
                        false
                    }
                } else if let Some(ChainRow::Vst {
                    entry: Some(e),
                    params,
                    ..
                }) = self.fx_rows[track].get_mut(r)
                {
                    e.set_param(i, v);
                    if let Some(p) = params.get_mut(i) {
                        *p = v;
                    }
                    self.fx_save_pending = true;
                    true
                } else {
                    false
                };
                if !applied {
                    self.status = "set value: chain slot not found".into();
                    return false;
                }
            }
            other => {
                self.status = format!("set value: unknown target {other}");
                return false;
            }
        }
        let name = if target == TARGET_PSEUDO_GAIN {
            "track gain".to_string()
        } else {
            describe_target(target)
        };
        self.status = format!("set {name} = {txt} {}", value_unit_label(target));
        true
    }

    /// Open the automation manager editing asset `k` (None = fresh curve).
    fn open_auto_editor(&mut self, k: Option<usize>) {
        let (name, target, len, nodes) = match k
            .and_then(|k| self.assets.get(k))
            .map(|a| a.automation.clone())
        {
            Some(Some(a)) => (
                self.assets[k.unwrap()].name.clone(),
                a.target.clone(),
                a.len_samples,
                a.nodes.clone(),
            ),
            _ => {
                // v0.15: a FRESH curve spans the SELECTED clips when any
                // are selected, else the LONGEST track in the arrangement
                // (the "same amount of measures" requirement).
                let end_of = |c: &Clip| c.pos as i64 + c.play_len.unwrap_or(0) as i64;
                let sel_end = self
                    .sel_clips
                    .iter()
                    .filter_map(|(t, k2, c)| self.arr.tracks.get(*t)?.lanes.get(*k2)?.clips.get(*c))
                    .map(end_of)
                    .max()
                    .unwrap_or(0);
                let longest = self
                    .arr
                    .tracks
                    .iter()
                    .flat_map(|t| t.lanes.iter())
                    .flat_map(|l| l.clips.iter())
                    .map(end_of)
                    .max()
                    .unwrap_or(0);
                let len = if sel_end > 0 {
                    sel_end
                } else {
                    longest.max((self.sample_rate.max(1) * 2) as i64)
                };
                (
                    "automation".into(),
                    crate::automation::TARGET_TRACK_VOL.to_string(),
                    len,
                    vec![
                        crate::automation::Node {
                            t: 0,
                            v: 1.0,
                            s: crate::automation::Shape::Linear,
                        },
                        crate::automation::Node {
                            t: len,
                            v: 1.0,
                            s: crate::automation::Shape::Linear,
                        },
                    ],
                )
            }
        };
        self.auto_edit = Some(Box::new(AutoEdit {
            asset: k,
            name,
            target,
            len_samples: len.max(1),
            nodes,
            sel: 0,
            drag: None,
            changed: false,
            rec: false,
            rec_start: 0,
        }));
    }

    /// v0.11 R5: the automation manager — the curve editor for automation
    /// assets (opened from the rack, a clip, or the toolbar button). Nodes
    /// are created by clicking, moved by dragging, deleted by right-click;
    /// the node editor shows ABSOLUTE values (seconds + the target's own
    /// units), never percentages, and every axis has numeric entry.
    /// v0.15: the "select value" popup — a rendering of the selected
    /// track, its mixer values and the header values, where EVERY
    /// automatable value carries a translucent overlay button. Clicking
    /// one binds the open automation curve to that value; the curve's
    /// min/max rescale to the new output's limits automatically.
    fn auto_pick_window(&mut self, ui: &mut egui::Ui) {
        if !self.auto_pick_open || self.auto_edit.is_none() {
            return;
        }
        let track = self.selected.min(self.n_tracks.saturating_sub(1));
        let targets = auto_targets(self, track);
        let mut pick: Option<(String, String)> = None;
        let mut open = true;
        egui::Window::new("Select value to automate")
            .id(egui::Id::new("auto_pick"))
            .default_pos([520.0, 160.0])
            .default_width(360.0)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.label(
                    RichText::new(format!(
                        "track {} \u{b7} bpm {} \u{b7} master {} \u{b7} metro {} \u{b7} click the \
                         overlay on the value this curve should drive",
                        track + 1,
                        self.metro_bpm,
                        db_num(vol_amp(self.master_midi)),
                        if self.metro_on { "on" } else { "off" },
                    ))
                    .weak()
                    .small(),
                );
                ui.separator();
                for (id, label) in &targets {
                    let frac = target_preview_frac(self, track, id);
                    let full = ui.available_width();
                    let (bar, _) =
                        ui.allocate_at_least(egui::vec2(full, 20.0), egui::Sense::hover());
                    let p = ui.painter();
                    p.rect_filled(bar, 2.0, Color32::from_rgb(24, 26, 32));
                    p.rect_filled(
                        egui::Rect::from_min_size(
                            bar.min,
                            egui::vec2(bar.width() * frac.clamp(0.0, 1.0), bar.height()),
                        ),
                        2.0,
                        Color32::from_rgb(96, 104, 132),
                    );
                    p.text(
                        bar.min + egui::vec2(5.0, bar.height() / 2.0),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(11.0),
                        Color32::from_rgb(226, 226, 232),
                    );
                    // Translucent overlay ADDED LAST: it sits on top and
                    // takes the click — the "semi-transparent button".
                    let btn = ui.put(
                        bar,
                        egui::Button::new(RichText::new("").small())
                            .fill(Color32::from_rgba_premultiplied(255, 120, 120, 24))
                            .min_size(egui::vec2(full, 20.0)),
                    );
                    if btn
                        .on_hover_text(format!("bind this curve to {label}"))
                        .clicked()
                    {
                        pick = Some((id.clone(), label.clone()));
                    }
                }
            });
        if let Some((id, label)) = pick {
            if let Some(ed) = self.auto_edit.as_mut() {
                // min/max follow the selected output: rescale the shape.
                let (omin, omax) = crate::automation::target_range(&ed.target);
                let (nmin, nmax) = crate::automation::target_range(&id);
                for n in ed.nodes.iter_mut() {
                    let f = ((n.v - omin) / (omax - omin).max(1e-9)).clamp(0.0, 1.0);
                    n.v = nmin + f * (nmax - nmin);
                }
                ed.target = id;
                ed.changed = true;
            }
            self.auto_pick_open = false;
            self.status = format!("automation now drives {label}");
        }
        if !open {
            self.auto_pick_open = false;
        }
    }

    fn auto_window(&mut self, ui: &mut egui::Ui) {
        use crate::automation::{Automation, Node, Shape};
        let Some(mut ed) = self.auto_edit.take() else {
            return;
        };
        let sr = self.sample_rate.max(1) as f64;
        let track = self.selected;
        let targets = auto_targets(self, track);
        let mut action: u8 = 0; // 1=save new, 2=overwrite, 3=cancel

        let mut open = true;
        egui::Window::new("Automation")
            .default_pos([190.0, 320.0])
            .default_width(620.0)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                let (rmin, rmax) = crate::automation::target_range(&ed.target);
                let len = ed.len_samples.max(1);
                ui.horizontal(|ui| {
                    ui.label("target");
                    let cur = targets
                        .iter()
                        .find(|(id, _)| id == &ed.target)
                        .map(|(_, l)| l.clone())
                        .unwrap_or_else(|| ed.target.clone());
                    egui::ComboBox::from_id_salt("auto_target")
                        .selected_text(cur)
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            for (id, label) in &targets {
                                // v0.15: switching outputs RESCALES the
                                // curve — the asset's min/max follow the
                                // selected output's limits automatically.
                                let (omin, omax) = crate::automation::target_range(&ed.target);
                                if ui.selectable_value(&mut ed.target, id.clone(), label).changed()
                                {
                                    let (nmin, nmax) =
                                        crate::automation::target_range(&ed.target);
                                    for n in ed.nodes.iter_mut() {
                                        let f = ((n.v - omin) / (omax - omin).max(1e-9))
                                            .clamp(0.0, 1.0);
                                        n.v = nmin + f * (nmax - nmin);
                                    }
                                    ed.changed = true;
                                }
                            }
                        });
                    ui.label("length (s)");
                    let mut secs = ed.len_samples as f64 / sr;
                    if ui
                        .add(
                            egui::DragValue::new(&mut secs)
                                .speed(0.05)
                                .range(0.05..=600.0),
                        )
                        .changed()
                    {
                        ed.len_samples = ((secs * sr) as i64).max((sr / 10.0) as i64);
                        ed.changed = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("name");
                    if ui
                        .add(egui::TextEdit::singleline(&mut ed.name).desired_width(160.0))
                        .changed()
                    {
                        ed.changed = true;
                    }
                    ui.label(
                        RichText::new("click = add node \u{b7} drag = move \u{b7} right-click = delete")
                            .weak()
                            .italics(),
                    );
                });

                // ---- v0.15: MIDI record sources -------------------------
                // Pick any bindings configured in Settings -> MIDI
                // Bindings. While this manager is open the picked bindings
                // are MUTED in the main window (they never fire their
                // action); with REC armed their raw moves draw the curve
                // as a percentage of the target's range (0 = min,
                // 127 = max: 0 -> -inf, 127 -> +6 dB on volume).
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("record:").weak());
                    let mut any = false;
                    for (i, bd) in self.midi_binds.iter().enumerate() {
                        let Some(b) = bd else { continue };
                        any = true;
                        let mut on = self.auto_mgr_rec.iter().any(|(s, _)| *s == i);
                        let tb = ui
                            .toggle_value(
                                &mut on,
                                format!("{} \u{2190} {}", MidiAction::ALL[i].label(), b.label()),
                            )
                            .on_hover_text(if on {
                                "MUTED in the main window while the automation manager is open \u{2014} \
                                 its moves draw the curve instead"
                            } else {
                                "record this binding's moves into the curve"
                            });
                        if tb.changed() {
                            if on {
                                if !self.auto_mgr_rec.iter().any(|(s, _)| *s == i) {
                                    self.auto_mgr_rec.push((i, *b));
                                }
                            } else {
                                self.auto_mgr_rec.retain(|(s, _)| *s != i);
                            }
                        }
                    }
                    if !any {
                        ui.label(
                            RichText::new(
                                "no MIDI bindings yet \u{2014} set them in Settings",
                            )
                            .weak()
                            .italics(),
                        );
                    }
                });
                ui.horizontal(|ui| {
                    let recbtn = ui.button(if ed.rec {
                        RichText::new("\u{25a0} stop").strong()
                    } else {
                        RichText::new("\u{25cf} record").strong()
                    });
                    if recbtn.on_hover_text(
                        "arm the manager recorder: raw knob moves from the picked bindings \
                         become curve nodes (percent of the target's min/max range)",
                    ).clicked()
                    {
                        if ed.rec {
                            ed.rec = false;
                        } else {
                            ed.rec = true;
                            ed.rec_start = self.backend.engine.pos() as i64;
                            // Recording APPENDS (never clears): nothing the
                            // user drew can be destroyed by arming a take.
                            ed.changed = true;
                            if !self.backend.engine.transport_on() {
                                self.backend.cmd(Cmd::PlayAll);
                            }
                        }
                    }
                    if ed.rec {
                        ui.label(
                            RichText::new("recording \u{2014} move a picked binding")
                                .weak()
                                .italics(),
                        );
                    }
                    if ui
                        .button("Select value\u{2026}")
                        .on_hover_text(
                            "choose which value this curve drives: a rendering of the \
                             track, its mixer, and the header values, each with a bind \
                             overlay",
                        )
                        .clicked()
                    {
                        self.auto_pick_open = true;
                    }
                });

                // ---- the curve canvas ------------------------------------
                let (rect, resp) =
                    ui.allocate_at_least(egui::vec2(596.0, 240.0), egui::Sense::click_and_drag());
                let x_of = |t: i64| rect.min.x + (t as f64 / len as f64) as f32 * rect.width();
                let y_of = |v: f32| {
                    rect.max.y
                        - ((v - rmin) / (rmax - rmin).max(1e-9)).clamp(0.0, 1.0) * rect.height()
                };
                let t_of = |x: f32| {
                    (((x - rect.min.x) / rect.width()) as f64 * len as f64)
                        .round()
                        .clamp(0.0, len as f64) as i64
                };
                let v_of = |y: f32| {
                    let f = ((rect.max.y - y) / rect.height()) as f64;
                    (rmin as f64 + f.clamp(0.0, 1.0) * (rmax - rmin).max(1e-9) as f64) as f32
                };
                ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(16, 18, 24));
                ui.painter().rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(1.0, Color32::from_gray(60)),
                    egui::StrokeKind::Inside,
                );
                // grid with ABSOLUTE value labels (the FL difference)
                for gi in 1..8 {
                    let gx = rect.min.x + rect.width() * gi as f32 / 8.0;
                    ui.painter().line_segment(
                        [egui::pos2(gx, rect.min.y), egui::pos2(gx, rect.max.y)],
                        egui::Stroke::new(1.0, Color32::from_gray(35)),
                    );
                    let gt = len as f64 * gi as f64 / 8.0;
                    ui.painter().text(
                        egui::pos2(gx + 2.0, rect.max.y - 8.0),
                        egui::Align2::LEFT_CENTER,
                        format!("{:.2}s", gt / sr),
                        egui::FontId::proportional(9.0),
                        Color32::from_gray(110),
                    );
                }
                for gi in 1..4 {
                    let gv = rmin + (rmax - rmin) * gi as f32 / 4.0;
                    let gy = y_of(gv);
                    ui.painter().line_segment(
                        [egui::pos2(rect.min.x, gy), egui::pos2(rect.max.x, gy)],
                        egui::Stroke::new(1.0, Color32::from_gray(35)),
                    );
                    ui.painter().text(
                        egui::pos2(rect.min.x + 2.0, gy - 6.0),
                        egui::Align2::LEFT_CENTER,
                        format_gv(gv),
                        egui::FontId::proportional(9.0),
                        Color32::from_gray(110),
                    );
                }
                // the curve
                let sample = Automation {
                    target: ed.target.clone(),
                    len_samples: len,
                    nodes: ed.nodes.clone(),
                };
                let mut pts: Vec<egui::Pos2> = Vec::new();
                let steps = (rect.width() / 2.0).max(2.0) as i64;
                for i in 0..=steps {
                    let t = (len as f64 * i as f64 / steps as f64) as i64;
                    pts.push(egui::pos2(x_of(t), y_of(sample.eval(t))));
                }
                if pts.len() > 1 {
                    // translucent fill under the curve (area = "how much
                    // of the range the curve holds")
                    let mut area = pts.clone();
                    area.push(egui::pos2(pts.last().unwrap().x, rect.max.y));
                    area.push(egui::pos2(pts[0].x, rect.max.y));
                    ui.painter().add(egui::Shape::convex_polygon(
                        area,
                        Color32::from_rgba_premultiplied(255, 120, 120, 36),
                        egui::Stroke::NONE,
                    ));
                    for w in pts.windows(2) {
                        ui.painter().line_segment(
                            [w[0], w[1]],
                            egui::Stroke::new(1.6, Color32::from_rgb(255, 120, 120)),
                        );
                    }
                }
                // nodes
                for (idx, n) in ed.nodes.iter().enumerate() {
                    let c = egui::pos2(x_of(n.t), y_of(n.v));
                    let sel = idx == ed.sel;
                    ui.painter().circle_filled(
                        c,
                        if sel { 5.0 } else { 3.5 },
                        if sel { Color32::from_rgb(255, 210, 120) } else { Color32::from_rgb(255, 120, 120) },
                    );
                    if sel {
                        ui.painter().circle(c, 8.0, Color32::TRANSPARENT, egui::Stroke::new(1.0, Color32::from_rgb(255, 210, 120)));
                        ui.painter().text(
                            egui::pos2(c.x + 10.0, c.y - 10.0),
                            egui::Align2::LEFT_BOTTOM,
                            format!("{:.3} @ {:.2}s", n.v, n.t as f64 / sr),
                            egui::FontId::proportional(10.0),
                            Color32::from_rgb(255, 220, 150),
                        );
                    }
                }
                // gestures
                if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.clicked()) {
                    let hit = nearest_auto_node(&ed.nodes, pos, x_of, y_of);
                    match hit {
                        Some(idx) => {
                            ed.sel = idx;
                            ed.drag = Some(idx);
                        }
                        None => {
                            let (t, v) = (t_of(pos.x), v_of(pos.y));
                            let at = ed
                                .nodes
                                .iter()
                                .position(|n| n.t > t)
                                .unwrap_or(ed.nodes.len());
                            ed.nodes.insert(
                                at,
                                Node { t, v, s: Shape::Linear },
                            );
                            ed.sel = at;
                            ed.drag = Some(at);
                            ed.changed = true;
                        }
                    }
                }
                // v0.15: arm the drag on PRESS-and-move. clicked() only
                // fires on release-without-move, so a press straight into
                // a drag used to move nothing at all.
                // v0.15: arm the drag at PRESS time (directly on a node).
                // clicked() only fires on release-without-move, so the old
                // "arm on click" code never let a press flow into a drag.
                let just_pressed =
                    resp.hovered() && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary));
                if just_pressed {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        if let Some(idx) = nearest_auto_node(&ed.nodes, pos, x_of, y_of) {
                            ed.sel = idx;
                            ed.drag = Some(idx);
                        }
                    }
                }
                if resp.drag_started() {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        if let Some(idx) = nearest_auto_node(&ed.nodes, pos, x_of, y_of) {
                            ed.sel = idx;
                            ed.drag = Some(idx);
                        }
                    }
                }
                if resp.dragged() {
                    if let Some(idx) = ed.drag {
                        let lo = if idx > 0 { ed.nodes.get(idx - 1).map(|q| q.t + 1) } else { None };
                        let hi = ed.nodes.get(idx + 1).map(|q| q.t - 1);
                        if let Some(n) = ed.nodes.get_mut(idx) {
                            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                                let mut t = t_of(pos.x);
                                if let Some(l) = lo {
                                    t = t.max(l);
                                }
                                if let Some(h) = hi {
                                    t = t.min(h);
                                }
                                n.t = t.clamp(0, len);
                                n.v = v_of(pos.y).clamp(rmin.min(n.v), rmax.max(n.v));
                                ed.changed = true;
                            }
                        }
                    }
                }
                if resp.secondary_clicked() {
                    let pos = ui.input(|i| i.pointer.interact_pos()).unwrap();
                    if let Some(idx) = nearest_auto_node(&ed.nodes, pos, x_of, y_of) {
                        if ed.nodes.len() > 1 {
                            ed.nodes.remove(idx);
                            ed.sel = ed.sel.min(ed.nodes.len().saturating_sub(1));
                            ed.drag = None;
                            ed.changed = true;
                        }
                    }
                }
                if !ui.input(|i| i.pointer.any_down()) {
                    ed.drag = None;
                }
                resp.on_hover_text("automation curve: click empty space to add a node, drag nodes to move them, right-click a node to delete it");

                // ---- node editor ------------------------------------------
                let mut resort: Option<i64> = None;
                ui.horizontal(|ui| {
                    if let Some(n) = ed.nodes.get_mut(ed.sel) {
                        let mut ts = n.t as f64 / sr;
                        ui.label(format!("node {}", ed.sel + 1));
                        if ui
                            .add(
                                egui::DragValue::new(&mut ts)
                                    .speed(0.01)
                                    .suffix(" s")
                                    .range(0.0..=len as f64 / sr),
                            )
                            .changed()
                        {
                            n.t = (ts * sr).round().clamp(0.0, len as f64) as i64;
                            resort = Some(n.t);
                            ed.changed = true;
                        }
                        let mut vv = n.v;
                        if ui
                            .add(egui::DragValue::new(&mut vv).speed(0.002).range(rmin..=rmax))
                            .changed()
                        {
                            n.v = vv;
                            ed.changed = true;
                        }
                        egui::ComboBox::from_id_salt("auto_shape")
                            .selected_text(n.s.label())
                            .show_ui(ui, |ui| {
                                for sh in Shape::ALL {
                                    if ui.selectable_value(&mut n.s, sh, sh.label()).changed() {
                                        ed.changed = true;
                                    }
                                }
                            });
                        if ui.button("delete").clicked() && ed.nodes.len() > 1 {
                            ed.nodes.remove(ed.sel);
                            ed.sel = ed.sel.saturating_sub(1);
                            ed.changed = true;
                        }
                    } else {
                        ui.label("no node selected \u{2014} click the curve to add one");
                    }
                });
                if let Some(tt) = resort {
                    ed.nodes.sort_by_key(|n| n.t);
                    ed.sel = ed.nodes.iter().position(|n| n.t == tt).unwrap_or(ed.sel);
                }
                ui.horizontal(|ui| {
                    if ui.button("Save as new asset + clip").clicked() {
                        action = 1;
                    }
                    if ed.asset.is_some() && ui.button("Save (overwrite asset)").clicked() {
                        action = 2;
                    }
                    if ui.button("Close").clicked() {
                        action = 3;
                    }
                    if ed.changed {
                        ui.label(RichText::new("unsaved").weak().italics());
                    }
                });
            });
        if action == 1 || action == 2 {
            self.auto_commit(&mut ed, track, action == 2);
        }
        // Save keeps the editor open on the (now-saved) buffer; Close or
        // the window X discards unsaved edits like the piano roll.
        self.auto_edit = if action == 3 || !open { None } else { Some(ed) };
    }

    /// Persist the automation manager buffer: overwrite the source asset
    /// in place, or save a new `.slauto` + clip on `track` at the playhead.
    fn auto_commit(&mut self, ed: &mut AutoEdit, track: usize, overwrite: bool) {
        use crate::automation::Automation;
        let mut auto = Automation {
            target: ed.target.clone(),
            len_samples: ed.len_samples.max(1),
            nodes: ed.nodes.clone(),
        };
        auto.normalize(0, f32::INFINITY);
        let json = match auto.to_json() {
            Ok(j) => j,
            Err(_) => {
                self.status = "automation: serialize failed".into();
                return;
            }
        };
        if overwrite {
            let mut saved_name: Option<String> = None;
            if let Some(k) = ed.asset {
                if let Some(a) = self.assets.get_mut(k) {
                    a.automation = Some(std::sync::Arc::new(auto.clone()));
                    if std::fs::write(&a.path, &json).is_ok() {
                        a.dirty = false;
                        saved_name = Some(a.name.clone());
                    }
                }
            }
            match saved_name {
                Some(nm) => {
                    ed.changed = false;
                    self.push_project();
                    self.status = format!("automation \u{201c}{nm}\u{201d} saved");
                }
                // File vanished (renamed rack?): save out a fresh one.
                None => {
                    self.create_auto_asset_named(&auto, &json, &ed.name, track);
                    ed.changed = false;
                }
            }
            return;
        }
        self.create_auto_asset_named(&auto, &json, &ed.name, track);
        ed.changed = false;
    }

    fn create_auto_asset_named(
        &mut self,
        auto: &crate::automation::Automation,
        json: &str,
        name: &str,
        track: usize,
    ) {
        let dir = self.primary_folder();
        let base: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let base = if base.trim_matches('-').is_empty() {
            "automation".to_string()
        } else {
            base.trim_matches('-').to_string()
        };
        let mut n = 1u32;
        let path = loop {
            let fname = if n == 1 {
                format!("{base}.slauto")
            } else {
                format!("{base}-{n}.slauto")
            };
            let p = dir.join(fname);
            if !p.exists() {
                break p;
            }
            n += 1;
        };
        if let Err(e) = std::fs::write(&path, json) {
            self.status = format!("automation: {e}");
            return;
        }
        let id = self.assets.len() as u32;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.assets.push(Asset::automation_asset(
            id,
            name.clone(),
            path,
            std::sync::Arc::new(auto.clone()),
            false,
        ));
        let pos = snap_start(self.backend.engine.pos(), self.snap_unit());
        let nl0 = self.arr.tracks[track].lanes.len();
        let lane = self
            .arr
            .place_auto(track, Clip::new(id, pos), auto.len_samples as u64);
        self.grow_for_lanes(track, nl0);
        if let Some(f) = self.rack.first_mut() {
            f.rescan();
        }
        self.push_project();
        self.status = format!(
            "automation \u{201c}{name}\u{201d} \u{2192} track {} lane {} \u{b7} {} on the clip edits it",
            track + 1,
            lane + 1,
            "\u{270e}"
        );
    }

    /// Arm/disarm MIDI pattern recording on a track. Arming starts the
    /// transport (the pattern clock is the transport); disarming bounces
    /// what was captured into an asset + clip, like an audio take.
    fn toggle_midi_rec(&mut self, i: usize) {
        if self.sample_rate == 0 {
            self.status = "MIDI rec: wait for the audio engine".into();
            return;
        }
        let armed = &mut self.midi_rec[i];
        if armed.armed {
            self.commit_midi_rec(i);
        } else {
            *armed = MidiRecState {
                armed: true,
                start: self.backend.engine.pos(),
                evs: Vec::new(),
            };
            if !self.backend.engine.transport_on() {
                self.backend.cmd(Cmd::PlayAll);
            }
            self.status = format!("track {}: MIDI recording…", i + 1);
        }
    }

    /// True when some binding (keyboard slot or MIDI bind) would claim
    /// `msg` — bound messages steer the app and are never recorded.
    fn midi_msg_is_bound(&self, msg: &MidiMsg) -> bool {
        let Some(bind) = MidiBind::from_msg(msg) else {
            return false;
        };
        for (i, b) in self.midi_binds.iter().enumerate() {
            let Some(b) = b else { continue };
            let hit = if MidiAction::ALL[i].is_continuous() {
                b.same_axis(&bind)
            } else {
                *b == bind
            };
            if hit {
                return true;
            }
        }
        false
    }

    /// Push one live message into every armed track's buffer (stamped with
    /// the engine position so the take lands exactly where it was played).
    fn capture_midi_msg(&mut self, msg: &MidiMsg) {
        let at = self.backend.engine.pos();
        let mut evs: Vec<(u64, u8, u8, u8, u8)> = Vec::with_capacity(1);
        match *msg {
            MidiMsg::NoteOn {
                channel,
                note,
                velocity,
            } if velocity > 0 => {
                evs.push((at, 3, channel, note, velocity));
            }
            MidiMsg::NoteOn { channel, note, .. } | MidiMsg::NoteOff { channel, note, .. } => {
                evs.push((at, 0, channel, note, 0));
            }
            MidiMsg::ControlChange {
                channel,
                number,
                value,
            } => {
                evs.push((at, 1, channel, number, value));
            }
            MidiMsg::ChannelPressure { channel, pressure } => {
                evs.push((at, 2, channel, 0, pressure));
            }
            MidiMsg::PolyPressure {
                channel,
                note,
                pressure,
            } => {
                evs.push((at, 4, channel, note, pressure));
            }
            _ => return, // PC / pitch bend / sysex: not part of the pattern model
        }
        for r in self.midi_rec.iter_mut() {
            if r.armed {
                r.evs.extend(evs.iter().copied());
            }
        }
    }

    /// Bounce an armed track: build the pattern, save it to the primary
    /// rack folder, add the asset and place a clip at the take position.
    fn commit_midi_rec(&mut self, t: usize) {
        let state = std::mem::take(&mut self.midi_rec[t]);
        if state.evs.is_empty() {
            self.status = format!("track {}: no MIDI captured", t + 1);
            return;
        }
        let start = state.start;
        let mut pat = Pattern::new(self.bpm().clamp(20, 999) as u16);
        let mut open: std::collections::HashMap<(u8, u8), (u64, u8, u8)> =
            std::collections::HashMap::new(); // (ch,note) -> (start, vel, press)
        let mut end = 0u64;
        let tick = |pat: &Pattern, at: u64| -> u32 {
            pat.samples_to_ticks(at.saturating_sub(start), self.sample_rate.max(1))
        };
        for (at, kind, ch, d1, d2) in state.evs.iter().copied() {
            end = end.max(at);
            match kind {
                3 => {
                    open.insert((ch, d1), (at, d2, 0));
                }
                0 => {
                    if let Some((s, vel, press)) = open.remove(&(ch, d1)) {
                        let st = tick(&pat, s);
                        let len = tick(&pat, at).saturating_sub(st).max(1);
                        pat.notes.push(crate::midipat::Note {
                            ch,
                            note: d1,
                            vel,
                            press,
                            start: st,
                            len,
                        });
                    }
                }
                1 => pat.ccs.push(crate::midipat::Cc {
                    at: tick(&pat, at),
                    ch,
                    num: d1,
                    val: d2,
                }),
                2 => pat.press.push(crate::midipat::Press {
                    at: tick(&pat, at),
                    ch,
                    val: d2,
                }),
                // Poly pressure: attach to the note that is still open.
                _ => {
                    if let Some(e) = open.get_mut(&(ch, d1)) {
                        e.2 = d2;
                    }
                }
            }
        }
        // Notes still held at stop ring to the end of the take.
        let end_tick = tick(&pat, end);
        for ((ch, note), (s, vel, press)) in open {
            let st = tick(&pat, s);
            pat.notes.push(crate::midipat::Note {
                ch,
                note,
                vel,
                press,
                start: st,
                len: end_tick.saturating_sub(st).max(1),
            });
        }
        pat.sort();
        // Pad the length to whole bars (min one bar).
        let bar = 4 * PPQ as u32;
        let used = pat
            .notes
            .iter()
            .map(|n| n.start + n.len)
            .chain(pat.ccs.iter().map(|m| m.at))
            .chain(pat.press.iter().map(|m| m.at))
            .max()
            .unwrap_or(1);
        pat.len_ticks = used.div_ceil(bar).max(1) * bar;
        if pat.is_empty() {
            self.status = format!("track {}: no MIDI captured", t + 1);
            return;
        }
        // Unique name in the primary folder ("take", "take-2", …).
        let dir = self.primary_folder();
        let base = format!("midi-t{}", t + 1);
        let mut n = 1u32;
        let path = loop {
            let name = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let p = dir.join(format!("{name}.mid"));
            if !p.exists() {
                break p;
            }
            n += 1;
        };
        if let Err(e) = pat.save_mid(&path) {
            self.status = format!("MIDI rec: {e}");
            return;
        }
        let id = self.assets.len() as u32;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let pat = Arc::new(pat);
        let len = pat.len_samples(self.sample_rate.max(1));
        self.assets
            .push(Asset::midi(id, name.clone(), path, pat, false));
        let pos = snap_start(start, self.snap_unit());
        let nl0 = self.arr.tracks[t].lanes.len();
        let lane = self.arr.place_auto(t, Clip::new(id, pos), len);
        self.grow_for_lanes(t, nl0);
        // Re-scan so the rack shows the new file instantly.
        if let Some(f) = self.rack.first_mut() {
            f.rescan();
        }
        self.push_project();
        self.save_config();
        self.status = format!(
            "track {}: MIDI take → clip in lane {} (“{name}”)",
            t + 1,
            lane + 1
        );
    }

    /// The default script synth instance used when no track is picked for
    /// auditioning: the bundled piano, then any stem.
    fn preview_synth_instance(&self) -> Option<std::sync::Arc<crate::synth::Instance>> {
        let reg = &self.synth_registry;
        let sr = self.sample_rate.max(1) as f64;
        reg.create_instance("piano", sr, &[]).or_else(|| {
            let alt = reg.stems().into_iter().next()?;
            reg.create_instance(&alt, sr, &[])
        })
    }

    /// Stage a whole pattern as `(sample, kind, ch, d1, d2)` events for the
    /// audition renderer (kind 3=note-on, 0=note-off, 1=CC, 2=pressure).
    fn pattern_events(&self, pat: &Pattern) -> Vec<(u64, u8, u8, u8, u8)> {
        let sr = self.sample_rate.max(1);
        let mut ev = Vec::with_capacity(pat.notes.len() * 2 + pat.ccs.len() + pat.press.len());
        for note in pat.notes.iter() {
            let s0 = pat.ticks_to_samples(note.start, sr);
            ev.push((s0, 3, note.ch, note.note, note.vel));
            ev.push((
                s0 + pat.ticks_to_samples(note.len, sr).max(1),
                0,
                note.ch,
                note.note,
                0,
            ));
        }
        for m in pat.ccs.iter() {
            ev.push((pat.ticks_to_samples(m.at, sr), 1, m.ch, m.num, m.val));
        }
        for m in pat.press.iter() {
            ev.push((pat.ticks_to_samples(m.at, sr), 2, m.ch, 0, m.val));
        }
        ev
    }

    /// Render staged events through the CURRENT audition target: when a
    /// piano-roll track is picked, that track's instrument AND its whole
    /// mixer chain (`bounce_track_preview`); otherwise the default script
    /// synth. This is what makes "select a track to preview its synth +
    /// effects" audible in the roll and every rack/key audition.
    fn audition_render(&mut self, ev: &[(u64, u8, u8, u8, u8)], secs: f32) -> Option<Arc<[f32]>> {
        let sr = self.sample_rate.max(1);
        let n = ((secs.max(0.05)) as usize) * sr as usize;
        if let Some(t) = self.preview_track {
            let mut ev = ev.to_vec();
            // None here only happens with no engine fallback voice yet
            // (startup race) — degrade to the plain default synth rather
            // than leave the user mute.
            if let Some(buf) = self.backend.engine.bounce_track_preview(t, &mut ev, n) {
                return Some(Arc::from(buf));
            }
        }
        let inst = self.preview_synth_instance()?;
        let mut ev = ev.to_vec();
        let mut buf = vec![0.0f32; n];
        crate::audio::render_midi_events(&inst, &mut ev, 0, n, &mut buf);
        Some(Arc::from(buf))
    }

    /// One short test note through the audition target (piano-key click,
    /// and the note-placement preview when the user drops a note).
    fn audition_note(&mut self, note: u8) {
        let sr = self.sample_rate.max(1);
        let ev: Vec<(u64, u8, u8, u8, u8)> =
            vec![(0, 3, 0, note, 100), ((sr as u64) / 3, 0, 0, note, 0)];
        match self.audition_render(&ev, 0.6) {
            Some(buf) => {
                // Additive (v0.10): repeated note taps stack like a real
                // keyboard instead of cutting each other off.
                self.backend.cmd(Cmd::PreviewPlay {
                    id: crate::audio::PREVIEW_TRANSIENT,
                    samples: buf,
                });
                self.preview_playing = Some(format!("key {note}"));
            }
            None => self.status = "audition: no synth available".into(),
        }
    }

    /// Render a whole pattern through the audition target for PREVIEW.
    /// Always the FULL pattern plus a release tail — the rack's audition
    /// length cap exists for one-shot samples; cutting a multi-bar
    /// pattern down to it made "preview" play a meaningless blip (the
    /// v10.2 "preview doesn't work in the piano roll" report).
    fn render_pattern_preview(&mut self, pat: &Pattern) -> Option<Arc<[f32]>> {
        let sr = self.sample_rate.max(1);
        let secs = (pat.len_samples(sr) as f32 / sr as f32 + 1.0).clamp(0.5, 60.0);
        self.audition_render(&self.pattern_events(pat), secs)
    }

    /// Audition a MIDI asset through the default synth.
    fn audition_pattern(&mut self, id: usize) {
        if self.sample_rate == 0 || id >= self.assets.len() {
            return;
        }
        if self.preview_playing.as_deref() == Some(self.assets[id].name.as_str()) {
            self.backend.cmd(Cmd::PreviewStop {
                id: Some(id as u32),
            });
            self.preview_playing = None;
            return;
        }
        let Some(pat) = self.assets[id].pattern.clone() else {
            return;
        };
        match self.render_pattern_preview(&pat) {
            Some(buf) => {
                let name = self.assets[id].name.clone();
                self.backend.cmd(Cmd::PreviewPlay {
                    id: id as u32,
                    samples: buf,
                });
                self.preview_playing = Some(name);
            }
            None => self.status = "preview: no synth available".into(),
        }
    }

    /// The piano-roll editor window. Grid tempo is the MAIN metronome
    /// bpm (the roll has no own bpm control); snap follows the main header
    /// checkbox. Gestures: press empty = draw a note (drag = length),
    /// press a note = select (+drag moves the whole selection, dragging
    /// its yellow end caps resizes it, loop-area style), Ctrl/Shift+press
    /// toggles / range-selects (notes) or rubber-band-selects (empty
    /// space), right-click deletes. Two manual scrollbars cover the full
    /// keyboard range and the pattern length; the window opens near
    /// fullscreen, closes with its X, and owns the keyboard while focused
    /// (space can never start the transport behind it).
    fn piano_roll_window(&mut self, ui: &mut egui::Ui) {
        let mut roll = match self.piano_roll.take() {
            Some(r) => r,
            None => return,
        };
        // The roll has NO own tempo: the metronome bpm IS the project
        // tempo, and the pattern grid follows it every frame.
        roll.pat.bpm = self.bpm().clamp(20, 999) as u16;
        let sr = self.sample_rate.max(1);
        let sec_per_beat = 60.0 / roll.pat.bpm.max(20) as f64;
        // Snap OFF (main header checkbox) = one sample, not a subdivision.
        let step_sec = if self.snap_on {
            sec_per_beat / SUBDIVISIONS[self.snap_sub].0.max(1) as f64
        } else {
            1.0 / sr as f64
        };
        let min_len_ticks = ((step_sec / sec_per_beat) * PPQ as f64).max(1.0) as u32;
        // Playhead source: while a preview plays, the TRANSPORT does not
        // move — the bar must follow the preview voice's own cursor or it
        // sits at zero all audition long (the v10.2 "bar never moves"
        // report). Otherwise the transport position as always.
        let playhead = if self.backend.engine.preview_active() {
            self.backend.engine.preview_pos()
        } else {
            self.backend.engine.pos()
        };
        let sel = self.selected;
        let mut save = false;
        let mut clear = false;
        let mut preview = false;
        let mut bar_delta: i32 = 0;
        let mut audition: Option<u8> = None;
        // Preview fires when a NEW note lands (pointer released after a
        // fresh draw) — immediate audible feedback while composing.
        let mut placed: Option<u8> = None;
        let mut open = true;

        // ---- roll keyboard (only while the roll owns the keyboard;
        // `handle_shortcuts` returns early for a focused roll, so these
        // keys cannot double-fire on clips) ----
        if roll.focused && !ui.ctx().text_edit_focused() {
            let (c, v, a, d) = ui.ctx().input(|i| {
                (
                    i.modifiers.command && i.key_pressed(egui::Key::C),
                    i.modifiers.command && i.key_pressed(egui::Key::V),
                    i.modifiers.command && i.key_pressed(egui::Key::A),
                    i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                )
            });
            if c {
                let mut ns: Vec<crate::midipat::Note> = roll
                    .sel_notes
                    .iter()
                    .filter_map(|i| roll.pat.notes.get(*i).copied())
                    .collect();
                if !ns.is_empty() {
                    let m = ns.iter().map(|n| n.start).min().unwrap_or(0);
                    for n in ns.iter_mut() {
                        n.start -= m;
                    }
                    let k = ns.len();
                    self.roll_clip = Some(ns);
                    self.status =
                        format!("copied {k} note(s) \u{b7} Ctrl+V pastes at the playhead");
                }
            }
            if v {
                if let Some(clip) = self.roll_clip.clone() {
                    // Anchor the group's left edge at the playhead (the
                    // same rule as the clip clipboard).
                    let ph = (playhead as f64 / sr as f64
                        * (roll.pat.bpm.max(20) as f64 / 60.0)
                        * PPQ as f64)
                        .round()
                        .max(0.0) as u32;
                    let mut idxs = Vec::with_capacity(clip.len());
                    for n in clip.iter() {
                        let mut n = *n;
                        n.start += ph;
                        roll.pat.notes.push(n);
                        idxs.push(roll.pat.notes.len() - 1);
                    }
                    roll.sel_notes = idxs;
                    roll.changed = true;
                    self.status = format!("pasted {} note(s) at the playhead", clip.len());
                }
            }
            if a {
                roll.sel_notes = (0..roll.pat.notes.len()).collect();
            }
            if d && !roll.sel_notes.is_empty() {
                let mut ids = roll.sel_notes.clone();
                ids.sort_unstable();
                ids.dedup();
                for i in ids.into_iter().rev() {
                    if i < roll.pat.notes.len() {
                        roll.pat.notes.remove(i);
                        // Indices after the hole shift down one.
                        for s in roll.sel_notes.iter_mut() {
                            if *s > i {
                                *s -= 1;
                            }
                        }
                    }
                }
                roll.sel_notes.clear();
                roll.changed = true;
            }
        }

        // ---- roll wheel router ----
        // Pointer over the canvas: ctrl/cmd+wheel zooms time, alt+wheel
        // zooms rows, PLAIN wheel scrolls the canvas (wheel-x horizontal,
        // wheel-y vertical). The global filter left the ctrl/alt events
        // alone while over the roll; claim them before anything else reads
        // the deltas (window content paints after the panels).
        let over_canvas = roll
            .canvas_rect
            .is_some_and(|r| ui.ctx().pointer_hover_pos().is_some_and(|p| r.contains(p)));
        let mut wheel_dx = 0.0f32;
        let mut wheel_dy = 0.0f32;
        if over_canvas {
            let mut tz = 0.0f32;
            let mut rz = 0.0f32;
            ui.ctx().input_mut(|i| {
                i.events.retain(|e| match e {
                    egui::Event::MouseWheel {
                        unit,
                        delta,
                        modifiers,
                        ..
                    } => {
                        let s = match unit {
                            egui::MouseWheelUnit::Line => ZOOM_WHEEL_LINE,
                            _ => ZOOM_WHEEL_PX,
                        };
                        if modifiers.alt {
                            rz += (delta.y * s).clamp(-1.5, 1.5);
                            false
                        } else if modifiers.command || modifiers.ctrl {
                            tz += (delta.y * s).clamp(-1.5, 1.5);
                            false
                        } else {
                            wheel_dx += delta.x;
                            wheel_dy += delta.y;
                            false
                        }
                    }
                    _ => true,
                });
                i.smooth_scroll_delta = egui::Vec2::ZERO;
            });
            if tz != 0.0 {
                roll.px_per_beat = (roll.px_per_beat * tz.exp()).clamp(30.0, 400.0);
            }
            if rz != 0.0 {
                roll.row_h = (roll.row_h * rz.exp()).clamp(6.0, 22.0);
            }
        }
        if wheel_dx != 0.0 {
            // Plain wheel: horizontal = a quarter beat per line notch,
            // vertical = two rows per notch (pitch scroll).
            roll.scroll_sec = (roll.scroll_sec - wheel_dx as f64 * 0.25 * sec_per_beat).max(0.0);
        }
        if wheel_dy != 0.0 {
            roll.vscroll_px = (roll.vscroll_px + wheel_dy * roll.row_h * 2.0)
                .clamp(0.0, (128.0 * roll.row_h - 40.0).max(0.0));
        }

        let screen = ui.ctx().viewport_rect();
        let resp = egui::Window::new("\u{266b} Piano roll")
            .id(egui::Id::new("piano_roll_win"))
            .open(&mut open)
            // Fill the GUI: open near-fullscreen so composing gets room.
            .default_pos(egui::pos2(36.0, 70.0))
            .default_size([
                (screen.width() - 64.0).clamp(560.0, 2600.0),
                (screen.height() - 150.0).clamp(380.0, 2000.0),
            ])
            .show(ui, |ui| {
                // ---- toolbar ----
                ui.horizontal(|ui| {
                    // FL-style routing: pick the TRACK whose instrument AND
                    // mixer chain previews this pattern.
                    ui.label("preview:");
                    let cur_label = match self.preview_track {
                        None => "\u{266a} default (piano)".to_string(),
                        Some(t) => format!("\u{266a} track {}", t + 1),
                    };
                    let mut pick: Option<Option<usize>> = None;
                    egui::ComboBox::from_id_salt("roll_track_pick")
                        .width(150.0)
                        .selected_text(RichText::new(cur_label).small())
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(self.preview_track.is_none(), "default (piano)")
                                .on_hover_text("Bundled piano script, no track chain")
                                .clicked()
                            {
                                pick = Some(None);
                            }
                            ui.separator();
                            for t in 0..self.n_tracks {
                                let lbl = self
                                    .track_synth_inst
                                    .get(t)
                                    .and_then(|h| h.as_ref())
                                    .map(|h| match h.as_ref() {
                                        crate::audio::SynthHandle::Vst(e) => e.name().to_string(),
                                        crate::audio::SynthHandle::Script(_) => {
                                            self.track_synth[t].clone().unwrap_or_default()
                                        }
                                    })
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or_else(|| "piano (default)".into());
                                if ui
                                    .selectable_label(
                                        self.preview_track == Some(t),
                                        format!("track {} \u{b7} {}", t + 1, lbl),
                                    )
                                    .on_hover_text("Preview through this track's synth AND its effects chain")
                                    .clicked()
                                {
                                    pick = Some(Some(t));
                                }
                            }
                        });
                    if let Some(v) = pick {
                        self.preview_track = v;
                        self.save_config();
                    }
                    if ui
                        .small_button("-bar")
                        .on_hover_text("One bar shorter (notes beyond it are dropped)")
                        .clicked()
                    {
                        bar_delta = -1;
                    }
                    if ui
                        .small_button("+bar")
                        .on_hover_text("One bar longer")
                        .clicked()
                    {
                        bar_delta = 1;
                    }
                    if ui
                        .small_button("quantize")
                        .on_hover_text("Snap every note onto the quantize grid")
                        .clicked()
                    {
                        let q_step = SUBDIVISIONS[self.q_sub].0.max(1);
                        let step = (PPQ as u32 * 4 / q_step.max(1)).max(1);
                        roll.pat.quantize_grid(step, true);
                        roll.changed = true;
                    }
                    if ui
                        .small_button(
                            RichText::new("\u{25b6} preview")
                                .color(Color32::from_rgb(120, 220, 160)),
                        )
                        .on_hover_text(
                            "Audition through the picked preview track (synth + chain)",
                        )
                        .clicked()
                    {
                        preview = true;
                    }
                    if ui.small_button("clear").clicked() {
                        clear = true;
                    }
                    if ui
                        .small_button(RichText::new("save").strong())
                        .on_hover_text(
                            "Save as a MIDI asset in the primary folder and place \
                             a clip on the selected track at the playhead",
                        )
                        .clicked()
                    {
                        save = true;
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} notes \u{b7} {} bars \u{b7} {} bpm (metronome)",
                            roll.pat.notes.len(),
                            (roll.pat.len_ticks / (4 * PPQ as u32)).max(1),
                            roll.pat.bpm
                        ))
                        .weak()
                        .small(),
                    );
                });
                // Pure time conversions (closures over roll.pat would fight
                // the borrow checker while editing notes).
                let t_of = |tk: u32| -> f64 { tk as f64 * sec_per_beat / PPQ as f64 };
                let tick_of = |t: f64| -> u32 { (t.max(0.0) * PPQ as f64 / sec_per_beat).round() as u32 };
                let snap = |t: f64| (t / step_sec).floor() * step_sec;
                let snap_up = |t: f64| (t / step_sec).ceil() * step_sec;

                // ---- canvas (fills the rest of the window; the manual
                // scrollbars painted on its edges own their pixels) ----
                let avail = ui.available_size();
                let (rect, resp) = ui.allocate_at_least(
                    egui::vec2(avail.x, (avail.y - 30.0).max(120.0)),
                    Sense::click_and_drag(),
                );
                if rect.width() <= 2.0 || rect.height() <= 2.0 {
                    roll.canvas_rect = Some(rect);
                    return;
                }
                // Remembered for the wheel routers on the NEXT frame.
                roll.canvas_rect = Some(rect);
                // Owned painter clipped to the canvas (painting before or
                // after `ui`-mutating widgets never races).
                let painter = ui.painter_at(rect);

                const SB: f32 = 10.0; // scrollbar thickness
                const KEYS_W: f32 = 58.0;
                let grid_x0 = rect.min.x + KEYS_W;
                let view_w = (rect.max.x - SB - grid_x0).max(40.0);
                let view_h = (rect.height() - SB).max(40.0);
                let px_per_sec = roll.px_per_beat as f64 / sec_per_beat;

                // Keyboard range visible: derived from the vertical scroll.
                let v_max = (128.0 * roll.row_h - view_h).max(0.0);
                roll.vscroll_px = roll.vscroll_px.clamp(0.0, v_max);
                let lo = ((roll.vscroll_px / roll.row_h).floor() as i32).clamp(0, 127) as u8;
                let hi = (lo as i32 + (view_h / roll.row_h) as i32 + 2).min(127) as u8;
                // Pixels from canvas bottom to the bottom of note 0.
                let s_px = roll.vscroll_px;
                painter.rect_filled(rect, 2.0, Color32::from_rgb(22, 22, 26));
                let x_of = |t: f64| grid_x0 + ((t - roll.scroll_sec) * px_per_sec) as f32;
                // vscroll_px counts keyboard pixels scrolled up from the
                // bottom, so a row's TOP sits at B + s − (n+1)·r. (The old
                // sign subtracted s: scrolling then slid the note grid one
                // way while lo/hi and the keyboard strip moved the other —
                // the "inverted scroll" users reported, plus notes vanishing
                // because drawing was culled against lo..hi.)
                let y_of = |n: i32| rect.max.y - SB + s_px - n as f32 * roll.row_h - roll.row_h;
                let sec_at =
                    |x: f32| roll.scroll_sec + (x - grid_x0) as f64 / px_per_sec;
                let row_at = |y: f32| -> i32 {
                    // Exact inverse of y_of: row = ceil((B + s − y)/r) − 1.
                    let b = rect.max.y - SB + s_px;
                    (((b - y - roll.row_h) / roll.row_h).ceil() as i32).clamp(0, 127)
                };

                // vertical grid: snap steps faint, beats medium, bars strong
                let visible = view_w as f64 / px_per_sec;
                let mut tt = (roll.scroll_sec / step_sec).floor() * step_sec;
                while tt < roll.scroll_sec + visible + step_sec {
                    let x = x_of(tt);
                    if x > grid_x0 {
                        let beats = tt / sec_per_beat;
                        let is_beat = (beats - beats.round()).abs() < 0.01;
                        let is_bar = is_beat && ((beats.round() as i64) % 4 == 0);
                        let col = if is_bar {
                            Color32::from_gray(120)
                        } else if is_beat {
                            Color32::from_gray(75)
                        } else {
                            Color32::from_gray(45)
                        };
                        painter.line_segment(
                            [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y - SB)],
                            egui::Stroke::new(if is_bar { 1.4f32 } else { 1.0f32 }, col),
                        );
                        if is_bar {
                            painter.text(
                                egui::pos2(x + 2.0, rect.min.y + 6.0),
                                egui::Align2::LEFT_TOP,
                                format!("{}", (beats.round() as i64) / 4 + 1),
                                egui::FontId::proportional(9.0),
                                Color32::from_gray(150),
                            );
                        }
                    }
                    tt += step_sec;
                }
                // keyboard strip + row lines (only rows intersecting view)
                for n in lo as i32..=hi as i32 {
                    let black = matches!((n.rem_euclid(12)) as u8, 1 | 3 | 6 | 8 | 10);
                    let y = y_of(n);
                    if y > rect.max.y - SB || y + roll.row_h < rect.min.y {
                        continue;
                    }
                    painter.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(rect.min.x, y),
                            egui::pos2(rect.min.x + KEYS_W - 1.0, y + roll.row_h),
                        ),
                        0.0,
                        if black {
                            Color32::from_gray(28)
                        } else {
                            Color32::from_gray(58)
                        },
                    );
                    painter.line_segment(
                        [
                            egui::pos2(grid_x0, y + roll.row_h),
                            egui::pos2(rect.max.x - SB, y + roll.row_h),
                        ],
                        egui::Stroke::new(0.5f32, Color32::from_gray(40)),
                    );
                    if n.rem_euclid(12) == 0 {
                        painter.text(
                            egui::pos2(rect.min.x + 4.0, y + roll.row_h * 0.5),
                            egui::Align2::LEFT_CENTER,
                            format!("C{}", n.div_euclid(12) - 1),
                            egui::FontId::proportional(8.0),
                            Color32::from_gray(225),
                        );
                    }
                }
                // pattern end + playhead markers
                let end_x = x_of(roll.pat.len_samples(sr) as f64 / sr as f64);
                if end_x > grid_x0 && end_x < rect.max.x {
                    painter.line_segment(
                        [egui::pos2(end_x, rect.min.y), egui::pos2(end_x, rect.max.y - SB)],
                        egui::Stroke::new(1.6f32, Color32::from_rgb(230, 180, 90)),
                    );
                }
                let phx = x_of(playhead as f64 / sr as f64);
                if phx > grid_x0 && phx < rect.max.x - SB {
                    painter.line_segment(
                        [egui::pos2(phx, rect.min.y), egui::pos2(phx, rect.max.y - SB)],
                        egui::Stroke::new(1.4f32, Color32::from_rgb(90, 220, 120)),
                    );
                }
                // notes (+ yellow end caps on the selected ones — the
                // resize handles, same language as the loop-area handles)
                const EDGE_PX: f32 = 6.0;
                let hover = ui.input(|i| i.pointer.hover_pos());
                let mut hot_edge = false;
                for (idx, note) in roll.pat.notes.iter().enumerate() {
                    let n = note.note as i32;
                    if n < lo as i32 - 1 || n > hi as i32 + 1 {
                        continue;
                    }
                    let s0 = t_of(note.start);
                    let dur = t_of(note.len.max(1));
                    let x0 = x_of(s0);
                    let x1 = (x_of(s0 + dur)).max(x0 + 2.0);
                    if x1 < grid_x0 || x0 > rect.max.x {
                        continue;
                    }
                    let y = y_of(n);
                    let is_sel = roll.sel_notes.contains(&idx);
                    let dragging = matches!(roll.gesture, RollGesture::Move { idx: d, .. } if d == idx);
                    let col = if dragging {
                        Color32::from_rgb(150, 200, 255)
                    } else if is_sel {
                        Color32::from_rgb(120, 185, 255)
                    } else {
                        Color32::from_rgb(80, 150, 245)
                    };
                    painter.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(x0.max(grid_x0), y + 1.0),
                            egui::pos2(x1, y + roll.row_h - 1.0),
                        ),
                        2.0,
                        col,
                    );
                    if is_sel {
                        for ex in [x0, x1] {
                            painter.rect_filled(
                                egui::Rect::from_min_max(
                                    egui::pos2((ex - 2.0).max(grid_x0), y + 1.0),
                                    egui::pos2((ex + 2.0).min(rect.max.x), y + roll.row_h - 1.0),
                                ),
                                1.0,
                                Color32::from_rgb(250, 200, 50),
                            );
                        }
                    }
                    if let Some(hp) = hover {
                        let y_in = hp.y >= y && hp.y <= y + roll.row_h;
                        if y_in
                            && ((hp.x - x0).abs() <= EDGE_PX || (hp.x - x1).abs() <= EDGE_PX)
                        {
                            hot_edge = true;
                        }
                    }
                }
                // marquee rectangle while rubber-banding
                if let RollGesture::Marquee { from, .. } = roll.gesture {
                    if let Some(hp) = hover {
                        painter.rect_stroke(
                            egui::Rect::from_two_pos(from, hp),
                            0.0,
                            egui::Stroke::new(1.2f32, Color32::from_rgb(150, 200, 255)),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
                // ================= gestures =================
                let grid_top = rect.min.y;
                let grid_bot = rect.max.y - SB;
                let in_grid = |pt: egui::Pos2| {
                    pt.x >= grid_x0 && pt.x <= rect.max.x - SB && pt.y >= grid_top && pt.y <= grid_bot
                };
                // Topmost note under a point (last drawn wins). Notes are
                // passed in as an arg so the helper borrows nothing mutable.
                let note_hit = |notes: &[crate::midipat::Note],
                                pt: egui::Pos2|
                 -> Option<(usize, f64, f64)> {
                    let sec = sec_at(pt.x);
                    let row = row_at(pt.y);
                    if !(0..=127).contains(&row) {
                        return None;
                    }
                    notes.iter().enumerate().rev().find_map(|(i, n)| {
                        let n0 = t_of(n.start);
                        let n1 = n0 + t_of(n.len.max(1));
                        (n.note as i32 == row && sec >= n0 - 1e-6 && sec <= n1 + 1e-6)
                            .then_some((i, n0, n1))
                    })
                };
                if let Some(pt) = resp.interact_pointer_pos() {
                    let mods = ui.input(|i| i.modifiers);
                    // Right-click deletes the note under the cursor.
                    if resp.secondary_clicked() && in_grid(pt) {
                        {
                            if let Some((i, _, _)) = note_hit(&roll.pat.notes, pt) {
                                roll.pat.notes.remove(i);
                                roll.sel_notes.retain(|s| *s != i);
                                for s in roll.sel_notes.iter_mut() {
                                    if *s > i {
                                        *s -= 1;
                                    }
                                }
                                roll.changed = true;
                            }
                        }
                    }
                    // ---- press decides the gesture + selection ----
                    // (egui 0.36 has no press-edge Response; `is_pointer
                    // _button_down_on` + "no gesture in flight yet" fires
                    // exactly once per press and STILL catches presses on
                    // the press frame — unlike drag_started, which only
                    // fires after the pointer has already drifted past the
                    // tiny edge-hit zones. That drift was why "can't grab
                    // a clip/note edge" felt impossible.)
                    if resp.is_pointer_button_down_on()
                        && matches!(roll.gesture, RollGesture::None)
                        && in_grid(pt)
                    {
                        let sec = sec_at(pt.x);
                        let row = (row_at(pt.y).clamp(0, 127)) as u8;
                        match note_hit(&roll.pat.notes, pt) {
                            Some((idx, n0, n1)) => {
                                let x1p = x_of(n1);
                                let x0p = x_of(n0);
                                if (x1p - pt.x).abs() <= EDGE_PX || (x0p - pt.x).abs() <= EDGE_PX {
                                    // Edge cap: resize (loop-area-handle
                                    // style). Head drag moves the start,
                                    // tail drag the end.
                                    let edge =
                                        if (x1p - pt.x).abs() <= (x0p - pt.x).abs() { 1 } else { 0 };
                                    if !roll.sel_notes.contains(&idx) {
                                        roll.sel_notes = vec![idx];
                                        roll.sel_anchor = Some(idx);
                                    }
                                    roll.gesture = RollGesture::Resize {
                                        idx,
                                        edge,
                                        t0: n0,
                                        end0: n1,
                                    };
                                } else {
                                    // Body press: FL selection semantics.
                                    if mods.command {
                                        match roll.sel_notes.iter().position(|x| *x == idx) {
                                            Some(p) => {
                                                roll.sel_notes.remove(p);
                                            }
                                            None => roll.sel_notes.push(idx),
                                        }
                                        roll.sel_notes.sort_unstable();
                                        roll.sel_anchor = Some(idx);
                                    } else if mods.shift {
                                        if let Some(a) = roll.sel_anchor.filter(|a| {
                                            roll.pat.notes.get(*a).is_some()
                                        }) {
                                            let (a0, a1) = (
                                                roll.pat.notes[a].start,
                                                roll.pat.notes[idx].start,
                                            );
                                            let (lo_t, hi_t) = (a0.min(a1), a0.max(a1));
                                            roll.sel_notes = roll
                                                .pat
                                                .notes
                                                .iter()
                                                .enumerate()
                                                .filter(|(_, n)| n.start >= lo_t && n.start <= hi_t)
                                                .map(|(i, _)| i)
                                                .collect();
                                        } else {
                                            roll.sel_notes = vec![idx];
                                            roll.sel_anchor = Some(idx);
                                        }
                                    } else if !roll.sel_notes.contains(&idx) {
                                        roll.sel_notes = vec![idx];
                                        roll.sel_anchor = Some(idx);
                                    }
                                    // Group move: snapshot every selected
                                    // note; drag moves them all.
                                    if roll.sel_notes.contains(&idx) {
                                        let notes: Vec<(usize, crate::midipat::Note)> = roll
                                            .sel_notes
                                            .iter()
                                            .filter_map(|i| {
                                                roll.pat.notes.get(*i).copied().map(|n| (*i, n))
                                            })
                                            .collect();
                                        let base = roll.pat.notes[idx].start;
                                        roll.drag_notes = notes;
                                        roll.gesture = RollGesture::Move {
                                            idx,
                                            off: sec - n0,
                                            base,
                                            fresh: false,
                                        };
                                    }
                                }
                                roll.changed = true;
                            }
                            None => {
                                if mods.command || mods.shift {
                                    // Rubber-band on empty space.
                                    roll.gesture = RollGesture::Marquee {
                                        from: pt,
                                        add: mods.shift,
                                    };
                                } else {
                                    // Draw a new note (drag lengthens it).
                                    let st = tick_of(snap(sec));
                                    roll.pat.notes.push(crate::midipat::Note {
                                        ch: 0,
                                        note: row,
                                        vel: roll.new_vel,
                                        press: 0,
                                        start: st,
                                        len: min_len_ticks,
                                    });
                                    let idx = roll.pat.notes.len() - 1;
                                    roll.sel_notes = vec![idx];
                                    roll.sel_anchor = Some(idx);
                                    roll.drag_notes.clear();
                                    roll.gesture = RollGesture::Move {
                                        idx,
                                        off: 0.0,
                                        base: st,
                                        fresh: true,
                                    };
                                    // Placement feedback: hear the note now.
                                    placed = Some(row);
                                    roll.changed = true;
                                }
                            }
                        }
                    } else if resp.dragged() {
                        match roll.gesture {
                            RollGesture::Move { idx, off, base, fresh } => {
                                if idx >= roll.pat.notes.len() {
                                    roll.gesture = RollGesture::None;
                                } else if in_grid(pt) {
                                    let sec = sec_at(pt.x);
                                    let row = (row_at(pt.y).clamp(0, 127)) as u8;
                                    if fresh {
                                        // Fresh draw: the tail follows the
                                        // pointer (FL stretch-draw).
                                        let note = &mut roll.pat.notes[idx];
                                        let tail = tick_of(snap_up(sec));
                                        note.len = tail.saturating_sub(note.start).max(min_len_ticks);
                                        note.note = row;
                                    } else {
                                        let d = tick_of(snap(sec - off).max(0.0)) as i64
                                            - base as i64;
                                        // Snapshot from the press frame —
                                        // the selection may have changed
                                        // since; the group moves rigidly.
                                        let dn = std::mem::take(&mut roll.drag_notes);
                                        // Pitch shift = rows the pointer
                                        // moved since the grab.
                                        let dnote = dn
                                            .iter()
                                            .find(|(i, _)| *i == idx)
                                            .map(|(_, n)| row as i32 - n.note as i32)
                                            .unwrap_or(0);
                                        for (i, n) in dn.iter() {
                                            if let Some(cur) = roll.pat.notes.get_mut(*i) {
                                                cur.start = (n.start as i64 + d).max(0) as u32;
                                                cur.note = (n.note as i32 + dnote)
                                                    .clamp(0, 127)
                                                    as u8;
                                            }
                                        }
                                        roll.drag_notes = dn;
                                    }
                                    roll.changed = true;
                                }
                            }
                            RollGesture::Resize { idx, edge, t0, end0 } => {
                                if idx < roll.pat.notes.len() && in_grid(pt) {
                                    let sec = sec_at(pt.x);
                                    let note = &mut roll.pat.notes[idx];
                                    let st = tick_of(t0);
                                    let en = tick_of(end0);
                                    if edge == 1 {
                                        let cand = tick_of(snap_up(sec).max(t0 + step_sec));
                                        note.len = cand.saturating_sub(st).max(min_len_ticks);
                                    } else {
                                        let cand = tick_of(snap(sec).max(0.0))
                                            .min(en.saturating_sub(min_len_ticks));
                                        note.start = cand;
                                        note.len = (en - cand).max(min_len_ticks);
                                    }
                                    roll.changed = true;
                                }
                            }
                            RollGesture::Marquee { .. } => {} // painted live
                            RollGesture::None => {}
                        }
                    }
                    if resp.drag_stopped() {
                        if let RollGesture::Marquee { from, add } = roll.gesture {
                            let box_r = egui::Rect::from_two_pos(from, pt);
                            let hits: Vec<usize> = roll
                                .pat
                                .notes
                                .iter()
                                .enumerate()
                                .filter(|(_, n)| {
                                    let x0 = x_of(t_of(n.start));
                                    let x1 = x_of(t_of(n.start + n.len.max(1)));
                                    let y0 = y_of(n.note as i32);
                                    let yr = egui::Rect::from_min_max(
                                        egui::pos2(x0.min(x1), y0),
                                        egui::pos2(x0.max(x1), y0 + roll.row_h),
                                    );
                                    box_r.intersects(yr)
                                })
                                .map(|(i, _)| i)
                                .collect();
                            if add {
                                for h in hits {
                                    if !roll.sel_notes.contains(&h) {
                                        roll.sel_notes.push(h);
                                    }
                                }
                                roll.sel_notes.sort_unstable();
                            } else {
                                roll.sel_notes = hits;
                            }
                            roll.sel_anchor = roll.sel_notes.first().copied();
                        }
                        roll.gesture = RollGesture::None;
                        roll.drag_notes.clear();
                        // NOTE: the pattern is NOT re-sorted here — sorting
                        // would invalidate every selection index. Sorting
                        // happens at save (SMF write) only.
                    }
                    // Piano keys: click plays the note through the
                    // preview track.
                    if pt.x >= rect.min.x
                        && pt.x < grid_x0
                        && pt.y >= grid_top
                        && pt.y <= grid_bot
                        && resp.clicked_by(egui::PointerButton::Primary)
                    {
                        audition = Some((row_at(pt.y).clamp(0, 127)) as u8);
                    }
                }
                if hot_edge {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }

                // ================= scrollbars =================
                // Content extents → thumb sizes. Manual because the canvas
                // is a painted rect, not a ScrollArea.
                let content_w = (roll.pat.len_ticks as f64 / PPQ as f64 * roll.px_per_beat as f64)
                    .max(view_w as f64);
                let hbar = egui::Rect::from_min_max(
                    egui::pos2(grid_x0, rect.max.y - SB),
                    egui::pos2(rect.max.x - SB, rect.max.y),
                );
                let vbar = egui::Rect::from_min_max(
                    egui::pos2(rect.max.x - SB, rect.min.y),
                    egui::pos2(rect.max.x, rect.max.y - SB),
                );
                painter.rect_filled(hbar, 0.0, Color32::from_black_alpha(90));
                painter.rect_filled(vbar, 0.0, Color32::from_black_alpha(90));
                let h_max_px = (content_w - view_w as f64).max(0.0);
                let thumb_w = (view_w as f64 / content_w * view_w as f64)
                    .clamp(24.0, view_w as f64) as f32;
                let hx = hbar.min.x
                    + if h_max_px > 0.0 {
                        (roll.scroll_sec * px_per_sec / h_max_px) as f32 * (hbar.width() - thumb_w)
                    } else {
                        0.0
                    };
                painter.rect_filled(
                    egui::Rect::from_min_max(egui::pos2(hx, hbar.min.y + 1.5), egui::pos2(hx + thumb_w, hbar.max.y - 1.5)),
                    3.0,
                    Color32::from_gray(120),
                );
                let thumb_h = (view_h / (128.0 * roll.row_h) * view_h).clamp(24.0, view_h);
                let v_frac = if v_max > 0.0 {
                    (roll.vscroll_px / v_max).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                // Bottom-anchored: fraction 0 (note 0 at the bottom) puts
                // the thumb at the bar's bottom edge.
                let vy = vbar.max.y - thumb_h - v_frac * (vbar.height() - thumb_h);
                painter.rect_filled(
                    egui::Rect::from_min_max(egui::pos2(vbar.min.x + 1.5, vy), egui::pos2(vbar.max.x - 1.5, vy + thumb_h)),
                    3.0,
                    Color32::from_gray(120),
                );
                // Alloced AFTER the canvas so the bars steal their pixels.
                let hresp = ui.interact(hbar, egui::Id::new("roll_hsb"), Sense::drag());
                if hresp.dragged_by(egui::PointerButton::Primary) || hresp.clicked() {
                    if let Some(hp) = hresp.interact_pointer_pos() {
                        if h_max_px > 0.0 && hbar.width() > thumb_w {
                            let r = ((hp.x - hbar.min.x - thumb_w / 2.0)
                                / (hbar.width() - thumb_w))
                                .clamp(0.0, 1.0) as f64;
                            roll.scroll_sec = r * h_max_px / px_per_sec;
                        }
                    }
                }
                if hresp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                let vresp = ui.interact(vbar, egui::Id::new("roll_vsb"), Sense::drag());
                if vresp.dragged_by(egui::PointerButton::Primary) || vresp.clicked() {
                    if let Some(hp) = vresp.interact_pointer_pos() {
                        if v_max > 0.0 && vbar.height() > thumb_h {
                            let r = ((vbar.max.y - hp.y - thumb_h / 2.0)
                                / (vbar.height() - thumb_h))
                                .clamp(0.0, 1.0) as f64;
                            roll.vscroll_px = (r * v_max as f64) as f32;
                        }
                    }
                }
                if vresp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                }

                ui.separator();
                ui.horizontal(|ui| {
                    ui.add(Slider::new(&mut roll.new_vel, 1..=127).text("vel"))
                        .on_hover_text("Velocity of newly drawn notes");
                    // The old "px/beat"/"row px" units were engineering
                    // labels, not what the sliders do: they are the two
                    // zoom controls (user complaint: "mislabeled").
                    ui.add(
                        Slider::new(&mut roll.px_per_beat, 30.0..=400.0).text("zoom time"),
                    )
                    .on_hover_text("Horizontal zoom (pixels per beat) \u{b7} ctrl+wheel over the grid does this too");
                    ui.add(
                        Slider::new(&mut roll.row_h, 6.0..=22.0).text("zoom rows"),
                    )
                    .on_hover_text("Vertical zoom (pixels per semitone row) \u{b7} alt+wheel over the grid does this too");
                    ui.label(
                        RichText::new(
                            "drag empty = draw \u{b7} drag note = move \u{b7} yellow caps = resize \u{b7} ctrl/shift+drag = box select \u{b7} right-click = delete",
                        )
                        .weak()
                        .small(),
                    );
                });
            });
        // Keyboard focus: the LAST primary click decides. Inside the
        // window → the roll owns Ctrl+C/V/A, Delete and swallows the
        // global shortcuts; outside → the main GUI takes them back.
        if let Some(win) = resp.as_ref() {
            // Store the FULL window frame (title bar included) so tests
            // and title-bar logic never have to re-scan stale geometry.
            let rect = win.response.rect;
            roll.win_rect = Some(rect);
            if let Some(click) = ui.ctx().input(|i| {
                if i.pointer.button_clicked(egui::PointerButton::Primary) {
                    i.pointer.hover_pos()
                } else {
                    None
                }
            }) {
                roll.focused = rect.contains(click);
            }
        }
        // A key in the piano strip was clicked: play it (deferred — the
        // renderer needs &mut self, which the window closure holds open).
        if let Some(n) = audition {
            self.audition_note(n);
        }
        if let Some(n) = placed {
            self.audition_note(n);
        }
        // ---- toolbar actions deferred (need &mut self) ----
        if bar_delta != 0 {
            let bar = 4 * PPQ as u32;
            let pat = &mut roll.pat;
            if bar_delta > 0 {
                pat.len_ticks += bar;
            } else if pat.len_ticks > bar {
                pat.len_ticks -= bar;
                let lim = pat.len_ticks;
                pat.notes.retain(|n| n.start < lim);
                for n in pat.notes.iter_mut() {
                    let end = (n.start + n.len).min(lim);
                    n.len = end.saturating_sub(n.start).max(1);
                }
                pat.ccs.retain(|m| m.at < lim);
                pat.press.retain(|m| m.at < lim);
            }
            roll.changed = true;
            roll.sel_notes.clear();
            roll.sel_anchor = None;
        }
        if clear {
            roll.pat.notes.clear();
            roll.pat.ccs.clear();
            roll.pat.press.clear();
            roll.sel_notes.clear();
            roll.sel_anchor = None;
            roll.changed = true;
        }
        if preview {
            let pat = roll.pat.clone();
            match self.render_pattern_preview(&pat) {
                Some(buf) => {
                    // Edit echo, not an audition: replace anything playing.
                    self.backend.cmd(Cmd::PreviewStop { id: None });
                    self.backend.cmd(Cmd::PreviewPlay {
                        id: crate::audio::PREVIEW_TRANSIENT,
                        samples: buf,
                    });
                    self.preview_playing = Some("piano roll".into());
                }
                None => self.status = "preview: no synth available".into(),
            }
        }
        if save {
            match roll.edit_asset {
                Some(id) => self.update_piano_roll_asset(&mut roll.pat, id),
                None => self.save_piano_roll(&mut roll.pat, sel),
            }
            roll.sel_notes.clear();
            roll.sel_anchor = None;
        }
        if !open {
            // The X closes the roll for real (was: reopened next frame,
            // leaving quit as the only escape).
            self.status = "piano roll closed (unsaved changes are kept until quit)".into();
            return;
        }
        // Watchdog (mirrors the main grid's post-frame reaper): gestures
        // ARM on press, so an armed gesture must die on button-up even
        // when no drag ever started — a click's release lands on whatever
        // widget holds the pointer (often the title bar), never on the
        // canvas response, so without this a no-travel click sticks the
        // roll in Move and every later press is ignored. Runs AFTER the
        // window paints so a real drag's drag_stopped() commits first.
        if !matches!(roll.gesture, RollGesture::None) && !ui.ctx().input(|i| i.pointer.any_down()) {
            roll.gesture = RollGesture::None;
            roll.drag_notes.clear();
        }
        self.piano_roll = Some(roll);
    }

    /// In-place edit-save: overwrite the MIDI asset the roll was opened
    /// from (file + in-memory pattern), refresh the engine. No new asset,
    /// no new clip — every clip already pointing at it plays the update.
    fn update_piano_roll_asset(&mut self, pat: &mut Pattern, id: u32) {
        pat.sort();
        let a = match self.assets.get_mut(id as usize) {
            Some(a) => a,
            None => {
                self.status = "piano roll: the edited asset no longer exists".into();
                return;
            }
        };
        if let Err(e) = pat.save_mid(&a.path) {
            self.status = format!("piano roll: {e}");
            return;
        }
        let name = a.name.clone();
        a.pattern = Some(Arc::new(pat.clone()));
        a.dirty = false;
        self.push_project();
        self.status = format!("piano roll: updated \u{201c}{name}\u{201d} in place");
    }

    /// Bounce the piano roll: unique .mid in the primary folder, new asset,
    /// clip on `track` at the playhead (snapped), rack refreshed.
    fn save_piano_roll(&mut self, pat: &mut Pattern, track: usize) {
        pat.sort();
        let dir = self.primary_folder();
        let mut n = 1u32;
        let path = loop {
            let name = if n == 1 {
                "roll".to_string()
            } else {
                format!("roll-{n}")
            };
            let p = dir.join(format!("{name}.mid"));
            if !p.exists() {
                break p;
            }
            n += 1;
        };
        if let Err(e) = pat.save_mid(&path) {
            self.status = format!("piano roll: {e}");
            return;
        }
        let id = self.assets.len() as u32;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let pat = Arc::new(pat.clone());
        let len = pat.len_samples(self.sample_rate.max(1));
        self.assets
            .push(Asset::midi(id, name.clone(), path, pat, false));
        let pos = snap_start(self.backend.engine.pos(), self.snap_unit());
        let track = track.min(self.n_tracks - 1);
        let nl0 = self.arr.tracks[track].lanes.len();
        let lane = self.arr.place_auto(track, Clip::new(id, pos), len);
        self.grow_for_lanes(track, nl0);
        if let Some(f) = self.rack.first_mut() {
            f.rescan();
        }
        self.push_project();
        self.status = format!(
            "piano roll: saved \u{201c}{name}\u{201d} \u{2192} track {}, lane {}",
            track + 1,
            lane + 1
        );
    }

    /// The synth slider window doubles as a SAMPLER editor when the
    /// bound script's first parameter is the asset picker.
    fn sampler_editor_open(&self) -> Option<usize> {
        let t = self.synth_win?;
        let stem = self.track_synth.get(t)?.clone()?;
        if !matches!(
            self.track_synth_inst.get(t)?.as_ref()?.as_ref(),
            SynthHandle::Script(_)
        ) {
            return None;
        }
        let desc = self.synth_registry.desc(&stem)?;
        desc.params.first().is_some_and(|p| p.label == "asset").then_some(t)
    }

    /// v18: apply an OS-drop batch to the live sampler on `t`: import
    /// each audio file (rack dedup via `ensure_asset`) and swap the
    /// patch — no restart, no re-pick. Kept status string identical to
    /// the old in-window handler so existing pins hold.
    fn sampler_files_dropped(&mut self, t: usize, paths: &[std::path::PathBuf]) {
        let Some(handle) = self.track_synth_inst.get(t).cloned().flatten() else {
            return;
        };
        let inst = match handle.as_ref() {
            SynthHandle::Script(i) => i.clone(),
            SynthHandle::Vst(_) => return,
        };
        let mut swapped: Option<usize> = None;
        let mut rejected = 0usize;
        for path in paths {
            if !crate::assets::is_audio_file(path) {
                rejected += 1;
                continue; // midi/auto/unknown: not a sample
            }
            if let Some(k) = self.ensure_asset(path) {
                inst.set_param(0, k as f32);
                if let Some(vals) = self.track_synth_params.get_mut(t) {
                    vals.clear();
                    vals.extend_from_slice(&inst.values());
                }
                self.save_config();
                swapped = Some(k);
            }
        }
        match swapped.and_then(|k| self.assets.get(k).map(|a| a.name.clone())) {
            Some(n) => self.status = format!("track {} sampler: {n} (dropped file)", t + 1),
            None if rejected > 0 => {
                self.status = format!("sampler: dropped file{} not audio", if rejected == 1 { "" } else { "s" });
            }
            None => {}
        }
    }

    /// Auto sliders for a track's live synth instance (values ride the
    /// instance atomics, exactly like effect params).
    fn synth_sliders_window(&mut self, ui: &mut egui::Ui, t: usize) {
        let Some(stem) = self.track_synth[t].clone() else {
            self.synth_win = None;
            return;
        };
        // v0.11 R3: hosted VST instruments never reach this window -- their
        // own GUI is the only parameter surface, so this is the SCRIPT
        // synth slider window only.
        if matches!(
            self.track_synth_inst[t].as_ref().map(|h| h.as_ref()),
            Some(SynthHandle::Vst(_))
        ) {
            self.synth_win = None;
            return;
        }
        let Some(desc) = self.synth_registry.desc(&stem).cloned() else {
            self.synth_win = None;
            return;
        };
        let Some(handle) = self.track_synth_inst[t].clone() else {
            self.synth_win = None;
            return;
        };
        let inst = match handle.as_ref() {
            SynthHandle::Script(i) => i.clone(),
            SynthHandle::Vst(_) => {
                self.synth_win = None;
                return;
            }
        };
        // Snapshot of the AUDIO assets a sampler can play (MIDI assets are
        // patterns, never samples). Taken outside the window closure — the
        // closure already borrows the synth instance, not the App.
        let audio_assets: Vec<(usize, String)> = self
            .assets
            .iter()
            .enumerate()
            .filter(|(_, a)| a.pattern.is_none())
            .map(|(k, a)| (k, a.name.clone()))
            .collect();
        let mut open = true;
        let mut dirty = false;
        egui::Window::new(format!("\u{266a} {stem} \u{b7} track {}", t + 1))
            .id(egui::Id::new(("synthwin", t)))
            .open(&mut open)
            .default_size([320.0, 340.0])
            .show(ui, |ui| {
                if let Some(d) = &desc.description {
                    ui.label(RichText::new(d).weak().small());
                }
                if let Some(a) = &desc.author {
                    ui.label(RichText::new(format!("by {a}")).weak().small());
                }
                ui.separator();
                let vals = inst.values();
                // A synth whose FIRST param is named "asset" samples rack
                // audio (the bundled sampler): give it a named picker and a
                // drop target instead of a blind 0..63 id slider — the user
                // could not tell which asset any id was, let alone pick one.
                let sampler = desc.params.first().is_some_and(|p| p.label == "asset");
                if sampler {
                    let cur = vals.first().copied().unwrap_or(0.0).round() as usize;
                    let cur_name = audio_assets
                        .iter()
                        .find(|(k, _)| *k == cur)
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| "\u{2014} no sample \u{2014}".to_string());
                    ui.label(
                        RichText::new(format!("sample: {cur_name}"))
                            .strong()
                            .small(),
                    );
                    let mut picked = None;
                    egui::ComboBox::from_id_salt(("sampler_asset_pick", t))
                        .width(240.0)
                        .selected_text(RichText::new(format!("#{cur}  {cur_name}")).small())
                        .show_ui(ui, |ui| {
                            if audio_assets.is_empty() {
                                ui.label(
                                    RichText::new(
                                        "no audio assets yet \u{2014} add a rack folder or record",
                                    )
                                    .weak()
                                    .small(),
                                );
                            }
                            for (k, name) in &audio_assets {
                                // ComboBox's button response never marks
                                // `changed` on a pick — watch the popup rows.
                                if ui
                                    .selectable_label(cur == *k, format!("#{k} {name}"))
                                    .clicked()
                                {
                                    picked = Some(*k);
                                }
                            }
                        });
                    if let Some(k) = picked {
                        inst.set_param(0, k as f32);
                        dirty = true;
                    }
                    ui.label(
                        RichText::new(
                            "or drag an AUDIO asset from the rack \u{2014} or straight from your \
                             file manager \u{2014} onto this box:",
                        )
                        .weak()
                        .small(),
                    );
                    let (drect, dresp) = ui.allocate_at_least(
                        egui::vec2(ui.available_width(), 34.0),
                        egui::Sense::hover(),
                    );
                    let dragging = egui::DragAndDrop::has_payload_of_type::<usize>(ui.ctx())
                        // v0.13: an OS-level file drag hovering the window
                        // lights the zone too (winit feeds raw.hovered_files).
                        || ui.input(|i| !i.raw.hovered_files.is_empty());
                    let col = if dragging {
                        Color32::from_rgb(150, 200, 255)
                    } else {
                        Color32::from_gray(75)
                    };
                    ui.painter().rect_stroke(
                        drect,
                        3.0,
                        egui::Stroke::new(if dragging { 2.0 } else { 1.0 }, col),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        drect.center(),
                        egui::Align2::CENTER_CENTER,
                        if dragging {
                            "release to sample this asset"
                        } else {
                            "drop zone \u{b7} drag an asset here"
                        },
                        egui::FontId::proportional(10.0),
                        col,
                    );
                    if let Some(a) = dresp.dnd_release_payload::<usize>().map(|a| *a) {
                        if audio_assets.iter().any(|(k, _)| *k == a) {
                            inst.set_param(0, a as f32);
                            dirty = true;
                            self.status = format!(
                                "track {} sampler: {}",
                                t + 1,
                                audio_assets
                                    .iter()
                                    .find(|(k, _)| *k == a)
                                    .map(|(_, n)| n.clone())
                                    .unwrap_or_default()
                            );
                        }
                    }
                    // v18: OS-level file drops (file manager → app while
                    // this window is open) are claimed CENTRALLY at the
                    // top of `ui_root` (one drain per frame, then the
                    // sampler swap via `sampler_files_dropped`); by the
                    // time this window paints the queue is already empty.
                    ui.separator();
                }
                for (k, pd) in desc.params.iter().enumerate() {
                    if sampler && k == 0 {
                        continue; // the picker replaces the raw id slider
                    }
                    let mut nv = vals.get(k).copied().unwrap_or(pd.default);
                    // Root note reads as STANDARD notation (C3, C#4, ...):
                    // the slider steps whole semitones and shows the note
                    // name instead of the raw MIDI number (AC3.1). The
                    // stored/passed value is still the MIDI number.
                    let is_root = sampler && k == 1 && pd.label == "root note";
                    let mut sl = Slider::new(&mut nv, pd.min..=pd.max).text(&pd.label);
                    if is_root {
                        sl = sl
                            .step_by(1.0)
                            .custom_formatter(|n, _| note_name(n.round().clamp(0.0, 127.0) as u8));
                    }
                    let resp = ui.add(sl);
                    resp.context_menu(|ui| {
                        auto_menu(ui, self, t, &format!("synth:{k}"), nv);
                    });
                    if resp.changed() {
                        inst.set_param(k, nv);
                        self.note_auto(t, &format!("synth:{k}"), nv);
                        dirty = true;
                    }
                }
            });
        if dirty {
            self.track_synth_params[t] = inst.values();
            self.save_config();
        }
        if !open {
            self.synth_win = None;
        }
    }

    /// v0.10 STANDARD: binding a plugin instrument opens the plugin's own
    /// editor window immediately. v0.11 R3: hosted plugins have no slider
    /// window at all -- the plugin's GUI is the only parameter surface.
    /// Returns false when the plugin advertises no editor or the editor
    /// refuses to open (a refused editor is reaped by `reap_vst_editors`
    /// and the status line names the reason).
    fn auto_open_synth_gui(&mut self, t: usize) -> bool {
        // v0.11 double-open guard: a second effEditOpen against the same
        // live instance while an editor is open races two toolkit editor
        // states (and silently leaked the first container when the map
        // entry was replaced). Retire any old window FIRST — v20: close it
        // IN PLACE (the slot gets overwritten below) so `reap_vst_editors`
        // still sees the dying window and harvests its state chunk.
        if let Some(w) = self.vst_editors.get_mut(&t) {
            w.close();
        }
        let Some(arc) = self.track_synth_inst[t].clone() else {
            return false;
        };
        let entry = match arc.as_ref() {
            crate::audio::SynthHandle::Vst(e) => e.clone(),
            _ => return false, // script synths keep their slider editor
        };
        if !entry.has_editor() {
            return false;
        }
        match self.plugin_window_open(&entry) {
            Ok(w) => {
                self.status = format!("{}: plugin window opened", entry.name());
                self.vst_editors.insert(t, w);
                true
            }
            Err(e) => {
                self.status = format!("plugin GUI: {e} \u{2014} closed");
                false
            }
        }
    }

    /// v0.11 R3: the track-row GUI button for a hosted plugin. Clicking
    /// while the plugin window is still alive closes it (this also makes a
    /// second effEditOpen against the same live instance structurally
    /// impossible); clicking with no live window opens it.
    fn toggle_synth_gui(&mut self, t: usize) {
        if self.vst_editors.get(&t).is_some_and(|w| !w.is_closed()) {
            // v20: close in place; the reaper harvests the patch (if the
            // user changed any) once the editor thread retires the window.
            if let Some(w) = self.vst_editors.get_mut(&t) {
                w.close();
            }
            self.status = format!("track {}: plugin window closed", t + 1);
            return;
        }
        // Drop any dead handle before opening the replacement.
        self.vst_editors.remove(&t);
        self.auto_open_synth_gui(t);
    }

    /// Chain-slot counterpart: click a plugin row and the plugin's own GUI
    /// opens (a second click closes it).
    fn vst_row_gui(&mut self, t: usize, row: usize) {
        // v20: toggle-close stays IN the map (vst_editors' rule) so the
        // reaper can harvest the shadow's state chunk after teardown.
        if self.fx_vst_editors.contains_key(&(t, row)) {
            if let Some(w) = self.fx_vst_editors.get_mut(&(t, row)) {
                if !w.is_closed() {
                    w.close();
                    return;
                }
            }
            self.fx_vst_editors.remove(&(t, row)); // dead handle
        }
        let Some(entry) = self
            .fx_rows
            .get(t)
            .and_then(|r| r.get(row))
            .and_then(|r| match r {
                ChainRow::Vst { entry, .. } => entry.clone(),
                _ => None,
            })
        else {
            return;
        };
        if entry.has_editor() {
            match self.plugin_window_open(&entry) {
                Ok(w) => {
                    self.status = format!("{}: plugin window opened", entry.name());
                    self.fx_vst_editors.insert((t, row), w);
                    return;
                }
                Err(e) => self.status = format!("plugin GUI: {e} \u{2014} closed"),
            }
        }
        // v0.11 R3: hosted plugins get NO slider window. A plugin with no
        // drawable editor keeps its current parameter values.
        self.status = format!(
            "{}: this plugin has no window to open \u{2014} its controls are not editable",
            entry.name()
        );
    }

    /// Close editor windows that are user-closed, failed, or whose track
    /// no longer hosts the plugin they were opened for.
    fn reap_vst_editors(&mut self) {
        let mut closed: Vec<usize> = Vec::new();
        let mut refused = false;
        // Chain-slot editors: drop when the window dies, when the plugin
        // refused to draw, or when the row moved/no longer holds it.
        let mut fx_closed: Vec<(usize, usize)> = Vec::new();
        for ((t, r), w) in self.fx_vst_editors.iter() {
            let cur = match self.fx_rows.get(*t).and_then(|rows| rows.get(*r)) {
                Some(ChainRow::Vst { path, .. }) => Some(path.as_str()),
                _ => None,
            };
            if w.is_closed() || w.open_failed() || cur != Some(w.path()) {
                fx_closed.push((*t, *r));
            }
        }
        for k in fx_closed {
            if let Some(mut w) = self.fx_vst_editors.remove(&k) {
                let same = matches!(
                    self.fx_rows.get(k.0).and_then(|rows| rows.get(k.1)),
                    Some(ChainRow::Vst { path, .. }) if path == w.path()
                );
                if same {
                    self.harvest_chain_state(k.0, k.1, &mut w);
                }
            }
        }
        for (t, w) in self.vst_editors.iter() {
            let cur = match self.track_synth_inst.get(*t).and_then(|slot| slot.as_ref()) {
                Some(arc) => match arc.as_ref() {
                    crate::audio::SynthHandle::Vst(e) => {
                        Some(e.path().to_string_lossy().into_owned())
                    }
                    _ => None,
                },
                None => None,
            };
            if w.open_failed() {
                refused = true;
            }
            if w.is_closed() || w.open_failed() || cur.as_deref() != Some(w.path()) {
                closed.push(*t);
            }
        }
        for t in closed {
            if let Some(mut w) = self.vst_editors.remove(&t) {
                let same = match self.track_synth_inst.get(t).and_then(|s| s.as_ref()) {
                    Some(arc) => matches!(arc.as_ref(),
                        crate::audio::SynthHandle::Vst(e)
                        if e.path().to_string_lossy() == w.path()),
                    None => false,
                };
                if same {
                    self.harvest_synth_state(t, &mut w);
                }
            }
        }
        if refused {
            self.status = "plugin editor could not draw \u{2014} its own window is the only \
                 parameter surface, so close and re-open it to retry"
                .into();
        }
    }

    /// v20 (user report: presets picked inside a plugin's own window did
    /// nothing): a VST3 editor drives a SHADOW instance. Sliders reach the
    /// sound through the cell mirror, but preset/patch changes never
    /// broadcast performEdits — they live only in the shadow's component
    /// state, which its teardown hands over (`EditorWindow::take_state`).
    /// Apply it: stash on the row and re-open the instance from the chunk,
    /// async like every other plugin load (the UI thread never waits on
    /// instantiation). VST2 editors edit the live instance in place and
    /// never hand over a chunk, so this never fires for them.
    fn harvest_chain_state(&mut self, t: usize, r: usize, w: &mut crate::vstedit::EditorWindow) {
        let Some(bytes) = w.take_state() else { return };
        let sr = self.sample_rate.max(1);
        {
            let Some(row) = self.fx_rows.get_mut(t).and_then(|rows| rows.get_mut(r)) else {
                return;
            };
            let ChainRow::Vst {
                path,
                kind,
                name,
                entry,
                open_rx,
                state,
                ..
            } = row
            else {
                return;
            };
            *state = Some(bytes.clone());
            if entry.is_some() || open_rx.is_some() {
                // Cells re-seed from the chunk (saved=None), so the patch's
                // own parameter values win over the stale config snapshot.
                *entry = None;
                *open_rx = Some(crate::vsthost::open_async_with_state(
                    std::path::Path::new(path),
                    *kind,
                    sr,
                    None,
                    Some(&bytes),
                ));
                self.vst_note = format!("{name}: applying preset\u{2026}");
            }
        }
        self.rebuild_fx(t);
    }

    /// Synth-binding twin of [`App::harvest_chain_state`]: rebuild the
    /// binding synchronously from the harvested chunk (opening a synth
    /// binding always paid this cost; the next note already sounds new).
    fn harvest_synth_state(&mut self, t: usize, w: &mut crate::vstedit::EditorWindow) {
        let Some(bytes) = w.take_state() else { return };
        self.track_synth_state[t] = Some(bytes);
        if self.rebuild_track_synth(t) {
            self.status = format!("track {}: preset applied", t + 1);
        }
    }

    fn toggle_record(&mut self, i: usize) {
        if self.sample_rate == 0 {
            self.status = "record: wait for the audio engine".into();
            return;
        }
        // The engine owns the record state (and auto-stops at buffer
        // capacity); the GUI just toggles and mirrors events.
        self.backend.cmd(Cmd::ToggleRecord { i });
    }

    fn clear_track(&mut self, i: usize) {
        self.arr.clear_track(i);
        self.cancel_grid_gestures();
        self.sel_clips.clear();
        self.sel_anchor = None;
        self.push_project();
        self.status = format!("track {} cleared", i + 1);
    }

    /// A finished take becomes: optional content quantize → WAV into the
    /// primary rack folder (when set) → asset → clip placed at the take's
    /// start position, snapped to the snap grid.
    fn take_pipeline(&mut self, track: usize, at: u64, data: Arc<[f32]>) {
        if track >= MAX_LOOPS {
            return;
        }
        let data = if self.auto_quantize {
            match quantize::quantize_asset(&data, self.sample_rate, self.q_unit()) {
                Some(q) => Arc::from(q),
                None => data,
            }
        } else {
            data
        };
        let mut path = PathBuf::new();
        let mut dirty = true;
        let name = format!("rec-t{}-{}", track + 1, Self::fstamp());
        if let Some(f) = self.rack.first_mut() {
            let dir = PathBuf::from(f.path.trim());
            if !dir.as_os_str().is_empty() {
                let p = dir.join(format!("{name}.wav"));
                if std::fs::create_dir_all(&dir).is_ok()
                    && crate::audio_io::export(
                        &p,
                        self.sample_rate,
                        &data,
                        crate::audio_io::ExportFormat::Wav,
                    )
                    .is_ok()
                {
                    path = p;
                    dirty = false;
                }
            }
        }
        let id = self.assets.len() as u32;
        let len = data.len() as u64;
        self.assets.push(Asset::audio(id, name, path, data, dirty));
        self.sel_asset = Some(id as usize);
        let pos = snap_start(at, self.snap_unit());
        let nl0 = self.arr.tracks[track].lanes.len();
        let lane = self.arr.place_auto(track, Clip::new(id, pos), len);
        self.grow_for_lanes(track, nl0);
        self.push_project();
        self.status = format!("track {}: recorded → clip in lane {}", track + 1, lane + 1);
    }

    /// Load (or reuse) an asset for a rack file path; returns its index.
    fn ensure_asset(&mut self, path: &Path) -> Option<usize> {
        if let Some(k) = self.assets.iter().position(|a| !a.dirty && a.path == path) {
            self.sel_asset = Some(k);
            return Some(k);
        }
        if crate::assets::is_auto_file(path) {
            return match std::fs::read_to_string(path)
                .map_err(|e| format!("{}: {e}", path.display()))
                .and_then(|s| crate::automation::Automation::from_json(&s))
            {
                Ok(auto) => {
                    let id = self.assets.len() as u32;
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.assets.push(Asset::automation_asset(
                        id,
                        name,
                        path.to_path_buf(),
                        std::sync::Arc::new(auto),
                        false,
                    ));
                    self.sel_asset = Some(id as usize);
                    self.push_project();
                    Some(id as usize)
                }
                Err(e) => {
                    self.status = e;
                    None
                }
            };
        }
        if crate::assets::is_midi_file(path) {
            return match Pattern::load_mid(path) {
                Ok(pat) => {
                    let id = self.assets.len() as u32;
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.assets.push(Asset::midi(
                        id,
                        name,
                        path.to_path_buf(),
                        Arc::new(pat),
                        false,
                    ));
                    self.sel_asset = Some(id as usize);
                    self.push_project();
                    Some(id as usize)
                }
                Err(e) => {
                    self.status = e;
                    None
                }
            };
        }
        if self.sample_rate == 0 {
            self.status = "rack: wait for the audio engine".into();
            return None;
        }
        match crate::audio_io::decode_to_mono(path, self.sample_rate) {
            Ok(v) => {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let id = self.assets.len() as u32;
                self.assets.push(Asset::audio(
                    id,
                    name,
                    path.to_path_buf(),
                    Arc::from(v),
                    false,
                ));
                self.sel_asset = Some(id as usize);
                // v0.13: push like the midi/auto branches do. Samplers read
                // the rack through the ENGINE's host table, and that table
                // only updates on SetProject — without this the freshly
                // decoded asset was invisible to the live audio thread and
                // the sampler kept replaying the old patch until an app
                // restart ("new sample needs a restart" report). Other
                // callers (clip placement) push again; idempotent.
                self.push_project();
                Some(id as usize)
            }
            Err(e) => {
                self.status = e;
                None
            }
        }
    }

    /// Transform the selected asset: mode 0 = quantize to grid,
    /// mode 1 = clip the silence off the ends. Content edits mark the asset
    /// dirty so the next save writes the new audio out.
    fn transform_asset(&mut self, k: usize, mode: u8) {
        if k >= self.assets.len() || self.sample_rate == 0 {
            return;
        }
        let res = if mode == 0 {
            let u = self.q_unit();
            quantize::quantize_asset(&self.assets[k].data, self.sample_rate, u)
        } else {
            quantize::trim_silence(&self.assets[k].data, self.sample_rate)
        };
        match res {
            Some(v) => {
                let a = &mut self.assets[k];
                a.data = Arc::from(v);
                a.dirty = true;
                let nm = a.name.clone();
                self.wave.remove(&(k as u32));
                self.status = if mode == 0 {
                    format!("asset “{nm}”: quantized to the grid")
                } else {
                    format!("asset “{nm}”: silence trimmed")
                };
                self.push_project();
            }
            None => self.status = "nothing to do (pure silence under the floor)".into(),
        }
    }

    /// Remove an asset from the project: its clips go with it, and the
    /// index-keyed clip ids shift down to follow the table.
    fn remove_asset_at(&mut self, k: usize) {
        if k >= self.assets.len() {
            return;
        }
        self.cancel_grid_gestures(); // its arr.remove_asset drops clips
        self.arr.remove_asset(k as u32);
        for tr in self.arr.tracks.iter_mut() {
            for lane in tr.lanes.iter_mut() {
                for c in lane.clips.iter_mut() {
                    if c.asset > k as u32 {
                        c.asset -= 1;
                    }
                }
            }
        }
        self.assets.remove(k);
        self.wave.clear();
        self.sel_asset = None;
        self.sel_clips.clear();
        self.sel_anchor = None;
        // Asset ids ARE table indices, so this removal invalidates every
        // id any open view still holds. Without this remap the piano roll
        // keeps editing whatever slid into (or past) the dead slot — the
        // v10.2 "deleting assets crashes to desktop" report.
        let mut roll_closed = false;
        if let Some(r) = self.piano_roll.as_mut() {
            match r.edit_asset {
                Some(id) if (id as usize) == k => roll_closed = true,
                Some(id) if (id as usize) > k => r.edit_asset = Some(id - 1),
                _ => {}
            }
        }
        if roll_closed {
            self.piano_roll = None;
        }
        // v0.15 bug hunt: the automation manager holds an asset id too.
        // Without this, deleting ANY earlier asset made overwrite-save
        // write into the wrong asset; deleting its own asset left the
        // editor saving nowhere at all.
        let mut mgr_closed = false;
        if let Some(ed) = self.auto_edit.as_mut() {
            match ed.asset {
                Some(id) if id == k => mgr_closed = true,
                Some(id) if id > k => ed.asset = Some(id - 1),
                _ => {}
            }
        }
        if mgr_closed {
            self.auto_edit = None;
        }
        // v0.15 bug hunt: the clipboard holds asset ids too. Without the
        // same remap, copy -> delete -> paste placed clips pointing at a
        // NEIGHBOUR asset (or a dangling id).
        if let Some(cb) = self.clipboard.as_mut() {
            cb.items.retain(|it| it.asset as usize != k);
            for it in cb.items.iter_mut() {
                if it.asset as usize > k {
                    it.asset -= 1;
                }
            }
            if cb.items.is_empty() {
                self.clipboard = None;
            }
        }
        // An in-flight audition may reference the dead id; the name-based
        // toggle would stop a DIFFERENT asset's preview on the next click.
        self.preview_playing = None;
        self.push_project();
        self.status = if mgr_closed {
            "asset removed (its clips too); automation manager closed with it".into()
        } else if roll_closed {
            "asset removed (its clips too); piano roll closed with it".into()
        } else {
            "asset removed (its clips too)".into()
        };
    }

    // ---- projects --------------------------------------------------------
    fn projects_dir(&self) -> PathBuf {
        let t = self.project_dir.trim();
        if t.is_empty() {
            home_dir().join("slooper-projects")
        } else {
            PathBuf::from(t)
        }
    }

    /// Ctrl+S: save under the current name, or open the name prompt for an
    /// untitled project (Shift always re-asks).
    fn request_save(&mut self, force_name: bool) {
        if self.proj_name.is_empty() || force_name {
            self.save_draft = self.proj_name.clone();
            self.show_save_prompt = true;
        } else {
            let name = self.proj_name.clone();
            self.do_save(&name);
        }
    }

    /// Snapshot the GUI master copies into a `.slooper` document.
    fn build_project(&self, name: String) -> Project {
        let tracks: Vec<ProjTrack> = (0..MAX_LOOPS)
            .map(|t| ProjTrack {
                vol: self.trk_vol[t],
                gain: self.trk_gain[t],
                pan: self.trk_pan[t],
                on: self.backend.engine.track_on(t),
                fx: self.fx_rows[t]
                    .iter()
                    .filter_map(|r| match r {
                        ChainRow::Script { stem, params } => Some(TrackFxSave {
                            stem: stem.clone(),
                            params: params.clone(),
                        }),
                        ChainRow::Vst { .. } => None,
                    })
                    .collect(),
                chain: self.fx_rows[t]
                    .iter()
                    .map(|r| match r {
                        ChainRow::Script { stem, params } => ChainSave::Fx {
                            stem: stem.clone(),
                            params: params.clone(),
                        },
                        ChainRow::Vst {
                            path,
                            kind,
                            name,
                            params,
                            entry: _,
                            open_rx: _,
                            state: _,
                        } => ChainSave::Vst {
                            path: path.clone(),
                            kind: kind.tag().to_string(),
                            name: name.clone(),
                            params: params.clone(),
                        },
                    })
                    .collect(),
                lanes: self.arr.tracks[t]
                    .lanes
                    .iter()
                    .map(|l| {
                        l.clips
                            .iter()
                            .map(|c| ProjClip {
                                asset: c.asset,
                                pos: c.pos,
                                cut: c.cut,
                                play_len: c.play_len,
                                gain: c.gain,
                                fade_in: c.fade_in,
                                fade_out: c.fade_out,
                            })
                            .collect()
                    })
                    .collect(),
            })
            .collect();
        Project {
            format: project::FORMAT.into(),
            version: project::VERSION,
            name,
            n_tracks: Some(self.n_tracks),
            metro: ProjMetro {
                bpm: self.bpm(),
                num: self.beats(),
                sub: SUBDIVISIONS[self.metro_sub].0,
            },
            quantize: ProjQuantize {
                q_sub: SUBDIVISIONS[self.q_sub].0,
                snap_sub: SUBDIVISIONS[self.snap_sub].0,
                auto: self.auto_quantize,
                loop_tl: self.loop_tl,
            },
            playhead: self.backend.engine.pos(),
            tracks,
            assets: self
                .assets
                .iter()
                .map(|a| ProjAsset {
                    name: a.name.clone(),
                    file: a.path.to_string_lossy().into_owned(),
                })
                .collect(),
            track_synths: self.track_synth.clone(),
            track_synth_params: self.track_synth_params.clone(),
        }
    }

    /// Write audio for new/edited assets, then the project file itself.
    fn do_save(&mut self, raw: &str) {
        let name = project::sanitize_name(raw);
        if name.is_empty() {
            self.status = "save: the project needs a name".into();
            return;
        }
        let dir = self.projects_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.status = format!("save: {e}");
            return;
        }
        let audio_dir = dir.join(format!("{name} audio"));
        for a in self.assets.iter_mut() {
            if !a.dirty && !a.path.as_os_str().is_empty() {
                continue;
            }
            if std::fs::create_dir_all(&audio_dir).is_err() {
                self.status = format!("save: cannot write {}", audio_dir.display());
                return;
            }
            let stem: String = a
                .name
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if let Some(auto) = a.automation.clone() {
                // v0.11 R5: automation assets round-trip as .slauto JSON.
                let p = audio_dir.join(format!(
                    "{:03}-{}.slauto",
                    a.id,
                    if stem.is_empty() {
                        "automation".to_string()
                    } else {
                        stem
                    }
                ));
                match std::fs::write(&p, auto.to_json().unwrap_or_default()) {
                    Ok(()) => {
                        a.path = p;
                        a.dirty = false;
                    }
                    Err(e) => self.status = format!("save: {e}"),
                }
                continue;
            }
            if let Some(pat) = a.pattern.clone() {
                // Pattern assets round-trip as Standard MIDI Files.
                let p = audio_dir.join(format!(
                    "{:03}-{}.mid",
                    a.id,
                    if stem.is_empty() {
                        "pattern".to_string()
                    } else {
                        stem
                    }
                ));
                if let Err(e) = pat.save_mid(&p) {
                    self.status = format!("save: {e}");
                    return;
                }
                a.path = p;
                a.dirty = false;
                continue;
            }
            let p = audio_dir.join(format!(
                "{:03}-{}.wav",
                a.id,
                if stem.is_empty() {
                    "asset".to_string()
                } else {
                    stem
                }
            ));
            match crate::audio_io::export(
                &p,
                self.sample_rate,
                &a.data,
                crate::audio_io::ExportFormat::Wav,
            ) {
                Ok(()) => {
                    a.path = p;
                    a.dirty = false;
                }
                Err(e) => {
                    self.status = format!("save: {e}");
                    return;
                }
            }
        }
        let pr = self.build_project(name.clone());
        match project::save(&project::project_path(&dir, &name), &pr) {
            Ok(()) => {
                self.proj_name = name.clone();
                self.show_save_prompt = false;
                self.status = format!("saved project “{name}”");
            }
            Err(e) => self.status = format!("save: {e}"),
        }
        self.save_config();
    }

    /// Replace the whole working state with a saved project. Assets resolve
    /// through the project's audio folder first, then the rack folders.
    fn do_load(&mut self, path: &Path) {
        // v18: a loaded project replaces the whole arrange — any armed
        // grid gesture points into the OLD indices.
        self.cancel_grid_gestures();
        let pr = match project::load(path) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("load: {e}");
                return;
            }
        };
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut dirs = vec![dir.clone(), dir.join(format!("{} audio", pr.name))];
        for f in &self.rack {
            let p = PathBuf::from(f.path.trim());
            if !p.as_os_str().is_empty() {
                dirs.push(p);
            }
        }
        let sr = self.sample_rate.max(1);
        let mut assets = Vec::new();
        let mut missing = 0usize;
        for (i, pa) in pr.assets.iter().enumerate() {
            let found = project::resolve_asset(&pa.file, &dirs);
            // v0.11 R5: automation curves load straight from .slauto JSON.
            if let Some(fp) = found
                .clone()
                .filter(|p| crate::assets::is_auto_file(p.as_path()))
            {
                match std::fs::read_to_string(&fp)
                    .map_err(|e| e.to_string())
                    .and_then(|s| crate::automation::Automation::from_json(&s))
                {
                    Ok(auto) => {
                        assets.push(Asset::automation_asset(
                            i as u32,
                            pa.name.clone(),
                            fp,
                            Arc::new(auto),
                            false,
                        ));
                        continue;
                    }
                    Err(_) => missing += 1,
                }
            }
            // MIDI pattern assets have their own decoder (.mid).
            if found.as_deref().is_some_and(crate::assets::is_midi_file) {
                let fp = found.unwrap_or_default();
                match Pattern::load_mid(&fp) {
                    Ok(pat) => {
                        assets.push(Asset::midi(
                            i as u32,
                            pa.name.clone(),
                            fp,
                            Arc::new(pat),
                            false,
                        ));
                        continue;
                    }
                    Err(_) => missing += 1,
                }
            } else {
                match found
                    .as_ref()
                    .and_then(|p| crate::audio_io::decode_to_mono(p, sr).ok())
                {
                    Some(v) => assets.push(Asset::audio(
                        i as u32,
                        pa.name.clone(),
                        found.unwrap_or_default(),
                        Arc::from(v),
                        false,
                    )),
                    None => {
                        missing += 1;
                        // Keep the id space intact: missing audio becomes a
                        // silent placeholder (its clips simply play nothing).
                        assets.push(Asset::audio(
                            i as u32,
                            pa.name.clone(),
                            PathBuf::new(),
                            Arc::from(vec![]),
                            false,
                        ));
                    }
                }
            }
        }
        let mut arr = Arrangement::new();
        for (t, pt) in pr.tracks.iter().enumerate().take(MAX_LOOPS) {
            self.trk_vol[t] = pt.vol;
            self.trk_gain[t] = pt.gain;
            self.trk_pan[t] = pt.pan;
            self.backend.engine.set_loop_vol(t, vol_amp(pt.vol));
            self.backend.engine.set_loop_gain(t, gain_amp(pt.gain));
            self.backend.engine.set_loop_pan(t, pan01(pt.pan));
            self.backend.cmd(Cmd::SetTrackOn { i: t, on: pt.on });
            while arr.tracks[t].lanes.len() < pt.lanes.len() {
                arr.tracks[t].add_lane();
            }
            for (k, lane) in pt.lanes.iter().enumerate() {
                while arr.tracks[t].lanes.len() <= k {
                    arr.tracks[t].add_lane();
                }
                for c in lane {
                    arr.insert(
                        t,
                        k,
                        Clip {
                            asset: c.asset,
                            pos: c.pos,
                            cut: c.cut,
                            play_len: c.play_len,
                            gain: c.gain,
                            fade_in: c.fade_in,
                            fade_out: c.fade_out,
                        },
                    );
                }
            }
            // Chains: prefer the full saved chain (scripts + plugins);
            // legacy fx-only projects materialize script rows. Plugin rows
            // open lazily on the first rebuild after the rate is known.
            let chain_src: Vec<ChainSave> = if !pt.chain.is_empty() {
                pt.chain.clone()
            } else {
                pt.fx
                    .iter()
                    .map(|s| ChainSave::Fx {
                        stem: s.stem.clone(),
                        params: s.params.clone(),
                    })
                    .collect()
            };
            self.fx_rows[t] = chain_src
                .into_iter()
                .filter_map(|s| match s {
                    ChainSave::Fx { stem, params } => Some(ChainRow::Script { stem, params }),
                    ChainSave::Vst {
                        path,
                        kind,
                        name,
                        params,
                    } => {
                        let kind = crate::vsthost::VstKind::parse(&kind)?;
                        Some(ChainRow::Vst {
                            path,
                            kind,
                            name,
                            params,
                            entry: None,
                            open_rx: None,
                            state: None,
                        })
                    }
                })
                .collect();
            self.rebuild_fx(t);
        }
        self.assets = assets;
        self.arr = arr;
        // The saved project carries its own configured track count.
        self.n_tracks = pr.n_tracks.unwrap_or(16).clamp(1, MAX_LOOPS) as usize;
        // Per-track synths (missing slots fall back to the default piano).
        for t in 0..MAX_LOOPS {
            self.track_synth[t] = pr.track_synths.get(t).cloned().flatten();
            self.track_synth_params[t] = pr.track_synth_params.get(t).cloned().unwrap_or_default();
            // v20: harvested editor patches are session-only — they do not
            // cross a project boundary.
            self.track_synth_state[t] = None;
        }
        self.rebuild_all_synths();
        // Heights are a per-session view; the new arrangement starts fresh.
        for h in self.trk_height.iter_mut() {
            *h = 0.0;
        }
        self.wave.clear();
        self.sel_asset = None;
        self.sel_clips.clear();
        self.sel_anchor = None;
        self.metro_bpm = pr.metro.bpm.to_string();
        self.metro_num = pr.metro.num.to_string();
        self.metro_sub = SUBDIVISIONS
            .iter()
            .position(|(c, _)| *c == pr.metro.sub)
            .unwrap_or(0);
        self.q_sub = SUBDIVISIONS
            .iter()
            .position(|(c, _)| *c == pr.quantize.q_sub)
            .unwrap_or(3);
        self.snap_sub = SUBDIVISIONS
            .iter()
            .position(|(c, _)| *c == pr.quantize.snap_sub)
            .unwrap_or(3);
        self.auto_quantize = pr.quantize.auto;
        self.loop_tl = pr.quantize.loop_tl;
        self.backend.cmd(Cmd::SetLoop(self.loop_tl));
        self.send_loop_region();
        self.proj_name = pr.name.clone();
        self.show_load = false;
        self.send_metro();
        self.backend.cmd(Cmd::Seek { pos: pr.playhead });
        self.push_project();
        self.save_config();
        self.status = if missing == 0 {
            format!("loaded project “{}” ({} assets)", pr.name, pr.assets.len())
        } else {
            format!(
                "loaded “{}”: {} asset(s) missing (silent)",
                pr.name, missing
            )
        };
    }

    fn do_export(&mut self) {
        let mut path = PathBuf::from(self.export_path.trim());
        if path.as_os_str().is_empty() {
            self.status = "export: choose a destination first".into();
            return;
        }
        let fmt = crate::audio_io::ExportFormat::ALL[self.export_fmt];
        // The selected format drives the extension: the destination is
        // picked without one, so switching formats never means re-picking.
        let ext = fmt.extension();
        let has_want = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext));
        if !has_want {
            path.set_extension(ext);
        }
        if self.export_sel {
            if self.sel_clips.is_empty() {
                self.status = "export selection: select at least one clip first".into();
                return;
            }
            let mut items = self.sel_clips.clone();
            items.sort();
            self.backend.cmd(Cmd::ExportSelection { path, fmt, items });
            self.show_export = false;
            return;
        }
        if self.export_whole {
            self.backend.cmd(Cmd::ExportProject { path, fmt });
        } else {
            self.backend.cmd(Cmd::ExportTrack {
                i: self.selected,
                path,
                fmt,
            });
        }
        self.show_export = false;
    }

    fn fire_midi_action(&mut self, a: MidiAction) {
        match a {
            MidiAction::Select(i) => self.selected = i,
            MidiAction::RecSelected => self.toggle_record(self.selected),
            MidiAction::ClearSelected => self.clear_track(self.selected),
            MidiAction::PlaySelected => self.backend.cmd(Cmd::ToggleTrackOn { i: self.selected }),
            MidiAction::Export => self.show_export = true,
            MidiAction::RecLoop(i) => self.toggle_record(i),
            MidiAction::ClearLoop(i) => self.clear_track(i),
            MidiAction::PlayLoop(i) => self.backend.cmd(Cmd::ToggleTrackOn { i }),
            MidiAction::Metronome => {
                self.metro_on = !self.metro_on;
                self.send_metro();
            }
            // Selection NEVER wraps: it stops at track 1 and track 16.
            MidiAction::SelectNext => self.selected = (self.selected + 1).min(self.n_tracks - 1),
            MidiAction::SelectPrev => self.selected = self.selected.saturating_sub(1),
            // The global transport: PlayAll toggles play/stop (space-bar
            // style), StopAll always parks (the playhead holds its place).
            MidiAction::PlayAll => {
                if self.backend.engine.transport_on() {
                    self.backend.cmd(Cmd::StopAll);
                } else {
                    self.backend.cmd(Cmd::PlayAll);
                }
            }
            MidiAction::StopAll => self.backend.cmd(Cmd::StopAll),
            // Value-driven knob actions fire only through the MIDI path
            // (handle_evt passes the CC value); a key press has no value.
            MidiAction::VolLoop(_) | MidiAction::GainLoop(_) | MidiAction::PanLoop(_) => {}
            MidiAction::MidiRec(i) => self.toggle_midi_rec(i),
            MidiAction::Reserved => {}
        }
    }

    fn handle_evt(&mut self, evt: Evt) {
        match evt {
            Evt::Unavailable(msg) => self.status = msg,
            Evt::Ready {
                client_name,
                sample_rate,
            } => {
                self.client_name = Some(client_name.clone());
                self.sample_rate = sample_rate;
                // Synth instances need the rate: create the default piano
                // and every bound track synth now.
                self.rebuild_all_synths();
                self.status = "JACK ready — select ports, then Apply".into();
                self.backend.cmd(Cmd::SetLatency {
                    ms: self.latency_ms.trim().parse().unwrap_or(0.0),
                });
                self.backend.cmd(Cmd::SetLoop(self.loop_tl));
                self.backend.cmd(Cmd::SetMonitor(self.monitor_enabled));
                self.send_metro();
                // Effect instances made before the backend was up were
                // created with rate 0; rebuild every chain at the real rate.
                // Chains (scripts + plugin rows) rebuild now at the real
                // rate; plugin rows still unopened open here (a plugin
                // whose file vanished simply stays unopened).
                for t in 0..MAX_LOOPS {
                    if !self.fx_rows[t].is_empty() {
                        self.rebuild_fx(t);
                    }
                }
                // Re-apply the port bindings persisted from the previous
                // session so audio flows on launch without re-clicking.
                if let Some((ins, outs)) = &self.applied_ports {
                    if !ins.is_empty() || !outs.is_empty() {
                        self.backend.cmd(Cmd::Apply {
                            ins: ins.clone(),
                            outs: outs.clone(),
                        });
                    }
                }
                self.push_project();
            }
            Evt::Ports { ins, outs } => {
                self.avail_in = ins;
                self.avail_out = outs;
            }
            Evt::Applied { ins, outs } => {
                self.applied = Some((ins, outs));
                self.status = format!("applied: {} in / {} out", ins, outs);
            }
            Evt::Transport { on } => {
                // The engine stops audition voices with the transport
                // (v0.10: previews never double over real playback);
                // mirror the flag so the rack stops highlighting.
                self.preview_playing = None;
                if !on {
                    // An armed MIDI take ends with the transport (audio
                    // takes stop for the same reason).
                    for t in 0..self.n_tracks {
                        if self.midi_rec[t].armed {
                            self.commit_midi_rec(t);
                        }
                    }
                }
                self.status = if on {
                    "transport running".into()
                } else {
                    "transport stopped".into()
                };
            }
            Evt::Take { track, at, data } => self.take_pipeline(track, at, data),

            Evt::Midi(raw) => {
                if let Some(msg) = MidiMsg::parse(&raw.bytes) {
                    // Bindable messages drive every bound action (Settings →
                    // MIDI Bindings); value/velocity are ignored.
                    if let Some(bind) = MidiBind::from_msg(&msg) {
                        // Live capture: a row in "listening" state takes the
                        // first bindable message it sees, like key learning.
                        if let Some(slot) = self.midi_learning_slot {
                            if self.midi_binds[slot] != Some(bind) {
                                self.midi_binds[slot] = Some(bind);
                                // A plain rebind replaces the axis: drop any
                                // stale min/max calibration for the slot.
                                self.knob_cal[slot] = None;
                                self.save_config();
                                self.status = format!(
                                    "bound {} → {}",
                                    MidiAction::ALL[slot].label(),
                                    bind.label()
                                );
                            }
                            self.midi_learning_slot = None;
                        } else if let Some(kl) = self.knob_learn.as_mut() {
                            // Three-step knob learn in flight (axis → min → max).
                            // The AXIS is the message TYPE + CHANNEL only — the
                            // data byte (CC value / note velocity / program
                            // number) is the VALUE steps 2 and 3 read, and must
                            // NEVER be part of the axis identity. Binding the
                            // data byte in step 1 is what locked min and max to
                            // the same value; matching d1 later rejected every
                            // sweep message from controllers that re-use
                            // numbers mid-sweep.
                            let cval = match &msg {
                                MidiMsg::ControlChange { value, .. } => Some(*value),
                                MidiMsg::NoteOn { velocity, .. } if *velocity > 0 => {
                                    Some(*velocity)
                                }
                                MidiMsg::ProgramChange { program, .. } => Some(*program),
                                _ => None,
                            };
                            if !kl.axis_locked {
                                // Step 1 — pick the axis. Until it is locked,
                                // the most recent message wins, so moving a
                                // different knob (or landing on another
                                // channel) simply re-selects the axis.
                                kl.bind = bind;
                                kl.has_axis = true;
                                if let Some(v) = cval {
                                    kl.last_val = Some(v);
                                }
                                self.status = format!(
                                    "axis {} — press Lock axis, or move a different control to change it",
                                    kl.bind.axis_label()
                                );
                            } else if !kl.bind.same_axis(&bind) {
                                self.status = format!(
                                    "knob learn: got {} — min and max must use the \
                                     axis {} (same message type and channel)",
                                    bind.label(),
                                    kl.bind.axis_label()
                                );
                            } else if let Some(v) = cval {
                                // Constant listeners: the number tracks the
                                // MOST RECENT message on this axis until
                                // Lock min / Finish freezes it.
                                if !kl.locked {
                                    kl.lo = v;
                                } else {
                                    kl.hi = v;
                                }
                            }
                        } else {
                            // Value for continuous (knob) bindings: the CC
                            // byte, note velocity, or program number. The
                            // knob wizard may have captured a raw min/max
                            // other than 0..=127 — rescale through the slot's
                            // calibration so the control's full physical
                            // travel maps onto the full program range (and
                            // inverted wiring is corrected).
                            let mut mgr_muted = false;
                            // v0.15 automation manager: a picked record
                            // source is MUTED in the main window while the
                            // manager is open; with REC armed its raw value
                            // draws the curve (percent of the target range:
                            // 0 = min = -inf, 127 = max = +6 dB on volume).
                            let mgr_slot = self.auto_mgr_rec.iter().find(|(slot, b)| {
                                if MidiAction::ALL[*slot].is_continuous() {
                                    b.same_axis(&bind)
                                } else {
                                    *b == bind
                                }
                            });
                            if mgr_slot.is_some() && self.auto_edit.is_some() {
                                if let Some(ed) = self.auto_edit.as_mut() {
                                    if ed.rec {
                                        let raw = match &msg {
                                            MidiMsg::ControlChange { value, .. } => Some(*value),
                                            MidiMsg::NoteOn { velocity, .. } => Some(*velocity),
                                            _ => None,
                                        };
                                        if let Some(raw) = raw {
                                            let (rmin, rmax) =
                                                crate::automation::target_range(&ed.target);
                                            let v = rmin + (raw as f32 / 127.0) * (rmax - rmin);
                                            let trel = self
                                                .backend
                                                .engine
                                                .pos()
                                                .saturating_sub(ed.rec_start.max(0) as u64)
                                                as i64;
                                            let skip = ed.nodes.last().is_some_and(|last| {
                                                (last.v - v).abs() < 1e-4
                                                    && (trel - last.t)
                                                        < (self.sample_rate.max(1) as i64 / 50)
                                            });
                                            // v0.15 bug hunt: never grow the
                                            // curve past its declared span —
                                            // nodes beyond len_samples would
                                            // pile up invisible and collapse
                                            // onto the endpoint on reload.
                                            let skip = skip || trel >= ed.len_samples.max(1);
                                            if !skip {
                                                ed.nodes.push(crate::automation::Node {
                                                    t: trel,
                                                    v,
                                                    s: crate::automation::Shape::Linear,
                                                });
                                                ed.changed = true;
                                            }
                                        }
                                    }
                                }
                                mgr_muted = true;
                            }
                            let cval = match &msg {
                                MidiMsg::ControlChange { value, .. } => Some(*value),
                                MidiMsg::NoteOn { velocity, .. } if *velocity > 0 => {
                                    Some(*velocity)
                                }
                                MidiMsg::ProgramChange { program, .. } => Some(*program),
                                _ => None,
                            };
                            for i in 0..N_ACTIONS {
                                if mgr_muted {
                                    break;
                                }
                                let Some(b) = self.midi_binds[i] else {
                                    continue;
                                };
                                // A continuous (knob) binding owns its whole
                                // axis (type + channel) — the wizard never
                                // binds the data byte, so CC-number drift
                                // mid-sweep keeps driving the parameter.
                                let hit = if MidiAction::ALL[i].is_continuous() {
                                    b.same_axis(&bind)
                                } else {
                                    b == bind
                                };
                                if hit {
                                    let a = MidiAction::ALL[i];
                                    let v = cval.map(|r| match self.knob_cal[i] {
                                        Some(c) => scale_axis(r, c.lo, c.hi),
                                        None => r,
                                    });
                                    match a {
                                        MidiAction::VolLoop(t) => {
                                            if let Some(v) = v {
                                                self.set_trk_vol(t, v);
                                                self.levels_dirty = true;
                                            }
                                        }
                                        MidiAction::GainLoop(t) => {
                                            if let Some(v) = v {
                                                self.set_trk_gain(t, v);
                                                self.levels_dirty = true;
                                            }
                                        }
                                        MidiAction::PanLoop(t) => {
                                            if let Some(v) = v {
                                                self.set_trk_pan(t, v);
                                                self.levels_dirty = true;
                                            }
                                        }
                                        _ => self.fire_midi_action(a),
                                    }
                                }
                            }
                        }
                    }
                    // MIDI recording tap: a message NO binding claims is
                    // captured verbatim into every armed track's buffer.
                    if self.midi_rec.iter().any(|r| r.armed) && !self.midi_msg_is_bound(&msg) {
                        self.capture_midi_msg(&msg);
                    }
                    let line = MidiLine {
                        ts: Self::now_stamp(),
                        type_name: msg.type_name(),
                        describe: msg.describe(),
                        raw_hex: hex(&raw.bytes),
                    };
                    // "print the last detected MIDI input" — also to stdout.
                    println!(
                        "[{}] {:<12} {}  | {}",
                        line.ts, line.type_name, line.describe, line.raw_hex
                    );
                    self.last_midi = Some(line.clone());
                    self.midi_log.push_front(line);
                    while self.midi_log.len() > 300 {
                        self.midi_log.pop_back();
                    }
                }
            }

            Evt::MidiSources(names) => {
                self.midi_sources = names;
            }
            Evt::Notice(msg) => self.status = msg,
            Evt::Xrun => {}
            Evt::Error(e) => self.status = e,
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        ctx.set_zoom_factor(self.gui_scale);
        // Keep the update loop ticking so backend events (MIDI, port
        // changes, transport) are drained promptly; while the transport
        // runs, a take rolls, or a preview auditions, tick at ~50 fps so
        // the playhead is smooth.
        let active = self.backend.engine.transport_on()
            || self.backend.engine.preview_active()
            || (0..MAX_LOOPS).any(|t| self.backend.engine.rec_state_of(t) == TR_REC);
        ctx.request_repaint_after(std::time::Duration::from_millis(if active {
            20
        } else {
            100
        }));

        self.handle_shortcuts(ctx);

        let mut changed = false;
        while let Ok(evt) = self.backend.evt_rx.try_recv() {
            self.handle_evt(evt);
            changed = true;
        }

        // VST2 editor windows: drop ones the user closed (or whose plugin
        // refused to open), and stale ones whose track swapped plugins —
        // before the held-drop below, so `effEditClose` always precedes
        // plugin teardown.
        self.reap_vst_editors();

        // v16: land async plugin work (chain loads, folder scan) without
        // ever blocking this thread on plugin instantiation.
        if self.poll_vst_opens() {
            ctx.request_repaint();
        }

        // Late VST releases: plugins replaced or removed since the last
        // tick drop here, so destructors (and the VST3 teardown the runner
        // hands off) run on the creating thread, never the audio thread.
        for e in self.backend.engine.drain_vst_held() {
            drop(e);
        }

        let p = f32::from_bits(self.backend.peak.load(Ordering::Relaxed));
        self.peak_disp = p.max(self.peak_disp * 0.9);

        if changed {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        self.ui_root(ui);
    }
}

impl App {
    /// The whole window's content. Split out from [`eframe::App::ui`] so
    /// headless tests can drive the real widget tree (panels, DnD, mixer)
    /// through `egui::Context::run` without an eframe frame.
    pub fn ui_root(&mut self, ui: &mut egui::Ui) {
        // ---- v18: OS file drops (drain exactly once per frame) ----
        // winit delivers file-manager drops through raw input. Folders
        // become rack folders on the spot; audio/midi files queue for
        // whoever owns the target: an open SAMPLER editor owns drops
        // (the v0.13 contract: swap the live patch), then lane strips
        // place clips at the pointer while painting, and anything the
        // pointer never visited is imported into the rack at frame end.
        // Previously ONLY the sampler window listened, so dropping a
        // sample anywhere else did nothing — "dragging audio clips in
        // is nonfunctional".
        let fresh: Vec<std::path::PathBuf> = ui.ctx().input_mut(|i| {
            i.raw
                .dropped_files
                .drain(..)
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for path in fresh {
            if path.is_dir() {
                let mut f = RackFolder::new(path.to_string_lossy().into_owned());
                f.rescan();
                let n = f.files.len();
                self.rack.push(f);
                self.save_config();
                self.status = format!(
                    "rack: added folder {} ({n} file(s))",
                    path.file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                );
            } else {
                self.os_drops.push(path);
            }
        }
        if !self.os_drops.is_empty() {
            if let Some(t) = self.sampler_editor_open() {
                let paths = std::mem::take(&mut self.os_drops);
                self.sampler_files_dropped(t, &paths);
            }
        }
        // ---- global wheel routing (must run first thing in the frame) ----
        // Ctrl/Cmd+wheel zooms the timeline (same state as the zoom slider);
        // Shift+wheel pans it horizontally. egui turns MouseWheel into
        // smooth_scroll_delta (and its own interface zoom) while processing
        // input — BEFORE any panel code runs — so the only reliable place to
        // claim those gestures is here: remove the raw events and zero the
        // deltas, then apply them ourselves. Plain wheel is left alone so
        // ScrollAreas (rack, mixer, track stack) keep scrolling normally.
        // Zoom accumulates in log space (sum of delta·scale, then exp) so
        // scrolling in then out by the same amount returns exactly, several
        // events in one frame compose geometrically, and the per-event
        // scale differs by wheel unit: one notch is a LINE, a trackpad
        // flick reports PIXELS.
        let mut zoom_ln = 0.0f32;
        let mut pan = 0.0f32;
        let bpm_rect = self.bpm_rect;
        let mut rmb_bpm: Option<egui::Pos2> = None;
        // While the pointer sits over an open piano-roll canvas its
        // ctrl/alt-wheel gestures belong to the ROLL (time / row zoom) —
        // leave those events for `piano_roll_window`'s own router.
        let over_roll = self
            .piano_roll
            .as_ref()
            .and_then(|r| r.canvas_rect)
            .is_some_and(|r| ui.ctx().pointer_hover_pos().is_some_and(|p| r.contains(p)));
        ui.ctx().input_mut(|i| {
            i.events.retain(|e| match e {
                egui::Event::PointerButton {
                    button: egui::PointerButton::Secondary,
                    pressed: true,
                    pos,
                    ..
                } => {
                    // v0.15: right-click on the bpm box answers with the
                    // automation menu, not the text-edit copy/paste menu —
                    // swallow the press and open it ourselves.
                    if bpm_rect.is_some_and(|r| r.contains(*pos)) {
                        rmb_bpm = Some(*pos);
                        false
                    } else {
                        true
                    }
                }
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } => {
                    if (modifiers.command || modifiers.ctrl) && !(over_roll && !modifiers.shift) {
                        let s = match unit {
                            egui::MouseWheelUnit::Line => ZOOM_WHEEL_LINE,
                            _ => ZOOM_WHEEL_PX,
                        };
                        zoom_ln += (delta.y * s).clamp(-1.5, 1.5);
                        false
                    } else if modifiers.shift {
                        // Line-unit notches carry ~3 units per click; scale
                        // them so one notch pans a real chunk of the view
                        // (the old unscaled `delta * 2.0` moved ~6 px and
                        // read as "way too slow").
                        let s = match unit {
                            egui::MouseWheelUnit::Line => PAN_WHEEL_LINE,
                            _ => 1.0,
                        };
                        pan += (delta.x + delta.y) * s;
                        false
                    } else {
                        true
                    }
                }
                _ => true,
            });
            if zoom_ln != 0.0 || pan != 0.0 {
                i.smooth_scroll_delta = egui::Vec2::ZERO;
            }
        });
        if zoom_ln != 0.0 {
            self.px_per_sec = (self.px_per_sec * zoom_ln.exp()).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if pan != 0.0 {
            self.hscroll = (self.hscroll - pan).max(0.0);
        }
        if let Some(pos) = rmb_bpm {
            let cur = self
                .metro_bpm
                .trim()
                .parse::<f32>()
                .unwrap_or(self.bpm() as f32);
            self.value_menu = Some((
                self.selected,
                crate::automation::TARGET_MASTER_TEMPO.to_string(),
                cur,
                pos,
            ));
        }

        // Master volume: same -inf..=+6 dB MIDI mapping as every track.
        let mamp = vol_amp(self.master_midi);
        self.backend.gain.store(mamp.to_bits(), Ordering::Relaxed);

        if self.sel_sent != self.selected {
            self.sel_sent = self.selected;
            self.backend.cmd(Cmd::SetSelected(self.selected));
        }

        // Flush level / chain edits to the config once the pointer is up:
        // sliders fire per pixel and knobs per message.
        if self.levels_dirty && !ui.ctx().input(|i| i.pointer.any_down()) {
            self.levels_dirty = false;
            self.save_config();
        }
        if self.fx_save_pending && !ui.ctx().input(|i| i.pointer.any_down()) {
            self.fx_save_pending = false;
            self.save_config();
        }

        top_bar(ui, self);
        // Left panels stack outward in add order, so rendering SETTINGS
        // before the rack pops it up to the rack's LEFT; the rack and the
        // timeline simply shift right while it is open.
        if self.show_settings {
            settings_panel(ui, self);
        }
        rack_panel(ui, self);
        mixer_panel(ui, self);
        midi_monitor(ui, self);
        central_panels(ui, self);

        // Watchdog: a scrub/clip/marquee drag that somehow loses its
        // release event must not stick (rack drops now run on egui's
        // native DnD; gestures now ARM on press, so every armed gesture
        // must also die on button-up, drag or no drag).
        if !ui.ctx().input(|i| i.pointer.any_down()) {
            self.scrub_drag = false;
            self.drag_clip = None;
            self.clip_resize = None;
            self.marquee = None;
            self.marquee_pending = None;
            self.drag_loop_handle = None;
            if self.sep_drag.take().is_some() {
                self.save_config();
            }
        }

        if self.show_export {
            export_window(ui, self);
        }
        save_prompt_window(ui, self);
        load_window(ui, self);
        knob_learn_window(ui, self);
        if self.fx_window.is_some() {
            effects_window(ui, self);
        }
        if self.fx_editor.is_some() {
            self.fx_editor_window(ui);
        }
        if self.piano_roll.is_some() {
            self.piano_roll_window(ui);
        }
        if self.auto_edit.is_some() {
            self.auto_window(ui);
            self.auto_pick_window(ui);
        }
        let sw = self.synth_win;
        if let Some(t) = sw {
            self.synth_sliders_window(ui, t);
        }
        let sp = self.synth_pick;
        if let Some(t) = sp {
            synth_pick_window(ui, self, t);
        }
        self.value_popups(ui);
        // v18: OS drops that never landed on a lane strip (dropped over
        // the rack panel, mixer, or anywhere else without a specific
        // target) import into the rack: `ensure_asset` dedups and selects,
        // so the new asset is immediately pickable.
        if !self.os_drops.is_empty() {
            let paths = std::mem::take(&mut self.os_drops);
            let mut names: Vec<String> = Vec::new();
            let mut bad: Vec<String> = Vec::new();
            for path in &paths {
                let shown = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let importable = crate::assets::is_audio_file(path)
                    || crate::assets::is_midi_file(path)
                    || crate::assets::is_auto_file(path);
                match importable.then(|| self.ensure_asset(path)).flatten() {
                    Some(ai) => names.push(
                        self.assets
                            .get(ai)
                            .map(|a| a.name.clone())
                            .unwrap_or(shown),
                    ),
                    None => bad.push(shown),
                }
            }
            let mut status = if names.is_empty() {
                String::new()
            } else {
                format!("rack: imported {}", names.join(", "))
            };
            if !bad.is_empty() {
                let part = format!("unsupported: {}", bad.join(", "));
                status = if status.is_empty() {
                    part
                } else {
                    format!("{status} \u{b7} skipped {part}")
                };
            }
            if !status.is_empty() {
                self.status = status;
            }
        }
    }

    /// v0.15: the two right-click popups every automatable value answers
    /// with — the desktop-style mini menu (for widgets that cannot host an
    /// egui context menu: the bpm box, FX-script bridge sliders) and the
    /// "Set value" dialog (1:1 user units).
    fn value_popups(&mut self, ui: &mut egui::Ui) {
        // ---- mini menu at the click point ----
        if let Some((track, target, current, pos)) = self.value_menu.clone() {
            // One frame after opening: the right-click that OPENED it must
            // not also count as the click-away that closes it.
            let armed = self.value_menu_armed;
            self.value_menu_armed = true;
            let mut done = false;
            let resp = egui::Window::new(crate::automation::describe_target(&target))
                .id(egui::Id::new("value_menu"))
                .fixed_pos(pos)
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    let tgt = target.clone();
                    done = value_menu_items(ui, self, track, &tgt, current);
                });
            if let Some(r) = resp {
                let rect = r.response.rect;
                // v18: same click-away rule, stated once: a fresh press of
                // either button (or Escape) OUTSIDE the menu dismisses it.
                // (The right-click that OPENED the menu releases on top of
                // it, which is inside — it never dismisses itself.)
                let click_elsewhere = armed
                    && ui.ctx().input(|i| {
                        (i.pointer.button_clicked(egui::PointerButton::Primary)
                            || i.pointer.button_clicked(egui::PointerButton::Secondary)
                            || i.key_pressed(egui::Key::Escape))
                            && !rect
                                .expand(4.0)
                                .contains(i.pointer.hover_pos().unwrap_or(rect.center()))
                    });
                if done || click_elsewhere {
                    self.value_menu = None;
                    self.value_menu_armed = false;
                }
            }
        } else {
            self.value_menu_armed = false;
        }
        // ---- Set value dialog ----
        if let Some((track, target, mut txt)) = self.value_dialog.clone() {
            let unit = value_unit_label(&target);
            let title = if target == TARGET_PSEUDO_GAIN {
                "track gain".to_string()
            } else {
                crate::automation::describe_target(&target)
            };
            let mut apply = false;
            let mut cancel = false;
            let mut open = true;
            egui::Window::new(format!("Set value \u{b7} {title}"))
                .id(egui::Id::new("value_dialog"))
                .default_pos([420.0, 300.0])
                .default_size([280.0, 90.0])
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(
                        RichText::new(format!("Value in {unit} \u{b7} Enter applies, Esc cancels"))
                            .weak()
                            .small(),
                    );
                    let re = ui.add_sized(
                        egui::vec2(240.0, 22.0),
                        egui::TextEdit::singleline(&mut txt).font(egui::TextStyle::Monospace),
                    );
                    if self.value_dialog_focus {
                        re.request_focus();
                        self.value_dialog_focus = false;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Apply").clicked() {
                            apply = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                    // Persist the LIVE edit buffer: egui writes the in-progress
                    // text back into `txt` every frame, and the next frame
                    // clones it fresh from self.value_dialog — without storing
                    // it here, each keystroke would be reset before the next.
                    if let Some(d) = self.value_dialog.as_mut() {
                        d.2 = txt.clone();
                    }
                    if re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        apply = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        cancel = true;
                    }
                });
            if apply {
                if self.apply_user_value(track, &target, &txt) {
                    self.value_dialog = None;
                }
            } else if cancel || !open {
                self.value_dialog = None;
            }
        }
    }
}

fn top_bar(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::top("topbar").exact_size(44.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            // Hamburger (settings) button: three horizontal bars.
            let size = egui::vec2(26.0, 18.0);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
            if rect.width() > 4.0 && rect.height() > 4.0 {
                let p = ui.painter();
                for k in 0..3u32 {
                    let y = rect.min.y + 4.0 + k as f32 * 5.0;
                    p.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(rect.min.x + 3.0, y),
                            egui::vec2(20.0, 2.0),
                        ),
                        1.0,
                        if app.show_settings {
                            Color32::WHITE
                        } else {
                            Color32::from_rgb(200, 200, 205)
                        },
                    );
                }
            }
            if resp.on_hover_text("Settings").clicked() {
                app.show_settings = !app.show_settings;
            }

            ui.separator();
            let title = match &app.proj_name {
                n if n.is_empty() => "slooper · untitled".to_string(),
                n => format!("slooper · {n}"),
            };
            ui.heading(RichText::new(title).strong());
            let status = match &app.client_name {
                Some(_) => format!("{} Hz \u{b7} {}", app.sample_rate, app.status),
                None => app.status.clone(),
            };
            // v18: the status lives in a FIXED-WIDTH, truncating field. It
            // used to be a bare label in the middle of the bar, so every
            // longer message (e.g. after creating an automation asset)
            // shoved the transport, tempo box and everything else rightward
            // by hundreds of pixels — mid-session the widgets you had just
            // right-clicked literally moved out from under the cursor, and
            // on a narrow window they vanished off-screen entirely.
            ui.add_sized(
                egui::vec2(300.0, 18.0),
                egui::Label::new(RichText::new(status).weak()).truncate(),
            )
            .on_hover_text(app.status.clone());
            if let Some((i, o)) = app.applied {
                ui.label(RichText::new(format!("applied: {} in / {} out", i, o)).weak());
            }
            let xruns = app.backend.xruns.load(Ordering::Relaxed);
            if xruns > 0 {
                ui.label(RichText::new(format!("xruns: {}", xruns)).color(Color32::YELLOW));
            }

            // ---- transport ----
            ui.separator();
            let on = app.backend.engine.transport_on();
            let play = if on { "STOP" } else { "PLAY" };
            let play_btn = ui.button(RichText::new(play).strong()).on_hover_text(
                "Global transport (Space): all sounding tracks play together \
                 from the playhead. Stop holds the position.",
            );
            if play_btn.clicked() {
                app.backend
                    .cmd(if on { Cmd::StopAll } else { Cmd::PlayAll });
            }
            ui.label(RichText::new(pos_label(app)).monospace());

            // ---- metronome / grid tempo ----
            ui.separator();
            // Start/stop button (was a checkbox): lights up while the
            // click runs, like the transport button next to it.
            let metro_btn = ui
                .add_sized(
                    [58.0, 20.0],
                    egui::Button::selectable(
                        app.metro_on,
                        RichText::new(if app.metro_on { "STOP" } else { "METRO" })
                            .strong()
                            .small(),
                    ),
                )
                .on_hover_text(
                    "Metronome start/stop: ticks the grid at the playhead \
                     without moving it (never recorded). M also toggles it.",
                );
            if metro_btn.clicked() {
                app.metro_on = !app.metro_on;
                app.send_metro();
            }
            ui.label("bpm");
            // Bound straight to the app state: the old pattern of cloning
            // into a local EVERY FRAME wiped each keystroke before it could
            // land, so the box could never be edited (it snapped back to
            // the saved value). Commit to the engine on Enter / focus-out.
            let bpm_rt = ui
                .add(
                    egui::TextEdit::singleline(&mut app.metro_bpm)
                        .desired_width(38.0)
                        .horizontal_align(egui::Align::Center)
                        .font(egui::TextStyle::Monospace),
                )
                .on_hover_text(
                    "Grid tempo 20..300 BPM. Press Enter to apply; right-click for automation.",
                );
            app.bpm_rect = Some(bpm_rt.rect);
            if bpm_rt.lost_focus() {
                app.send_metro();
            }
            ui.label("beat");
            let num_rt = ui
                .add(
                    egui::TextEdit::singleline(&mut app.metro_num)
                        .desired_width(24.0)
                        .horizontal_align(egui::Align::Center)
                        .font(egui::TextStyle::Monospace),
                )
                .on_hover_text("Beats per measure 1..16. Press Enter to apply.");
            if num_rt.lost_focus() {
                app.send_metro();
            }

            // File actions; the master volume + peak meter moved to the top
            // of the mixer dock (`master_strip`).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Export").clicked() {
                    app.show_export = true;
                }
                if ui
                    .button("Load")
                    .on_hover_text("Browse saved projects")
                    .clicked()
                {
                    app.show_load = true;
                }
                if ui
                    .button("Save as")
                    .on_hover_text("Save under a new name")
                    .clicked()
                {
                    app.request_save(true);
                }
                let save_tip = match &app.proj_name {
                    n if n.is_empty() => "Save — asks for a project name (Ctrl+S)".to_string(),
                    n => format!("Save “{n}” (Ctrl+S)"),
                };
                if ui.button("Save").on_hover_text(save_tip).clicked() {
                    app.request_save(false);
                }
            });
        });
    });
}

/// Playhead position as `bar.beat` + `m:ss.t`.
fn pos_label(app: &App) -> String {
    let sr = app.sample_rate.max(1) as f64;
    let pos = app.backend.engine.pos();
    let beats = quantize::beat_samples(app.sample_rate, app.bpm());
    let total_beats = pos as f64 / beats;
    let num = app.beats() as f64;
    let bar = (total_beats / num).floor() as u64 + 1;
    let beat = (total_beats % num).floor() as u64 + 1;
    let t = pos as f64 / sr;
    format!("{}.{}  {}:{:04.1}", bar, beat, (t / 60.0) as u64, t % 60.0)
}

fn base_name(p: &str) -> String {
    Path::new(p.trim())
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.trim().to_string())
}

/// Label for a curve value: trim meaningless decimals but keep absolute
/// digits (the user reads/enters real values, not percentages).
fn format_gv(v: f32) -> String {
    if v.abs() >= 100.0 || v == v.round() {
        format!("{:.0}", v)
    } else if v.abs() >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    }
}

/// Canvas hit-test: node index within 8 px of `pos`, if any.
fn nearest_auto_node(
    nodes: &[crate::automation::Node],
    pos: egui::Pos2,
    x_of: impl Fn(i64) -> f32,
    y_of: impl Fn(f32) -> f32,
) -> Option<usize> {
    let mut best: Option<(f32, usize)> = None;
    for (idx, n) in nodes.iter().enumerate() {
        let d = (x_of(n.t) - pos.x).powi(2) + (y_of(n.v) - pos.y).powi(2);
        if d < 64.0 && best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, idx));
        }
    }
    best.map(|(_, i)| i)
}

/// v0.15: the right-click menu shared by every automatable widget (mixer
/// levels, mute, master, tempo, synth / chain parameters) — exactly two
/// actions, like a desktop context menu: "Set value" (typed 1:1 in the
/// target's USER unit, replacing the old always-on text box) and "Create
/// automation asset" (a flat curve at the current value, editable later in
/// the automation manager).
fn value_menu_items(
    ui: &mut egui::Ui,
    app: &mut App,
    track: usize,
    target: &str,
    current: f32,
) -> bool {
    if ui
        .button(format!(
            "Set value   [{}]",
            value_user_display(target, current)
        ))
        .on_hover_text(format!(
            "Type an exact value ({}). Enter applies. Volume is dB: 0 = +0 dB, -20 = -20 dB.",
            value_unit_label(target)
        ))
        .clicked()
    {
        app.value_dialog = Some((
            track,
            target.to_string(),
            value_user_display(target, current),
        ));
        app.value_dialog_focus = true;
        // v18: deliberately NO ui.close() in here. One host of these items
        // is the manual mini-menu WINDOW — and ui.close() there marks that
        // window's id as closed in egui's memory FOREVER (the window has no
        // open-flag), so every future attempt to open the value menu paints
        // nothing: "right-click automation stopped working". The context
        // menu host closes itself in `auto_menu`; the window host simply
        // stops drawing once `value_menu` clears.
        return true;
    }
    if ui
        .button("Create automation asset")
        .on_hover_text(
            "Create a red automation asset holding this exact value and place it at the \
             playhead; edit its curve any time in the automation manager.",
        )
        .clicked()
    {
        app.create_flat_auto_asset(track, target, current);
        return true;
    }
    false
}

/// Context-menu host for the shared value items (volume/pan/synth/fx
/// sliders right-click through here).
fn auto_menu(ui: &mut egui::Ui, app: &mut App, track: usize, target: &str, current: f32) {
    // v18: this host is an egui CONTEXT menu: dismiss it once an item was
    // chosen (the shared items cannot call ui.close() themselves — see the
    // note there about the mini-menu window's closed-state memory).
    if value_menu_items(ui, app, track, target, current) {
        ui.close();
    }
}

/// Rough current position of a target, 0..1, for the bind popup's faux
/// strips (display only — the popup binds by TARGET ID, not by this).
fn target_preview_frac(app: &App, track: usize, id: &str) -> f32 {
    use crate::automation::*;
    let g = |v: &[u8]| v.get(track).map(|m| *m as f32 / 127.0).unwrap_or(0.5);
    match id {
        TARGET_TRACK_VOL => g(&app.trk_vol),
        TARGET_PSEUDO_GAIN => g(&app.trk_gain),
        TARGET_TRACK_PAN => g(&app.trk_pan),
        TARGET_TRACK_MUTE => {
            if app.backend.engine.track_on(track) {
                0.0
            } else {
                1.0
            }
        }
        TARGET_MASTER_VOL => app.master_midi as f32 / 127.0,
        TARGET_MASTER_TEMPO => {
            let bpm: f32 = app.metro_bpm.trim().parse().unwrap_or(120.0);
            ((bpm - 20.0) / 280.0).clamp(0.0, 1.0)
        }
        _ => {
            // synth:/fx: rows — read the live value when reachable.
            if let Some(rest) = id.strip_prefix("synth:") {
                let k: usize = rest.parse().unwrap_or(0);
                return app
                    .track_synth_params
                    .get(track)
                    .and_then(|p| p.get(k).copied())
                    .unwrap_or(0.5);
            }
            if let Some(rest) = id.strip_prefix("fx:") {
                let mut it = rest.split(':');
                if let (Some(r), Some(i)) = (
                    it.next().and_then(|v| v.parse::<usize>().ok()),
                    it.next().and_then(|v| v.parse::<usize>().ok()),
                ) {
                    if let Some(ChainRow::Script { params, .. }) =
                        app.fx_rows.get(track).and_then(|rows| rows.get(r))
                    {
                        return params.get(i).copied().unwrap_or(0.5);
                    }
                }
            }
            0.5
        }
    }
}

/// Unit label a "Set value" dialog shows for a target.
fn value_unit_label(target: &str) -> &'static str {
    match target {
        crate::automation::TARGET_TRACK_VOL
        | crate::automation::TARGET_MASTER_VOL
        | TARGET_PSEUDO_GAIN => "dB",
        crate::automation::TARGET_TRACK_PAN => "-100..+100",
        crate::automation::TARGET_MASTER_TEMPO => "BPM",
        crate::automation::TARGET_TRACK_MUTE => "0 / 1",
        _ => "%",
    }
}

/// Pseudo-target id for the manual-only gain trim (no automation domain).
const TARGET_PSEUDO_GAIN: &str = "track.gain";

/// Internal target value -> user-unit display string (volume/gain in dB so
/// typing 0 means +0 dB; plugin/synth params in 0..100 %).
fn value_user_display(target: &str, v: f32) -> String {
    use crate::automation::*;
    match target {
        TARGET_TRACK_VOL | TARGET_MASTER_VOL | TARGET_PSEUDO_GAIN => db_num(v),
        TARGET_TRACK_PAN => format!("{}", (v * 200.0 - 100.0).round() as i32),
        TARGET_MASTER_TEMPO => format!("{v:.0}"),
        TARGET_TRACK_MUTE => format!("{v:.0}"),
        _ => format!("{:.1}", v * 100.0),
    }
}

/// Parse a user-entered value into the target's INTERNAL unit. Volume/gain
/// are typed in dB (0 dB -> 1.0 linear, -20 -> 0.1, "-inf" -> 0); pan in
/// -100..+100; tempo raw BPM; plugin/synth params in 0..100 percent.
fn value_user_parse(target: &str, s: &str) -> Option<f32> {
    use crate::automation::*;
    let t: String = s.trim().to_ascii_lowercase().replace('\u{2212}', "-");
    let t = t.strip_suffix("db").unwrap_or(&t).trim().to_string();
    // v0.15 bug hunt: f32::parse ACCEPTS "nan"/"inf" — an unguarded
    // value slid NaN through clamp (NaN.clamp is NaN) straight into the
    // engine. Every branch must demand a finite number.
    let num = |t: &str| -> Option<f32> { t.parse::<f32>().ok().filter(|x| x.is_finite()) };
    match target {
        TARGET_TRACK_VOL | TARGET_MASTER_VOL | TARGET_PSEUDO_GAIN => {
            if t == "-inf" {
                return Some(0.0);
            }
            let db = num(&t)?;
            Some(10f32.powf(db / 20.0).clamp(0.0, 2.0))
        }
        TARGET_TRACK_PAN => {
            let p = num(&t)?;
            Some(((p.clamp(-100.0, 100.0) + 100.0) / 200.0).clamp(0.0, 1.0))
        }
        TARGET_MASTER_TEMPO => num(&t).map(|bpm| bpm.clamp(20.0, 300.0)),
        TARGET_TRACK_MUTE => num(&t).map(|m| m.clamp(0.0, 1.0)),
        _ => {
            let p = num(&t)?;
            Some((p / 100.0).clamp(0.0, 1.0))
        }
    }
}

/// Target id list for one track: the fixed per-track controls, the bound
/// synth's parameters (script or plugin — plugin parameters carry the
/// NAMES the plugin publishes through its parameter description, the
/// documented host-visible value system), the FX chain rows, and the two
/// global controls.
fn auto_targets(app: &App, t: usize) -> Vec<(String, String)> {
    let mut out = vec![
        (
            crate::automation::TARGET_TRACK_VOL.to_string(),
            "track volume (linear)".to_string(),
        ),
        (
            crate::automation::TARGET_TRACK_PAN.to_string(),
            "track pan (0=L, 0.5=C, 1=R)".to_string(),
        ),
        (
            crate::automation::TARGET_TRACK_MUTE.to_string(),
            "track mute (\u{2265}0.5 = muted)".to_string(),
        ),
    ];
    // The track's instrument: script synths expose their descriptor's
    // parameter labels; plugins expose the NAMES THEY PUBLISH through the
    // API's parameter description — the documented host-visible value
    // system, exactly what the user asked to be reachable.
    let stem = app
        .track_synth
        .get(t)
        .cloned()
        .flatten()
        .unwrap_or_default();
    if !stem.is_empty() {
        if let Some(d) = app.synth_registry.desc(&stem) {
            for (k, pd) in d.params.iter().enumerate() {
                out.push((format!("synth:{k}"), format!("synth \u{b7} {}", pd.label)));
            }
        } else if let Some(Some(h)) = app.track_synth_inst.get(t) {
            if let SynthHandle::Vst(e) = h.as_ref() {
                for (k, d) in e.descs().iter().enumerate() {
                    if d.writable {
                        out.push((format!("synth:{k}"), format!("synth \u{b7} {}", d.name)));
                    }
                }
            }
        }
    }
    if let Some(rows) = app.fx_rows.get(t) {
        for (r, row) in rows.iter().enumerate() {
            match row {
                ChainRow::Vst {
                    entry: Some(e),
                    name,
                    ..
                } => {
                    for (k, d) in e.descs().iter().enumerate() {
                        if d.writable {
                            out.push((format!("fx:{r}:{k}"), format!("{name} \u{b7} {}", d.name)));
                        }
                    }
                }
                ChainRow::Vst { name, params, .. } => {
                    for k in 0..params.len() {
                        out.push((format!("fx:{r}:{k}"), format!("{name} \u{b7} param {k}")));
                    }
                }
                ChainRow::Script { stem, params } => {
                    let d = app.fx_registry.desc(stem);
                    match d {
                        Some(d) => {
                            let nm = d.name.clone();
                            for (k, pd) in d.params.iter().enumerate() {
                                out.push((
                                    format!("fx:{r}:{k}"),
                                    format!("{nm} \u{b7} {}", pd.label),
                                ));
                            }
                        }
                        None => {
                            for k in 0..params.len() {
                                out.push((
                                    format!("fx:{r}:{k}"),
                                    format!("{stem} \u{b7} param {k}"),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    out.push((
        crate::automation::TARGET_MASTER_VOL.to_string(),
        "master volume (linear)".to_string(),
    ));
    out.push((
        crate::automation::TARGET_MASTER_TEMPO.to_string(),
        "tempo (BPM)".to_string(),
    ));
    out
}

/// Rack row snapshot: (folder path, [(file name, path, is_midi, is_auto)], error).
type RackRow = (String, Vec<(String, PathBuf, bool, bool)>, Option<String>);

/// One rack file row: click = load + audition, drag = place a clip.
/// Audio and MIDI entries share it (MIDI decodes via `ensure_asset`).
fn rack_file_row(ui: &mut egui::Ui, app: &mut App, fi: usize, name: &str, fp: &Path, kind: u8) {
    let sel = app
        .sel_asset
        .is_some_and(|k| app.assets.get(k).is_some_and(|a| a.path == fp));
    // Full-width row: a plain selectable_label shrink-wraps to the text,
    // leaving most of the row dead to clicks; available_size() sizing made
    // every row fill the panel (the "huge hat" bug).
    let w = ui.available_width();
    let row = ui.add_sized(
        egui::vec2(w, 20.0),
        egui::Button::selectable(sel, RichText::new(name).small()),
    );
    // Right-edge kind badge (audio vs MIDI). Painted into the row's own
    // rect, so it costs no layout width and never wraps the name.
    rack_kind_badge(ui, row.rect, kind);
    // ONE click_and_drag response for the row: a separate drag-sense
    // overlay on top would CAPTURE the press and kill clicked().
    let dr = ui.interact(
        row.rect,
        ui.id().with(("rackdrag", fi, name)),
        Sense::click_and_drag(),
    );
    if dr.clicked() {
        // Click = select + audition (asset preview).
        if let Some(k) = app.ensure_asset(fp) {
            app.preview_asset(k);
        }
    }
    if dr.drag_started() {
        if let Some(k) = app.ensure_asset(fp) {
            app.sel_asset = Some(k);
            // egui-native drag: the payload travels with the gesture and
            // lane strips receive the release themselves.
            dr.dnd_set_drag_payload(k);
        }
    }
    row.on_hover_text(format!(
        "{} \u{b7} {} \u{b7} click to audition (preview), drag onto a lane to place a clip{}",
        fp.display(),
        match kind {
            1 => "MIDI pattern",
            2 => "automation curve",
            _ => "audio",
        },
        match kind {
            1 => " \u{b7} plays through the track's synth",
            2 => " \u{b7} drives the control it targets, on the track it sits on",
            _ => "",
        }
    ));
}

/// Tiny right-aligned kind badge drawn inside an asset row: amber
/// "AUDIO" / blue "MIDI" (the same blue the MREC button uses, so MIDI
/// assets read as MIDI everywhere). Painted into the row rect after the
/// button — no layout width, no wrapping risk (the justified-label trap).
fn rack_kind_badge(ui: &mut egui::Ui, row: egui::Rect, kind: u8) {
    // v0.11 R5: automation is the third kind and wears red, matching its
    // red clips on the grid.
    let (txt, col) = match kind {
        1 => ("MIDI", Color32::from_rgb(150, 200, 255)),
        2 => ("AUTO", Color32::from_rgb(255, 120, 120)),
        _ => ("AUDIO", Color32::from_rgb(210, 175, 120)),
    };
    ui.painter().text(
        egui::pos2(row.max.x - 5.0, row.center().y),
        egui::Align2::RIGHT_CENTER,
        txt.to_string(),
        egui::FontId::proportional(9.0),
        col,
    );
}

/// The asset rack: folders (primary first = recordings land there), one
/// draggable row per audio file, project-only assets below, and the
/// selected-asset toolbar (quantize / trim / remove).
fn rack_panel(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::left("asset_rack")
        .resizable(true)
        .default_size(260.0)
        .min_size(190.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("ASSET RACK").strong());
                if ui
                    .button("+ folder")
                    .on_hover_text("Add an audio folder to the rack")
                    .clicked()
                {
                    if let Some(p) = rfd::FileDialog::new()
                        .set_title("Add rack folder")
                        .pick_folder()
                    {
                        let mut f = RackFolder::new(p.to_string_lossy().into_owned());
                        f.rescan();
                        app.rack.push(f);
                        app.save_config();
                    }
                }
            });
            if app.rack.is_empty() {
                ui.label(
                    RichText::new(
                        "Add a folder: its audio files become clips you can \
                         drag onto tracks. The FIRST folder also receives \
                         every recording as a WAV.",
                    )
                    .weak()
                    .small(),
                );
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Snapshot so rows can call &mut App methods (ensure_asset).
                    let snap: Vec<RackRow> = app
                        .rack
                        .iter()
                        .map(|f| {
                            (
                                f.path.clone(),
                                f.files
                                    .iter()
                                    .map(|r| (r.name.clone(), r.path.clone(), r.is_midi, r.is_auto))
                                    .collect(),
                                f.err.clone(),
                            )
                        })
                        .collect();
                    let mut rescan = None;
                    let mut browse = None;
                    let mut rm = None;
                    for (fi, (path, files, err)) in snap.into_iter().enumerate() {
                        ui.add_space(3.0);
                        ui.separator();
                        ui.horizontal(|ui| {
                            let tag = if fi == 0 { " [primary]" } else { "" };
                            ui.label(
                                RichText::new(format!("{}{}", base_name(&path), tag))
                                    .strong()
                                    .small(),
                            )
                            .on_hover_text(format!(
                                "{}{}",
                                path,
                                if fi == 0 {
                                    " (primary — recordings are stored here)"
                                } else {
                                    ""
                                }
                            ));
                            if ui.small_button("scan").clicked() {
                                rescan = Some(fi);
                            }
                            if ui
                                .small_button("...")
                                .on_hover_text("Change folder")
                                .clicked()
                            {
                                browse = Some(fi);
                            }
                            if ui
                                .small_button("x")
                                .on_hover_text("Remove from rack")
                                .clicked()
                            {
                                rm = Some(fi);
                            }
                        });
                        if let Some(e) = err {
                            ui.label(
                                RichText::new(format!("× {}", e))
                                    .color(Color32::from_rgb(220, 120, 120))
                                    .small(),
                            );
                        }
                        // Two sections per folder: AUDIO then MIDI clips.
                        // Section headers show whenever a section has entries
                        // (previously only when BOTH did, so a folder of pure
                        // MIDI files carried no label at all), and every row
                        // additionally carries a kind badge.
                        let audio: Vec<(String, PathBuf)> = files
                            .iter()
                            .filter(|(_, _, m, a)| !*m && !*a)
                            .map(|(n, p, _, _)| (n.clone(), p.clone()))
                            .collect();
                        let midis: Vec<(String, PathBuf)> = files
                            .iter()
                            .filter(|(_, _, m, _)| *m)
                            .map(|(n, p, _, _)| (n.clone(), p.clone()))
                            .collect();
                        let autos: Vec<(String, PathBuf)> = files
                            .iter()
                            .filter(|(_, _, _, a)| *a)
                            .map(|(n, p, _, _)| (n.clone(), p.clone()))
                            .collect();
                        if !audio.is_empty() {
                            ui.label(
                                RichText::new("audio")
                                    .weak()
                                    .small()
                                    .color(Color32::from_rgb(210, 175, 120)),
                            );
                        }
                        for (name, fp) in audio {
                            rack_file_row(ui, app, fi, &name, &fp, 0);
                        }
                        if !midis.is_empty() {
                            ui.label(
                                RichText::new("MIDI")
                                    .weak()
                                    .small()
                                    .color(Color32::from_rgb(150, 200, 255)),
                            );
                        }
                        for (name, fp) in midis {
                            rack_file_row(ui, app, fi, &name, &fp, 1);
                        }
                        if !autos.is_empty() {
                            ui.label(
                                RichText::new("AUTOMATION")
                                    .weak()
                                    .small()
                                    .color(Color32::from_rgb(255, 120, 120)),
                            );
                        }
                        for (name, fp) in autos {
                            rack_file_row(ui, app, fi, &name, &fp, 2);
                        }
                    }
                    if let Some(fi) = rescan {
                        if let Some(f) = app.rack.get_mut(fi) {
                            f.rescan();
                        }
                    }
                    if let Some(fi) = browse {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_title("Rack folder")
                            .pick_folder()
                        {
                            app.rack[fi].path = p.to_string_lossy().into_owned();
                            app.rack[fi].rescan();
                            app.save_config();
                        }
                    }
                    if let Some(fi) = rm {
                        if app.rack.len() > 1 || fi != 0 {
                            app.rack.remove(fi);
                        } else {
                            app.rack.clear();
                        }
                        app.save_config();
                    }

                    // Project-only assets: recordings with no rack folder,
                    // dirty edits, and files whose folder was removed.
                    let mut extra: Vec<(usize, String, bool, u8)> = Vec::new();
                    for (k, a) in app.assets.iter().enumerate() {
                        let in_rack = app
                            .rack
                            .iter()
                            .any(|f| f.files.iter().any(|r| r.path == a.path && !a.dirty));
                        if !in_rack {
                            extra.push((
                                k,
                                a.name.clone(),
                                a.dirty,
                                if a.is_auto() {
                                    2u8
                                } else if a.is_midi() {
                                    1
                                } else {
                                    0
                                },
                            ));
                        }
                    }
                    if !extra.is_empty() {
                        ui.add_space(3.0);
                        ui.separator();
                        ui.label(RichText::new("project assets").weak().small());
                        for (k, name, dirty, kind) in extra {
                            let sel = app.sel_asset == Some(k);
                            let w = ui.available_width();
                            let row = ui.add_sized(
                                egui::vec2(w, 20.0),
                                egui::Button::selectable(
                                    sel,
                                    RichText::new(format!(
                                        "{}{}",
                                        name,
                                        if dirty { " *" } else { "" }
                                    ))
                                    .small(),
                                ),
                            );
                            rack_kind_badge(ui, row.rect, kind);
                            // Merged click+drag response (see rack rows above).
                            let dr = ui.interact(
                                row.rect,
                                ui.id().with(("assetdrag", k)),
                                Sense::click_and_drag(),
                            );
                            if dr.clicked() {
                                app.sel_asset = Some(k);
                                app.preview_asset(k);
                            }
                            if dr.drag_started() {
                                dr.dnd_set_drag_payload(k);
                            }
                            row.on_hover_text("part of this project (not a rack file) — draggable");
                        }
                    }

                    // ---- selected-asset toolbar ----
                    ui.add_space(6.0);
                    ui.separator();
                    match app.sel_asset {
                        Some(k) if k < app.assets.len() => {
                            let (nm, secs) = (
                                app.assets[k].name.clone(),
                                app.asset_len(k as u32) as f64 / app.sample_rate.max(1) as f64,
                            );
                            ui.label(RichText::new(format!("asset: {}", nm)).small());
                            ui.label(RichText::new(format!("{:.2} s", secs)).weak().small());
                            ui.horizontal(|ui| {
                                if ui
                                    .small_button("Quantize")
                                    .on_hover_text(format!(
                                        "Snap the content to the {} grid",
                                        SUBDIVISIONS[app.q_sub].1
                                    ))
                                    .clicked()
                                {
                                    app.transform_asset(k, 0);
                                }
                                if ui
                                    .small_button("Trim")
                                    .on_hover_text("Clip silence off the start and end")
                                    .clicked()
                                {
                                    app.transform_asset(k, 1);
                                }
                                if ui
                                    .small_button("Remove")
                                    .on_hover_text("Remove this asset and its clips")
                                    .clicked()
                                {
                                    app.remove_asset_at(k);
                                }
                            });
                        }
                        _ => {
                            ui.label(
                                RichText::new("select an asset to quantize, trim or remove")
                                    .weak()
                                    .small(),
                            );
                        }
                    }
                });
        });
}

/// The mixer dock: every track's controls, as one strip per track, pinned
/// to the right like the asset rack. Strip heights come from [`App::track_h`]
/// and the column is offset by the grid's scroll — so each strip sits exactly
/// level with its track, and clips/waveforms get the grid's full height.
/// Master output section at the TOP of the mixer dock — it lives in the
/// band between the panel header and the Track 1 strip (level with the
/// timeline grid header). ONE compact row: "master" label, slider, dB
/// readout, "0" zeroize and the peak meter — the band is only ~30 px
/// tall, so a second row would spill onto Track 1's widgets (a two-row
/// version did: click stealing). v0.15: the dB text box became a plain
/// readout; exact entry moved to right-click → Set value.
fn master_strip(ui: &mut egui::Ui, app: &mut App) {
    ui.spacing_mut().item_spacing.x = 5.0;
    ui.label(RichText::new("master").strong().small());
    {
        // v0.15: no text box — right-click opens Set value / Create
        // automation asset, matching every other automatable value.
        let mut mv = app.master_midi as i16;
        let msl = ui
            .add_sized(
                egui::vec2(44.0, 18.0),
                Slider::new(&mut mv, 0..=127).show_value(false),
            )
            .on_hover_text(
                "Master volume: -inf .. +6 dB. Right-click for exact entry and automation.",
            );
        let mvr = ui
            .add_sized(
                egui::vec2(50.0, 18.0),
                egui::Label::new(
                    RichText::new(format!("{:>5}", db_num(vol_amp(app.master_midi))))
                        .monospace()
                        .small(),
                )
                .selectable(false)
                .sense(egui::Sense::click()),
            )
            .on_hover_text("dB");
        if msl.changed() {
            app.master_midi = mv.clamp(0, 127) as u8;
            let amp = vol_amp(app.master_midi);
            app.note_auto(0, crate::automation::TARGET_MASTER_VOL, amp);
            app.levels_dirty = true;
        }
        let mmenu = |ui: &mut egui::Ui, app: &mut App| {
            auto_menu(
                ui,
                app,
                app.selected,
                crate::automation::TARGET_MASTER_VOL,
                vol_amp(app.master_midi),
            )
        };
        msl.context_menu(|ui| mmenu(ui, app));
        mvr.context_menu(|ui| mmenu(ui, app));
        if ui
            .small_button(RichText::new("0").monospace())
            .on_hover_text("Reset master volume to 0 dB")
            .clicked()
        {
            app.master_midi = VOL_UNITY_MIDI;
            app.levels_dirty = true;
        }
        // Peak meter last: it absorbs whatever width is left, so the fixed
        // widgets (box, slider, zeroize) can never be clipped off the edge
        // when the mixer dock is narrow.
        let meter_w = (ui.available_width() - ui.spacing().item_spacing.x).max(12.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(meter_w, 8.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 1.0, Color32::from_rgb(40, 40, 45));
        let frac = (app.peak_disp * 0.9).clamp(0.0, 1.0);
        let color = if app.peak_disp > 0.99 {
            Color32::RED
        } else if app.peak_disp > 0.7 {
            Color32::YELLOW
        } else {
            Color32::LIGHT_GREEN
        };
        if frac > 0.01 {
            painter.rect_filled(
                egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height())),
                1.0,
                color,
            );
        }
    }
}

fn mixer_panel(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::right("mixer")
        .resizable(true)
        .default_size(250.0)
        .min_size(210.0)
        .show(ui, |ui| {
            ui.heading(RichText::new("MIXER").strong());
            ui.separator();
            let base = ui.available_rect_before_wrap();
            // Clip to the visible column, then lay strips out on absolute
            // rects shifted by the grid's vertical scroll (shared offset —
            // the alignment is the whole point of this panel). The column
            // starts at the timeline track stack's top edge (measured last
            // frame by `central_panels`), so strips sit level with tracks
            // even though the two panels' headers differ in height; any
            // leftover room between panels is intentionally left empty.
            ui.shrink_clip_rect(base);
            let top = if app.grid_anchor > 0.0 {
                app.grid_anchor.min(base.max.y)
            } else {
                base.min.y
            };
            let mut y = top - app.vscroll;
            // Master strip in the header band: from the panel header down
            // to where the track stack begins — the band level with the
            // timeline grid header, directly above the Track 1 strip.
            // The band gets an OPAQUE backdrop that belongs to it: without
            // one, scrolled track-strip widgets show through the transparent
            // header and overlay the master controls (user report).
            let band_bottom = top.max(base.min.y);
            if band_bottom - base.min.y > 24.0 {
                let band = egui::Rect::from_min_max(
                    egui::pos2(base.min.x, base.min.y),
                    egui::pos2(base.max.x, band_bottom),
                );
                let pfill = ui.visuals().panel_fill;
                ui.painter().rect_filled(band, 0.0, pfill);
                ui.painter().hline(
                    band.x_range(),
                    band.bottom() - 0.5,
                    egui::Stroke::new(1.0, Color32::from_gray(64)),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .id_salt("masterstrip")
                        .max_rect(band)
                        .layout(
                            egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(false),
                        ),
                    |ui| master_strip(ui, app),
                );
            }
            // Track strips may only paint BELOW the band — their absolute
            // rects shift with the shared vertical scroll, so without this
            // they'd draw over the master header when scrolled up.
            ui.shrink_clip_rect(egui::Rect::from_min_max(
                egui::pos2(base.min.x, band_bottom.max(base.min.y)),
                base.max,
            ));
            for t in 0..app.n_tracks {
                let th = app.track_h(t);
                if y + th >= base.min.y && y <= base.max.y {
                    let r = egui::Rect::from_min_size(
                        egui::pos2(base.min.x, y),
                        egui::vec2(base.width(), th),
                    );
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .id_salt(("mixstrip", t))
                            .max_rect(r)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                        |ui| mixer_strip(ui, app, t),
                    );
                    // v0.15: hairline under each strip — consecutive
                    // strips used to melt into one wall of controls.
                    ui.painter().hline(
                        r.x_range(),
                        r.max.y + TRACK_SEP_HIT * 0.5,
                        egui::Stroke::new(1.0, Color32::from_gray(48)),
                    );
                }
                y += th + TRACK_SEP_HIT;
            }
            let _ = ui.allocate_exact_size(base.size(), Sense::hover());
        });
}

/// One track's mixer strip: select / record / mute / child lanes, then the
/// vol-gain-pan stack and the FX chain chips.
fn mixer_strip(ui: &mut egui::Ui, app: &mut App, t: usize) {
    // v0.15 UI pass: strips live on fixed rects shared with the grid.
    // Anything that does not fit must CLIP, never bleed onto the
    // neighbour (the ghosted "Track 6" report).
    let strip_rect = ui.max_rect();
    ui.shrink_clip_rect(strip_rect);
    let sel = app.selected == t;
    let rec = app.backend.engine.rec_state_of(t) == TR_REC;
    let on = app.backend.engine.track_on(t);
    let nl = app.arr.tracks[t].lanes.len();
    let clips: usize = app.arr.tracks[t].lanes.iter().map(|l| l.clips.len()).sum();
    ui.horizontal(|ui| {
        let head = ui
            .selectable_label(sel, RichText::new(format!("Track {}", t + 1)).strong())
            .on_hover_text("Click to select \u{b7} right-click this track's grid for its menu (copy / paste / clear)");
        if head.clicked() {
            app.selected = t;
        }
        let rec_lbl = if rec { "STOP" } else { "REC" };
        if ui
            .small_button(RichText::new(rec_lbl).color(if rec {
                Color32::from_rgb(255, 90, 90)
            } else {
                Color32::from_rgb(220, 180, 120)
            }))
            .on_hover_text(
                "Record: the take becomes an asset plus a clip at the playhead; \
                 recording over an occupied lane opens a child lane automatically",
            )
            .clicked()
        {
            app.toggle_record(t);
        }
        let mute_resp = ui
            .small_button(if on { "on" } else { "mute" })
            .on_hover_text(
                "While the transport runs, a muted track contributes nothing \
                 (its clips stay on the timeline). Right-click for automation.",
            );
        mute_resp.context_menu(|ui| {
            auto_menu(
                ui,
                app,
                t,
                crate::automation::TARGET_TRACK_MUTE,
                if on { 0.0 } else { 1.0 },
            );
        });
        if mute_resp.clicked() {
            // v0.11 R5: mute rides automation too (1 = muted).
            app.note_auto(t, crate::automation::TARGET_TRACK_MUTE, if on { 0.0 } else { 1.0 });
            app.backend.cmd(Cmd::ToggleTrackOn { i: t });
        }
        let mrec = app.midi_rec[t].armed;
        if ui
            .small_button(RichText::new(if mrec { "M\u{25a0}" } else { "MREC" }).color(if mrec {
                Color32::from_rgb(120, 220, 255)
            } else {
                Color32::from_rgb(160, 190, 220)
            }))
            .on_hover_text(
                "MIDI record: captures every UNBOUND incoming MIDI message \
                 while armed; stop (button again or stop the transport) and \
                 the take becomes a MIDI asset + clip at the playhead",
            )
            .clicked()
        {
            app.toggle_midi_rec(t);
        }
        // v0.11 R5: write-automation arm (mirrors MREC for MIDI).
        let arec = app.auto_rec[t].armed;
        if ui
            .small_button(RichText::new(if arec { "A\u{25a0}" } else { "AREC" }).color(if arec {
                Color32::from_rgb(255, 130, 130)
            } else {
                Color32::from_rgb(200, 160, 160)
            }))
            .on_hover_text(
                "Automation record: while armed, MOVE any control on this track                  \u{2014} the first one touched becomes the target and its moves                  are captured as an automation curve. Stop (button again) and the                  take becomes a red automation asset + clip at the playhead.",
            )
            .clicked()
        {
            app.toggle_auto_rec(t);
        }
        if ui
            .small_button("+")
            .on_hover_text("Add a child lane (stacks additively)")
            .clicked()
        {
            app.arr.add_lane(t);
            app.grow_for_lanes(t, nl);
            app.push_project();
        }
        if nl > 1
            && ui
                .small_button("-")
                .on_hover_text("Remove the last child lane")
                .clicked()
        {
            let per = app.track_h(t) / nl.max(1) as f32;
            app.arr.remove_lane(t, nl - 1);
            app.set_track_h(t, per * (nl - 1).max(1) as f32);
            app.push_project();
        }
        ui.label(RichText::new(format!("{clips}c/{nl}L")).weak().small());
    });
    level_row(ui, app, t, KIND_VOL);
    // v0.15 UI pass: a one-lane track is 96px tall — the old stack of six
    // rows overflowed it and painted over the track below. Optional rows
    // now ask for room first; the leftovers fold into a popup.
    let room = ui.available_height();
    if room >= 92.0 {
        level_row(ui, app, t, KIND_GAIN);
        level_row(ui, app, t, KIND_PAN);
        strip_fx_row(ui, app, t);
        strip_synth_row(ui, app, t);
    } else {
        if room >= 70.0 {
            level_row(ui, app, t, KIND_PAN);
        }
        if ui.available_height() >= 20.0 {
            ui.horizontal(|ui| {
                let more = ui.small_button("\u{2026} more").on_hover_text(
                    "This strip is short \u{2014} click for gain, pan, FX and synth",
                );
                egui::Popup::menu(&more).show(|ui| {
                    level_row(ui, app, t, KIND_GAIN);
                    level_row(ui, app, t, KIND_PAN);
                    strip_fx_row(ui, app, t);
                    strip_synth_row(ui, app, t);
                });
            });
        }
    }
}

/// FX-chain row of a mixer strip (also rendered inside the "more" menu).
fn strip_fx_row(ui: &mut egui::Ui, app: &mut App, t: usize) {
    if ui
        .small_button("FX")
        .on_hover_text(
            "This track's mixer chain: effects scripts and VST plugins in one ordered list",
        )
        .clicked()
    {
        app.fx_window = Some(t);
    }
    fx_buttons(ui, app, t);
}

/// Synth-pick row of a mixer strip (also rendered inside the "more" menu).
/// v17: the old ComboBox is gone — the ♪ chip opens the SYNTHS window
/// (folder(s), kind-tagged list, border on the active synth), matching how
/// the FX chip opens the chain window.
fn strip_synth_row(ui: &mut egui::Ui, app: &mut App, t: usize) {
    // Per-track synthesizer for MIDI clips (default: bundled piano;
    // scanned VST instruments can be bound as "vst:<path>").
    let cur = app.track_synth[t].clone().unwrap_or_default();
    let show = if cur.is_empty() {
        "piano".to_string()
    } else if let Some((_, p)) = vst_synth_decode(&cur) {
        let name = app
            .vst_scan
            .iter()
            .find(|e| e.path.to_string_lossy() == p)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.to_string())
            });
        format!("{name} (VSTi)")
    } else {
        cur.clone()
    };
    if ui
        .small_button(RichText::new(format!("\u{266a} {show}")))
        .on_hover_text("This track's synthesizer \u{2014} opens the synth picker")
        .clicked()
    {
        app.synth_pick = Some(t);
    }
    if app.track_synth_inst[t].is_some() {
        // v0.11 R3: hosted plugins are plug-and-play -- this button
        // opens the plugin's OWN window (never a slider panel). Script
        // synths keep their parameter sliders.
        let is_vst = app.track_synth[t]
            .as_deref()
            .is_some_and(|s| vst_synth_decode(s).is_some());
        let label = if is_vst { "GUI" } else { "sliders" };
        let hover = if is_vst {
            "Open this plugin's own window (its controls live inside it)"
        } else {
            "This synth's parameters"
        };
        if ui.small_button(label).on_hover_text(hover).clicked() {
            if is_vst {
                app.toggle_synth_gui(t);
            } else {
                app.synth_win = Some(t);
            }
        }
    }
}

/// v17: the SYNTHS window — a simpler sibling of the mixer-chain window
/// that replaces the old per-track synth ComboBox. It shows WHERE
/// synthesizers live (the compiled-Rust-script folder and the plugin scan
/// folders), lists every choice with a kind tag (`[rust]` for native synth
/// scripts, `[vst2]`/`[vst3]` for plugin instruments), draws a BORDER
/// around the one bound to this track, and binds on click (a plugin
/// selection keeps the v0.10 standard: its own window opens with it).
fn synth_pick_window(ui: &mut egui::Ui, app: &mut App, t: usize) {
    if !app.vst_scanned && app.vst_scan_rx.is_none() {
        app.vst_rescan();
    }
    let cur = app.track_synth[t].clone().unwrap_or_default();
    let mut open = true;
    egui::Window::new(RichText::new(format!("SYNTHS \u{2014} track {}", t + 1)).strong())
        .open(&mut open)
        .min_width(540.0)
        .default_height(460.0)
        .show(ui, |ui| {
            ui.label(RichText::new("Synthesizers (click binds to this track):").strong());
            // (tag, label, stem-to-bind) for every choice: the default,
            // every script synth, every scanned plugin instrument.
            let mut rows: Vec<(&'static str, String, String)> =
                vec![("rust", "piano \u{2014} default".to_string(), String::new())];
            for stem in app.synth_registry.stems() {
                if stem != "piano" {
                    rows.push(("rust", stem.clone(), stem));
                }
            }
            for e in app.vst_scan.iter() {
                if !e.instrument || e.error.is_some() {
                    continue;
                }
                let tag = if e.kind == crate::vsthost::VstKind::V2 { "vst2" } else { "vst3" };
                rows.push((
                    tag,
                    format!("{} \u{2014} plugin instrument", e.name),
                    vst_synth_encode(e.kind, &e.path.to_string_lossy()),
                ));
            }
            let scanning = !app.vst_scanned || app.vst_scan_rx.is_some();
            egui::ScrollArea::vertical()
                .id_salt(("synthpicklist", t))
                .max_height(190.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let full_w = ui.available_width();
                    let n_rows = rows.len();
                    let mut pick: Option<String> = None;
                    for (tag, label, stem) in rows {
                        let active = stem == cur;
                        let txt = if active {
                            RichText::new(format!("[{tag}]  {label}")).strong()
                        } else {
                            RichText::new(format!("[{tag}]  {label}"))
                        };
                        let resp = ui.add_sized(
                            [full_w, 24.0],
                            egui::Button::new(txt).frame(false),
                        );
                        if resp.clicked() && !active {
                            pick = Some(stem);
                        }
                        if active {
                            // THE marker: a border around the selected synth.
                            let rect = resp.rect.expand(2.0);
                            ui.painter().rect(
                                rect,
                                4.0,
                                Color32::TRANSPARENT,
                                egui::Stroke::new(1.6, Color32::from_rgb(120, 190, 255)),
                                egui::StrokeKind::Middle,
                            );
                        }
                    }
                    if n_rows <= 2 {
                        ui.label(
                            RichText::new(if scanning {
                                "scanning plugin folders\u{2026}"
                            } else {
                                "(only script synths found \u{2014} drop a VST instrument into \
                                 one of the folders below and press Rescan Plugins)"
                            })
                            .weak()
                            .small(),
                        );
                    }
                    if let Some(stem) = pick {
                        // set_track_synth_stem auto-opens the plugin's GUI
                        // (v0.10 standard) and persists the binding.
                        app.set_track_synth_stem(t, &stem);
                    }
                });
            ui.label(
                RichText::new(
                    "[rust] native synth script (compiled Rust, in the script folder) \u{2014} \
                     [vst2] / [vst3] plugin instrument (a `.so` in a plugin folder). A border \
                     marks the synth this track is playing.",
                )
                .weak()
                .small(),
            );
            ui.separator();

            // --- where synthesizers live ----------------------------------
            ui.label(RichText::new("Synth script folder (native Rust):").strong());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(app.synth_registry.dir().display().to_string())
                        .monospace()
                        .small(),
                );
                if ui
                    .small_button("Rescan scripts")
                    .on_hover_text("Re-scan and re-compile the synth script folder")
                    .clicked()
                {
                    app.synth_registry.rescan();
                    app.status = "synths re-scanned".into();
                }
            });
            if !fx::rustc_available() {
                ui.label(
                    RichText::new(
                        "rustc is not on PATH \u{2014} synth scripts cannot compile. Install \
                         Rust (rustup).",
                    )
                    .color(Color32::YELLOW),
                );
            }
            ui.label(RichText::new("Plugin folders:").strong());
            for d in crate::vsthost::default_vst_dirs() {
                ui.label(
                    RichText::new(d.display().to_string())
                        .monospace()
                        .small()
                        .weak(),
                );
            }
            for d in &app.vst_dirs {
                ui.label(
                    RichText::new(d.as_str())
                        .monospace()
                        .small()
                        .weak(),
                );
            }
            ui.horizontal(|ui| {
                if ui
                    .button("Rescan Plugins")
                    .on_hover_text("Scan the folders above for new plugins")
                    .clicked()
                {
                    app.vst_rescan();
                }
                ui.label(
                    RichText::new(
                        "add your own folders to the list in the MIXER CHAIN window",
                    )
                    .weak()
                    .small(),
                );
            });
            if !app.vst_note.is_empty() {
                ui.label(RichText::new(&app.vst_note).weak().small());
            }
        });
    app.synth_pick = if open { Some(t) } else { None };
}

/// The toolbar row above the timeline: quantize / snap controls, asset
/// actions, loop and zoom.
fn grid_toolbar(ui: &mut egui::Ui, app: &mut App) {
    ui.horizontal(|ui| {
        // v0.15: this row is dense — give the toggles/combos a real gutter
        // so "…new recordings" and "quantize" stop running together.
        ui.spacing_mut().item_spacing.x = 8.0;
        if ui
            .button(RichText::new("\u{270e} Automation").strong())
            .on_hover_text(
                "Open the automation manager (curve editor). With a red                  automation clip selected (or an AUTO asset picked in the                  rack) it OPENS THAT curve; otherwise it starts a fresh one                  \u{2014} save creates a new asset + clip. Right-click any                  automatable control to record or create one.",
            )
            .clicked()
            && app.auto_edit.is_none()
        {
            let edit = app
                .sel_clips
                .first()
                .filter(|_| app.sel_clips.len() == 1)
                .and_then(|(t, k, c)| {
                    let cl = app.arr.tracks.get(*t)?.lanes.get(*k)?.clips.get(*c)?;
                    Some(cl.asset as usize)
                })
                .or_else(|| {
                    let r = app.sel_asset?;
                    app.assets.get(r).filter(|a| a.is_auto()).map(|_| r)
                });
            app.open_auto_editor(edit);
        }
        if ui
            .button(RichText::new("\u{266b} Piano roll").strong())
            .on_hover_text(
                "Open the piano-roll editor. With a MIDI clip selected (or a \
                 MIDI asset picked in the rack) the roll OPENS THAT for \
                 editing — save overwrites it in place. Otherwise it starts a \
                 fresh pattern; save then creates a new asset + clip.",
            )
            .clicked()
            && app.piano_roll.is_none()
        {
            // Edit target: the single selected clip's asset, else the
            // rack-selected asset — and only when it is a MIDI (pattern)
            // asset. Anything else keeps the old fresh-pattern behaviour.
            let edit = app
                .sel_clips
                .first()
                .filter(|_| app.sel_clips.len() == 1)
                .and_then(|(t, k, c)| {
                    let cl = app.arr.tracks.get(*t)?.lanes.get(*k)?.clips.get(*c)?;
                    Some(cl.asset)
                })
                .or_else(|| app.sel_asset.map(|i| i as u32))
                .filter(|id| {
                    app.assets
                        .get(*id as usize)
                        .and_then(|a| a.pattern.as_ref())
                        .is_some()
                });
            let mut pat = match edit {
                Some(id) => (**app.assets[id as usize].pattern.as_ref().unwrap()).clone(),
                None => {
                    let mut p = Pattern::new(app.bpm().clamp(20, 999) as u16);
                    p.len_ticks = 4 * PPQ as u32;
                    p
                }
            };
            pat.bpm = app.bpm().clamp(20, 999) as u16;
            if edit.is_some() {
                app.status =
                    "piano roll: editing the selected MIDI asset \u{2014} save overwrites it"
                        .into();
            }
            app.piano_roll = Some(Box::new(PianoRoll {
                pat,
                px_per_beat: 90.0,
                row_h: 11.0,
                vscroll_px: 396.0,
                scroll_sec: 0.0,
                changed: false,
                gesture: RollGesture::None,
                canvas_rect: None,
                new_vel: 100,
                sel_notes: Vec::new(),
                sel_anchor: None,
                drag_notes: Vec::new(),
                focused: true,
                win_rect: None,
                edit_asset: edit,
            }));
        }
        ui.label(RichText::new("grid:").strong());
        if ui
            .checkbox(&mut app.auto_quantize, "auto-quantize new recordings")
            .on_hover_text("Quantize each fresh recording's content onto the metronome grid")
            .changed()
        {
            app.save_config();
        }
        ui.label("quantize");
        egui::ComboBox::from_id_salt("q_sub_cb")
            .selected_text(SUBDIVISIONS[app.q_sub].1)
            .show_ui(ui, |ui| {
                for (n, s) in SUBDIVISIONS.iter().enumerate() {
                    if ui.selectable_value(&mut app.q_sub, n, s.1).changed() {
                        app.save_config();
                    }
                }
            });
        ui.label("snap");
        // Master snap switch (requirement: OFF = snap to the program's
        // minimum resolution — one sample — not to a subdivision).
        if ui
            .checkbox(&mut app.snap_on, "")
            .on_hover_text(
                "Snap on/off. When off, placement, drags and clip resize \
                 land on any SAMPLE (no quantization to the grid).",
            )
            .changed()
        {
            app.save_config();
        }
        ui.add_enabled_ui(app.snap_on, |ui| {
            egui::ComboBox::from_id_salt("snap_sub_cb")
                .selected_text(SUBDIVISIONS[app.snap_sub].1)
                .show_ui(ui, |ui| {
                    for (n, s) in SUBDIVISIONS.iter().enumerate() {
                        if ui.selectable_value(&mut app.snap_sub, n, s.1).changed() {
                            app.save_config();
                        }
                    }
                });
        });
        if !app.snap_on {
            ui.label(RichText::new("(off: 1 sample)").weak().small());
        }
        ui.separator();
        // ---- looping (grid header) ----
        let mut lt = app.loop_tl;
        if ui
            .checkbox(&mut lt, "loop")
            .on_hover_text(
                "Wrap the playhead at the end of the loop region instead of \
                 parking at the project end",
            )
            .changed()
        {
            app.loop_tl = lt;
            app.backend.cmd(Cmd::SetLoop(app.loop_tl));
            app.send_loop_region();
            app.save_config();
        }
        let mut mch = false;
        ui.add_enabled_ui(app.loop_tl, |ui| {
            egui::ComboBox::from_id_salt("loop_mode_cb")
                .selected_text(LOOP_MODES[app.loop_mode.min(2)])
                .show_ui(ui, |ui| {
                    for (n, s) in LOOP_MODES.iter().enumerate() {
                        if ui.selectable_value(&mut app.loop_mode, n, *s).changed() {
                            mch = true;
                        }
                    }
                });
        });
        if mch {
            app.send_loop_region();
            app.save_config();
        }
        if app.loop_tl && app.loop_mode == 0 {
            ui.label(RichText::new("(drag the ruler handles)").weak().small());
        }
        ui.separator();
        if ui
            .button(RichText::new("\u{2702} split").strong())
            .on_hover_text(
                "Split the selected clip(s) at the playhead (key: S). The two \\
                 halves keep playing seamlessly; fades stay on the left half.",
            )
            .clicked()
        {
            app.split_selection();
        }
        ui.separator();
        ui.label("zoom");
        // Logarithmic so the default (160 px/s) sits near the centre of the
        // 3..2400 range: max-out shows ~100 bars, max-in edits samples.
        if ui
            .add(
                Slider::new(&mut app.px_per_sec, ZOOM_MIN..=ZOOM_MAX)
                    .logarithmic(true)
                    .show_value(false),
            )
            .changed()
        {}
    });
}

fn central_panels(ui: &mut egui::Ui, app: &mut App) {
    egui::CentralPanel::default().show(ui, |ui| {
        grid_toolbar(ui, app);
        clip_props_strip(ui, app);
        ui.separator();
        let sr = app.sample_rate.max(1) as f64;
        let total = app.total_len().max(1);
        let px = app.px_per_sec as f64;
        let w = ((total as f64 / sr) * px).max(640.0) as f32;
        if ui.input(|i| !i.pointer.any_down()) {
            app.scrub_drag = false;
        }
        // Plain horizontal wheel (no modifiers) pans the timeline; the
        // modifier combos (Ctrl = zoom, Shift = pan) are intercepted at the
        // top of `ui_root` before egui converts them to scroll deltas.
        let area = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(area) {
            let dx = ui.input(|i| i.smooth_scroll_delta.x);
            if dx != 0.0 && !ui.input(|i| i.modifiers.shift) {
                app.hscroll -= dx;
            }
        }
        let avail = ui.available_width().max(200.0);
        app.hscroll = app.hscroll.clamp(0.0, (w - avail).max(0.0));
        // Pinned ruler: always visible above the track scroll (it used to
        // scroll away with the content), same width + x origin as the lanes.
        app.grid_view_w = avail;
        ruler_row(ui, app, avail, px, sr);
        let grid_top = ui.cursor().min;
        app.grid_anchor = grid_top.y;
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for t in 0..app.n_tracks {
                    track_block(ui, app, t, avail, px, sr);
                }
            });
        // The mixer panel mirrors this offset so strips stay aligned.
        app.vscroll = out.state.offset.y;
        // One continuous playhead across the whole track stack (drawn over
        // lanes AND separation bars, so it shows no break).
        let gx = x_of(grid_top.x - app.hscroll, app.backend.engine.pos(), sr, px);
        let gr = egui::Rect::from_min_max(grid_top, ui.max_rect().right_bottom());
        if gx >= gr.min.x && gx <= gr.max.x {
            ui.painter().line_segment(
                [egui::pos2(gx, gr.min.y), egui::pos2(gx, gr.max.y)],
                egui::Stroke::new(1.6, COL_PLAYHEAD),
            );
        }
    });
}

/// Exactly one clip selected: an inline property row under the toolbar —
/// per-clip **gain** (dB), **fade in** / **fade out** (ms) and length.
/// Values write through live; the engine snapshot rides on drag release.
fn clip_props_strip(ui: &mut egui::Ui, app: &mut App) {
    if app.sel_clips.len() != 1 {
        return;
    }
    let (t, k, c) = app.sel_clips[0];
    let Some(cl) = app.arr.tracks[t].lanes[k].clips.get(c).copied() else {
        return;
    };
    let Some(a) = app.assets.get(cl.asset as usize) else {
        return;
    };
    let name = a.name.clone();
    let sr = app.sample_rate.max(1) as f64;
    let span = cl.span(a.samples());
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("clip: {name} \u{b7} {:.2}s", span as f64 / sr))
                .strong()
                .small(),
        );
        // gain, in dB (the house convention: everything the user edits is dB)
        let mut db: f32 = (20.0f32 * cl.gain.max(1e-4).log10()).clamp(-60.0, 12.0);
        let r = ui.add(
            Slider::new(&mut db, -60.0..=12.0)
                .text("gain")
                .step_by(0.5)
                .trailing_fill(true),
        );
        if r.changed() {
            if let Some(cl) = app.arr.tracks[t].lanes[k].clips.get_mut(c) {
                cl.gain = 10f32.powf(db / 20.0);
            }
        }
        if r.drag_stopped() {
            app.push_project();
        }
        for (label, cur, is_in) in [
            ("fade in", cl.fade_in, true),
            ("fade out", cl.fade_out, false),
        ] {
            let mut ms = cur as f64 * 1000.0 / sr;
            let r = ui.add(
                Slider::new(&mut ms, 0.0..=5000.0)
                    .text(label)
                    .step_by(5.0)
                    .trailing_fill(true),
            );
            if r.changed() {
                if let Some(cl) = app.arr.tracks[t].lanes[k].clips.get_mut(c) {
                    let samples = (ms * sr / 1000.0) as u64;
                    if is_in {
                        cl.fade_in = samples;
                    } else {
                        cl.fade_out = samples;
                    }
                }
            }
            if r.drag_stopped() {
                app.push_project();
            }
        }
        if (cl.gain != 1.0 || cl.fade_in > 0 || cl.fade_out > 0)
            && ui
                .button("reset")
                .on_hover_text("Unity gain, no fades")
                .clicked()
        {
            if let Some(cl) = app.arr.tracks[t].lanes[k].clips.get_mut(c) {
                cl.gain = 1.0;
                cl.fade_in = 0;
                cl.fade_out = 0;
            }
            app.push_project();
        }
    });
}

/// Seconds -> x within a strip starting at `x0`.
fn x_of(x0: f32, pos: u64, sr: f64, px: f64) -> f32 {
    x0 + (pos as f64 / sr * px) as f32
}

/// Samples at x within a strip starting at `x0`.
fn s_at(x0: f32, x: f32, sr: f64, px: f64) -> u64 {
    (((x - x0).max(0.0) as f64 / px) * sr) as u64
}

/// Paint the metronome grid (bar lines bold, beat lines faint, sub-beat
/// snap lines even fainter — drawn at the snap subdivision) plus the
/// playhead across `rect`. `x0` is where timeline position 0 sits in point
/// coordinates — left of `rect` whenever the view is panned — so every strip
/// (pinned ruler and lane alike) maps samples through the identical window
/// and the playhead renders as one straight line across the whole grid.
fn paint_grid(
    ui: &mut egui::Ui,
    app: &App,
    rect: egui::Rect,
    px: f64,
    sr: f64,
    ruler: bool,
    x0: f32,
) {
    let p = ui.painter();
    p.rect_filled(
        rect,
        0.0,
        if ruler {
            Color32::from_rgb(34, 34, 40)
        } else {
            Color32::from_rgb(24, 24, 29)
        },
    );
    let beat_px = quantize::beat_samples(sr as u32, app.bpm()) * (px / sr);
    let num = app.beats() as u64;
    // Sub-beat snap tier: follow the snap combo, but only while the step
    // stays readable (≥4 px apart) and actually subdivides the beat.
    let steps = SUBDIVISIONS[app.snap_sub].0;
    let snap_on = steps > 1 && (beat_px / steps as f64) > 4.0;
    if beat_px > 3.0 {
        // Only the beats inside this strip's visible window (cheap at any
        // project length, correct under panning).
        let b_lo = (((rect.min.x - x0).max(0.0) as f64 / beat_px) as u64).saturating_sub(1);
        let b_hi = (((rect.max.x - x0).max(0.0) as f64 / beat_px) as u64) + 2;
        for b in b_lo..b_hi.min(b_lo + 4096) {
            let x = x0 + (b as f64 * beat_px) as f32;
            if x < rect.min.x - 1.0 || x > rect.max.x + 1.0 {
                continue;
            }
            // Snap steps between this beat and the next (k = 0 is the beat
            // line itself; snap subdivisions never coincide with a beat).
            if snap_on {
                for k in 1..steps {
                    let sx = x0 + ((b as f64 + k as f64 / steps as f64) * beat_px) as f32;
                    if sx < rect.min.x - 1.0 || sx > rect.max.x + 1.0 {
                        continue;
                    }
                    p.line_segment(
                        [egui::pos2(sx, rect.min.y), egui::pos2(sx, rect.max.y)],
                        egui::Stroke::new(0.6, COL_SNAP_LINE),
                    );
                }
            }
            let bar = b % num == 0;
            p.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(
                    if bar { 1.3 } else { 0.6 },
                    if bar { COL_BAR_LINE } else { COL_BEAT_LINE },
                ),
            );
            if ruler && bar && beat_px * num as f64 > 44.0 {
                p.text(
                    egui::pos2(x + 3.0, rect.min.y + 3.0),
                    egui::Align2::LEFT_TOP,
                    format!("{}", b / num + 1),
                    egui::FontId::proportional(10.0),
                    Color32::GRAY,
                );
            }
        }
    }
    // Playhead: the ruler paints its own segment; the track stack gets ONE
    // continuous line painted over lanes and separation bars together (see
    // `central_panels`), so it never breaks at a track boundary.
    if ruler {
        let pos = app.backend.engine.pos();
        let x = x_of(x0, pos, sr, px);
        if x >= rect.min.x && x <= rect.max.x {
            p.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                egui::Stroke::new(1.6, COL_PLAYHEAD),
            );
        }
    }
}

/// Timeline ruler: measure grid, bar numbers, and the scrub handle — press
/// or drag anywhere here to move the global playhead. Pinned above the track
/// scroll so it is always visible; shares its x window with the lane strips.
fn ruler_row(ui: &mut egui::Ui, app: &mut App, avail: f32, px: f64, sr: f64) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(avail, 20.0), Sense::click_and_drag());
    let x0 = rect.min.x - app.hscroll;
    paint_grid(ui, app, rect, px, sr, true, x0);
    // Loop region on the ruler ("grid header"): a shaded span, plus two
    // draggable handles in area mode. Handles follow the snap setting.
    let (ls, le) = app.loop_region_eff();
    let hx0 = x_of(x0, ls, sr, px);
    let hx1 = x_of(x0, le, sr, px);
    if app.loop_tl && le > ls {
        let band = egui::Rect::from_min_max(
            egui::pos2(hx0.max(rect.min.x), rect.min.y),
            egui::pos2(hx1.min(rect.max.x), rect.max.y),
        );
        if band.width() > 0.5 {
            ui.painter().rect_filled(
                band,
                0.0,
                Color32::from_rgba_premultiplied(90, 150, 230, 55),
            );
        }
        if app.loop_mode == 0 {
            // Grab bars in loud yellow: this is where the user drags the
            // loop's start/end. They brighten while one is being dragged.
            let dragging = app.drag_loop_handle.is_some();
            for (n, hx) in [hx0, hx1].into_iter().enumerate() {
                if hx > rect.min.x - 4.0 && hx < rect.max.x + 4.0 {
                    let held = dragging && app.drag_loop_handle == Some(n as u8);
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(hx - 3.0, rect.min.y),
                            egui::vec2(6.0, rect.height()),
                        ),
                        1.0,
                        if held {
                            Color32::from_rgb(255, 235, 120)
                        } else {
                            Color32::from_rgb(250, 200, 50)
                        },
                    );
                }
            }
        }
    }
    if let Some(pt) = resp.interact_pointer_pos() {
        if resp.drag_started_by(egui::PointerButton::Primary) {
            app.drag_loop_handle = if app.loop_tl
                && app.loop_mode == 0
                && (pt.x - hx0).abs().min((pt.x - hx1).abs()) <= 7.0
            {
                Some(if (pt.x - hx0).abs() <= (pt.x - hx1).abs() {
                    0
                } else {
                    1
                })
            } else {
                None
            };
        }
        if let Some(h) = app.drag_loop_handle {
            if resp.dragged() {
                let snapu = app.snap_unit();
                let pos = snap_start(s_at(x0, pt.x, sr, px), snapu);
                let bar = app.bar_samples();
                let (mut s, mut e) = (
                    app.loop_area.0,
                    if app.loop_area.1 == 0 {
                        2 * bar
                    } else {
                        app.loop_area.1
                    },
                );
                if h == 0 {
                    s = pos.min(e.saturating_sub(1));
                } else {
                    e = pos.max(s + 1);
                }
                app.loop_area = (s, e);
                app.send_loop_region();
            }
            if resp.drag_stopped() {
                app.drag_loop_handle = None;
                app.save_config();
            }
        } else if (resp.dragged() || resp.clicked()) && rect.contains(pt) {
            let pos = s_at(x0, pt.x, sr, px);
            app.backend.cmd(Cmd::Seek { pos });
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
}

/// One track's grid block: every lane strip, edge to edge with zero spacing,
/// closed by a thin separation bar. All controls live in the mixer panel —
/// this block's height is exactly [`App::track_h`], which the mixer shares.
fn track_block(ui: &mut egui::Ui, app: &mut App, t: usize, w: f32, px: f64, sr: f64) {
    let th = app.track_h(t);
    let nl = app.arr.tracks[t].lanes.len().max(1);
    let lane_h = th / nl as f32;
    for k in 0..nl {
        lane_row(ui, app, t, k, w, px, sr, lane_h);
    }
    // The separation bar doubles as the resize handle: dragging it changes
    // ONLY the track above (children inside share the new height equally).
    let (r, sep) = ui.allocate_exact_size(egui::vec2(w, TRACK_SEP_HIT), Sense::drag());
    let hot = sep.hovered() || app.sep_drag.is_some_and(|(d, _, _)| d == t);
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(r.min.x, r.center().y - TRACK_SEP / 2.0),
            egui::vec2(r.width(), TRACK_SEP),
        ),
        0.0,
        if hot {
            Color32::from_rgb(96, 96, 116)
        } else {
            Color32::from_rgb(56, 56, 64)
        },
    );
    if sep.drag_started_by(egui::PointerButton::Primary) {
        if let Some(pt) = sep.interact_pointer_pos() {
            app.sep_drag = Some((t, app.track_h(t), pt.y));
        }
    }
    if sep.dragged() {
        if let Some((tt, h0, y0)) = app.sep_drag {
            if tt == t {
                if let Some(pt) = sep.interact_pointer_pos() {
                    app.set_track_h(t, h0 + (pt.y - y0));
                }
            }
        }
    }
    if sep.drag_stopped() && app.sep_drag.is_some() {
        app.sep_drag = None;
        app.save_config();
    }
    if sep.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
}

/// One lane strip: grid + clips + interactions, clipped to its own rect and
/// sharing the pinned ruler's x window (pan offset included). Pressing empty
/// grid scrubs the playhead; clips drag with snap; rack drags arrive through
/// egui's native drag-and-drop (the old cross-response handoff dropped
/// nothing because the rack owned the pointer).
#[allow(clippy::too_many_arguments)] // (grid geometry) + (view state) tuples read worse
fn lane_row(
    ui: &mut egui::Ui,
    app: &mut App,
    t: usize,
    k: usize,
    w: f32,
    px: f64,
    sr: f64,
    h: f32,
) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), Sense::click_and_drag());
    let x0 = rect.min.x - app.hscroll;
    let snapu = app.snap_unit();
    let clips: Vec<(Clip, String)> = app.arr.tracks[t].lanes[k]
        .clips
        .iter()
        .map(|c| {
            (
                *c,
                app.assets
                    .get(c.asset as usize)
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
            )
        })
        .collect();
    // v0.11 R5: double-clicking a red automation clip opens its curve in
    // the automation manager (like double-clicking a MIDI clip region).
    if resp.double_clicked() {
        let pt = ui
            .input(|i| i.pointer.interact_pos())
            .unwrap_or(rect.center());
        for (c, _name) in clips.iter() {
            let alen = app.asset_len(c.asset);
            if app
                .assets
                .get(c.asset as usize)
                .is_some_and(|a| a.is_auto())
                && pt.x >= x_of(x0, c.pos, sr, px)
                && pt.x <= x_of(x0, c.end(alen), sr, px)
            {
                app.open_auto_editor(Some(c.asset as usize));
                break;
            }
        }
    }
    // Edge zones become trim/extend handles on ANY clip (grabbing an edge
    // selects the clip on the way — the old "only selected clips resize"
    // rule made the feature undiscoverable and read as broken).
    const EDGE_PX: f32 = 7.0;
    let edge_at = |app: &App, pt: egui::Pos2| -> Option<(usize, u8)> {
        let mut best: Option<(f32, usize, u8)> = None;
        for (ci, (c, _)) in clips.iter().enumerate() {
            let alen = app.asset_len(c.asset);
            for (edge, ex) in [
                (0u8, x_of(x0, c.pos, sr, px)),
                (1u8, x_of(x0, c.end(alen), sr, px)),
            ] {
                let d = (pt.x - ex).abs();
                if d <= EDGE_PX && best.is_none_or(|(bd, _, _)| d < bd) {
                    best = Some((d, ci, edge));
                }
            }
        }
        best.map(|(_, ci, edge)| (ci, edge))
    };
    // v18: one shared hover point for all hover reactions below.
    let hover_pt = resp.hover_pos();

    // ---- paint (clipped to this strip) ----
    let old_clip = ui.clip_rect();
    ui.shrink_clip_rect(rect);
    paint_grid(ui, app, rect, px, sr, false, x0);
    // Lane tag rides inside the strip's left edge: it must NOT take layout
    // room, or the lane grids would sit right of the ruler's.
    ui.painter().text(
        rect.min + egui::vec2(4.0, 1.0),
        egui::Align2::LEFT_TOP,
        format!("T{} {}", t + 1, if k == 0 { "track" } else { "child" }),
        egui::FontId::proportional(9.0),
        Color32::from_gray(110),
    );
    for (ci, (c, name)) in clips.iter().enumerate() {
        let alen = app.asset_len(c.asset);
        let cx0 = x_of(x0, c.pos, sr, px);
        let cx1 = x_of(x0, c.end(alen), sr, px);
        if cx1 < rect.min.x || cx0 > rect.max.x {
            continue;
        }
        let sel = app.sel_clips.contains(&(t, k, ci));
        let cres = egui::Rect::from_min_max(
            egui::pos2(cx0, rect.min.y + 1.0),
            egui::pos2(cx1, rect.max.y - 1.0),
        );
        // Paint window = the VISIBLE slice of the source (span ∩ data),
        // never the full `avail`: passing avail made the whole remaining
        // asset scale into the trimmed rect, i.e. the content STRETCHED
        // when the user dragged an edge (the "clips stretch instead of
        // clip" report). Content now stays anchored per-sample; the trim
        // hides it exactly like the engine does.
        let vlen = c.span(alen).min(c.avail(alen));
        // v0.11 R5: automation clips are the third kind and wear red.
        let is_auto = app
            .assets
            .get(c.asset as usize)
            .is_some_and(|a| a.is_auto());
        let fill = if is_auto {
            Color32::from_rgb(170, 70, 74)
        } else {
            COL_CLIP
        };
        // v18: hover brighten — the clip under the pointer lifts a touch.
        let fill = if hover_pt.is_some_and(|p| cres.contains(p)) {
            fill.linear_multiply(1.4)
        } else {
            fill
        };
        draw_asset_clip(ui, app, c.asset, (c.cut, vlen), cres, fill, sel);
        // Extended-past-data tail: silence, hatched dark so it reads as
        // "clip is longer than its source".
        let dend = x_of(x0, c.data_end(alen), sr, px);
        if dend < cx1 - 0.5 {
            ui.painter().rect_filled(
                egui::Rect::from_min_max(egui::pos2(dend.max(cres.min.x), cres.min.y), cres.max),
                0.0,
                Color32::from_rgba_premultiplied(10, 10, 14, 160),
            );
        }
        // Fade wedges visualise the envelope the sliders control: the
        // shaded triangle is where the clip is quieter than full level.
        let wedge = Color32::from_black_alpha(120);
        if c.fade_in > 0 {
            let fx = ((c.fade_in as f64 / sr * px) as f32).min(cres.width());
            if fx > 1.0 {
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        egui::pos2(cres.min.x, cres.min.y),
                        egui::pos2(cres.min.x, cres.max.y),
                        egui::pos2(cres.min.x + fx, cres.min.y),
                    ],
                    wedge,
                    egui::Stroke::NONE,
                ));
            }
        }
        if c.fade_out > 0 {
            let fx = ((c.fade_out as f64 / sr * px) as f32).min(cres.width());
            if fx > 1.0 {
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        egui::pos2(cres.max.x - fx, cres.max.y),
                        egui::pos2(cres.max.x, cres.min.y),
                        egui::pos2(cres.max.x, cres.max.y),
                    ],
                    wedge,
                    egui::Stroke::NONE,
                ));
            }
        }
        if cx1 - cx0 > 36.0 && !name.is_empty() {
            ui.painter().text(
                egui::pos2(cx0 + 3.0, rect.min.y + 2.0),
                egui::Align2::LEFT_TOP,
                name,
                egui::FontId::proportional(10.0),
                Color32::from_gray(220),
            );
        }
        // Trimmed state: mark where the asset's ORIGINAL data boundary
        // sits, so the user can see what a re-extend would reveal.
        let trimmed_left = c.cut > 0;
        let trimmed_right = c.play_len.is_some_and(|l| l < c.avail(alen));
        let extended = c.play_len.is_some_and(|l| l > c.avail(alen));
        if sel && (trimmed_left || trimmed_right || extended) {
            for bx in [(dend, true), (x_of(x0, c.pos - c.cut, sr, px), false)] {
                let (x, right_of_clip) = bx;
                if x < rect.min.x - 30.0 || x > rect.max.x + 30.0 {
                    continue;
                }
                let _ = right_of_clip;
                ui.painter().line_segment(
                    [egui::pos2(x, cres.min.y), egui::pos2(x, cres.max.y)],
                    egui::Stroke::new(1.0, Color32::from_rgb(250, 200, 90)),
                );
            }
            ui.painter().text(
                egui::pos2(dend - 3.0, cres.max.y - 11.0),
                egui::Align2::RIGHT_BOTTOM,
                "original",
                egui::FontId::proportional(8.0),
                Color32::from_rgb(250, 200, 90),
            );
        }
        if sel {
            // Yellow end caps = the resize handles (same language as the
            // loop-area handles in the ruler and the piano roll notes).
            for ex in [cx0, cx1] {
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(ex - 2.0, cres.min.y),
                        egui::pos2(ex + 2.0, cres.max.y),
                    ),
                    1.0,
                    Color32::from_rgb(250, 200, 50),
                );
            }
            // Selection hint: these clips are what Delete removes.
            ui.painter().text(
                egui::pos2(cx1 - 3.0, rect.min.y + 1.0),
                egui::Align2::RIGHT_TOP,
                "\u{d7}",
                egui::FontId::proportional(13.0),
                Color32::from_rgb(235, 120, 120),
            );
        }
    }
    // Rack drag: ghost preview while a native egui drag hovers this strip
    // (contains_pointer keeps working even while the source is dragged).
    let drop_in: Option<usize> = resp.dnd_hover_payload::<usize>().map(|a| *a);
    if let (Some(a), Some(pt)) = (drop_in, ui.input(|i| i.pointer.hover_pos())) {
        let pt = pt.clamp(
            egui::pos2(rect.min.x, rect.min.y),
            egui::pos2(rect.max.x, rect.max.y),
        );
        let ns = snap_start(s_at(x0, pt.x, sr, px), snapu);
        let len = app.asset_len(a as u32);
        let gx0 = x_of(x0, ns, sr, px).clamp(rect.min.x, rect.max.x);
        let gx1 = x_of(x0, ns + len, sr, px).clamp(rect.min.x + 2.0, rect.max.x);
        ui.painter().rect_stroke(
            egui::Rect::from_min_max(
                egui::pos2(gx0, rect.min.y + 1.0),
                egui::pos2(gx1, rect.max.y - 1.0),
            ),
            2.0,
            egui::Stroke::new(2.0, COL_GHOST),
            egui::StrokeKind::Inside,
        );
    }
    ui.set_clip_rect(old_clip);

    // Selection marquee (rubber box): (t, lane, x0, add) at press → x1
    // follows the pointer; the selection is rebuilt live from the box.
    if let Some((_mt, _mk, mx0, add)) = app.marquee {
        if let Some(pt) = resp.interact_pointer_pos() {
            let x1 = pt.x.clamp(rect.min.x, rect.max.x);
            let (a, b) = (mx0.min(x1), mx0.max(x1));
            let p0 = s_at(x0, a, sr, px);
            let p1 = s_at(x0, b, sr, px);
            let ids: Vec<(usize, usize, usize)> = clips
                .iter()
                .enumerate()
                .filter(|(_, (c, _))| {
                    let alen = app.asset_len(c.asset);
                    c.pos < p1 && c.end(alen) > p0
                })
                .map(|(j, _)| (t, k, j))
                .collect();
            let mut ids = ids;
            if add {
                for b0 in &app.marquee_base {
                    if b0.0 == t && b0.1 == k && !ids.contains(b0) {
                        ids.push(*b0);
                    }
                }
                ids.sort();
            }
            // Live preview while dragging: selection = box ∩ clips.
            app.sel_clips = ids;
            app.sel_anchor = app.sel_clips.first().copied();
            ui.painter().rect_stroke(
                egui::Rect::from_min_max(egui::pos2(a, rect.min.y), egui::pos2(b, rect.max.y)),
                0.0,
                egui::Stroke::new(1.2f32, Color32::from_rgb(150, 200, 255)),
                egui::StrokeKind::Inside,
            );
        }
    }
    // ---- v18: hover reactions (cursor + trim glow) ----
    // Display-only: pointer events already repaint, so there is no
    // animation timer forcing frames. The cursor and a lit edge tell the
    // user what a click or drag WOULD do, before they do it — the fix
    // for "I didn't know clips could be trimmed / lanes dragged".
    if let Some(pt) = hover_pt {
        if rect.contains(pt) {
            if let Some((ci, edge)) = edge_at(app, pt) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                let (c, _) = &clips[ci];
                let alen = app.asset_len(c.asset);
                let ex = if edge == 0 {
                    x_of(x0, c.pos, sr, px)
                } else {
                    x_of(x0, c.end(alen), sr, px)
                };
                ui.painter().vline(
                    ex,
                    rect.min.y + 1.0..=rect.max.y - 1.0,
                    egui::Stroke::new(2.0, Color32::from_rgb(255, 214, 120)),
                );
            } else if clips.iter().any(|(c, _)| {
                let alen = app.asset_len(c.asset);
                pt.x >= x_of(x0, c.pos, sr, px) && pt.x <= x_of(x0, c.end(alen), sr, px)
            }) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
    }
    // Drop-target outline: while an asset rides an in-app drag or an OS
    // file drop is in flight, the strip under the pointer lights up so
    // the release target is unmistakable.
    let drop_in_flight = !app.os_drops.is_empty()
        || egui::DragAndDrop::payload::<usize>(ui.ctx()).is_some();
    if drop_in_flight && hover_pt.is_some_and(|p| rect.contains(p)) {
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            2.0,
            egui::Stroke::new(1.5, Color32::from_rgb(150, 200, 255)),
            egui::StrokeKind::Inside,
        );
    }
    // ---- interactions ----
    // Release of a rack drag over this strip places the clip (the strip
    // receives it directly; the old manual cross-panel handoff never did).
    if let Some(a) = resp.dnd_release_payload::<usize>().map(|a| *a) {
        if let Some(pt) = ui.input(|i| i.pointer.hover_pos()) {
            if rect.contains(pt) {
                let ns = snap_start(s_at(x0, pt.x, sr, px), snapu);
                app.arr.insert(t, k, Clip::new(a as u32, ns));
                let li = app.arr.tracks[t].lanes[k].clips.len() - 1;
                app.select_one((t, k, li));
                app.push_project();
                app.status = format!(
                    "clip placed on track {} \u{b7} {} \u{b7} drag its edges to trim",
                    t + 1,
                    if k == 0 { "track" } else { "child lane" }
                );
            }
        }
        return; // the drop consumed this frame's pointer
    }
    // v18: OS file-drop release over this strip. While the queue from
    // this frame's drain is non-empty and the pointer sits here, the
    // files land on THIS lane: imported via `ensure_asset`, snapped to
    // the grid, collision-split into child lanes exactly like paste.
    // Undroppable types are named in the status line instead of
    // silently vanishing into the rack.
    if !app.os_drops.is_empty() {
        if let Some(pt) = ui.input(|i| i.pointer.hover_pos()) {
            if rect.contains(pt) {
                let pos = snap_start(s_at(x0, pt.x, sr, px), snapu);
                let mut placed: Vec<(usize, usize, usize)> = Vec::new();
                let mut names: Vec<String> = Vec::new();
                let mut bad: Vec<String> = Vec::new();
                for path in std::mem::take(&mut app.os_drops) {
                    let shown = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    let importable = crate::assets::is_audio_file(&path)
                        || crate::assets::is_midi_file(&path)
                        || crate::assets::is_auto_file(&path);
                    let Some(ai) = importable.then(|| app.ensure_asset(&path)).flatten() else {
                        bad.push(shown);
                        continue;
                    };
                    let id = ai as u32;
                    let nl0 = app.arr.tracks[t].lanes.len();
                    let lane = app.arr.place_auto(t, Clip::new(id, pos), app.asset_len(id));
                    app.grow_for_lanes(t, nl0);
                    let li = app
                        .arr
                        .tracks
                        .get(t)
                        .and_then(|tr| tr.lanes.get(lane))
                        .map(|l| l.clips.len().saturating_sub(1))
                        .unwrap_or(0);
                    placed.push((t, lane, li));
                    names.push(
                        app.assets
                            .get(ai)
                            .map(|a| a.name.clone())
                            .unwrap_or(shown),
                    );
                }
                if !placed.is_empty() {
                    app.sel_clips = placed;
                    app.sel_anchor = app.sel_clips.first().copied();
                    app.push_project();
                }
                let mut status = if names.is_empty() {
                    String::new()
                } else {
                    format!(
                        "dropped {} on track {} \u{b7} drag \
                         its{} edges to trim",
                        names.join(", "),
                        t + 1,
                        if names.len() == 1 { "" } else { "s" }
                    )
                };
                if !bad.is_empty() {
                    let part = format!("unsupported: {}", bad.join(", "));
                    status = if status.is_empty() {
                        part
                    } else {
                        format!("{status} \u{b7} skipped {part}")
                    };
                }
                if !status.is_empty() {
                    app.status = status;
                }
            }
        }
    }
    // Any primary press in the grid remembers this track as the paste
    // target ("paste into the nearest track to the last click").
    //
    // GESTURES ARM ON PRESS, not on drag_started: egui only fires
    // `drag_started` once the pointer has already travelled several pixels
    // — by then a press that meant to grab a 6 px edge handle has drifted
    // out of its own hit zone and the clip MOVES instead of resizing.
    // That drift was the reported "clip length cannot be edited / notes
    // cannot be adjusted" bug in both grids. Press-arming also lets an
    // edge grab SELECT the clip on the way into the drag.
    let no_gesture = app.drag_clip.is_none()
        && app.clip_resize.is_none()
        && app.marquee.is_none()
        && app.marquee_pending.is_none()
        && !app.scrub_drag;
    if no_gesture && resp.is_pointer_button_down_on() {
        if let Some(pt) = resp.interact_pointer_pos() {
            if rect.contains(pt) {
                app.last_click_track = Some(t);
                let mods = ui.input(|i| i.modifiers);
                let pos = s_at(x0, pt.x, sr, px);
                // Edge of ANY clip wins over everything: trim/extend.
                if let Some((ci, edge)) = edge_at(app, pt) {
                    if !app.is_sel((t, k, ci)) {
                        app.select_one((t, k, ci));
                    }
                    let c = app.arr.tracks[t].lanes[k].clips[ci];
                    app.clip_resize = Some(ClipResize {
                        t,
                        lane: k,
                        c: ci,
                        edge,
                        pos0: c.pos,
                        end0: c.end(app.asset_len(c.asset)),
                        cut0: c.cut,
                    });
                } else {
                    let hit = clips
                        .iter()
                        .position(|(c, _)| pos >= c.pos && pos < c.end(app.asset_len(c.asset)));
                    match hit {
                        Some(ci) => {
                            let id = (t, k, ci);
                            if mods.command {
                                app.select_toggle(id);
                            } else if mods.shift
                                && app.sel_anchor.is_some_and(|a| a.0 == t && a.1 == k)
                            {
                                // Shift+press: the same-lane run between the
                                // anchor clip and this one, inclusive.
                                let apos = app
                                    .sel_anchor
                                    .and_then(|(at, ak, ac)| {
                                        app.arr.tracks[at].lanes[ak].clips.get(ac)
                                    })
                                    .map(|c| c.pos)
                                    .unwrap_or(pos);
                                let (lo, hi) = (apos.min(pos), apos.max(pos));
                                // Intersect, don't just compare `pos`: the
                                // click point sits INSIDE the clicked clip,
                                // past its start, so a start-only filter
                                // silently dropped the very clip clicked and
                                // broke the run at both ends.
                                let ids: Vec<(usize, usize, usize)> = clips
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, (c, _))| {
                                        c.pos <= hi && c.end(app.asset_len(c.asset)) >= lo
                                    })
                                    .map(|(j, _)| (t, k, j))
                                    .collect();
                                app.select_range(ids);
                            } else if !app.is_sel(id) {
                                app.select_one(id);
                            }
                            // Group drag: remember every selected clip and
                            // its position; the group moves by the snapped
                            // delta of this strip's pointer.
                            let mut ids = app.sel_clips.clone();
                            ids.sort();
                            let items: Vec<(usize, usize, usize, u64)> = ids
                                .iter()
                                .filter_map(|(tt, kk, cc)| {
                                    let p = app.arr.tracks[*tt].lanes[*kk]
                                        .clips
                                        .get(*cc)
                                        .map(|c| c.pos)?;
                                    Some((*tt, *kk, *cc, p))
                                })
                                .collect();
                            if !items.is_empty() {
                                let base0 = items.iter().map(|x| x.3).min().unwrap_or(0);
                                let lane0 = if items.len() == 1 {
                                    Some(items[0].1)
                                } else {
                                    None
                                };
                                app.drag_clip = Some(ClipDrag {
                                    items,
                                    base0,
                                    grab: base0.saturating_sub(pos),
                                    lane0,
                                });
                            }
                        }
                        None => {
                            // Empty grid: press seeks (playhead jumps), drag
                            // scrubs AND — once past a small threshold —
                            // turns into the rubber-band selection box, so
                            // dragging across clips multi-selects them.
                            let add = mods.command || mods.shift;
                            if !add {
                                app.sel_clips.clear();
                                app.sel_anchor = None;
                            }
                            app.scrub_drag = true;
                            app.marquee_pending = Some((t, k, pt.x, add, app.sel_clips.clone()));
                            app.backend.cmd(Cmd::Seek { pos });
                        }
                    }
                }
            }
        }
    } else if resp.dragged_by(egui::PointerButton::Primary) {
        // Pending marquee: leave scrub mode once the pointer really drags,
        // so a horizontal rubber band can replace the playhead scan.
        let pending = app.marquee_pending.clone();
        if let Some((mt, mk, mx, add, base)) = pending {
            if mt == t && mk == k {
                if let Some(pt) = resp.interact_pointer_pos() {
                    if (pt.x - mx).abs() > 6.0 {
                        app.marquee = Some((mt, mk, mx, add));
                        app.marquee_base = base;
                        app.marquee_pending = None;
                        app.scrub_drag = false;
                    }
                }
            }
        }
        if let Some(pt) = resp.interact_pointer_pos() {
            if app.scrub_drag {
                if rect.contains(pt) {
                    let pos = s_at(x0, pt.x, sr, px);
                    app.backend.cmd(Cmd::Seek { pos });
                }
            } else if let Some(r) = app.clip_resize {
                // v18: the clip under the gesture can be removed mid-drag
                // (Ctrl+X / right-click Delete while the button is still
                // down) — if its slot is gone, DISARM; never index dead
                // slots here. (delete_selection now also cancels gestures;
                // this is the belt to that belt, per the crash pin.)
                let alive = app
                    .arr
                    .tracks
                    .get(r.t)
                    .and_then(|tr| tr.lanes.get(r.lane))
                    .and_then(|l| l.clips.get(r.c))
                    .is_some();
                if !alive {
                    app.clip_resize = None;
                } else if rect.contains(pt) {
                    let asset = app.arr.tracks[r.t].lanes[r.lane].clips[r.c].asset;
                    let alen = app.asset_len(asset);
                    let unit = (snapu as u64).max(1);
                    let raw = s_at(x0, pt.x, sr, px);
                    let mut cand = snap_start(raw, unit as f64);
                    // Guaranteed snap at the asset's ORIGINAL boundaries,
                    // regardless of the snap setting.
                    if r.edge == 1 {
                        let orig_data_end = r.pos0 + alen.saturating_sub(r.cut0);
                        if (cand as i64 - orig_data_end as i64).abs() <= (unit / 2).max(1) as i64 {
                            cand = orig_data_end;
                        }
                        // Never smaller than one subdivision.
                        cand = cand.max(r.pos0 + unit);
                        let new_len = cand - r.pos0;
                        let avail = alen.saturating_sub(r.cut0);
                        let cl = &mut app.arr.tracks[r.t].lanes[r.lane].clips[r.c];
                        cl.play_len = if new_len == avail {
                            None
                        } else {
                            Some(new_len)
                        };
                    } else {
                        let orig_start = r.pos0.saturating_sub(r.cut0);
                        if (cand as i64 - orig_start as i64).abs() <= (unit / 2).max(1) as i64 {
                            cand = orig_start;
                        }
                        // Start never moves before the asset's data begins,
                        // and the clip keeps at least one subdivision.
                        cand = cand.max(orig_start).min(r.end0.saturating_sub(unit));
                        // cand may sit LEFT of pos0 (re-exposing cut audio):
                        // signed math, u64 would wrap (debug panics).
                        let new_cut = (r.cut0 as i64 + (cand as i64 - r.pos0 as i64))
                            .clamp(0, alen as i64) as u64;
                        let new_len = r.end0 - cand;
                        let avail = alen.saturating_sub(new_cut);
                        let cl = &mut app.arr.tracks[r.t].lanes[r.lane].clips[r.c];
                        cl.pos = cand;
                        cl.cut = new_cut;
                        cl.play_len = if new_len >= avail {
                            None
                        } else {
                            Some(new_len)
                        };
                    }
                }
            } else if let Some(d) = app.drag_clip.clone() {
                let new_base = snap_start(s_at(x0, pt.x, sr, px).saturating_add(d.grab), snapu);
                let dx = new_base as i64 - d.base0 as i64;
                if dx != 0 {
                    for (tt, kk, cc, p0) in &d.items {
                        let np = (*p0 as i64 + dx).max(0) as u64;
                        app.arr.move_clip(*tt, *kk, *cc, np);
                    }
                }
                // Single-clip drag: vertical pointer travel moves the clip
                // between its track's lanes (groups stay horizontal-only).
                if let Some(l0) = d.lane0 {
                    let dl = ((pt.y - rect.center().y) / rect.height().max(1.0)).round() as i64;
                    let (tt, cur, cc, _) = d.items[0];
                    let nl = app.arr.tracks[tt].lanes.len() as i64;
                    let target = (l0 as i64 + dl).clamp(0, nl - 1) as usize;
                    if target != cur {
                        if let Some(ni) = app.arr.move_clip_lane(tt, cur, target, cc) {
                            if let Some(dd) = app.drag_clip.as_mut() {
                                dd.items[0].1 = target;
                                dd.items[0].2 = ni;
                            }
                            let old = (tt, cur, cc);
                            let fresh = app
                                .sel_clips
                                .iter()
                                .map(|x| if *x == old { (tt, target, ni) } else { *x })
                                .collect::<Vec<_>>();
                            app.sel_clips = fresh;
                            app.sel_clips.sort();
                            if app.sel_anchor == Some(old) {
                                app.sel_anchor = Some((tt, target, ni));
                            }
                        }
                    }
                }
            }
        }
    }
    if resp.drag_stopped() && (app.drag_clip.take().is_some() || app.clip_resize.take().is_some()) {
        app.push_project();
    }
    if resp.clicked() && !resp.dragged() && !app.scrub_drag && app.drag_clip.is_none() {
        // Plain click (no drag happened): select the clip under it, or scrub
        // if the click landed on empty grid.
        if let Some(pt) = resp.interact_pointer_pos() {
            if rect.contains(pt) {
                app.last_click_track = Some(t);
                let pos = s_at(x0, pt.x, sr, px);
                match clips
                    .iter()
                    .position(|(c, _)| pos >= c.pos && pos < c.end(app.asset_len(c.asset)))
                {
                    Some(ci) => {
                        let mods = ui.input(|i| i.modifiers);
                        if mods.command {
                            app.select_toggle((t, k, ci));
                        } else {
                            app.select_one((t, k, ci));
                        }
                    }
                    None => {
                        app.sel_clips.clear();
                        app.sel_anchor = None;
                        app.backend.cmd(Cmd::Seek { pos });
                    }
                }
            }
        }
    }
    // v0.15: right-click opens a menu — clip actions on a clip, track
    // actions on empty grid. The old "right-click anywhere clears this
    // track" gesture destroyed a whole track on one accidental press and
    // blocked every discovery path a context menu should own.
    resp.clone().context_menu(|ui| {
        grid_context_menu(ui, app, t, k, &clips, rect, x0, sr, px);
    });
}

/// v0.15: the grid right-click menu. On a clip: Copy / Cut / Duplicate /
/// Delete (operating on the selection — the clip is selected first if it
/// is not part of it). On empty grid: Paste here. Both: Select all and an
/// explicit (safe) "Clear this track".
/// Bare dB number for the level readouts ("-inf", "+0.0", "-12.3").
fn db_num(amp: f32) -> String {
    if amp <= 0.0 {
        "-inf".into()
    } else {
        format!("{:+.1}", 20.0 * amp.log10())
    }
}

#[allow(clippy::too_many_arguments)] // ui, app + the lane's whole geometry
fn grid_context_menu(
    ui: &mut egui::Ui,
    app: &mut App,
    t: usize,
    k: usize,
    clips: &[(Clip, String)],
    rect: egui::Rect,
    x0: f32,
    sr: f64,
    px: f64,
) {
    let hit: Option<usize> = ui
        .input(|i| i.pointer.interact_pos())
        .filter(|p| rect.contains(*p))
        .and_then(|p| {
            let pos = s_at(x0, p.x, sr, px);
            clips
                .iter()
                .position(|(c, _)| pos >= c.pos && pos < c.end(app.asset_len(c.asset)))
        });
    if let Some(ci) = hit {
        let id = (t, k, ci);
        if !app.is_sel(id) {
            app.select_one(id);
        }
        if ui.button("Copy  (Ctrl+C)").clicked() {
            app.clipboard_copy();
            ui.close();
        }
        if ui.button("Cut  (Ctrl+X)").clicked() {
            app.clipboard_copy();
            app.delete_selection();
            ui.close();
        }
        if ui.button("Duplicate  (Ctrl+D)").clicked() {
            app.duplicate_selection();
            ui.close();
        }
        if ui.button("Delete  (Del)").clicked() {
            app.delete_selection();
            ui.close();
        }
        if app
            .assets
            .get(clips[ci].0.asset as usize)
            .is_some_and(|a| a.is_auto())
            && ui.button("Edit automation curve").clicked()
        {
            app.open_auto_editor(Some(clips[ci].0.asset as usize));
            ui.close();
        }
        ui.separator();
    } else {
        let has = app.clipboard.is_some();
        let mut pasted = false;
        ui.add_enabled_ui(has, |ui| {
            if ui
                .button("Paste here  (Ctrl+V)")
                .on_hover_text(if has {
                    "Pastes at the playhead on this track (child lanes open when busy)"
                } else {
                    "clipboard is empty \u{2014} Copy a clip first"
                })
                .clicked()
            {
                app.last_click_track = Some(t);
                app.clipboard_paste();
                pasted = true;
            }
        });
        if pasted {
            ui.close();
        }
        ui.separator();
    }
    if ui.button("Select all clips").clicked() {
        app.select_all_clips();
        ui.close();
    }
    if ui.button("Clear this track's clips").clicked() {
        app.clear_track(t);
        ui.close();
    }
}

/// Draw one clip's rectangle with its asset's waveform, normalized by the
/// asset's own peak so quiet takes stay legible. `win` is the window of
/// asset data the clip shows — `(cut, samples shown)` — so trims reveal
/// the right slice; the caller paints any silent extension separately.
fn draw_asset_clip(
    ui: &mut egui::Ui,
    app: &mut App,
    asset: u32,
    win: (u64, u64),
    rect: egui::Rect,
    fill: Color32,
    sel: bool,
) {
    let p = ui.painter();
    p.rect_filled(rect, 2.0, fill.linear_multiply(0.45));
    // MIDI clips preview as an approximate piano roll (FL-style): every
    // note is a small bar; pitch maps to the clip's vertical extent.
    if let Some(pat) = app
        .assets
        .get(asset as usize)
        .and_then(|a| a.pattern.clone())
    {
        if rect.width() >= 4.0 {
            let (cut, wlen) = win;
            let wlen = wlen.max(1);
            let alen = app.asset_len(asset).max(1);
            let lt = (pat.len_ticks.max(1)) as f64;
            let (lo_n, hi_n) = (24i32, 108i32);
            let h = rect.height().max(6.0);
            let col = if sel {
                Color32::from_rgb(180, 235, 255)
            } else {
                Color32::from_rgb(120, 205, 255)
            };
            for note in pat.notes.iter() {
                let n = note.note as i32;
                if n < lo_n || n > hi_n {
                    continue;
                }
                let s0 = note.start as f64 / lt * alen as f64;
                let s1 = (note.start + note.len.max(1)) as f64 / lt * alen as f64;
                let x0 = rect.min.x
                    + ((s0 - cut as f64).max(0.0) / wlen as f64 * rect.width() as f64) as f32;
                let x1 = rect.min.x
                    + ((s1.min((cut + wlen) as f64) - cut as f64).max(0.0) / wlen as f64
                        * rect.width() as f64) as f32;
                if x1 <= x0 + 0.5 && (x0 < rect.min.x || x0 > rect.max.x) {
                    continue;
                }
                let y = rect.max.y - ((n - lo_n) as f32 / (hi_n - lo_n) as f32) * (h - 2.0) - 1.0;
                let bar = (h / 28.0).clamp(1.2, 5.0);
                p.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x0, y - bar * 0.5),
                        egui::pos2(x1.max(x0 + 1.0), y + bar * 0.5),
                    ),
                    1.0,
                    col,
                );
            }
        }
        let stroke = if sel {
            egui::Stroke::new(2.0, COL_CLIP_SEL)
        } else {
            egui::Stroke::new(1.0, fill)
        };
        p.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
        return;
    }
    // v0.11 R5: automation clips preview as the actual curve (red),
    // sampled through the same trim window the audio preview uses.
    if let Some(auto) = app
        .assets
        .get(asset as usize)
        .and_then(|a| a.automation.clone())
    {
        if rect.width() >= 4.0 {
            let (cut, wlen) = win;
            let wlen = wlen.max(1);
            let (mut lo, mut hi) = (f32::MAX, f32::MIN);
            for n in auto.nodes.iter() {
                lo = lo.min(n.v);
                hi = hi.max(n.v);
            }
            if hi - lo <= 0.0 {
                lo -= 0.5;
                hi += 0.5;
            }
            let col = if sel {
                Color32::from_rgb(255, 190, 190)
            } else {
                Color32::from_rgb(255, 130, 130)
            };
            let steps = (rect.width() / 2.0).max(2.0) as i64;
            let h = rect.height().max(6.0);
            let mut prev: Option<egui::Pos2> = None;
            for i in 0..=steps {
                let frac = i as f64 / steps as f64;
                let t = (cut as f64 + frac * wlen as f64) as i64;
                let v = auto.eval(t);
                let x = rect.min.x + (frac * rect.width() as f64) as f32;
                let y = rect.max.y - (v - lo) / (hi - lo) * (h - 3.0) - 1.5;
                let pt = egui::pos2(x, y);
                if let Some(q) = prev {
                    p.line_segment([q, pt], egui::Stroke::new(1.4, col));
                }
                prev = Some(pt);
            }
        }
        let stroke = if sel {
            egui::Stroke::new(2.0, COL_CLIP_SEL)
        } else {
            egui::Stroke::new(1.0, fill)
        };
        p.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
        return;
    }
    if rect.width() >= 4.0 {
        let alen = app.asset_len(asset).max(1);
        let (cut, wlen) = win;
        let wlen = wlen.min(alen.saturating_sub(cut)).max(1);
        // Full assets keep the old one-column-per-pixel path; trimmed
        // windows ask for enough full-asset columns to stay crisp when the
        // visible slice is zoomed.
        let cols_req = if cut == 0 && wlen >= alen {
            rect.width() as usize
        } else {
            ((rect.width() * (alen as f32 / wlen as f32)) as usize).clamp(1, 4096)
        };
        if let Some(data) = asset_wave(app, asset, cols_req) {
            let n = data.len().max(1);
            let cy = rect.center().y;
            let amp = rect.height() * 0.46;
            let col = if sel {
                COL_CLIP_SEL
            } else {
                Color32::from_rgb(210, 225, 250)
            };
            let f0 = cut as f32 / alen as f32;
            let f1 = ((cut + wlen) as f32 / alen as f32).max(f0 + 1e-6);
            let i0 = ((f0 * n as f32) as usize).min(n.saturating_sub(1));
            let i1 = ((f1 * n as f32) as usize).clamp(i0 + 1, n);
            let col_w = rect.width() / ((i1 - i0) as f32).max(1.0);
            let stroke = egui::Stroke::new(col_w.clamp(0.8, 3.0), col);
            for (j, i) in (i0..i1).enumerate() {
                let [lo, hi] = data[i];
                let x = rect.min.x + (j as f32 + 0.5) * col_w;
                p.line_segment(
                    [egui::pos2(x, cy - hi * amp), egui::pos2(x, cy - lo * amp)],
                    stroke,
                );
            }
        }
    }
    let stroke = if sel {
        egui::Stroke::new(2.0, COL_CLIP_SEL)
    } else {
        egui::Stroke::new(1.0, fill)
    };
    p.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
}

/// Cached min/max columns for one asset at the requested width.
fn asset_wave(app: &mut App, asset: u32, cols: usize) -> Option<Vec<[f32; 2]>> {
    let cols = cols.clamp(1, 4096);
    if let Some(c) = app.wave.get(&asset) {
        if c.cols == cols {
            return Some(c.data.clone());
        }
    }
    let a = app.assets.get(asset as usize)?;
    let data = wave_cols(&a.data, cols);
    let mut peak = 1e-4f32;
    let stride = (a.data.len() / 4096).max(1);
    for &v in a.data.iter().step_by(stride) {
        peak = peak.max(v.abs());
    }
    // Normalize the columns by the peak so drawing only scales by height.
    let mut norm = Vec::with_capacity(data.len());
    for [lo, hi] in data {
        norm.push([lo / peak, hi / peak]);
    }
    app.wave.insert(
        asset,
        AssetWave {
            cols,
            data: norm.clone(),
        },
    );
    Some(norm)
}

const KIND_VOL: u8 = 0;
const KIND_GAIN: u8 = 1;
const KIND_PAN: u8 = 2;

/// One stacked level row: `label | editable value box (centred) | slider |
/// raw MIDI readout`. The slider always runs the raw MIDI range 0..=127
/// (what a bound knob sends); the text box shows the human value — dB for
/// volume and gain, -100..=100 for pan (0 = centre) — and is typed into
/// directly, committed with Enter. Text hugs its slider with no filler
/// padding; every row's slider starts at the same x, so the three line up.
fn level_row(ui: &mut egui::Ui, app: &mut App, i: usize, kind: u8) {
    let label = ["vol ", "gain", "pan "][kind as usize];
    let midi = match kind {
        KIND_VOL => app.trk_vol[i],
        KIND_GAIN => app.trk_gain[i],
        _ => app.trk_pan[i],
    };
    let hover = match kind {
        KIND_VOL => "Track playback volume: -inf…+6 dB (0 dB = three-quarters). Right-click for exact entry and automation.",
        KIND_GAIN => "Track gain trim: 0 = silence, 80 = 0 dB, 127 = +10 dB. Right-click for exact entry.",
        _ => "Track pan: -100 = hard left, 0 = centre, +100 = hard right. Right-click for exact entry and automation.",
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        // v0.15: the editable text box is GONE — right-click (slider OR
        // value) opens Set value / Create automation asset. Fixed-width
        // label + value columns keep every slider the same length, so the
        // three tracks of a strip line up (they never did with boxes whose
        // text made rows differ).
        ui.add_sized(
            egui::vec2(30.0, 18.0),
            egui::Label::new(RichText::new(label).monospace().small()).selectable(false),
        );
        let mut v = midi;
        let sw = (ui.available_width() - 86.0).max(30.0);
        let r = ui
            .add_sized(
                egui::vec2(sw, 18.0),
                Slider::new(&mut v, 0..=127).show_value(false),
            )
            .on_hover_text(hover);
        let vr = ui.add_sized(
            egui::vec2(52.0, 18.0),
            egui::Label::new(
                RichText::new(if kind == KIND_PAN {
                    format!("{:>4}", pan_text(midi))
                } else {
                    format!(
                        "{:>5}",
                        db_num(match kind {
                            KIND_VOL => vol_amp(midi),
                            KIND_GAIN => gain_amp(midi),
                            _ => 1.0,
                        })
                    )
                })
                .monospace()
                .small(),
            )
            .selectable(false)
            .sense(egui::Sense::click()),
        );
        ui.add(
            egui::Label::new(
                RichText::new(if kind == KIND_PAN { "" } else { "dB" })
                    .monospace()
                    .weak()
                    .small(),
            )
            .selectable(false),
        );
        if r.changed() {
            match kind {
                KIND_VOL => app.set_trk_vol(i, v),
                KIND_GAIN => app.set_trk_gain(i, v),
                _ => app.set_trk_pan(i, v),
            }
            app.levels_dirty = true;
        }
        // v0.15: right-click either the slider or its value readout — both
        // answer the automation menu (vol/pan; gain is a manual trim).
        let menu = |ui: &mut egui::Ui, app: &mut App| match kind {
            KIND_VOL => auto_menu(
                ui,
                app,
                i,
                crate::automation::TARGET_TRACK_VOL,
                vol_amp(midi),
            ),
            KIND_GAIN => {
                if ui
                    .button(format!(
                        "Set value   [{} dB]",
                        db_num(gain_amp(midi)).trim_start_matches('+')
                    ))
                    .clicked()
                {
                    app.value_dialog =
                        Some((i, TARGET_PSEUDO_GAIN.to_string(), db_num(gain_amp(midi))));
                    app.value_dialog_focus = true;
                    ui.close();
                }
            }
            _ => auto_menu(ui, app, i, crate::automation::TARGET_TRACK_PAN, pan01(midi)),
        };
        r.context_menu(|ui| menu(ui, app));
        vr.context_menu(|ui| menu(ui, app));
        // Reset this strip straight to its "0" position.
        if ui
            .small_button(RichText::new("0").monospace())
            .on_hover_text(
                [
                    "Reset volume to 0 dB",
                    "Reset gain to 0 dB",
                    "Center the pan",
                ][kind as usize],
            )
            .clicked()
        {
            match kind {
                KIND_VOL => app.set_trk_vol(i, VOL_UNITY_MIDI),
                KIND_GAIN => app.set_trk_gain(i, GAIN_UNITY_MIDI),
                _ => app.set_trk_pan(i, 64),
            }
            app.levels_dirty = true;
        }
    });
}

/// the exact "-inf"); pan maps -100..=100 -> 0..=127.
/// One small chip per mixer-chain slot on the track header; clicking opens
/// that slot's controls (script editor window; plugins open their own).
fn fx_buttons(ui: &mut egui::Ui, app: &mut App, i: usize) {
    let mut open_slot: Option<usize> = None;
    for (k, row) in app.fx_rows[i].clone().iter().enumerate() {
        let (label, ok) = match row {
            ChainRow::Script { stem, .. } => {
                let d = app.fx_registry.desc(stem);
                (
                    d.map(|d| d.name.clone()).unwrap_or_else(|| stem.clone()),
                    d.is_some(),
                )
            }
            ChainRow::Vst { name, entry, .. } => (name.clone(), entry.is_some()),
        };
        let mut hover = String::new();
        if let ChainRow::Script { stem, .. } = row {
            if let Some(d) = app.fx_registry.desc(stem) {
                if let Some(d) = d.description.as_deref() {
                    hover.push_str(d);
                    hover.push('\n');
                }
            }
        }
        hover.push_str("Click to open this slot's controls window.");
        let color = if ok {
            Color32::from_rgb(150, 200, 255)
        } else {
            Color32::from_gray(140)
        };
        if ui
            .button(RichText::new(label).small().color(color))
            .on_hover_text(hover)
            .clicked()
        {
            open_slot = Some(k);
        }
    }
    if let Some(k) = open_slot {
        match &app.fx_rows[i][k] {
            ChainRow::Script { .. } => app.open_fx_editor(i, k),
            ChainRow::Vst { entry: Some(_), .. } => app.vst_row_gui(i, k),
            ChainRow::Vst { .. } => app.fx_window = Some(i),
        }
    }
}

fn wave_cols(buf: &[f32], cols: usize) -> Vec<[f32; 2]> {
    let n = buf.len();
    if n == 0 || cols == 0 {
        return Vec::new();
    }
    let mut out: Vec<[f32; 2]> = vec![[0.0, 0.0]; cols];
    let stride = (n / (cols * 8)).max(1);
    for (c, col) in out.iter_mut().enumerate() {
        let lo = c * n / cols;
        let hi = ((c + 1) * n / cols).max(lo + 1);
        let mut mn = f32::MAX;
        let mut mx = f32::MIN;
        let mut k = lo;
        while k < hi {
            let v = buf[k];
            if v < mn {
                mn = v;
            }
            if v > mx {
                mx = v;
            }
            k += stride;
        }
        *col = [mn, mx];
    }
    out
}

/// Min/max per column for the dependent-mode tiled view: each column covers a
/// window of the ensemble period, sampled modulo the recording length — every
/// pixel column maps to a sample of the (repeating) loop, never to silence.
/// Same bounded stride sampling as [`wave_cols`], so the whole tiled view is
/// computed once per cache rebuild instead of once per frame.
/// Three-step knob-learn sub-window (opened by the knob buttons in
/// `bind_row`).
///
/// 1. Move any control: the axis — message TYPE + CHANNEL — is detected.
///    While it is not locked, moving a different control re-picks it;
///    **Lock axis** freezes it and advances to step 2.
/// 2. Listen on that axis for ANY 0..=127 data value; **Lock min** freezes
///    the raw minimum and advances to step 3.
/// 3. Listen again for ANY value; **Finish** commits. If the maximum read
///    lower than the minimum the axis is inverted — `scale_axis` flips the
///    translation so the knob still maps continuously low\u{2192}high, and
///    the window says so in plain words.
///
/// The raw lo/hi is stored per slot and incoming values are rescaled with
/// `scale_axis`, so a knob wired to a partial range (or inverted) still
/// covers the full program travel.
fn knob_learn_window(ui: &mut egui::Ui, app: &mut App) {
    if app.knob_learn.is_none() {
        return;
    }
    egui::Window::new("Learn knob")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ui, |ui| {
            ui.set_width(380.0);
            enum Act {
                None,
                LockAxis,
                LockMin,
                Finish,
                Cancel,
                Restart,
            }
            let mut act = Act::None;
            {
                let kl = app.knob_learn.as_ref().unwrap();
                let action = MidiAction::ALL[kl.slot].label();
                ui.label(RichText::new(format!("Bind knob: {action}")).strong());
                let axis = kl.bind.axis_label();
                if !kl.has_axis {
                    ui.label("1. Move the knob or slider you want to bind.");
                    ui.label(
                        RichText::new(
                            "The axis is the message TYPE + CHANNEL; its data byte (0-127) \
                             is the value that gets learned. Waiting for the control's \
                             first message\u{2026}",
                        )
                        .weak()
                        .small()
                        .italics(),
                    );
                } else if !kl.axis_locked {
                    ui.label(RichText::new(format!("1. Axis found: {axis}")).strong());
                    ui.label(
                        RichText::new(format!("last message seen: {}", kl.bind.label()))
                            .monospace()
                            .small(),
                    );
                    ui.label(
                        RichText::new(
                            "Wrong knob? Just move another one \u{2014} the axis follows the \
                             most recent control until you lock it. Lock the axis when it \
                             shows the type + channel you want.",
                        )
                        .weak()
                        .small(),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("Lock axis").clicked() {
                            act = Act::LockAxis;
                        }
                        ui.label(
                            RichText::new("\u{2192} then capture the minimum")
                                .weak()
                                .small(),
                        );
                    });
                } else if !kl.locked {
                    ui.label(RichText::new(format!("2. Axis locked: {axis}")).strong());
                    ui.horizontal(|ui| {
                        ui.label(format!("minimum raw value: {}", kl.lo));
                        if ui.button("Lock min").clicked() {
                            act = Act::LockMin;
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Move the control to its LOWEST position \u{2014} the number \
                             follows every message on this axis (any 0-127 value) \u{2014} \
                             then Lock min.",
                        )
                        .weak()
                        .small(),
                    );
                } else {
                    ui.label(RichText::new(format!("3. Axis locked: {axis}")).strong());
                    ui.label(
                        RichText::new(format!("minimum raw value: {} (locked)", kl.lo))
                            .weak()
                            .small(),
                    );
                    ui.horizontal(|ui| {
                        ui.label(format!("maximum raw value: {}", kl.hi));
                        let ok = kl.hi != kl.lo;
                        let fin = ui
                            .add_enabled(ok, egui::Button::new("Finish"))
                            .on_hover_text("needs a maximum different from the minimum");
                        if fin.clicked() {
                            act = Act::Finish;
                        }
                    });
                    let hint = if kl.lo > kl.hi {
                        format!(
                            "Min {} > max {}: this control is INVERTED. Finish flips the \
                             translation so turning the knob still sweeps low\u{2192}high \
                             across the full range.",
                            kl.lo, kl.hi
                        )
                    } else {
                        "Move the control to its HIGHEST position, then Finish.".to_string()
                    };
                    ui.label(RichText::new(hint).weak().small());
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        act = Act::Cancel;
                    }
                    if kl.has_axis && ui.button("Restart").clicked() {
                        act = Act::Restart;
                    }
                });
            }
            match act {
                Act::None => {}
                Act::LockAxis => app.knob_lock_axis(),
                Act::LockMin => app.knob_lock_min(),
                Act::Finish => app.knob_finish(),
                Act::Cancel => {
                    let kl = app.knob_learn.take().unwrap();
                    app.status =
                        format!("cancelled knob learn: {}", MidiAction::ALL[kl.slot].label());
                }
                Act::Restart => {
                    let slot = app.knob_learn.as_ref().unwrap().slot;
                    app.knob_learn = Some(KnobLearn {
                        slot,
                        bind: MidiBind {
                            ty: 0xff,
                            ch: 0,
                            d1: 0,
                        },
                        has_axis: false,
                        axis_locked: false,
                        last_val: None,
                        lo: 0,
                        hi: 127,
                        locked: false,
                    });
                    app.status = format!(
                        "knob learn restarted: {} \u{2014} move the knob",
                        MidiAction::ALL[slot].label()
                    );
                }
            }
        });
}

/// One row of the bindings table: action label + MIDI capture button +
/// keyboard button; persists on change. Both columns work like "press a key":
/// click the cell to arm it, then send the trigger — the next matching input
/// becomes the binding (click again or Esc cancels).
///
/// Continuous (knob) actions bind differently: they need the axis minimum
/// and maximum, so their button opens the two-step learn window instead of
/// grabbing the first message that arrives.
fn bind_row(ui: &mut egui::Ui, app: &mut App, a: MidiAction) {
    let idx = MidiAction::idx(a);
    ui.horizontal(|ui| {
        ui.set_min_width(150.0);
        ui.label(RichText::new(a.label()).weak());

        // MIDI binding: live capture (no dropdown — the controller is the
        // menu). Knobs get the calibrated two-step flow.
        let knob = a.is_continuous();
        let learning_knob = app.knob_learn.as_ref().is_some_and(|k| k.slot == idx);
        let listening = app.midi_learning_slot == Some(idx) && !learning_knob;
        let txt = if learning_knob {
            "learning knob…".to_string()
        } else if listening {
            "listening…".to_string()
        } else {
            match app.midi_binds[idx] {
                // A knob binding owns its AXIS (type + channel) — the data
                // byte is never part of it, so show the axis, not a CC id.
                Some(b) if knob => match app.knob_cal.get(idx).copied().flatten() {
                    Some(c) => format!("{}  [{}/{}]", b.axis_label(), c.lo, c.hi),
                    None => b.axis_label(),
                },
                Some(b) => match app.knob_cal.get(idx).copied().flatten() {
                    // Show the captured raw range next to the axis id.
                    Some(c) => format!("{}  [{}/{}]", b.label(), c.lo, c.hi),
                    None => b.label(),
                },
                None => "—".to_string(),
            }
        };
        let mut btn =
            egui::Button::new(RichText::new(txt).monospace()).min_size(egui::vec2(150.0, 0.0));
        if listening || learning_knob {
            btn = btn.fill(Color32::from_rgb(60, 90, 120));
        }
        let hover = if knob {
            "Click to open the knob wizard: move the control to its minimum, \
             lock it, move to the maximum, finish. Captures the full travel of \
             the knob (min and max must be on the same MIDI channel). \
             Esc cancels; Delete clears the binding and its calibration."
        } else {
            "Click to listen: the next NOTE ON / CC / PROGRAM CHANGE \
             from your controller becomes this binding. Click again (or Esc) \
             to cancel; Delete clears the binding."
        };
        let resp = ui.add(btn).on_hover_text(hover);
        if resp.clicked() {
            if learning_knob {
                app.knob_learn = None;
            } else if knob {
                app.arm_knob_learn(idx);
            } else if listening {
                app.midi_learning_slot = None;
            } else {
                app.arm_midi_slot(idx);
            }
        }

        // Keyboard binding for the same action.
        ui.add_space(4.0);
        let learning = app.learning_slot == Some(idx);
        let txt = if learning {
            "press a key…".to_string()
        } else {
            match app.short_keys[idx] {
                Some(k) => key_display(k),
                None => "—".to_string(),
            }
        };
        let mut btn =
            egui::Button::new(RichText::new(txt).monospace()).min_size(egui::vec2(44.0, 0.0));
        if learning {
            btn = btn.fill(Color32::from_rgb(60, 90, 120));
        }
        if ui.add(btn).clicked() {
            app.arm_key_slot(idx);
        }
    });
}

/// Central dashboard: mode checkbox + the loop track rows.
/// Export prompt (Export button / bound key): pick one loop or the whole
/// project, plus destination file and format. Whole-project export forces
/// dependent mode on before running — it only exists there.
fn export_window(ui: &mut egui::Ui, app: &mut App) {
    // Local copy so the closure can borrow `app` while `.open()` holds the
    // window's open-state; written back after show.
    let mut open = app.show_export;
    let mut cancelled = false;
    let mut done = false;
    egui::Window::new(RichText::new("EXPORT").strong())
        .open(&mut open)
        .min_width(430.0)
        .show(ui, |ui| {
            ui.label(RichText::new("What to export:").strong());
            let n_sel = app.sel_clips.len();
            if ui
                .radio_value(&mut app.export_sel, true, format!("Current selection ({n_sel} clip(s))"))
                .changed()
                && app.export_sel
            {
                app.export_whole = false;
            }
            if ui
                .radio_value(
                    &mut app.export_whole,
                    false,
                    format!("Track {} (selected)", app.selected + 1),
                )
                .changed()
                && !app.export_whole
            {
                app.export_sel = false;
            }
            if ui.radio_value(&mut app.export_whole, true, "Whole project").changed() && app.export_whole {
                app.export_sel = false;
            }
            if app.export_sel && n_sel == 0 {
                ui.label(
                    RichText::new("Nothing selected — click clips (Ctrl+click adds, Ctrl+A selects all).")
                        .weak()
                        .small(),
                );
            }
            if app.export_whole {
                ui.label(
                    RichText::new("Whole project = every sounding track, all lanes, mixer trims, pans and effects.")
                        .weak()
                        .small(),
                );
            }
            ui.add_space(6.0);
            ui.label("Destination:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut app.export_path).desired_width(290.0));
                if ui.button("Browse…").clicked() {
                    // No extension here — the Format dropdown adds it at
                    // export time (see `do_export`), so changing the format
                    // never means re-picking the destination.
                    let name = if app.export_sel {
                        "slooper-selection".to_string()
                    } else if app.export_whole {
                        "slooper-project".to_string()
                    } else {
                        format!("slooper-track-{}", app.selected + 1)
                    };
                    if let Some(p) = rfd::FileDialog::new()
                        .set_title("Export destination")
                        .set_file_name(name)
                        .save_file()
                    {
                        app.export_path = p.to_string_lossy().into_owned();
                    }
                }
            });
            ui.label("Format:");
            egui::ComboBox::from_id_salt("export_fmt")
                .selected_text(crate::audio_io::ExportFormat::ALL[app.export_fmt].label())
                .show_ui(ui, |ui| {
                    for (k, f) in crate::audio_io::ExportFormat::ALL.iter().enumerate() {
                        ui.selectable_value(&mut app.export_fmt, k, f.label());
                    }
                });
            if crate::audio_io::ExportFormat::ALL[app.export_fmt].needs_ffmpeg() {
                ui.label(RichText::new("MP3 / OGG encoding needs ffmpeg on PATH.").weak().small());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                if ui.button("Export").clicked() {
                    app.do_export();
                    done = true;
                }
            });
        });
    app.show_export = open && !cancelled && !done;
}

/// The unified per-track mixer chain window. Scripted effects and hosted
/// VST plugins are one ordered slot list (FL Studio's mixer model): a
/// plugin can sit anywhere in the chain, in any number. Each row opens its
/// own controls; "Add" popups pull from the script folder and the plugin
/// scan; rejected scripts and the plugin folder manager sit below.
fn effects_window(ui: &mut egui::Ui, app: &mut App) {
    let Some(loop_i) = app.fx_window else { return };
    if !app.vst_scanned && app.vst_scan_rx.is_none() {
        app.vst_rescan();
    }
    let mut open = true;
    egui::Window::new(RichText::new(format!("MIXER CHAIN — track {}", loop_i + 1)).strong())
        .open(&mut open)
        .min_width(560.0)
        .default_height(480.0)
        .show(ui, |ui| {
            // --- chain (audio order, top first) ---------------------------
            ui.label(RichText::new("Chain (top = first):").strong());
            let mut act: Option<(u8, usize)> = None; // 0 open, 1 up, 2 down, 3 remove
            let rows = app.fx_rows[loop_i].clone();
            egui::ScrollArea::vertical()
                .max_height(210.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (k, row) in rows.iter().enumerate() {
                        let (tag, label, ok) = match row {
                            ChainRow::Script { stem, .. } => {
                                let d = app.fx_registry.desc(stem);
                                (
                                    "fx",
                                    d.map(|d| d.name.clone()).unwrap_or_else(|| stem.clone()),
                                    d.is_some(),
                                )
                            }
                            ChainRow::Vst { kind, name, entry, .. } => {
                                (kind.tag(), name.clone(), entry.is_some())
                            }
                        };
                        ui.horizontal(|ui| {
                            ui.monospace(format!("[{tag}]"));
                            let txt = if ok {
                                RichText::new(&label).strong()
                            } else {
                                RichText::new(&label).weak()
                            };
                            let resp = ui.add(egui::Button::new(txt).frame(false));
                            if resp.clicked() {
                                act = Some((0, k));
                            }
                            if matches!(row, ChainRow::Vst { entry: None, open_rx: Some(_), .. }) {
                                // v16: loading on the plugin thread — the
                                // UI stayed live; the entry lands via
                                // `poll_vst_opens` and repaints.
                                ui.label(
                                    RichText::new("loading…").weak().small(),
                                );
                            } else if matches!(row, ChainRow::Vst { entry: None, .. }) {
                                ui.label(
                                    RichText::new("× not loaded (Rescan Plugins below)")
                                        .color(Color32::from_rgb(220, 120, 120))
                                        .small(),
                                );
                            } else if !ok {
                                ui.label(
                                    RichText::new("× script missing (Rescan effects below)")
                                        .color(Color32::from_rgb(220, 120, 120))
                                        .small(),
                                );
                            } else {
                                ui.label(
                                    RichText::new("click to open controls")
                                        .weak()
                                        .small(),
                                );
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("×").on_hover_text("Remove from chain").clicked()
                                {
                                    act = Some((3, k));
                                }
                                if arrow_button(ui, false).clicked() {
                                    act = Some((2, k));
                                }
                                if arrow_button(ui, true).clicked() {
                                    act = Some((1, k));
                                }
                            });
                        });
                    }
                    if rows.is_empty() {
                        ui.label(
                            RichText::new("(chain empty — add an effect or a plugin below)")
                                .weak()
                                .small(),
                        );
                    }
                });
            match act {
                Some((0, k)) => match &app.fx_rows[loop_i][k] {
                    ChainRow::Script { .. } => app.open_fx_editor(loop_i, k),
                    ChainRow::Vst { entry: Some(_), .. } => {
                        // v0.10: open the plugin's own window; sliders only
                        // as the no-editor fallback (inside vst_row_gui).
                        app.vst_row_gui(loop_i, k);
                    }
                    ChainRow::Vst { .. } => {}
                },
                Some((1, k)) => app.move_fx_row(loop_i, k, -1),
                Some((2, k)) => app.move_fx_row(loop_i, k, 1),
                Some((3, k)) => app.remove_fx_row(loop_i, k),
                _ => {}
            }
            ui.separator();

            // --- add ------------------------------------------------------
            ui.horizontal(|ui| {
                let stems = app.fx_registry.stems();
                egui::ComboBox::from_id_salt(("addfx", loop_i))
                    .selected_text("+ effect")
                    .show_ui(ui, |ui| {
                        for s in &stems {
                            let name = app
                                .fx_registry
                                .desc(s)
                                .map(|d| d.name.clone())
                                .unwrap_or_else(|| s.clone());
                            if ui.selectable_label(false, format!("{name}  ({s})")).clicked() {
                                app.add_fx_script(loop_i, s);
                            }
                        }
                    });
                egui::ComboBox::from_id_salt(("addvst", loop_i))
                    .selected_text("+ VST plugin")
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        if app.vst_scan.is_empty() {
                            ui.label(
                                RichText::new(if app.vst_scan_rx.is_some() {
                                    "scanning plugin folders\u{2026}"
                                } else {
                                    "no plugins found \u{2014} add the folder your plugins live \
                                     in (drag-and-drop installs included) and press \
                                     Rescan Plugins below"
                                })
                                .weak()
                                .small(),
                            );
                        }
                        egui::ScrollArea::vertical()
                            .max_height(240.0)
                            .show(ui, |ui| {
                                for e in app.vst_scan.clone() {
                                    if e.error.is_some() {
                                        continue;
                                    }
                                    // v0.19: an instrument is NOT an effect.
                                    // Synths never belong in the mixer
                                    // chain — they are bound through the
                                    // track header's synth (♪) picker.
                                    if e.instrument {
                                        continue;
                                    }
                                    let label =
                                        format!("[{}] {} \u{2014} {}", e.kind.tag(), e.name, e.vendor);
                                    if ui
                                        .selectable_label(false, &label)
                                        .on_hover_text("Effect plugin for this track's chain")
                                        .clicked()
                                    {
                                        app.add_fx_plugin(loop_i, &e);
                                    }
                                }
                            });
                    });
            });
            ui.separator();

            // --- effects script folder ------------------------------------
            ui.label("Effects folder (Rust scripts):");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut app.fx_dir_edit).desired_width(330.0));
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("Effects folder").pick_folder()
                    {
                        app.fx_dir_edit = p.to_string_lossy().into_owned();
                        app.apply_fx_dir();
                    }
                }
                if ui.button("Apply").clicked() {
                    app.apply_fx_dir();
                }
                if ui.button("Rescan").clicked() {
                    app.rescan_fx();
                }
                if ui.button("Add template").clicked() {
                    match app.fx_registry.add_template() {
                        Ok(stem) => {
                            app.add_fx_script(loop_i, &stem);
                            app.fx_note =
                                format!("created {stem}.rs — added to this track's chain");
                        }
                        Err(e) => app.fx_note = e,
                    }
                }
            });
            if !fx::rustc_available() {
                ui.label(
                    RichText::new(
                        "rustc is not on PATH — effect scripts cannot compile. Install Rust (rustup).",
                    )
                    .color(Color32::YELLOW),
                );
            }
            // Scripts that failed to compile or load: dimmed rows with the
            // rejection reason, so an effect author can fix the script in
            // place and Rescan.
            for it in app.fx_registry.items().iter().filter(|it| it.desc.is_none()) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&it.stem).weak().monospace());
                    let t = it.error.clone().unwrap_or_else(|| "invalid".into());
                    let short: String = t.chars().take(140).collect();
                    ui.label(
                        RichText::new(format!("× {}", short.replace('\n', " · ")))
                            .color(Color32::from_rgb(220, 120, 120))
                            .small(),
                    )
                    .on_hover_text(t);
                });
            }
            if !app.fx_note.is_empty() {
                ui.label(RichText::new(&app.fx_note).weak().small());
            }
            ui.separator();

            // --- plugin scan folders ---------------------------------------
            ui.label("Plugin folders:");
            if app.vst_dirs.is_empty() {
                ui.label(RichText::new("(standard system folders only)").weak().small());
            }
            for (i, d) in app.vst_dirs.clone().iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(d).monospace().small());
                    if ui.small_button("×").on_hover_text("Remove this folder").clicked() {
                        app.vst_dirs.remove(i);
                        app.vst_scanned = false;
                    }
                });
            }
            ui.horizontal(|ui| {
                if ui.button("Add folder…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("VST folder").pick_folder() {
                        let d = p.to_string_lossy().into_owned();
                        if !app.vst_dirs.contains(&d) {
                            app.vst_dirs.push(d);
                            app.vst_scanned = false;
                        }
                    }
                }
                if ui.button("Rescan Plugins").clicked() {
                    app.vst_rescan();
                }
            });
            if !app.vst_note.is_empty() {
                ui.label(RichText::new(&app.vst_note).weak().small());
            }
            ui.separator();
            ui.label(
                RichText::new(
                    "Chain slots run top-down on this track's signal, after gain and volume, \
                     before pan. Effects scripts and VST2 (`.so`) / VST3 (`.vst3`) plugins mix \
                     freely in any order; click a slot to open its controls.",
                )
                .weak()
                .small(),
            );
        });
    app.fx_window = if open { Some(loop_i) } else { None };
}

/// Small ↑/↓ reorder button with a DRAWN arrow. egui's default font stack
/// (Ubuntu-Light/NotoEmoji/emoji-icon-font) has no arrow or triangle glyphs
/// at all — "▲"/"▼" (and even "↑"/"↓") render as hollow tofu boxes — so the
/// icon is vector-painted into the button rect instead.
fn arrow_button(ui: &mut egui::Ui, up: bool) -> egui::Response {
    let resp = ui
        .add(egui::Button::new(" ").min_size(egui::vec2(20.0, 16.0)))
        .on_hover_text(if up {
            "Move earlier in the chain"
        } else {
            "Move later in the chain"
        });
    let c = resp.rect.center();
    let col = if resp.hovered() {
        ui.visuals().widgets.active.fg_stroke.color
    } else {
        ui.visuals().widgets.noninteractive.fg_stroke.color
    };
    let s = egui::Stroke::new(1.4, col);
    let p = ui.painter();
    // shaft
    p.line_segment([egui::pos2(c.x, c.y - 4.0), egui::pos2(c.x, c.y + 4.5)], s);
    // head (tip up or down)
    let tip = if up { c.y - 4.5 } else { c.y + 4.5 };
    let jaw = if up { tip + 3.5 } else { tip - 3.5 };
    p.line_segment([egui::pos2(c.x, tip), egui::pos2(c.x - 3.5, jaw)], s);
    p.line_segment([egui::pos2(c.x, tip), egui::pos2(c.x + 3.5, jaw)], s);
    resp
}

fn port_list(
    ui: &mut egui::Ui,
    title: &str,
    hint: &str,
    avail: &[PortInfo],
    sel: &mut HashSet<String>,
) {
    ui.label(RichText::new(format!("{}  ({})", title, avail.len())).strong());
    ui.label(RichText::new(hint).weak().small());
    if avail.is_empty() {
        ui.label(RichText::new("no remote audio ports found").weak());
        return;
    }
    let mut last_client = String::new();
    for p in avail {
        if p.client != last_client {
            last_client = p.client.clone();
            ui.add_space(4.0);
            ui.label(RichText::new(format!("▸ {}", p.client)).strong());
        }
        let short = p.name.split(':').nth(1).unwrap_or(&p.name);
        let mut checked = sel.contains(&p.name);
        if ui.checkbox(&mut checked, short).clicked() {
            if checked {
                sel.insert(p.name.clone());
            } else {
                sel.remove(&p.name);
            }
        }
        ui.label(
            RichText::new(format!("  {}", p.name))
                .weak()
                .small()
                .monospace(),
        );
    }
}

/// Like [`port_list`], but each output row carries two checkboxes — the
/// left and right side of the stereo mix the port plays. Both ticked =
/// mono downmix `(L + R)/sqrt2` (the classic behaviour); one ticked routes
/// only that side. Unticking both deselects the port.
fn port_list_lr(
    ui: &mut egui::Ui,
    title: &str,
    hint: &str,
    avail: &[PortInfo],
    sel: &mut HashMap<String, u8>,
) {
    ui.label(RichText::new(format!("{}  ({})", title, avail.len())).strong());
    ui.label(RichText::new(hint).weak().small());
    if avail.is_empty() {
        ui.label(RichText::new("no remote audio ports found").weak());
        return;
    }
    let mut last_client = String::new();
    for p in avail {
        if p.client != last_client {
            last_client = p.client.clone();
            ui.add_space(4.0);
            ui.label(RichText::new(format!("▸ {}", p.client)).strong());
        }
        let short = p.name.split(':').nth(1).unwrap_or(&p.name);
        let entry = sel.get(&p.name).copied().unwrap_or(0u8);
        let (mut l, mut r) = (entry & 1 != 0, entry & 2 != 0);
        ui.horizontal(|ui| {
            let rl = ui.checkbox(&mut l, "L").on_hover_text("play the LEFT side");
            let rr = ui
                .checkbox(&mut r, "R")
                .on_hover_text("play the RIGHT side");
            ui.label(RichText::new(short).strong().small());
            ui.label(
                RichText::new(format!("  {}", p.name))
                    .weak()
                    .small()
                    .monospace(),
            );
            if rl.changed() || rr.changed() {
                let bits = (l as u8) | ((r as u8) << 1);
                if bits == 0 {
                    sel.remove(&p.name);
                } else {
                    sel.insert(p.name.clone(), bits);
                }
            }
        });
    }
}

fn midi_monitor(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::bottom("midi")
        .resizable(true)
        .min_size(140.0)
        .default_size(220.0)
        .show(ui, |ui| {
            ui.label(RichText::new("MIDI MONITOR").strong());
            ui.label(
                RichText::new("last detected MIDI input (also printed to stdout)")
                    .weak()
                    .small(),
            );

            if let Some(last) = &app.last_midi {
                ui.add_space(4.0);
                egui::Frame::group(ui.style())
                    .fill(Color32::from_rgb(35, 40, 50))
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("[{}]", last.ts)).weak().monospace());
                            ui.label(RichText::new(last.type_name).strong().monospace());
                            ui.label(RichText::new(&last.describe).monospace());
                            ui.separator();
                            ui.label(
                                RichText::new(format!("| {}", last.raw_hex))
                                    .weak()
                                    .monospace(),
                            );
                        });
                    });
            } else {
                ui.add_space(4.0);
                ui.label(RichText::new("waiting for MIDI input…").weak());
                ui.label(
                    RichText::new(
                        "the audio thread reads the JACK port slooper:midi_in and \
                         listens on every channel — link your device to it (e.g. \
                         pw-link 'your-midi-port' 'slooper:midi_in') and send a \
                         message; new source ports appear below automatically",
                    )
                    .weak()
                    .small(),
                );
            }

            if !app.midi_sources.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("sources linked to slooper:midi_in")
                        .weak()
                        .small(),
                );
                for s in &app.midi_sources {
                    ui.label(RichText::new(format!("  {s}")).weak().small().monospace());
                }
            }

            egui::ScrollArea::vertical()
                .min_scrolled_height(f32::INFINITY)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    // Newest first (log is push_front).
                    for line in app.midi_log.iter() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("[{}]", line.ts)).weak().monospace());
                            ui.label(RichText::new(line.type_name).monospace());
                            ui.label(RichText::new(&line.describe).weak().monospace());
                            ui.label(
                                RichText::new(format!("| {}", line.raw_hex))
                                    .weak()
                                    .monospace(),
                            );
                        });
                    }
                });
        });
}

fn settings_panel(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::left("settings").resizable(true).default_size(410.0).min_size(330.0).show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading(RichText::new("SETTINGS").strong());

            // ---- GUI -----------------------------------------------------
            ui.separator();
            ui.label(RichText::new("GUI").strong());
            ui.horizontal(|ui| {
                let r = ui.add(egui::Slider::new(&mut app.gui_scale, 0.7..=1.6).suffix("×"));
                if r.changed() {
                    app.save_config();
                }
            });
            ui.label(RichText::new("Scales the whole interface; applied live.").weak().small());
            ui.horizontal(|ui| {
                ui.label("tracks:");
                let mut n = app.n_tracks as i64;
                let r = ui.add(
                    egui::DragValue::new(&mut n)
                        .range(1..=MAX_LOOPS as i64)
                        .speed(1),
                );
                if r.changed() {
                    app.n_tracks = n as usize;
                    app.selected = app.selected.min(app.n_tracks - 1);
                    app.bind_loop_sel = app.bind_loop_sel.min(app.n_tracks - 1);
                    app.save_config();
                    app.status = format!("{} tracks", app.n_tracks);
                }
            });
            ui.label(
                RichText::new(
                    "How many tracks the program shows (up to 128 — the engine \
                     capacity). Per-track bindings for hidden tracks are kept — \
                     raise the number again and they return exactly as bound.",
                )
                .weak()
                .small(),
            );

            // ---- latency compensation -------------------------------------
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("latency (ms):");
                ui.add(egui::TextEdit::singleline(&mut app.latency_ms).desired_width(60.0));
                if ui.button("Apply").clicked() {
                    let ms = app.latency_ms.trim().parse::<f32>().unwrap_or(0.0).clamp(0.0, 1000.0);
                    app.latency_ms = format!("{ms}");
                    app.backend.cmd(Cmd::SetLatency { ms });
                    app.save_config();
                    app.status = format!("latency compensation: {ms} ms");
                }
            });
            ui.label(
                RichText::new(
                    "Each fresh recording discards its first this-many milliseconds of \
                     input, trimming the silence gap between pressing REC and that button \
                     click reaching your mic. Set it to your system's capture latency and \
                     the take begins with the hit, not with dead air. Monitoring and the \
                     output mix are never delayed.",
                )
                    .weak()
                    .small(),
            );

            // ---- monitoring -------------------------------------------------
            ui.separator();
            ui.label(RichText::new("MONITORING").strong());
            if ui
                .checkbox(&mut app.monitor_enabled, "live input monitoring")
                .on_hover_text(
                    "When on (default), the live input rides the SELECTED track's \
                     chain: gain × volume × effects × pan. Turn off to silence the \
                     input without touching any level — recording, playback and the \
                     meter are unaffected.",
                )
                .changed()
            {
                app.backend.cmd(Cmd::SetMonitor(app.monitor_enabled));
                app.save_config();
            }

            // ---- asset preview --------------------------------------------
            ui.separator();
            ui.label(RichText::new("ASSET PREVIEW").strong());
            if ui
                .checkbox(&mut app.preview_enabled, "audition rack entries on click")
                .on_hover_text("Clicking an asset-rack entry plays it at the master output (never recorded). Click the entry again to stop.")
                .changed()
            {
                app.save_config();
            }
            ui.horizontal(|ui| {
                ui.label("preview length (s):");
                let prt = ui
                    .add(egui::TextEdit::singleline(&mut app.preview_secs).desired_width(60.0))
                    .on_hover_text("Seconds of audio to audition: shorter entries play in full, longer ones are cut after this many seconds.");
                if prt.lost_focus() {
                    let v = app.preview_secs_f();
                    app.preview_secs = v.to_string();
                    app.save_config();
                }
            });
            ui.label(
                RichText::new(
                    "Entries at most this long play in full; longer ones are auditioned \
                     from the start for exactly this many seconds.",
                )
                .weak()
                .small(),
            );

            // ---- projects --------------------------------------------------
            ui.separator();
            ui.label(RichText::new("PROJECTS").strong());
            ui.horizontal(|ui| {
                ui.label("folder:");
                if ui
                    .add(egui::TextEdit::singleline(&mut app.project_dir).desired_width(220.0))
                    .lost_focus()
                {
                    app.save_config();
                }
                if ui.button("Browse…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("Projects folder").pick_folder()
                    {
                        app.project_dir = p.to_string_lossy().into_owned();
                        app.save_config();
                    }
                }
            });
            ui.label(
                RichText::new(format!(
                    "Projects ({} files) save into this folder: “{}” — empty means ~/slooper-projects. Each project keeps its audio next to it in “<name> audio”.",
                    project::EXT,
                    app.projects_dir().display()
                ))
                .weak()
                .small(),
            );

            // ---- MIDI + keyboard bindings -----------------------------------
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(RichText::new("MIDI + KEYBOARD BINDINGS").strong());
                if ui.button("Reset to Defaults").clicked() {
                    for (k, d) in app.short_keys.iter_mut().zip(SHORTCUT_DEFAULTS.iter()) {
                        *k = *d;
                    }
                    for m in app.midi_binds.iter_mut() {
                        *m = None;
                    }
                    app.save_config();
                    app.status = "bindings reset to defaults".into();
                }
            });
            ui.label(
                RichText::new(
                    "Click a MIDI cell, then send a NOTE ON / CC / PROGRAM \
                     CHANGE from your controller — the first matching message \
                     becomes that binding (click again or Esc to cancel; while \
                     a MIDI cell listens, Delete clears its binding and no \
                     other key does anything). A bound action fires on every \
                     matching message (value/velocity ignored). The key column \
                     works the same way: click, then press the new key (Esc \
                     cancels); Delete clears that cell's binding and is never \
                     bindable itself. Keys are ignored while a text field has \
                     focus.",
                )
                .weak()
                .small(),
            );
            ui.label(RichText::new("global").weak().small());
            for a in [
                MidiAction::RecSelected,
                MidiAction::ClearSelected,
                MidiAction::PlaySelected,
                MidiAction::PlayAll,
                MidiAction::StopAll,
                MidiAction::Export,
                MidiAction::Metronome,
                MidiAction::SelectNext,
                MidiAction::SelectPrev,
            ] {
                bind_row(ui, app, a);
            }
            ui.label(RichText::new("per track bindings").weak().small());
            ui.horizontal(|ui| {
                ui.label("Track");
                egui::ComboBox::from_id_salt("bind_loop_sel")
                    .selected_text(format!("{}", app.bind_loop_sel + 1))
                    .show_ui(ui, |ui| {
                        for n in 0..app.n_tracks {
                            ui.selectable_value(&mut app.bind_loop_sel, n, format!("{}", n + 1));
                        }
                    });
            });
            for off in 0..8usize {
                let a = match off {
                    0 => MidiAction::RecLoop(app.bind_loop_sel),
                    1 => MidiAction::ClearLoop(app.bind_loop_sel),
                    2 => MidiAction::PlayLoop(app.bind_loop_sel),
                    3 => MidiAction::Select(app.bind_loop_sel),
                    4 => MidiAction::VolLoop(app.bind_loop_sel),
                    5 => MidiAction::GainLoop(app.bind_loop_sel),
                    6 => MidiAction::PanLoop(app.bind_loop_sel),
                    _ => MidiAction::MidiRec(app.bind_loop_sel),
                };
                bind_row(ui, app, a);
            }

            // ---- audio ports -------------------------------------------------
            ui.separator();
            ui.label(RichText::new("AUDIO PORTS").strong());
            port_list(ui, "INPUTS", "remote output ports (sources)", &app.avail_in, &mut app.sel_in);
            ui.add_space(8.0);
            port_list_lr(
                ui,
                "OUTPUTS",
                "remote input ports (sinks) — tick L and/or R for what each port plays",
                &app.avail_out,
                &mut app.sel_out,
            );
            ui.horizontal(|ui| {
                if ui.button("Apply Selection").clicked() {
                    // Sort for deterministic slot assignment, so the same
                    // selection maps to the same slots across restarts.
                    let mut ins: Vec<String> = app.sel_in.iter().cloned().collect();
                    ins.sort();
                    let mut outs: Vec<OutSel> = app
                        .sel_out
                        .iter()
                        .filter(|(_, b)| **b != 0)
                        .map(|(n, b)| (n.clone(), *b))
                        .collect();
                    outs.sort();
                    app.applied_ports = Some((ins.clone(), outs.clone()));
                    app.backend.cmd(Cmd::Apply { ins, outs });
                    app.save_config();
                }
                if ui.button("Stop").clicked() {
                    app.backend.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
                }
                if ui.button("Refresh").clicked() {
                    app.backend.cmd(Cmd::Refresh);
                }
            });
        });
    });
}

/// First-save (or Save As) name prompt.
fn save_prompt_window(ui: &mut egui::Ui, app: &mut App) {
    if !app.show_save_prompt {
        return;
    }
    let mut open = true;
    let dir = app.projects_dir();
    let mut do_save = false;
    let mut enter_hit = false;
    egui::Window::new(RichText::new("SAVE PROJECT").strong())
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ui, |ui| {
            ui.set_width(360.0);
            ui.label("Project name:");
            let re = ui.add(egui::TextEdit::singleline(&mut app.save_draft).desired_width(320.0));
            enter_hit = re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.label(
                RichText::new(format!("→ {}/<name>.{}", dir.display(), project::EXT))
                    .weak()
                    .small(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    do_save = true;
                }
                if ui.button("Cancel").clicked() {
                    app.show_save_prompt = false;
                }
            });
        });
    if do_save || enter_hit {
        let n = app.save_draft.clone();
        app.do_save(&n);
    }
    if !open {
        app.show_save_prompt = false;
    }
}

/// Browse the projects folder; one button per saved project.
fn load_window(ui: &mut egui::Ui, app: &mut App) {
    if !app.show_load {
        return;
    }
    let mut open = true;
    let mut load: Option<PathBuf> = None;
    egui::Window::new(RichText::new("LOAD PROJECT").strong())
        .open(&mut open)
        .default_width(420.0)
        .show(ui, |ui| {
            let dir = app.projects_dir();
            ui.label(RichText::new(format!("{}", dir.display())).weak().small());
            let list = project::list_projects(&dir);
            if list.is_empty() {
                ui.label("No projects here yet — Ctrl+S saves the first one.");
            }
            egui::ScrollArea::vertical()
                .max_height(340.0)
                .show(ui, |ui| {
                    for (name, path) in &list {
                        let cur = *name == app.proj_name;
                        let b = ui
                            .add_sized(
                                [390.0, 24.0],
                                egui::Button::new(RichText::new(name).strong()),
                            )
                            .on_hover_text(path.display().to_string());
                        if cur {
                            ui.label(RichText::new("current project").weak().small());
                        }
                        if b.clicked() {
                            load = Some(path.clone());
                        }
                    }
                });
        });
    if let Some(p) = load {
        app.do_load(&p);
    }
    app.show_load = open;
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_table_matches_actions() {
        assert_eq!(SHORTCUT_DEFAULTS.len(), N_ACTIONS);
        // 121 historical + 16 appended `MidiRec` slots.
        assert_eq!(N_ACTIONS, 137);
        // Space is the global transport toggle; knob slots are never keyed.
        assert_eq!(
            SHORTCUT_DEFAULTS[MidiAction::idx(MidiAction::PlayAll)],
            Some(egui::Key::Space)
        );
        assert!(SHORTCUT_DEFAULTS[73..].iter().all(|k| k.is_none()));
        // Every default key is unique (learn mode enforces the same).
        let mut seen: Vec<egui::Key> = SHORTCUT_DEFAULTS.iter().flatten().copied().collect();
        seen.sort_by_key(|k| format!("{k:?}"));
        let n = seen.len();
        seen.dedup_by_key(|k| format!("{k:?}"));
        assert_eq!(seen.len(), n);
    }

    // ---- headless window tests (real widget tree, synthetic pointer) ----

    /// Fresh App with null audio, a scratch config dir, and one rack folder
    /// holding a generated kick.wav. Returns the app and the file's y-row
    /// finder left to the caller (layout is deterministic at px 1.0).
    fn headless_app(tag: &str) -> App {
        std::env::set_var("SLOOPER_NULL_AUDIO", "1");
        let dir = std::env::temp_dir().join(format!("sp-uity-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("slooper")).unwrap();
        std::fs::create_dir_all(dir.join("slooper/effects")).unwrap();
        let samples = dir.join("samples");
        std::fs::create_dir_all(&samples).unwrap();
        // 3 s of sine: v07 clip tests need an asset comfortably longer
        // than the clips they drag (30 k samples) so a 200 px edge drag
        // at the default 160 px/s trims well past the asserted delta.
        let data: Vec<f32> = (0..144_000)
            .map(|i| (i as f32 * 0.05).sin() * 0.5)
            .collect();
        crate::audio_io::export(
            &samples.join("kick.wav"),
            48000,
            &data,
            crate::audio_io::ExportFormat::Wav,
        )
        .unwrap();
        std::fs::write(
            dir.join("slooper/config.json"),
            format!(r#"{{"rack_folders":["{}"]}}"#, samples.display()),
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let mut app = App::new(Backend::start());
        app.drain_events(); // discard the null-audio notice
        app.sample_rate = 48_000; // null audio never sends Evt::Ready
                                  // Same wiring Evt::Ready performs in the real app: shared default
                                  // piano instance + per-track bindings into the engine, so engine
                                  // paths like bounce_track_preview have their fallback voice.
        app.rebuild_all_synths();
        // rebuild_all_synths sends SetDefaultSynth through the manager
        // queue (async). Tests call engine paths SYNCHRONOUSLY, so apply
        // the same default voice to the shared engine directly too.
        app.backend.engine.set_default_synth(
            app.synth_registry
                .create_instance("piano", 48_000.0, &[])
                .map(|i| std::sync::Arc::new(SynthHandle::Script(i))),
        );
        app
    }

    /// One UI frame with the given pointer events.
    fn frame(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
    }

    fn moved(p: egui::Pos2) -> egui::Event {
        egui::Event::PointerMoved(p)
    }
    fn btn(p: egui::Pos2, b: egui::PointerButton, down: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: p,
            button: b,
            pressed: down,
            modifiers: Default::default(),
        }
    }

    /// v11 pin ("clips STRETCH instead of clip when the length is
    /// changed"): trimming a clip edge must anchor the content per-sample
    /// and hide the rest — never rescale it into the new rect. A pattern
    /// with a note at tick 0 and one at 90 % of the pattern: after a tail
    /// trim past half, only the FIRST note may still be painted, and its
    /// x position must not have moved. The bug: the paint window was
    /// `(cut, avail)` while the rect covered `span`, so the whole
    /// remainder scaled into the trimmed rect (both notes always visible,
    /// squished).
    #[test]
    fn v11_clip_trim_clips_paint_without_stretch() {
        let mut app = headless_app("v11stretch");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let mut pat = crate::midipat::Pattern::new(120);
        pat.len_ticks = 32 * 480; // 16 s at 120 BPM = 640 px @ 40 px/s
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 120,
        });
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note: 72,
            vel: 100,
            press: 0,
            start: (pat.len_ticks as f32 * 0.9) as u32,
            len: 120,
        });
        let a = app.assets.len() as u32;
        app.assets.push(crate::assets::Asset::midi(
            a,
            "two-note".into(),
            std::path::PathBuf::from("/tmp/two-note.mid"),
            std::sync::Arc::new(pat),
            false,
        ));
        app.px_per_sec = 40.0;
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        app.select_one((0, 0, 0));
        app.last_click_track = Some(0);
        app.snap_on = false;

        // Mini piano-roll bars: the unselected/selected note-bar colors.
        let note_bars = |app: &mut App| -> Vec<f32> {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let shapes = std::mem::take(&mut out.shapes);
            out.textures_delta.clear();
            shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    egui::epaint::Shape::Rect(r) => {
                        let (cr, cg, cb, ca) = r.fill.to_tuple();
                        let blue = ca == 255
                            && ((cr.abs_diff(120) < 8 && cg.abs_diff(205) < 8 && cb == 255)
                                || (cr.abs_diff(180) < 8 && cg.abs_diff(235) < 8 && cb == 255));
                        (blue && r.rect.width() >= 1.0 && r.rect.height() <= 6.0)
                            .then_some(r.rect.min.x)
                    }
                    _ => None,
                })
                .collect()
        };
        let before = note_bars(&mut app);
        assert_eq!(
            before.len(),
            2,
            "both notes painted before the trim: {before:?}"
        );
        let (n0, n1) = (
            before.iter().copied().reduce(f32::min).unwrap(),
            before.iter().copied().fold(f32::MIN, f32::max),
        );

        // Grab the clip's right edge (selected body rect) and trim ~200 px.
        let sel_rgb = {
            let c = crate::ui::COL_CLIP.linear_multiply(0.45);
            let (r, g, b, _) = c.to_tuple();
            (r, g, b)
        };
        let body = painted_rects(&mut app, &ctx, sel_rgb)
            .into_iter()
            .max_by(|a, b| a.area().total_cmp(&b.area()))
            .expect("clip body painted");
        let cy = body.center().y;
        drag_pt(
            &mut app,
            &ctx,
            egui::pos2(body.max.x - 2.0, cy),
            egui::pos2(body.max.x - 160.0, cy),
        );
        let cl = app.arr.tracks[0].lanes[0].clips[0];
        let alen = app.asset_len(a);
        assert!(
            cl.end(alen) - cl.pos <= alen * 4 / 5 + 1,
            "tail drag must trim away >= 20 %: {cl:?}"
        );

        let after = note_bars(&mut app);
        assert_eq!(
            after.len(),
            1,
            "the 90 % note is CLIPPED out, not squeezed in: {after:?}"
        );
        assert!(
            (after[0] - n0.min(n1)).abs() < 2.0,
            "the surviving note sits at the SAME pixel (anchored, not stretched): {} vs {}",
            after[0],
            n0.min(n1)
        );
    }

    /// v11 pin, audio half: a one-shot burst at the END of the asset must
    /// vanish when the tail is trimmed, not be stretched to stay visible.
    #[test]
    fn v11_audio_trim_clips_waveform_without_stretch() {
        let mut app = headless_app("v11wave");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        // 8 s: silent for 75 %, loud burst in the last quarter.
        let n = 48_000 * 8;
        let mut data = vec![0.0f32; n];
        for (i, v) in data.iter_mut().enumerate().skip(n * 3 / 4) {
            *v = ((i % 20) as f32 * 0.6).sin().min(1.0);
        }
        let data: std::sync::Arc<[f32]> = data.into();
        let a = app.assets.len() as u32;
        app.assets.push(crate::assets::Asset::audio(
            a,
            "burst".into(),
            std::path::PathBuf::from("/tmp/burst-test.wav"),
            data,
            false,
        ));
        app.px_per_sec = 40.0;
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        app.select_one((0, 0, 0));
        app.last_click_track = Some(0);
        app.snap_on = false;

        // Tall waveform strokes = burst columns (peak-normalized to ~full
        // lane height) in the selected-clip waveform color; grid hairlines
        // are tall too but a different color, and silence paints ~zero-
        // extent strokes.
        let tall = |app: &mut App| -> usize {
            let (wr, wg, wb, _) = crate::ui::COL_CLIP_SEL.to_tuple();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let shapes = std::mem::take(&mut out.shapes);
            out.textures_delta.clear();
            shapes
                .iter()
                .filter(|cs| match &cs.shape {
                    egui::epaint::Shape::LineSegment { points, stroke } => {
                        let (r, g, b, _) = stroke.color.to_tuple();
                        (points[1].y - points[0].y).abs() > 8.0
                            && r.abs_diff(wr) < 8
                            && g.abs_diff(wg) < 8
                            && b.abs_diff(wb) < 8
                    }
                    _ => false,
                })
                .count()
        };
        assert!(tall(&mut app) > 10, "burst columns painted before the trim");
        let sel_rgb = {
            let c = crate::ui::COL_CLIP.linear_multiply(0.45);
            let (r, g, b, _) = c.to_tuple();
            (r, g, b)
        };
        let body = painted_rects(&mut app, &ctx, sel_rgb)
            .into_iter()
            .max_by(|a, b| a.area().total_cmp(&b.area()))
            .expect("clip body painted");
        let cy = body.center().y;
        drag_pt(
            &mut app,
            &ctx,
            egui::pos2(body.max.x - 2.0, cy),
            egui::pos2(body.max.x - 170.0, cy),
        );
        let cl = app.arr.tracks[0].lanes[0].clips[0];
        let alen = app.asset_len(a);
        assert!(
            cl.end(alen) < cl.pos + alen * 7 / 10,
            "trim must end before the burst: {cl:?}"
        );
        assert_eq!(
            tall(&mut app),
            0,
            "trimmed-away burst must NOT be stretched into view"
        );
    }

    /// SETTINGS must dock to the LEFT of the asset rack (add-order test:
    /// both panels are left panels; whichever is added first sits outermost,
    /// so its heading text renders further left).
    #[test]
    fn settings_panel_pops_up_left_of_the_rack() {
        let mut app = headless_app("settings");
        let ctx = egui::Context::default();
        app.show_settings = true;
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        // Read headings from the pre-tessellate shapes (0.36 tessellates
        // text away into callbacks, but the Shape list still carries galleys).
        let mut x_settings = f32::MAX;
        let mut x_rack = f32::MAX;
        for cs in &out.shapes {
            if let egui::epaint::Shape::Text(t) = &cs.shape {
                let txt = t.galley.text();
                if txt.contains("SETTINGS") {
                    x_settings = x_settings.min(t.pos.x);
                }
                if txt.contains("ASSET RACK") {
                    x_rack = x_rack.min(t.pos.x);
                }
            }
        }
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        assert!(
            x_settings < f32::MAX && x_rack < f32::MAX,
            "both headings painted"
        );
        assert!(
            x_settings < x_rack,
            "SETTINGS x={x_settings} must be left of ASSET RACK x={x_rack}"
        );
    }

    /// The full user gesture: press a rack row, drag onto a track's lane
    /// strip, release -> a clip lands on that track (and Delete-selection
    /// is set). Then a right-click on the track header clears it again.
    /// Drives the real egui DnD plugin end to end — this is the regression
    /// test for "dragging from the rack dropped nothing".
    #[test]
    fn rack_drag_drops_a_clip_and_grid_rmb_menus() {
        let mut app = headless_app("drop");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]); // layout pass

        // Click-probe the rack rows to locate kick.wav deterministically.
        let mut y_row = None;
        for y in (96..220).step_by(3) {
            let p = egui::pos2(40.0, y as f32);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Primary, false)],
            );
            if app.sel_asset.is_some() {
                y_row = Some(y as f32);
                break;
            }
        }
        let y_row = y_row.expect("a rack row was clickable (kick.wav)");

        // Drag: press, move past the drag threshold, hover the lane strip of
        // track 1, release there (well inside the lane: the separation bar
        // under track 0 owns a few pixels above it now).
        let start = egui::pos2(40.0, y_row);
        let lane = egui::pos2(500.0, 240.0);
        frame(
            &mut app,
            &ctx,
            vec![moved(start), btn(start, egui::PointerButton::Primary, true)],
        );
        frame(&mut app, &ctx, vec![moved(egui::pos2(120.0, y_row + 6.0))]);
        frame(&mut app, &ctx, vec![moved(egui::pos2(320.0, 170.0))]);
        frame(&mut app, &ctx, vec![moved(lane)]);
        assert!(
            egui::DragAndDrop::has_payload_of_type::<usize>(&ctx),
            "drag carries the asset"
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(lane, egui::PointerButton::Primary, false)],
        );

        let total = |a: &App| -> usize {
            a.arr
                .tracks
                .iter()
                .map(|t| t.lanes.iter().map(|l| l.clips.len()).sum::<usize>())
                .sum()
        };
        assert_eq!(total(&app), 1, "drag-drop placed exactly one clip");
        assert!(!app.sel_clips.is_empty(), "the dropped clip is selected");
        assert_eq!(
            app.status,
            "clip placed on track 2 \u{b7} track \u{b7} drag its edges to trim"
        );

        // v0.15: right-clicking the grid no longer destroys the track — it
        // opens the clip menu, and deletion moved behind an explicit item.
        // Settle first: the drop may open a child lane and re-row the
        // grid for a few frames, and a press during a re-row belongs to no
        // widget. Park the pointer clear of everything, idle, then click.
        frame(&mut app, &ctx, vec![moved(egui::pos2(900.0, 600.0))]);
        for _ in 0..4 {
            frame(&mut app, &ctx, vec![]);
        }
        let p = egui::pos2(700.0, 246.0);
        frame(&mut app, &ctx, vec![moved(p)]);
        frame(&mut app, &ctx, vec![moved(p)]);
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Secondary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Secondary, false)],
        );
        assert_eq!(
            total(&app),
            1,
            "right-click kept the clips (no more instant clear)"
        );
        // Right on the clip body: the clip menu opens.
        if galley_at(&mut app, &ctx, "Duplicate").is_some() {
            let del = galley_at(&mut app, &ctx, "Delete")
                .expect("Delete item")
                .center();
            frame(
                &mut app,
                &ctx,
                vec![moved(del), btn(del, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(del, egui::PointerButton::Primary, false)],
            );
            assert_eq!(total(&app), 0, "menu Delete removed the clip");
        } else {
            // Right-click landed on empty grid: the track menu with an
            // explicit Clear item must be there instead.
            let cl = galley_at(&mut app, &ctx, "Clear this track")
                .expect("Clear item")
                .center();
            frame(
                &mut app,
                &ctx,
                vec![moved(cl), btn(cl, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(cl, egui::PointerButton::Primary, false)],
            );
            assert_eq!(total(&app), 0, "menu Clear emptied the track");
        }
    }

    #[test]
    fn loop_region_follows_mode() {
        let mut app = headless_app("loopmode");
        // 120 bpm, 4/4 at 48 kHz -> one bar = 96_000 samples.
        app.metro_bpm = "120".into();
        app.metro_num = "4".into();
        assert_eq!(app.bar_samples(), 96_000);
        app.loop_mode = 0;
        app.loop_area = (0, 0);
        assert_eq!(app.loop_region_eff(), (0, 192_000), "area default = 2 bars");
        app.loop_area = (24_000, 96_000);
        assert_eq!(
            app.loop_region_eff(),
            (24_000, 96_000),
            "dragged handles win"
        );
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        let len = app.asset_len(a);
        app.loop_mode = 1;
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        assert_eq!(
            app.loop_region_eff(),
            (0, 48_000 + len),
            "longest = last clip end"
        );
        app.loop_mode = 2;
        assert_eq!(app.loop_region_eff(), (0, app.total_len()));
    }

    #[test]
    fn track_height_grows_with_lanes() {
        let mut app = headless_app("trackh");
        assert_eq!(app.track_h(0), 96.0, "one lane = the mixer minimum");
        app.arr.add_lane(0);
        assert_eq!(app.track_h(0), 96.0, "two lanes still fit the minimum");
        app.arr.add_lane(0);
        assert_eq!(app.track_h(0), 144.0, "three lanes extend the track");
    }

    #[test]
    fn key_names_roundtrip() {
        for k in [
            egui::Key::A,
            egui::Key::Num3,
            egui::Key::F7,
            egui::Key::Space,
            egui::Key::M,
        ] {
            assert_eq!(key_from_name(&key_debug(k)), Some(k), "roundtrip {k:?}");
        }
    }

    #[test]
    fn level_text_roundtrip() {
        // v0.15: the text boxes moved into the Set value dialog; the SAME
        // dB semantics live in value_user_parse / value_user_display.
        use crate::automation::{TARGET_TRACK_PAN, TARGET_TRACK_VOL};
        assert_eq!(db_num(vol_amp(0)), "-inf");
        assert_eq!(
            value_user_display(TARGET_TRACK_VOL, vol_amp(VOL_UNITY_MIDI)),
            "+0.0"
        );
        assert_eq!(value_user_display(TARGET_TRACK_VOL, vol_amp(127)), "+6.0");
        // Typed values are 1:1 dB and round-trip through the fader.
        let db = |t: &str| {
            let v = value_user_parse(TARGET_TRACK_VOL, t)?;
            Some(vol_midi(if v <= 0.0 {
                f32::NEG_INFINITY
            } else {
                20.0 * v.log10()
            }))
        };
        assert_eq!(db("+0.0"), Some(VOL_UNITY_MIDI));
        assert_eq!(db("+6.0"), Some(127));
        assert_eq!(db("-inf"), Some(0));
        assert_eq!(db("-12.3"), Some(vol_midi(-12.3)));
        // Monotone over the whole range, and unity sits at 0 dB exactly.
        for v in 1..127u8 {
            assert!(vol_amp(v) < vol_amp(v + 1), "vol_amp not monotone at {v}");
        }
        assert!((20.0 * vol_amp(VOL_UNITY_MIDI).log10()).abs() < 0.02);
        // Pan: centre reads 0.
        assert_eq!(value_user_display(TARGET_TRACK_PAN, 0.5), "0");
        assert_eq!(value_user_parse(TARGET_TRACK_PAN, "0"), Some(0.5));
    }

    #[test]
    fn grid_units_follow_subdivision() {
        // 120 BPM: quarter = 2× eighth; snap unit ordering is monotonic.
        let (sr, bpm) = (48_000, 120);
        assert!(quantize::unit_samples(sr, bpm, 1) > quantize::unit_samples(sr, bpm, 2));
        assert!(quantize::unit_samples(sr, bpm, 2) > quantize::unit_samples(sr, bpm, 3));
    }

    #[test]
    fn projects_dir_defaults_to_home() {
        let cfg = Config::default();
        assert!(cfg.projects_dir().ends_with("slooper-projects"));
    }

    // ---- track resizing, lane growth, wheel routing ----

    #[test]
    fn separator_resize_is_per_track_and_clamped() {
        let mut app = headless_app("resize");
        assert_eq!(app.track_h(0), TRACK_MIN_H, "automatic height");
        app.set_track_h(0, 200.0);
        assert_eq!(app.track_h(0), 200.0);
        assert_eq!(
            app.track_h(1),
            TRACK_MIN_H,
            "a sibling keeps its own height"
        );
        app.set_track_h(0, 10.0);
        assert_eq!(app.track_h(0), TRACK_MIN_H, "clamped to the lane floor");
    }

    #[test]
    fn new_lane_duplicates_a_lanes_height() {
        // The child lane must not steal room from the parent: the track
        // grows by one old-lane height so every lane keeps its size.
        let mut app = headless_app("grow");
        app.set_track_h(0, 200.0);
        app.arr.tracks[0].add_lane();
        app.grow_for_lanes(0, 1);
        assert_eq!(app.track_h(0), 400.0, "two lanes at the old lane height");
        // One number for the family: the lanes share whatever the user sets.
        app.set_track_h(0, 120.0);
        assert_eq!(app.track_h(0), 120.0);
        // ... but never below the track floor (two squeeze-able lanes
        // still can't shrink the block under TRACK_MIN_H).
        app.set_track_h(0, 40.0);
        assert_eq!(app.track_h(0), TRACK_MIN_H);
    }

    #[test]
    fn wheel_modifiers_zoom_and_pan() {
        // Ctrl+wheel zooms the timeline, Shift+wheel pans it horizontally —
        // the regression test for "ctrl/shift + scrollwheel does nothing".
        let mut app = headless_app("wheel");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let z0 = app.px_per_sec;
        let wheel = |dy: f32, m: egui::Modifiers| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            modifiers: m,
        };
        frame(&mut app, &ctx, vec![wheel(3.0, egui::Modifiers::COMMAND)]);
        assert!(app.px_per_sec > z0, "ctrl+wheel zooms in");
        frame(&mut app, &ctx, vec![wheel(-3.0, egui::Modifiers::COMMAND)]);
        assert!((app.px_per_sec - z0).abs() < 1.0, "and back out again");
        // Zoom far in first: horizontal scroll is clamped to the content
        // width, and an empty timeline is narrower than the viewport.
        app.px_per_sec = 1200.0;
        frame(&mut app, &ctx, vec![wheel(-2.0, egui::Modifiers::SHIFT)]);
        assert!(app.hscroll > 0.0, "shift+wheel pans the timeline");
        // A plain wheel must NOT zoom (ScrollAreas keep vertical scrolling).
        let z1 = app.px_per_sec;
        frame(&mut app, &ctx, vec![wheel(5.0, egui::Modifiers::NONE)]);
        assert_eq!(app.px_per_sec, z1, "plain wheel leaves zoom alone");
    }

    // ---- regression pins: zoom range, sensitivity & centred default ----

    #[test]
    fn zoom_is_fast_falls_far_and_centers_default() {
        let mut app = headless_app("zoom");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let wheel = |dy: f32, m: egui::Modifiers| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            modifiers: m,
        };
        // One notch must move the view visibly (~12 %, was 0.6 % — the
        // "ctrl+wheel is too slow" complaint).
        let z0 = app.px_per_sec;
        frame(&mut app, &ctx, vec![wheel(1.0, egui::Modifiers::COMMAND)]);
        assert!(
            app.px_per_sec >= z0 * 1.08,
            "one ctrl+wheel notch zoomed only {:.1} %",
            (app.px_per_sec / z0 - 1.0) * 100.0
        );
        // One notch out lands back (log-space accumulation is symmetric).
        frame(&mut app, &ctx, vec![wheel(-1.0, egui::Modifiers::COMMAND)]);
        assert!(
            (app.px_per_sec - z0).abs() < 0.5,
            "notch in then out cancels"
        );
        // Max zoom-out shows at least 100 measures in the ~936 pt viewport
        // of the 1440×853 window at 80 BPM 4/4 (3 s per bar — the exact
        // geometry the report quoted: 13 bars at the old 24 px/s floor).
        let bar_s = 60.0 * 4.0 / 80.0;
        let viewport_pt = 936.0;
        let bars = viewport_pt / (ZOOM_MIN * bar_s);
        assert!(bars >= 100.0, "ZOOM_MIN shows only {bars:.0} bars");
        // Wheeling all the way out lands exactly on the floor.
        app.px_per_sec = ZOOM_DEFAULT;
        for _ in 0..60 {
            frame(&mut app, &ctx, vec![wheel(-3.0, egui::Modifiers::COMMAND)]);
        }
        assert_eq!(app.px_per_sec, ZOOM_MIN);
        // The logarithmic slider keeps the untouched default near centre.
        let t = (ZOOM_DEFAULT.ln() - ZOOM_MIN.ln()) / (ZOOM_MAX.ln() - ZOOM_MIN.ln());
        assert!(
            (0.45..=0.75).contains(&t),
            "default sits at {t:.2} of the slider"
        );
    }

    // ---- regression pins: metronome editor + start/stop button ----

    /// Center of the first painted glyph galley exactly `want` (top-level
    /// draw order: the top bar paints before every dock).
    fn label_center(app: &mut App, ctx: &egui::Context, want: &str) -> Option<egui::Pos2> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let mut c = None;
        for cs in &out.shapes {
            if let egui::epaint::Shape::Text(t) = &cs.shape {
                if t.galley.text() == want {
                    c = Some(t.pos + t.galley.rect.center().to_vec2());
                    break;
                }
            }
        }
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        c
    }

    fn key_ev(key: egui::Key, m: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: m,
        }
    }

    /// One frame driving the REAL shortcut pipeline (logic's
    /// handle_shortcuts before ui_root), then run the widget tree.
    fn frame_keys(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            app.handle_shortcuts(ui.ctx());
            app.ui_root(ui);
        });
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
    }

    /// v16: one async-plumbing tick for headless pins — exactly the
    /// per-tick drains `logic()` runs for plugin work (editor reaping +
    /// `poll_vst_opens` landing async loads/scans), without an eframe
    /// `Frame`. Pump loops that wait for plugin results must call it;
    /// `frame()` alone only paints.
    fn logic_tick(app: &mut App) {
        app.reap_vst_editors();
        app.poll_vst_opens();
    }

    /// The regression test for "Ctrl+C copies, Ctrl+V pastes nothing":
    /// copy must remember the selected clip's asset and paste must place a
    /// new clip at the playhead on the last-clicked track.
    #[test]
    fn ctrl_c_v_copies_and_pastes_clip() {
        let mut app = headless_app("copyv");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]); // layout pass
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        app.select_one((0, 0, 0));
        app.last_click_track = Some(0);
        let total = |a: &App| -> usize {
            a.arr
                .tracks
                .iter()
                .map(|t| t.lanes.iter().map(|l| l.clips.len()).sum::<usize>())
                .sum()
        };
        assert_eq!(total(&app), 1);
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::C, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(
            app.clipboard.as_ref().map(|c| c.items[0].asset),
            Some(a),
            "Ctrl+C remembered the asset"
        );
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::V, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(total(&app), 2, "Ctrl+V placed a second clip");
    }

    /// End-to-end clipboard through the REAL gestures: rack DnD drop, click
    /// to select, Ctrl+C, click the ruler to move the playhead, Ctrl+V.
    /// Mirrors exactly what winit delivers (ModifiersChanged frames for the
    /// Ctrl hold, Key press inside the same event batch).
    #[test]
    fn clipboard_survives_real_click_flow() {
        let mut app = headless_app("clipflow");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]); // layout pass
                                            // 1. Rack DnD drop onto a lane (same recipe as the DnD pin test).
        let mut y_row = None;
        for y in (96..220).step_by(3) {
            let p = egui::pos2(40.0, y as f32);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Primary, false)],
            );
            if app.sel_asset.is_some() {
                y_row = Some(y as f32);
                break;
            }
        }
        let y_row = y_row.expect("rack row");
        let start = egui::pos2(40.0, y_row);
        let lane = egui::pos2(500.0, 240.0);
        frame(
            &mut app,
            &ctx,
            vec![moved(start), btn(start, egui::PointerButton::Primary, true)],
        );
        frame(&mut app, &ctx, vec![moved(egui::pos2(120.0, y_row + 6.0))]);
        frame(&mut app, &ctx, vec![moved(egui::pos2(500.0, 240.0))]);
        frame(
            &mut app,
            &ctx,
            vec![btn(lane, egui::PointerButton::Primary, false)],
        );
        let total = |a: &App| -> usize {
            a.arr
                .tracks
                .iter()
                .map(|t| t.lanes.iter().map(|l| l.clips.len()).sum::<usize>())
                .sum()
        };
        assert_eq!(total(&app), 1, "drop placed a clip");
        assert!(!app.sel_clips.is_empty(), "drop selected it");

        // 2. Ctrl+C with a Ctrl-hold ModifiersChanged frame.
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::C, egui::Modifiers::COMMAND),
            ],
        );
        assert!(app.clipboard.is_some(), "Ctrl+C copied the selection");

        // 3. Click the ruler to move the playhead (null audio cannot seek,
        // so this only proves the ruler click does not eat the clipboard).
        let r = egui::pos2(650.0, 128.0); // pinned ruler band
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                moved(r),
                btn(r, egui::PointerButton::Primary, true),
            ],
        );
        frame_keys(
            &mut app,
            &ctx,
            vec![btn(r, egui::PointerButton::Primary, false)],
        );

        // 4. Ctrl+V -> a second clip exists.
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::V, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(total(&app), 2, "Ctrl+V placed the clip at the playhead");
        // Paste must scroll the view to the new clip (the "paste does
        // nothing" complaint = clip landed off-screen and nothing moved).
        assert!(
            app.hscroll_visible(48_000),
            "view scrolled so the pasted clip is visible"
        );
    }

    /// Click, select-all, replace with `value`, Enter. `find` answers
    /// whether the click landed on the widget under test after typing "7".
    fn type_into_focused(app: &mut App, ctx: &egui::Context, value: &str) {
        frame(app, ctx, vec![key_ev(egui::Key::Home, Default::default())]);
        frame(
            app,
            ctx,
            vec![key_ev(egui::Key::End, egui::Modifiers::SHIFT)],
        );
        for c in value.chars() {
            frame(app, ctx, vec![egui::Event::Text(c.to_string())]);
        }
        // Enter: single-line TextEdits commit by losing focus (egui 0.36).
        frame(app, ctx, vec![key_ev(egui::Key::Enter, Default::default())]);
    }

    /// The regression test for "bpm and beats/measure can never be edited
    /// — the box always snaps back to 120/4": typing must stick to the app
    /// state, Enter commits, and the saved config follows.
    #[test]
    fn metro_tempo_boxes_are_editable_and_commit() {
        let mut app = headless_app("metroedit");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);

        // Find the bpm box: click-scan the top bar, type a digit, and the
        // app field that moved says which box got focus.
        let mut found = None;
        for x in (160..1000).step_by(4) {
            let p = egui::pos2(x as f32, 22.0);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Primary, false)],
            );
            let before = (app.metro_bpm.clone(), app.metro_num.clone());
            frame(&mut app, &ctx, vec![egui::Event::Text("7".into())]);
            if app.metro_bpm != before.0 {
                found = Some(("bpm", x));
                break;
            }
            app.metro_bpm = before.0;
            app.metro_num = before.1;
        }
        let (_, x_bpm) = found.expect("a click in the top bar focuses the bpm box");
        type_into_focused(&mut app, &ctx, "96");
        assert_eq!(app.metro_bpm, "96", "typed bpm survives the frame loop");

        // Same trick for the beats box, scanning right of the bpm box.
        let mut found = None;
        for x in (x_bpm + 10..1040).step_by(4) {
            let p = egui::pos2(x as f32, 22.0);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Primary, false)],
            );
            let before = (app.metro_bpm.clone(), app.metro_num.clone());
            frame(&mut app, &ctx, vec![egui::Event::Text("7".into())]);
            if app.metro_num != before.1 {
                found = Some(x);
                break;
            }
            app.metro_bpm = before.0;
            app.metro_num = before.1;
        }
        assert!(found.is_some(), "a click focuses the beats box");
        type_into_focused(&mut app, &ctx, "3");
        assert_eq!(app.metro_num, "3", "typed beats-per-measure survives");

        // Commit landed in the config (send_metro saves on commit).
        let cfg_txt = std::fs::read_to_string(Config::path()).unwrap();
        assert!(cfg_txt.contains("\"metro_bpm\": 96"), "config: {cfg_txt}");
        assert!(cfg_txt.contains("\"metro_num\": 3"), "config: {cfg_txt}");
    }

    /// The metro checkbox became a start/stop button: clicking it toggles
    /// the click and relabels (METRO → STOP → METRO).
    #[test]
    fn metro_button_starts_and_stops() {
        let mut app = headless_app("metrobtn");
        let ctx = egui::Context::default();
        let p = label_center(&mut app, &ctx, "METRO").expect("METRO button painted");
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        assert!(app.metro_on, "click starts the click track");
        let p = label_center(&mut app, &ctx, "STOP").expect("running button reads STOP");
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        assert!(!app.metro_on, "clicking again stops it");
    }

    // ---- regression pins: asset preview, master widgets, snap grid ----

    /// Clicking a rack row auditions the sample; clicking the same row
    /// again stops it; the Settings checkbox disables the whole feature.
    #[test]
    fn rack_click_previews_and_toggles() {
        let mut app = headless_app("rackprev");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let click = |app: &mut App, ctx: &egui::Context, y: f32| {
            let p = egui::pos2(40.0, y);
            frame(
                app,
                ctx,
                vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
            );
            frame(app, ctx, vec![btn(p, egui::PointerButton::Primary, false)]);
        };
        // Find kick.wav's row: the click that starts a preview.
        let mut y_row = None;
        for y in (96..220).step_by(3) {
            click(&mut app, &ctx, y as f32);
            if app.preview_playing.is_some() {
                y_row = Some(y as f32);
                break;
            }
        }
        let y_row = y_row.expect("clicking a rack row previews it");
        assert!(app
            .preview_playing
            .as_deref()
            .is_some_and(|p| p.ends_with("kick.wav")));
        assert!(app.status.starts_with("previewing kick"), "{}", app.status);
        // Second click on the same entry stops (toggle).
        click(&mut app, &ctx, y_row);
        assert_eq!(
            app.preview_playing, None,
            "clicking the auditioning row stops it"
        );
        // Disabled in Settings → clicks never preview.
        app.preview_enabled = false;
        click(&mut app, &ctx, y_row);
        assert_eq!(app.preview_playing, None, "preview disabled in settings");
        // Re-enabled → clicks audition again.
        app.preview_enabled = true;
        click(&mut app, &ctx, y_row);
        assert!(app.preview_playing.is_some(), "re-enabled preview works");
    }

    /// Preview length parses/clamps and defaults to 5 s (config defaults
    /// are pinned in config::tests; the cut logic lives in preview_asset).
    #[test]
    fn preview_secs_parses_and_clamps() {
        let mut app = headless_app("prevsecs");
        assert_eq!(app.preview_secs_f(), 5.0, "default 5 s");
        app.preview_secs = "12.5".into();
        assert_eq!(app.preview_secs_f(), 12.5);
        app.preview_secs = "junk".into();
        assert_eq!(app.preview_secs_f(), 5.0, "garbage falls back to 5");
        app.preview_secs = "9999".into();
        assert_eq!(app.preview_secs_f(), 600.0, "clamped");
    }

    /// v0.15 pin: the master row's dB text box is gone (a plain readout
    /// now); right-click answers the two-item menu, and "Set value" opens
    /// the dialog that applies 1:1 dB (typed -12 = -12 dB). The "0"
    /// zeroize button still resets to 0 dB.
    #[test]
    fn v15_ui_master_set_value_dialog() {
        let mut app = headless_app("v15mastervol");
        let ctx = egui::Context::default();
        app.master_midi = 50;
        frame(&mut app, &ctx, vec![]);
        // Zeroize button: the first painted "0" galley is the mixer dock's
        // master strip — panels draw in add order and the master strip
        // precedes every track strip inside the mixer.
        let z = label_center(&mut app, &ctx, "0").expect("a \"0\" button is painted");
        frame(
            &mut app,
            &ctx,
            vec![moved(z), btn(z, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(z, egui::PointerButton::Primary, false)],
        );
        assert_eq!(
            app.master_midi, VOL_UNITY_MIDI,
            "zeroize resets master to 0 dB"
        );

        // Right-click the dB readout: the two-item menu opens.
        let want = format!("{:>5}", db_num(vol_amp(app.master_midi)));
        let cp = label_center(&mut app, &ctx, &want)
            .expect("master dB readout is painted")
            .to_vec2();
        let p = egui::pos2(cp.x, cp.y);
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Secondary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Secondary, false)],
        );
        let sv = galley_at(&mut app, &ctx, "Set value").expect("Set value item is painted");
        assert!(
            galley_at(&mut app, &ctx, "Create automation asset").is_some(),
            "the menu also offers Create automation asset"
        );

        // Clicking Set value opens the dialog bound to master.vol.
        let sv = sv.center();
        frame(
            &mut app,
            &ctx,
            vec![moved(sv), btn(sv, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(sv, egui::PointerButton::Primary, false)],
        );
        assert!(app.value_dialog.is_some(), "Set value opens the dialog");
        assert_eq!(app.value_dialog.as_ref().unwrap().1, "master.vol");

        // Type -12 and commit: 1:1 dB semantics (NOT MIDI 12, NOT -12/127).
        type_into_focused(&mut app, &ctx, "-12");
        assert_eq!(
            app.master_midi,
            vol_midi(-12.0),
            "typed dB value applies to master 1:1"
        );
        assert!(app.value_dialog.is_none(), "dialog closes on apply");
    }

    /// v0.15: a fresh automation curve spans the SELECTED clips, else the
    /// longest track in the arrangement.
    #[test]
    fn v15_auto_open_spans_selection_then_longest_track() {
        let mut app = headless_app("v15autolen");
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        let mk = |pos: u64, len: u64| Clip {
            asset: a,
            pos,
            cut: 0,
            play_len: Some(len),
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
        };
        app.arr.insert(0, 0, mk(0, 192_000));
        app.arr.insert(1, 0, mk(0, 48_000));
        app.open_auto_editor(None);
        assert_eq!(
            app.auto_edit.as_ref().unwrap().len_samples,
            192_000,
            "fresh curve spans the LONGEST track"
        );
        app.auto_edit = None;
        app.select_one((1, 0, 0));
        app.open_auto_editor(None);
        assert_eq!(
            app.auto_edit.as_ref().unwrap().len_samples,
            48_000,
            "with a selection the curve spans the SELECTED clip"
        );
    }

    /// v0.15 pin: pressing on a node and dragging MOVES it (the old code
    /// armed the drag only on clicked(), which never fires once you move).
    #[test]
    fn v15_auto_canvas_press_drag_moves_node() {
        let mut app = headless_app("v15autodrag");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        app.open_auto_editor(None);
        frame(&mut app, &ctx, vec![]);
        let n0 = app.auto_edit.as_ref().unwrap().nodes.len();
        // Find the curve canvas: a click inside adds exactly one node.
        let mut cp = None;
        'outer: for y in (380i32..=560).step_by(6) {
            for x in (260i32..=700).step_by(40) {
                let p = egui::pos2(x as f32, y as f32);
                frame(
                    &mut app,
                    &ctx,
                    vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
                );
                frame(
                    &mut app,
                    &ctx,
                    vec![btn(p, egui::PointerButton::Primary, false)],
                );
                let n = app.auto_edit.as_ref().unwrap().nodes.len();
                if n == n0 + 1 {
                    cp = Some(p);
                    break 'outer;
                }
                // Undo a hit that landed on a node instead (selects, no add).
                assert!(n == n0, "click never deletes nodes");
            }
        }
        let p = cp.expect("automation canvas is clickable (adds a node)");
        let before_t = {
            let ed = app.auto_edit.as_ref().unwrap();
            ed.nodes[ed.sel].t
        };
        // Press on that spot (a node now sits there) and DRAG right.
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        for dx in [12, 30, 60] {
            frame(
                &mut app,
                &ctx,
                vec![moved(egui::pos2(p.x + dx as f32, p.y))],
            );
        }
        let end = egui::pos2(p.x + 60.0, p.y);
        frame(
            &mut app,
            &ctx,
            vec![btn(end, egui::PointerButton::Primary, false)],
        );
        let ed = app.auto_edit.as_ref().unwrap();
        assert_eq!(
            ed.nodes.len(),
            n0 + 1,
            "the drag moved the node, added none"
        );
        let moved_t = ed.nodes[ed.sel].t;
        assert!(
            moved_t > before_t + 1000,
            "press-drag slid the node right: {before_t} -> {moved_t}"
        );
    }

    /// v0.15: picked bindings are MUTED in the main window while the
    /// manager is open, and with REC armed their raw values draw the curve
    /// as PERCENT of the target's limits (0 = -inf, 127 = +6 dB).
    #[test]
    fn v15_auto_mgr_rec_mutes_and_scales_percent() {
        let mut app = headless_app("v15mgrrec");
        let ctx = egui::Context::default();
        let slot = MidiAction::ALL
            .iter()
            .position(|a| *a == MidiAction::VolLoop(0))
            .unwrap();
        let bind = crate::midi::MidiBind {
            ty: 1,
            ch: 0,
            d1: 20,
        };
        app.midi_binds[slot] = Some(bind);
        app.open_auto_editor(None);
        app.auto_mgr_rec = vec![(slot, bind)];
        {
            let ed = app.auto_edit.as_mut().unwrap();
            ed.rec = true;
            ed.rec_start = 0;
            ed.nodes.clear();
        }
        let cc = |v: u8| {
            crate::audio::Evt::Midi(crate::audio::RawMidiMsg {
                bytes: vec![0xB0, 20, v],
            })
        };
        app.handle_evt(cc(0));
        app.handle_evt(cc(127));
        frame(&mut app, &ctx, vec![]);
        let vs: Vec<f32> = app
            .auto_edit
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.v)
            .collect();
        assert_eq!(vs.len(), 2, "two recorded nodes: {vs:?}");
        assert!(
            vs[0].abs() < 1e-6,
            "MIDI 0 = range min (0 linear = -inf), got {}",
            vs[0]
        );
        assert!(
            (vs[1] - 2.0).abs() < 1e-6,
            "MIDI 127 = range max (+6 dB = 2.0), got {}",
            vs[1]
        );
        assert_eq!(
            app.trk_vol[0], VOL_UNITY_MIDI,
            "recorded binding never touched the fader"
        );

        // Manager open but REC off: still MUTED (its action must not fire).
        app.auto_edit.as_mut().unwrap().rec = false;
        app.handle_evt(cc(64));
        assert_eq!(
            app.trk_vol[0], VOL_UNITY_MIDI,
            "binding stays muted while the manager is open"
        );

        // Manager CLOSED: the same message drives the fader again.
        app.auto_edit = None;
        app.handle_evt(cc(64));
        assert_eq!(
            app.trk_vol[0], 64,
            "binding works normally when the manager is closed"
        );
    }

    /// v0.15: the "select value" popup lists the track, its mixer and the
    /// header values under translucent bind buttons; binding rescales the
    /// curve's min/max to the new output's limits.
    #[test]
    fn v15_auto_pick_binds_and_rescales() {
        use crate::automation::{Node, Shape};
        let mut app = headless_app("v15autopick");
        let ctx = egui::Context::default();
        app.open_auto_editor(None);
        {
            let ed = app.auto_edit.as_mut().unwrap();
            ed.target = crate::automation::TARGET_TRACK_VOL.to_string();
            ed.nodes = vec![
                Node {
                    t: 0,
                    v: 0.0,
                    s: Shape::Linear,
                },
                Node {
                    t: 96_000,
                    v: 2.0,
                    s: Shape::Linear,
                },
            ];
        }
        app.auto_pick_open = true;
        frame(&mut app, &ctx, vec![]);
        // The metronome/bpm value lives in the header block of the popup.
        let row = galley_at(&mut app, &ctx, "tempo").expect("tempo row painted in the popup");
        click_pt(&mut app, &ctx, row.center());
        let ed = app.auto_edit.as_ref().unwrap();
        assert_eq!(
            ed.target,
            crate::automation::TARGET_MASTER_TEMPO,
            "bind switched the target"
        );
        assert!(
            (ed.nodes[0].v - 20.0).abs() < 1e-3 && (ed.nodes[1].v - 300.0).abs() < 1e-3,
            "min/max rescaled to the bpm limits: {:?}",
            ed.nodes.iter().map(|n| n.v).collect::<Vec<_>>()
        );
        assert!(!app.auto_pick_open, "popup closes after binding");
    }

    /// v0.15 pin: right-clicking a track's grid no longer wipes the track.
    /// A menu opens instead, and clearing takes an explicit click.
    #[test]
    fn v15_ui_rmb_grid_menu_no_destructive_clear() {
        let mut app = headless_app("v15rmbgrid");
        let ctx = egui::Context::default();
        app.arr.insert(0, 0, Clip::new(0, 48_000));
        let n = |a: &App| -> usize {
            a.arr
                .tracks
                .iter()
                .map(|t| t.lanes.iter().map(|l| l.clips.len()).sum::<usize>())
                .sum()
        };
        // Sweep the grid right of the clip until a track menu opens.
        let mut menu = None;
        for y in (120..=340).step_by(4) {
            let p = egui::pos2(700.0, y as f32);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Secondary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Secondary, false)],
            );
            assert_eq!(n(&app), 1, "right-click never wipes the track instantly");
            if let Some(g) = galley_at(&mut app, &ctx, "Clear this track") {
                menu = Some(g.center());
                break;
            }
        }
        let clear = menu.expect("right-click opens a grid menu with an explicit Clear");
        frame(
            &mut app,
            &ctx,
            vec![moved(clear), btn(clear, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(clear, egui::PointerButton::Primary, false)],
        );
        assert_eq!(n(&app), 0, "explicit Clear empties the row");
    }

    /// v0.15 pin: clip right-click carries Copy/Cut; the grid right-click
    /// pastes at the playhead (Ctrl+X now cuts as well as Ctrl+C copies).
    #[test]
    fn v15_ui_clip_menu_copy_cut_paste() {
        let mut app = headless_app("v15copy");
        let ctx = egui::Context::default();
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        frame(&mut app, &ctx, vec![]);
        // Find the clip by its painted body color and click its centre.
        let body = {
            let c = COL_CLIP.linear_multiply(0.45);
            let (r, g, b, _) = c.to_tuple();
            (r, g, b)
        };
        let rects = painted_rects(&mut app, &ctx, body);
        let cp = rects[0].center();
        click_pt(&mut app, &ctx, cp);
        assert!(!app.sel_clips.is_empty(), "click selected the clip");
        frame(
            &mut app,
            &ctx,
            vec![moved(cp), btn(cp, egui::PointerButton::Secondary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(cp, egui::PointerButton::Secondary, false)],
        );
        assert!(
            galley_at(&mut app, &ctx, "Copy").is_some(),
            "clip menu offers Copy"
        );
        assert!(
            galley_at(&mut app, &ctx, "Cut").is_some(),
            "clip menu offers Cut"
        );
        let copy = galley_at(&mut app, &ctx, "Copy").unwrap().center();
        frame(
            &mut app,
            &ctx,
            vec![moved(copy), btn(copy, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(copy, egui::PointerButton::Primary, false)],
        );
        assert!(app.clipboard.is_some(), "Copy fills the clipboard");
        // Ctrl+X cuts the same selection.
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::X, egui::Modifiers::COMMAND),
            ],
        );
        assert!(app.status.contains("cut"), "Ctrl+X cuts: {}", app.status);
        // Grid right-click offers Paste back in.
        let mut paste = None;
        for y in (120..=340).step_by(4) {
            let p = egui::pos2(700.0, y as f32);
            frame(
                &mut app,
                &ctx,
                vec![moved(p), btn(p, egui::PointerButton::Secondary, true)],
            );
            frame(
                &mut app,
                &ctx,
                vec![btn(p, egui::PointerButton::Secondary, false)],
            );
            if let Some(g) = galley_at(&mut app, &ctx, "Paste here") {
                paste = Some(g.center());
                break;
            }
        }
        let paste = paste.expect("grid menu offers Paste here");
        frame(
            &mut app,
            &ctx,
            vec![moved(paste), btn(paste, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(paste, egui::PointerButton::Primary, false)],
        );
        assert!(
            app.arr
                .tracks
                .iter()
                .any(|t| t.lanes.iter().any(|l| !l.clips.is_empty())),
            "Paste put the clip back"
        );
    }

    /// v0.15 pin: the value readouts beside sliders are display-only —
    /// clicking one focuses no text edit (the old side box is gone).
    #[test]
    fn v15_ui_value_readouts_are_not_editable() {
        let mut app = headless_app("v15noreadout");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let p = label_center(
            &mut app,
            &ctx,
            &format!("{:>5}", db_num(vol_amp(VOL_UNITY_MIDI))),
        )
        .expect("a dB readout is painted");
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        frame(&mut app, &ctx, vec![]);
        assert!(
            ctx.memory(|m| m.focused().is_none()),
            "clicking a readout must not open a text edit"
        );
    }

    /// Grid tier brightness: measure > beat > subdivision — the user's
    /// legibility order. (The old alpha-blended snap color painted as an
    /// opaque gray-120 on the live build and out-shone the beat lines.)

    #[test]
    fn grid_line_brightness_is_bar_beat_snap() {
        let lum = |c: Color32| {
            let a = c.to_array();
            (a[0] as u32 + a[1] as u32 + a[2] as u32) / 3
        };
        assert!(lum(COL_BAR_LINE) > lum(COL_BEAT_LINE), "measure over beat");
        assert!(
            lum(COL_BEAT_LINE) > lum(COL_SNAP_LINE),
            "beat over subdivision"
        );
    }

    /// Settings → "live input monitoring": default on; the checkbox flips
    /// the app state, pushes Cmd::SetMonitor to the engine and saves the
    /// config.
    #[test]
    fn settings_monitoring_toggle() {
        let mut app = headless_app("monitor");
        let ctx = egui::Context::default();
        assert!(
            app.monitor_enabled,
            "monitoring defaults on (current behavior)"
        );
        app.show_settings = true;
        let p = label_center(&mut app, &ctx, "live input monitoring")
            .expect("monitoring checkbox is painted in settings");
        frame(
            &mut app,
            &ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        assert!(!app.monitor_enabled, "click turns live monitoring off");
        // (The engine-side gate of the same flag is pinned by
        // `audio::tests::monitoring_toggle_gates_live_input`, and the RT
        // pass-through muting by `e2e_selected_loop_monitoring` — headless
        // lib tests run with no cmd consumer, so we assert app state +
        // persisted config only here.)
        let cfg_txt = std::fs::read_to_string(Config::path()).unwrap();
        assert!(
            cfg_txt.contains("\"monitor_on\": false"),
            "config: {cfg_txt}"
        );
    }

    /// Sub-beat snap lines: a third, fainter grid tier drawn at the snap
    /// subdivision; gone when snap is the plain beat or the steps would
    /// crowd (< 4 px apart).
    #[test]
    fn snap_grid_lines_follow_snap_setting() {
        let mut app = headless_app("snapgrid");
        let ctx = egui::Context::default();
        let snap_lines = |app: &mut App, ctx: &egui::Context| -> usize {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let n = out
                .shapes
                .iter()
                .filter(|cs| {
                    matches!(&cs.shape, egui::epaint::Shape::LineSegment { stroke, .. }
                        if stroke.color == COL_SNAP_LINE)
                })
                .count();
            let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            drop(prims);
            out.textures_delta.clear();
            n
        };
        // Default snap = sixteenth notes at the default zoom: the sub tier
        // paints (ruler + lanes).
        assert!(
            snap_lines(&mut app, &ctx) > 0,
            "snap tier painted at snap subdivision"
        );
        // Quarter-note snap: the beat line IS the snap grid → no sub tier.
        app.snap_sub = 0;
        assert_eq!(
            snap_lines(&mut app, &ctx),
            0,
            "no sub tier for quarter-note snap"
        );
        // Zoomed to the floor the steps fall under 4 px → tier hidden.
        app.snap_sub = SUBDIVISIONS.iter().position(|s| s.0 == 8).unwrap();
        app.px_per_sec = ZOOM_MIN;
        assert_eq!(
            snap_lines(&mut app, &ctx),
            0,
            "sub tier hides when steps crowd"
        );
    }

    #[test]
    fn multi_track_group_copy_paste_keeps_offsets_and_trims() {
        let mut app = headless_app("grpclip");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]); // one layout pass so state settles
        app.arr.insert(0, 0, Clip::new(0, 0));
        app.arr.insert(
            2,
            0,
            Clip {
                asset: 0,
                pos: 9_600,
                cut: 480,
                play_len: Some(2_400),
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            },
        );
        app.sel_clips = vec![(0, 0, 0), (2, 0, 0)];
        app.clipboard_copy();
        let cb = app.clipboard.clone().expect("group copied");
        assert_eq!(cb.items.len(), 2);
        assert_eq!((cb.items[0].t_off, cb.items[1].t_off), (0, 2));
        assert_eq!((cb.items[0].pos_off, cb.items[1].pos_off), (0, 9_600));

        app.clipboard_paste();
        let total: usize = app
            .arr
            .tracks
            .iter()
            .map(|t| t.lanes.iter().map(|l| l.clips.len()).sum::<usize>())
            .sum();
        assert_eq!(total, 4, "the group pasted as two new clips");
        // Null-audio playhead sits at 0, so the paste mirrors positions.
        // The copies overlap the originals, so place_auto opens child lanes.
        let p2 = app.arr.tracks[2]
            .lanes
            .iter()
            .flat_map(|l| l.clips.iter())
            .find(|c| c.cut == 480)
            .expect("pasted trim clip on a free lane");
        assert_eq!((p2.pos, p2.play_len), (9_600, Some(2_400)), "trim survived");
        assert_eq!(app.sel_clips.len(), 2, "the pasted group is selected");
        assert!(app.hscroll_visible(9_600), "pasted clips are in view");
    }

    #[test]
    fn select_all_then_delete_clears_group() {
        let mut app = headless_app("selall");
        app.arr.insert(0, 0, Clip::new(0, 0));
        app.arr.insert(0, 0, Clip::new(0, 9_600));
        app.arr.insert(1, 0, Clip::new(0, 4_800));
        app.select_all_clips();
        assert_eq!(app.sel_clips.len(), 3);
        app.delete_selection();
        assert!(app.sel_clips.is_empty());
        let total: usize = app
            .arr
            .tracks
            .iter()
            .map(|t| t.lanes.iter().map(|l| l.clips.len()).sum::<usize>())
            .sum();
        assert_eq!(total, 0, "the whole group vanished");
        // Deleting placements never deletes rack assets.
        let before = app.assets.len();
        assert!(before <= 1);
    }

    #[test]
    fn export_selection_requires_a_selection_and_uses_the_clipboard_path() {
        let mut app = headless_app("expsel");
        app.export_sel = true;
        app.export_path = "/tmp/slooper-exp-sel.wav".into();
        app.sel_clips.clear();
        app.do_export();
        assert!(
            app.status.contains("select at least one clip"),
            "guard fired: {}",
            app.status
        );
        app.arr.insert(0, 0, Clip::new(0, 0));
        app.sel_clips = vec![(0, 0, 0)];
        app.do_export(); // engine writes async; just must not guard-error
        assert!(!app.show_export, "dialog closes when the export is queued");
    }

    #[test]
    fn track_count_config_survives_project_roundtrip() {
        let mut app = headless_app("ntracks");
        app.n_tracks = 6;
        assert_eq!(app.build_project("rt".into()).n_tracks, Some(6));
        app.do_save("rt");
        app.n_tracks = 12;
        let path = crate::project::project_path(&app.projects_dir(), "rt");
        app.do_load(&path);
        assert_eq!(app.n_tracks, 6, "saved project restores its track count");

        // Selection cycling stops at the configured count, not MAX_LOOPS.
        app.n_tracks = 8;
        app.selected = 7;
        app.fire_midi_action(crate::midi::MidiAction::SelectNext);
        assert_eq!(app.selected, 7, "never past the configured last track");
        app.fire_midi_action(crate::midi::MidiAction::SelectPrev);
        assert_eq!(app.selected, 6);
    }

    #[test]
    fn midi_rec_take_bounces_asset_and_places_clip() {
        let mut app = headless_app("midirec");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        app.sample_rate = 48_000;
        let sel = app.selected;
        assert!(!app.midi_msg_is_bound(&MidiMsg::NoteOn {
            channel: 1,
            note: 60,
            velocity: 100
        }));

        app.toggle_midi_rec(sel);
        assert!(app.midi_rec[sel].armed);
        // An unmapped note is captured, the matching off closes it.
        app.capture_midi_msg(&MidiMsg::NoteOn {
            channel: 1,
            note: 60,
            velocity: 100,
        });
        app.capture_midi_msg(&MidiMsg::NoteOff {
            channel: 1,
            note: 60,
            velocity: 0,
        });
        // A CC lands in the take too (nothing is bound to it).
        app.capture_midi_msg(&MidiMsg::ControlChange {
            channel: 1,
            number: 7,
            value: 90,
        });
        let before = app.assets.len();
        app.toggle_midi_rec(sel); // second press = commit
        assert!(!app.midi_rec[sel].armed);
        let a = app.assets.last().expect("midi asset added");
        assert_eq!(app.assets.len(), before + 1);
        assert!(a.is_midi());
        assert!(a.path.extension().is_some_and(|e| e == "mid"));
        assert!(a.path.exists(), "take written to the primary folder");
        let pat = a.pattern.clone().expect("pattern attached");
        assert_eq!(pat.notes.len(), 1);
        assert_eq!(pat.notes[0].note, 60);
        assert_eq!(pat.ccs.len(), 1);
        // Clip placed on the recorded track pointing at the new asset.
        let id = a.id;
        let hit = app.arr.tracks[sel]
            .lanes
            .iter()
            .flat_map(|l| l.clips.iter())
            .any(|c| c.asset == id);
        assert!(hit, "clip placed at the playhead on the armed track");
    }

    #[test]
    fn bound_midi_messages_are_not_recorded() {
        let mut app = headless_app("midirecbound");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        // Bind NOTE ON note 61 (any velocity) to an action, like a pad.
        let idx = MidiAction::ALL
            .iter()
            .position(|a| matches!(a, MidiAction::PlayLoop(0)))
            .unwrap();
        app.midi_binds[idx] = Some(crate::midi::MidiBind {
            ty: 0,
            ch: 1,
            d1: 61,
        });
        let bound = app.midi_msg_is_bound(&MidiMsg::NoteOn {
            channel: 1,
            note: 61,
            velocity: 100,
        });
        assert!(bound, "mapped note counts as bound");
        let free = app.midi_msg_is_bound(&MidiMsg::NoteOn {
            channel: 1,
            note: 62,
            velocity: 100,
        });
        assert!(!free);
    }

    /// Full piano-roll I/O path: the toolbar button opens the window, a
    /// drag on the canvas draws a note, and PREVIEW renders audio through
    /// the chosen synth (regression pin for the "does nothing" report).
    #[test]
    fn piano_roll_opens_draws_and_previews() {
        let sdir = std::env::temp_dir().join("sp-rollsynths");
        let _ = std::fs::remove_dir_all(&sdir);
        std::env::set_var("SLOOPER_SYNTHS_DIR", &sdir);
        std::env::remove_var("SLOOPER_SYNTH"); // not relevant, hygiene
        let mut app = headless_app("rollio");
        let ctx = egui::Context::default();

        // Scan painted galleys for a text; return its screen rect.
        fn find_galley(app: &mut App, ctx: &egui::Context, needle: &str) -> Option<egui::Rect> {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let hit = out.shapes.iter().find_map(|cs| {
                if let egui::epaint::Shape::Text(t) = &cs.shape {
                    if t.galley.text().contains(needle) {
                        return Some(t.pos);
                    }
                }
                None
            });
            let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            drop(prims);
            out.textures_delta.clear();
            hit.map(|p| egui::Rect::from_min_size(p, egui::vec2(4.0, 12.0)))
        }

        frame(&mut app, &ctx, vec![]);
        // 1) Open via the real toolbar button.
        let btn = find_galley(&mut app, &ctx, "Piano roll").expect("toolbar button painted");
        let p = btn.center();
        frame(&mut app, &ctx, vec![moved(p), btn_btn(p, true)]);
        frame(&mut app, &ctx, vec![btn_btn(p, false)]);
        assert!(app.piano_roll.is_some(), "button opens the roll");
        // Let the window's open animation finish (each frame ticks ~25 ms)
        // so shapes reach full alpha before being matched.
        for _ in 0..24 {
            frame(&mut app, &ctx, vec![]);
        }

        // 2) Locate the canvas: it paints itself with the dark 22,22,26 fill.
        let canvas = {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let rect = out.shapes.iter().find_map(|cs| {
                if let egui::epaint::Shape::Rect(r) = &cs.shape {
                    let (rr, gg, bb, aa) = r.fill.to_tuple();
                    if aa == 255
                        && (rr as i16 - 22).abs() <= 2
                        && (gg as i16 - 22).abs() <= 2
                        && (bb as i16 - 26).abs() <= 2
                        && r.rect.width() > 200.0
                        && r.rect.height() > 100.0
                    {
                        return Some(r.rect);
                    }
                }
                None
            });
            let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            drop(prims);
            out.textures_delta.clear();
            rect.expect("roll canvas painted")
        };

        // 3) Drag on the grid (right of the 58px keys strip) draws a note.
        let x0 = canvas.min.x + 58.0 + 60.0;
        let y = canvas.center().y;
        let start = egui::pos2(x0, y);
        frame(&mut app, &ctx, vec![moved(start), btn_btn(start, true)]);
        frame(&mut app, &ctx, vec![moved(egui::pos2(x0 + 30.0, y))]);
        frame(&mut app, &ctx, vec![moved(egui::pos2(x0 + 70.0, y))]);
        frame(
            &mut app,
            &ctx,
            vec![btn_btn(egui::pos2(x0 + 70.0, y), false)],
        );
        let n_notes = app.piano_roll.as_ref().unwrap().pat.notes.len();
        assert_eq!(n_notes, 1, "drag draws exactly one note");
        // Fresh draw stretches the note to the pointer (FL-style).
        assert!(
            app.piano_roll.as_ref().unwrap().pat.notes[0].len > 0,
            "drawn note has length"
        );

        // 4) Preview renders audio: PreviewPlay fires and the status is not
        // the "no synth available" failure. (Match the BUTTON, not the
        // "preview:" combo label.)
        //
        // Quality gate first: the rendered buffer itself must contain real
        // signal — "preview doesn't work with any synth" shipped once as a
        // silent success, so peak level is asserted, not just status text.
        {
            let pat = app.piano_roll.as_ref().unwrap().pat.clone();
            let buf = app.render_pattern_preview(&pat).expect("preview render");
            let peak = buf.iter().fold(0f32, |m, s| m.max(s.abs()));
            assert!(
                peak > 1e-3,
                "preview through the default synth must make sound, peak {peak}"
            );
        }
        app.drain_events();
        let pv = find_galley(&mut app, &ctx, "\u{25b6} preview")
            .expect("preview button")
            .center();
        frame(&mut app, &ctx, vec![moved(pv), btn_btn(pv, true)]);
        frame(&mut app, &ctx, vec![btn_btn(pv, false)]);
        assert!(
            !app.status.contains("no synth"),
            "preview synth available: {}",
            app.status
        );
        assert_eq!(app.preview_playing.as_deref(), Some("piano roll"));

        // 5) The preview TRACK picker (a combobox): open it, choose track 1
        // so previews route through that track's synth + chain.
        let combo = {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let hit = out.shapes.iter().find_map(|cs| {
                if let egui::epaint::Shape::Text(t) = &cs.shape {
                    if t.galley.text() == "preview:" {
                        return Some(t.pos);
                    }
                }
                None
            });
            let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            drop(prims);
            out.textures_delta.clear();
            hit.expect("preview track picker painted")
        };
        // The combobox button sits just right of the "preview:" label.
        let cb = egui::pos2(combo.x + 120.0, combo.y + 6.0);
        frame(&mut app, &ctx, vec![moved(cb), btn_btn(cb, true)]);
        frame(&mut app, &ctx, vec![btn_btn(cb, false)]);
        frame(&mut app, &ctx, vec![]); // popup opens
        let tr = find_galley(&mut app, &ctx, "track 1")
            .expect("track 1 popup row")
            .center();
        frame(&mut app, &ctx, vec![moved(tr), btn_btn(tr, true)]);
        frame(&mut app, &ctx, vec![btn_btn(tr, false)]);
        assert_eq!(app.preview_track, Some(0), "preview routes to track 1");
        // Track 1 has no explicit synth binding → the fallback keeps the
        // preview alive (synth + empty chain still renders).
        let pv2 = find_galley(&mut app, &ctx, "\u{25b6} preview")
            .expect("preview button")
            .center();
        frame(&mut app, &ctx, vec![moved(pv2), btn_btn(pv2, true)]);
        frame(&mut app, &ctx, vec![btn_btn(pv2, false)]);
        assert!(!app.status.contains("no synth"), "{}", app.status);

        // 6) Clicking a piano key auditions that single note.
        let kc = egui::pos2(canvas.min.x + 25.0, canvas.center().y);
        frame(&mut app, &ctx, vec![moved(kc), btn_btn(kc, true)]);
        frame(&mut app, &ctx, vec![btn_btn(kc, false)]);
        assert!(
            app.preview_playing
                .as_deref()
                .is_some_and(|p| p.starts_with("key ")),
            "key click auditions: {:?}",
            app.preview_playing
        );

        std::env::remove_var("SLOOPER_SYNTHS_DIR");
        let _ = std::fs::remove_dir_all(&sdir);
    }

    fn btn_btn(p: egui::Pos2, down: bool) -> egui::Event {
        btn(p, egui::PointerButton::Primary, down)
    }

    #[test]
    fn piano_roll_save_writes_asset_and_clip() {
        let mut app = headless_app("rollsave");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        app.sample_rate = 48_000;
        let mut pat = Pattern::new(120);
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
        });
        pat.quantize_grid(480, false);
        let before = app.assets.len();
        app.save_piano_roll(&mut pat, 2);
        assert_eq!(app.assets.len(), before + 1);
        let a = app.assets.last().unwrap();
        assert!(a.is_midi());
        assert!(a.path.exists());
        let id = a.id;
        let hit = app.arr.tracks[2]
            .lanes
            .iter()
            .flat_map(|l| l.clips.iter())
            .any(|c| c.asset == id);
        assert!(hit);
        // Saving again bumps the name (roll-2.mid), never overwrites.
        app.save_piano_roll(&mut pat, 2);
        assert!(app.assets.last().unwrap().name.contains("roll-2"));
    }

    /// v0.15 bug-hunt pin: user value entry must reject the non-finite
    /// literals f32::parse accepts ("nan"/"inf"), while "-inf" keeps its
    /// dB meaning and tempo/mute clamp into their legal ranges.
    #[test]
    fn v15_parse_rejects_nonfinite_values() {
        use crate::automation::{
            TARGET_MASTER_TEMPO, TARGET_TRACK_MUTE, TARGET_TRACK_PAN, TARGET_TRACK_VOL,
        };
        for tgt in [
            TARGET_TRACK_PAN,
            TARGET_MASTER_TEMPO,
            TARGET_TRACK_MUTE,
            "fx.3.p0",
        ] {
            for bad in ["nan", "NaN", "inf", "-inf", "+inf"] {
                assert!(
                    value_user_parse(tgt, bad).is_none(),
                    "{tgt} accepted non-finite {bad}"
                );
            }
        }
        // "-inf" is MEANINGFUL for dB targets: silence, not a bad parse.
        assert_eq!(value_user_parse(TARGET_TRACK_VOL, "-inf"), Some(0.0));
        assert!(value_user_parse(TARGET_TRACK_VOL, "nan").is_none());
        // 1:1 entry semantics + range clamps survive.
        assert!((value_user_parse(TARGET_TRACK_VOL, "-6").unwrap() - 0.501_187_2).abs() < 1e-3);
        assert_eq!(value_user_parse(TARGET_MASTER_TEMPO, "500"), Some(300.0));
        assert_eq!(value_user_parse(TARGET_TRACK_MUTE, "7"), Some(1.0));
    }

    /// v0.15 bug-hunt pin: MIDI recording in the automation manager
    /// draws the curve INSIDE its declared span only — moves after the
    /// span end must not pile up invisible nodes.
    #[test]
    fn v15_mgr_record_never_grows_past_span() {
        use crate::audio::{Evt, RawMidiMsg};
        use crate::automation::{Node, Shape};
        use crate::midi::{MidiAction, MidiBind};
        let mut app = headless_app("reclim");
        app.sample_rate = 48_000;
        let slot = (0..MidiAction::ALL.len())
            .find(|i| MidiAction::ALL[*i].is_continuous())
            .expect("a continuous binding slot");
        app.auto_mgr_rec = vec![(
            slot,
            MidiBind {
                ty: 1,
                ch: 0,
                d1: 42,
            },
        )];
        app.open_auto_editor(None);
        {
            let ed = app.auto_edit.as_mut().unwrap();
            ed.len_samples = 48_000; // one second
            ed.rec = true;
            ed.rec_start = 0;
            ed.nodes = vec![Node {
                t: 0,
                v: 0.5,
                s: Shape::Linear,
            }];
        }
        let cc = |v| {
            Evt::Midi(RawMidiMsg {
                bytes: vec![0xB0, 42, v],
            })
        };
        app.backend.engine.seek_to(24_000);
        app.handle_evt(cc(80));
        let n1 = app.auto_edit.as_ref().unwrap().nodes.len();
        assert_eq!(n1, 2, "an in-span move must record");
        app.backend.engine.seek_to(120_000);
        app.handle_evt(cc(100));
        app.handle_evt(cc(110));
        assert_eq!(
            app.auto_edit.as_ref().unwrap().nodes.len(),
            n1,
            "no nodes may grow past len_samples"
        );
    }

    /// v0.15 bug-hunt pin: deleting an asset must remap the clip
    /// clipboard — copy -> delete (earlier asset) -> paste must place the
    /// SAME asset, and deleting the copied asset must empty the clipboard.
    #[test]
    fn v15_asset_removal_remaps_clipboard() {
        use crate::automation::{Node, Shape, TARGET_TRACK_VOL};
        let mut app = headless_app("cbremap");
        app.sample_rate = 48_000;
        for _ in 0..2 {
            app.open_auto_editor(None);
            let mut ed = app.auto_edit.take().unwrap();
            ed.target = TARGET_TRACK_VOL.to_string();
            ed.nodes = vec![
                Node {
                    t: 0,
                    v: 0.3,
                    s: Shape::Linear,
                },
                Node {
                    t: 48_000,
                    v: 0.6,
                    s: Shape::Linear,
                },
            ];
            app.auto_commit(&mut ed, 0, false);
        }
        let n = app.assets.len();
        let (k0, k1) = (n - 2, n - 1);
        // place a clip of the LATER asset, select it, copy.
        let lane = app.arr.place_auto(
            0,
            crate::arrange::Clip {
                asset: k1 as u32,
                pos: 0,
                cut: 0,
                play_len: None,
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            },
            app.assets[k1].samples(),
        );
        app.sel_clips = vec![(0, lane, app.arr.tracks[0].lanes[lane].clips.len() - 1)];
        app.clipboard_copy();
        assert_eq!(app.clipboard.as_ref().unwrap().items[0].asset as usize, k1);

        // Delete the EARLIER asset: clipboard id slides with the table.
        app.remove_asset_at(k0);
        assert_eq!(
            app.clipboard.as_ref().unwrap().items[0].asset as usize,
            k1 - 1,
            "clipboard asset id must follow the table"
        );
        app.clipboard_paste();
        let placed = app.arr.tracks[0]
            .lanes
            .iter()
            .flat_map(|l| l.clips.iter())
            .any(|c| c.asset as usize == k1 - 1 && c.pos == app.backend.engine.pos());
        assert!(placed, "paste must land on the (remapped) same asset");

        // Deleting the clipboard's own asset leaves nothing to paste.
        app.remove_asset_at(k1 - 1);
        assert!(
            app.clipboard.is_none(),
            "clipboard must empty with its asset"
        );
    }

    /// v0.15 bug-hunt pin: deleting assets while the automation manager
    /// is open must remap its asset id (earlier asset deleted) or close
    /// it (its OWN asset deleted) — never leave it writing into a
    /// neighbour's asset.
    #[test]
    fn v15_asset_removal_remaps_auto_editor() {
        use crate::automation::{Node, Shape, TARGET_TRACK_VOL};
        let mut app = headless_app("autorem");
        app.sample_rate = 48_000;
        // Two saved automation assets: k0 then k1.
        for _ in 0..2 {
            app.open_auto_editor(None);
            let mut ed = app.auto_edit.take().unwrap();
            ed.target = TARGET_TRACK_VOL.to_string();
            ed.nodes = vec![
                Node {
                    t: 0,
                    v: 0.3,
                    s: Shape::Linear,
                },
                Node {
                    t: 48_000,
                    v: 0.6,
                    s: Shape::Linear,
                },
            ];
            app.auto_commit(&mut ed, 0, false);
        }
        let k1 = app.assets.len() - 1;
        let k0 = k1 - 1;

        // Editor open on the LATER asset; delete an EARLIER one -> remap.
        app.open_auto_editor(Some(k1));
        app.remove_asset_at(k0);
        assert_eq!(
            app.auto_edit.as_ref().and_then(|e| e.asset),
            Some(k1 - 1),
            "id must slide with the table"
        );
        assert_eq!(
            app.auto_edit.as_ref().and_then(|e| e.asset),
            Some(app.assets.len() - 1)
        );

        // Editor open on the deleted asset itself -> manager closes.
        let k = app.assets.len() - 1;
        app.open_auto_editor(Some(k));
        app.remove_asset_at(k);
        assert!(app.auto_edit.is_none(), "manager must close with its asset");
    }

    /// v0.11 R5 pin: the automation manager buffer -> asset -> clip ->
    /// file round-trip, in-place overwrite, and the write-recorder path
    /// (arm -> move track volume -> stop -> asset + red clip).
    #[test]
    fn v11_automation_manager_roundtrip_and_write_rec() {
        use crate::automation::{Node, Shape, TARGET_TRACK_PAN, TARGET_TRACK_VOL};
        let mut app = headless_app("autoedit");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        app.sample_rate = 48_000;

        // --- manager: new curve -> save as new asset -------------------
        app.open_auto_editor(None);
        let mut ed = app.auto_edit.take().expect("editor opened");
        ed.target = TARGET_TRACK_PAN.to_string();
        ed.nodes = vec![
            Node {
                t: 0,
                v: 0.0,
                s: Shape::Linear,
            },
            Node {
                t: 48_000,
                v: 1.0,
                s: Shape::Smooth,
            },
        ];
        let before = app.assets.len();
        app.auto_commit(&mut ed, 1, false);
        app.auto_edit = Some(ed);
        assert_eq!(app.assets.len(), before + 1);
        let k = app.assets.len() - 1;
        let a = &app.assets[k];
        assert!(a.is_auto());
        assert!(a.path.to_string_lossy().ends_with(".slauto"));
        let placed = app.arr.tracks[1]
            .lanes
            .iter()
            .any(|l| l.clips.iter().any(|c| c.asset as usize == k));
        assert!(placed, "manager save must drop a clip on the track");
        let txt = std::fs::read_to_string(&a.path).unwrap();
        let back = crate::automation::Automation::from_json(&txt).unwrap();
        assert_eq!(back.target, TARGET_TRACK_PAN);
        assert!((back.eval(24_000) - 0.5).abs() < 0.02);

        // --- overwrite in place -----------------------------------------
        app.open_auto_editor(Some(k));
        let mut ed = app.auto_edit.take().unwrap();
        assert_eq!(ed.asset, Some(k));
        ed.nodes[0].v = 0.25;
        app.auto_commit(&mut ed, 1, true);
        let txt = std::fs::read_to_string(&app.assets[k].path).unwrap();
        let back = crate::automation::Automation::from_json(&txt).unwrap();
        assert!((back.nodes[0].v - 0.25).abs() < 1e-6);
        assert_eq!(app.assets.len(), before + 1, "overwrite adds nothing");

        // --- write recording ---------------------------------------------
        app.auto_rec[0] = crate::ui::AutoRecState {
            armed: true,
            target: None,
            start: 0,
            nodes: vec![],
        };
        app.set_trk_vol(0, 100);
        app.set_trk_vol(0, 60);
        let n_before = app.assets.len();
        app.toggle_auto_rec(0); // stop + bounce
        assert_eq!(app.assets.len(), n_before + 1);
        let a = app.assets.last().unwrap();
        assert!(a.is_auto());
        assert_eq!(a.automation.as_ref().unwrap().target, TARGET_TRACK_VOL);
        assert!(app.arr.tracks[0].lanes.iter().any(|l| l
            .clips
            .iter()
            .any(|c| c.asset as usize == app.assets.len() - 1)));
    }

    /// Item 3+4 regression pins: opening the roll with a MIDI asset
    /// SELECTED (clip or rack) edits that asset in place, the wheel scrolls
    /// the note grid in the conventional direction (wheel up = view up =
    /// higher notes, matching the lo/hi culling the old sign contradicted),
    /// and save on an edited asset overwrites the file instead of minting
    /// a new asset.
    #[test]
    fn v08_roll_edits_selected_asset_and_wheel_direction() {
        fn galley_of(
            app: &mut App,
            ctx: &egui::Context,
            needle: &str,
            exact: bool,
        ) -> Option<egui::Rect> {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let hit = out.shapes.iter().find_map(|cs| {
                if let egui::epaint::Shape::Text(t) = &cs.shape {
                    let txt = t.galley.text();
                    let m = if exact {
                        txt == needle
                    } else {
                        txt.contains(needle)
                    };
                    if m {
                        return Some(t.pos);
                    }
                }
                None
            });
            let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            drop(prims);
            out.textures_delta.clear();
            hit.map(|p| egui::Rect::from_min_size(p, egui::vec2(4.0, 12.0)))
        }

        let mut app = headless_app("rolldit");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        app.sample_rate = 48_000;

        // One MIDI asset + a clip pointing at it.
        let mut pat = Pattern::new(120);
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
        });
        let before = app.assets.len();
        app.save_piano_roll(&mut pat, 1);
        let id = app.assets.len() as u32 - 1;
        let sel = app
            .arr
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(t, tr)| {
                tr.lanes.iter().enumerate().flat_map(move |(k, l)| {
                    l.clips
                        .iter()
                        .enumerate()
                        .filter(move |c| c.1.asset == id)
                        .map(move |(c, _)| (t, k, c))
                })
            })
            .next()
            .expect("placed clip");
        app.sel_clips = vec![sel];
        let n_assets = app.assets.len();
        assert_eq!(n_assets, before + 1);

        // Open through the toolbar button: it must load the selection.
        let btn = galley_of(&mut app, &ctx, "Piano roll", false)
            .expect("toolbar button")
            .center();
        frame(&mut app, &ctx, vec![moved(btn), btn_btn(btn, true)]);
        frame(&mut app, &ctx, vec![btn_btn(btn, false)]);
        {
            let roll = app.piano_roll.as_ref().expect("roll opened");
            assert_eq!(
                roll.edit_asset,
                Some(id),
                "opens the SELECTED asset for editing"
            );
            assert_eq!(roll.pat.notes.len(), 1, "asset notes loaded into the roll");
        }
        for _ in 0..8 {
            frame(&mut app, &ctx, vec![]);
        }

        // Wheel direction over the canvas: positive delta (wheel up /
        // scroll-up) raises vscroll_px — the direction the lo/hi note
        // culling and the keyboard strip already move.
        let canvas = app
            .piano_roll
            .as_ref()
            .unwrap()
            .canvas_rect
            .expect("canvas rect");
        let c = canvas.center();
        let wheel = |dy: f32| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, dy),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut app, &ctx, vec![moved(c)]);
        let s0 = app.piano_roll.as_ref().unwrap().vscroll_px;
        frame(&mut app, &ctx, vec![wheel(1.0)]);
        let s1 = app.piano_roll.as_ref().unwrap().vscroll_px;
        assert!(s1 > s0, "wheel up scrolls the keyboard up ({s0} -> {s1})");
        frame(&mut app, &ctx, vec![wheel(-1.0), wheel(-1.0)]);
        let s2 = app.piano_roll.as_ref().unwrap().vscroll_px;
        assert!(s2 < s1, "wheel down scrolls back down ({s1} -> {s2})");

        // Edit and SAVE: in place, no new asset, file really updated.
        app.piano_roll
            .as_mut()
            .unwrap()
            .pat
            .notes
            .push(crate::midipat::Note {
                ch: 0,
                note: 64,
                vel: 90,
                press: 0,
                start: 480,
                len: 240,
            });
        let sv = galley_of(&mut app, &ctx, "save", true).expect("roll save button");
        let sv = sv.center();
        frame(&mut app, &ctx, vec![moved(sv), btn_btn(sv, true)]);
        frame(&mut app, &ctx, vec![btn_btn(sv, false)]);
        assert!(
            app.status.contains("in place"),
            "in-place save status: {}",
            app.status
        );
        assert_eq!(app.assets.len(), n_assets, "no new asset minted");
        assert_eq!(
            app.assets[id as usize]
                .pattern
                .as_ref()
                .unwrap()
                .notes
                .len(),
            2
        );
        let back = Pattern::load_mid(&app.assets[id as usize].path).unwrap();
        assert_eq!(back.notes.len(), 2, "the .mid on disk was rewritten");

        // Bottom-toolbar zoom sliders carry honest labels now (the old
        // "px/beat" / "row px" engineering units read as mislabeled).
        let labels_raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(labels_raw, |ui| app.ui_root(ui));
        // Cold context: discard the first-frame atlas delta (epaint asserts
        // on dropping an unapplied delta).
        out.textures_delta.clear();
        let has = |needle: &str| {
            out.shapes.iter().any(|cs| {
                matches!(&cs.shape, egui::epaint::Shape::Text(t)
                    if t.galley.text().contains(needle))
            })
        };
        assert!(has("zoom time"), "time-zoom slider must read 'zoom time'");
        assert!(has("zoom rows"), "row-zoom slider must read 'zoom rows'");
    }

    /// The preview path must put real signal through the chosen synth —
    /// "preview doesn't work with any synth" was silent-success rendering.
    #[test]
    fn v08_preview_default_synth_makes_sound() {
        let mut app = headless_app("prevsound");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        app.sample_rate = 48_000;
        let mut pat = Pattern::new(120);
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note: 57,
            vel: 110,
            press: 0,
            start: 0,
            len: 240,
        });
        let buf = app.render_pattern_preview(&pat).expect("preview renders");
        let peak = buf.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 1e-3, "preview peak must be audible, got {peak}");
        // Second render must also work right after a "playhead move" (the
        // old per-click synth instance died silently on the second go).
        let buf2 = app.render_pattern_preview(&pat).expect("second preview");
        let peak2 = buf2.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak2 > 1e-3, "repeat preview peak, got {peak2}");
    }

    /// Mixer chain regression pin: the "FX" chip opens the unified per-track
    /// chain window, the "+ VST plugin" picker lists the folder-scanned
    /// plugin, and clicking its row appends a loaded plugin slot to the
    /// chain and persists it in `track_chain`. Uses a private scan folder;
    /// skips without a local plugin.
    #[test]
    fn chain_window_adds_scanned_plugin_slot() {
        let home = std::env::var("HOME").unwrap_or_default();
        let plugin = std::path::PathBuf::from(format!("{home}/.vst/DragonflyHallReverb-vst.so"));
        if !plugin.is_file() {
            eprintln!("skipping chain plugin pin: no VST2 test plugin installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("sp-vstpin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&plugin, dir.join("dfhall.so")).unwrap();

        let mut app = headless_app("vstpin");
        // Isolate the default scan dirs (default_vst_dirs follows $HOME);
        // the suite runs one-threaded and HOME is restored below.
        std::env::set_var("HOME", &dir);
        app.vst_dirs = vec![dir.to_string_lossy().into_owned()];
        let ctx = egui::Context::default();

        frame(&mut app, &ctx, vec![]);
        // v0.15: on the default short track the FX row folds into the
        // "more" popup \u{2014} open it first, then take the chip.
        let m = label_center(&mut app, &ctx, "\u{2026} more").expect("more button on the strip");
        frame(&mut app, &ctx, vec![moved(m)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(m, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(m, egui::PointerButton::Primary, false)],
        );
        let p = label_center(&mut app, &ctx, "FX").expect("FX chip in the more popup");
        frame(&mut app, &ctx, vec![moved(p)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        assert_eq!(
            app.fx_window,
            Some(0),
            "chip opens the track's chain window"
        );

        // One text scan per frame (the plugin row label carries vendor
        // text, so match by substring, like the old VST window pin did).
        let find_contains =
            |app: &mut App, ctx: &egui::Context, want: &str| -> Option<egui::Pos2> {
                let raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1440.0, 853.0),
                    )),
                    ..Default::default()
                };
                let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
                let mut c = None;
                for cs in &out.shapes {
                    if let egui::epaint::Shape::Text(t) = &cs.shape {
                        if t.galley.text().contains(want) {
                            c = Some(t.pos + t.galley.rect.center().to_vec2());
                            break;
                        }
                    }
                }
                let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
                drop(prims);
                out.textures_delta.clear();
                c
            };

        // v16: the folder scan runs on the plugin GUI thread (it dlopens
        // every plugin) — pump frames until `poll_vst_opens` lands it.
        let mut scanned = false;
        for _ in 0..800 {
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            if app.vst_scanned {
                scanned = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        assert!(
            scanned,
            "async folder scan must land (note: {:?}, pending: {})",
            app.vst_note,
            app.vst_scan_rx.is_some()
        );
        // Open the plugin picker.
        let q =
            label_center(&mut app, &ctx, "+ VST plugin").expect("plugin picker in chain window");
        frame(&mut app, &ctx, vec![moved(q)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(q, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(q, egui::PointerButton::Primary, false)],
        );
        frame(&mut app, &ctx, vec![]); // the popup paints on the next frame
        let d = find_contains(&mut app, &ctx, "Dragonfly").expect("scanned plugin row listed");
        frame(&mut app, &ctx, vec![moved(d)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(d, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(d, egui::PointerButton::Primary, false)],
        );
        // v16: instantiation itself is async too — the click only queued
        // the open job. Pump until the entry lands (then the auto-opened
        // plugin window is already in `fx_vst_editors`).
        let mut loaded = false;
        for _ in 0..400 {
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            loaded = matches!(
                &app.fx_rows[0].first(),
                Some(ChainRow::Vst { entry: Some(_), .. })
            );
            if loaded {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        std::env::set_var("HOME", &home);
        assert!(loaded, "picked plugin loaded into a chain slot");
        assert!(app.vst_note.contains("loaded"), "{}", app.vst_note);
        // v0.10 standard: adding an editor-capable plugin opens the plugin's
        // OWN window right away. Let the editor thread settle (painted, or a
        // clean refusal) BEFORE the temp plugin dir is removed: plugins load
        // their UI assets lazily from beside the binary, and pulling the file
        // out from under effEditOpen kills the process (GL-path crash).
        let editor = app
            .fx_vst_editors
            .remove(&(0, 0))
            .expect("adding an editor-capable plugin opens its own window");
        let t0 = std::time::Instant::now();
        loop {
            if editor.painted() || editor.open_failed() || editor.is_closed() {
                break;
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(15),
                "editor never painted and never refused"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let painted = editor.painted();
        // Hand the window back so app state stays coherent; it is closed
        // (thread joined) below BEFORE the temp plugin dir disappears.
        app.fx_vst_editors.insert((0, 0), editor);
        if !painted {
            // v0.11 R3 refusal path: an editor that never painted leaves NO
            // window of any kind behind (hosted plugins get no slider panel).
            // Reaping lives in the per-tick drains — drive them, not paint.
            logic_tick(&mut app);
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            assert!(
                app.fx_vst_editors.is_empty(),
                "refused editor must be reaped"
            );
        }
        // Persisted state must survive a config round-trip.
        app.save_config();
        let cfg = crate::config::Config::load();
        let saved = cfg.track_chain.first().and_then(|v| v.first());
        assert!(
            matches!(saved, Some(crate::config::ChainSave::Vst { path, kind, .. })
                if path.contains("dfhall.so") && kind == "VST2"),
            "saved chain round-trips"
        );
        // Close every editor while the plugin file STILL EXISTS, then clean.
        let editors: Vec<_> = app.fx_vst_editors.drain().collect();
        drop(editors);
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.19 AC1: the FX chain window's "+ VST plugin" picker is for
    /// EFFECTS only. With one effect and one instrument in the scan
    /// folder, the popup must list the effect and never the instrument
    /// (instruments are bound through the ♪ synth picker). Headless,
    /// real folder scan, real popup galleys.
    #[test]
    fn v19_fx_menu_hides_instruments() {
        fn copy_tree(src: &std::path::Path, dst: &std::path::Path) {
            std::fs::create_dir_all(dst).unwrap();
            for en in std::fs::read_dir(src).unwrap() {
                let en = en.unwrap();
                let to = dst.join(en.file_name());
                if en.file_type().unwrap().is_dir() {
                    copy_tree(&en.path(), &to);
                } else {
                    std::fs::copy(en.path(), &to).unwrap();
                }
            }
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let fx_src = std::path::PathBuf::from(format!("{home}/.vst/DragonflyHallReverb-vst.so"));
        let ins_src = std::path::PathBuf::from(format!("{home}/.vst3/Geonkick.vst3"));
        if !fx_src.is_file() || !ins_src.is_dir() {
            eprintln!("skipping v19 fx-menu pin: need Dragonfly VST2 + Geonkick VST3 local");
            return;
        }
        let dir = std::env::temp_dir().join(format!("sp-fxmenu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&fx_src, dir.join("dfhall.so")).unwrap();
        copy_tree(&ins_src, &dir.join("Geonkick.vst3"));

        // Ground truth straight from the scanner: one instrument, one effect.
        let entries = crate::vsthost::scan(std::slice::from_ref(&dir));
        let ok: Vec<_> = entries.iter().filter(|e| e.error.is_none()).collect();
        assert_eq!(ok.len(), 2, "both plugins must scan: {entries:?}");
        let ins = ok.iter().find(|e| e.instrument).expect("scan sees the instrument");
        let fx = ok.iter().find(|e| !e.instrument).expect("scan sees the effect");

        let mut app = headless_app("fxmenupin");
        std::env::set_var("HOME", &dir);
        app.vst_dirs = vec![dir.to_string_lossy().into_owned()];
        let ctx = egui::Context::default();

        frame(&mut app, &ctx, vec![]);
        let m = label_center(&mut app, &ctx, "\u{2026} more").expect("more button on the strip");
        frame(&mut app, &ctx, vec![moved(m)]);
        frame(&mut app, &ctx, vec![btn(m, egui::PointerButton::Primary, true)]);
        frame(&mut app, &ctx, vec![btn(m, egui::PointerButton::Primary, false)]);
        let p = label_center(&mut app, &ctx, "FX").expect("FX chip in the more popup");
        frame(&mut app, &ctx, vec![moved(p)]);
        frame(&mut app, &ctx, vec![btn(p, egui::PointerButton::Primary, true)]);
        frame(&mut app, &ctx, vec![btn(p, egui::PointerButton::Primary, false)]);
        assert_eq!(app.fx_window, Some(0), "chip opens the chain window");

        let mut scanned = false;
        for _ in 0..800 {
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            if app.vst_scanned {
                scanned = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        assert!(scanned, "folder scan must land ({:?})", app.vst_note);

        // Open the picker and read the popup's painted text. (This runs
        // BEFORE the direct-add guard test below: that test's refusal note
        // quotes the instrument's name, and the popup scan joins every
        // painted text — including status notes.)
        let q = label_center(&mut app, &ctx, "+ VST plugin").expect("plugin picker");
        frame(&mut app, &ctx, vec![moved(q)]);
        frame(&mut app, &ctx, vec![btn(q, egui::PointerButton::Primary, true)]);
        frame(&mut app, &ctx, vec![btn(q, egui::PointerButton::Primary, false)]);
        frame(&mut app, &ctx, vec![]); // the popup paints on the next frame
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let all: String = out
            .shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::epaint::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();

        assert!(
            all.contains(&fx.name) && all.contains("Dragonfly"),
            "the effect plugin must be listed in the chain picker"
        );
        assert!(
            !all.contains(&ins.name),
            "instrument {:?} must NOT be listed in the chain picker",
            ins.name
        );

        // Belt-and-braces guard: even a direct add refuses an instrument.
        let n_before = app.fx_rows[0].len();
        app.add_fx_plugin(0, ins);
        assert_eq!(app.fx_rows[0].len(), n_before, "instrument never enters a chain");
        assert!(app.vst_note.contains("instrument"), "refusal is explained: {}", app.vst_note);

        std::env::set_var("HOME", &home);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.12 R2 through the REAL UI path: chain window → "+ VST plugin" →
    /// pick the VST3 wolf-shaper → the plugin's own window must open AND
    /// paint. This is the click sequence the user performs; the VST3 path
    /// (shadow editor instance) reaches parity with VST2 here or it fails.
    #[test]
    fn v12_ui_chain_click_vst3_opens_own_window() {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1"
            || !crate::vstedit::x11_available()
        {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 + X11 required");
            return;
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let vst3dir = format!("{home}/.vst3");
        if !std::path::Path::new(&format!("{vst3dir}/wolf-shaper.vst3")).exists() {
            eprintln!("skip: wolf-shaper.vst3 not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("sp-v12v3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = headless_app("v12v3pin");
        // Isolate the default scan dirs; scan ONLY the VST3 folder so the
        // picker lists one wolf-shaper entry and nothing else can win.
        std::env::set_var("HOME", &dir);
        app.vst_dirs = vec![vst3dir.clone()];
        let ctx = egui::Context::default();

        frame(&mut app, &ctx, vec![]);
        // v0.15: on the default short track the FX row folds into the
        // "more" popup \u{2014} open it first, then take the chip.
        let m = label_center(&mut app, &ctx, "\u{2026} more").expect("more button on the strip");
        frame(&mut app, &ctx, vec![moved(m)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(m, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(m, egui::PointerButton::Primary, false)],
        );
        let p = label_center(&mut app, &ctx, "FX").expect("FX chip in the more popup");
        frame(&mut app, &ctx, vec![moved(p)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(p, egui::PointerButton::Primary, false)],
        );
        assert_eq!(app.fx_window, Some(0), "chip opens the chain window");

        // v16: scan + load are async (plugin GUI thread) — pump frames.
        let mut scanned = false;
        for _ in 0..800 {
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            if app.vst_scanned {
                scanned = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        assert!(
            scanned,
            "async folder scan must land (note: {:?}, pending: {})",
            app.vst_note,
            app.vst_scan_rx.is_some()
        );
        let q =
            label_center(&mut app, &ctx, "+ VST plugin").expect("plugin picker in chain window");
        frame(&mut app, &ctx, vec![moved(q)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(q, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(q, egui::PointerButton::Primary, false)],
        );
        frame(&mut app, &ctx, vec![]); // popup paints next frame
                                       // Picker rows carry vendor text, so match by substring (the pin
                                       // helper for exactly this is the v0.7 galley scanner).
        let d = galley_at(&mut app, &ctx, "Wolf Shaper")
            .map(|r| r.center())
            .expect("VST3 row listed in picker");
        frame(&mut app, &ctx, vec![moved(d)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(d, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(d, egui::PointerButton::Primary, false)],
        );
        let mut loaded = false;
        for _ in 0..400 {
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            loaded = matches!(
                &app.fx_rows[0].first(),
                Some(ChainRow::Vst { entry: Some(_), .. })
            );
            if loaded {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        std::env::set_var("HOME", &home);

        assert!(loaded, "VST3 loaded into the chain slot");
        // v0.12: the plugin's OWN window opened on add and PAINTS (the
        // shadow-instance path end to end: load, attach, verify, decorate).
        let mut editor = app
            .fx_vst_editors
            .remove(&(0, 0))
            .expect("v0.10 standard: adding an editor-capable plugin opens its own window");
        let t0 = std::time::Instant::now();
        loop {
            if editor.painted() || editor.open_failed() || editor.is_closed() {
                break;
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(15),
                "VST3 editor never painted and never refused"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            editor.painted(),
            "VST3 window from the UI path never painted"
        );
        // Everything closes while the plugin file still exists (v0.10 rule:
        // never pull a .vst3 bundle out from under a live editor).
        editor.close();
        let _ = std::fs::remove_dir_all(&dir);
        drop(app);
    }

    // =============== v16 responsiveness pass pins (UI) ===================
    // (AC4/AC5: the UI thread must NEVER dlopen — instantiation and the
    // folder scan run on the plugin GUI thread; clicks return immediately
    // and results land per tick via `poll_vst_opens`. The v0.15 code
    // loaded plugins synchronously inside the click handler, freezing
    // every window mid-frame: the "buttons are unresponsive" report.)

    /// v16 helper: fresh app whose (isolated) HOME/plugin folder holds
    /// exactly ONE scannable VST2. `None` = skip without the plugin.
    /// Returns (app, temp dir, REAL home) — the caller restores HOME.
    fn v16_plugin_env(tag: &str) -> Option<(App, std::path::PathBuf, String)> {
        let home = std::env::var("HOME").unwrap_or_default();
        let plugin = std::path::PathBuf::from(format!("{home}/.vst/DragonflyHallReverb-vst.so"));
        if !plugin.is_file() {
            eprintln!("skip v16 ui pin: no VST2 test plugin installed");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("sp-v16{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&plugin, dir.join("dfhall.so")).unwrap();
        let mut app = headless_app(&format!("v16{tag}"));
        std::env::set_var("HOME", &dir);
        app.vst_dirs = vec![dir.to_string_lossy().into_owned()];
        Some((app, dir, home))
    }

    /// v16 helper: open track 0's chain window through the real clicks
    /// ("\u{2026} more" popup → "FX" chip).
    fn v16_open_chain(app: &mut App, ctx: &egui::Context) {
        frame(app, ctx, vec![]);
        let m = label_center(app, ctx, "\u{2026} more").expect("more button on the strip");
        frame(app, ctx, vec![moved(m)]);
        frame(app, ctx, vec![btn(m, egui::PointerButton::Primary, true)]);
        frame(app, ctx, vec![btn(m, egui::PointerButton::Primary, false)]);
        let p = label_center(app, ctx, "FX").expect("FX chip in the more popup");
        frame(app, ctx, vec![moved(p)]);
        frame(app, ctx, vec![btn(p, egui::PointerButton::Primary, true)]);
        frame(app, ctx, vec![btn(p, egui::PointerButton::Primary, false)]);
        assert_eq!(app.fx_window, Some(0), "chip opens the chain window");
    }

    /// AC5 pin: the folder scan must never touch the UI thread. Proof by
    /// construction — `vst_rescan()` returns with the job STILL in flight
    /// (`vst_scan_rx` full, `vst_scanned` false, and the call itself
    /// unmeasurably short), and the completed list then lands via poll.
    /// v0.15 ran every plugin's dlopen+instantiate inside the window's
    /// paint: multi-second full-app freeze on first chain-window open.
    #[test]
    fn v16_ui_scan_first_open_nonblocking() {
        let Some((mut app, dir, real_home)) = v16_plugin_env("scan") else {
            return;
        };
        let ctx = egui::Context::default();
        v16_open_chain(&mut app, &ctx);
        // Window open must NOT have run a synchronous scan.
        assert!(!app.vst_scanned, "opening the window must not scan inline");
        // Deterministic core: rescan() may only QUEUE the job. (The job
        // dlopens the plugin on the GUI thread; measuring this call is
        // exactly the v0.15 freeze.)
        app.vst_scanned = false;
        app.vst_scan_rx = None;
        let t0 = std::time::Instant::now();
        app.vst_rescan();
        let dt = t0.elapsed();
        assert!(
            app.vst_scan_rx.is_some(),
            "rescan must hand back a pending scan channel"
        );
        assert!(!app.vst_scanned, "rescan must not complete the scan inline");
        assert!(
            dt < std::time::Duration::from_millis(50),
            "vst_rescan blocked the UI thread for {dt:?} — it must only post the job"
        );
        // ... and the completed list must arrive through the poll path.
        let mut landed = false;
        for _ in 0..400 {
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            if app.vst_scanned {
                landed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        std::env::set_var("HOME", &real_home);
        assert!(landed, "async scan must land via poll_vst_opens");
        assert!(
            app.vst_scan
                .iter()
                .any(|e| e.path.to_string_lossy().contains("dfhall.so") && e.error.is_none()),
            "scan list must carry the plugin"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC4 pin: clicking a plugin row in the chain window returns with
    /// the load STILL in flight (row shows loading, instance absent) —
    /// the freeze window where v0.15 ate clicks is structurally gone.
    /// The entry then lands via poll and the v0.10 auto-open still runs.
    #[test]
    fn v16_ui_add_vst_plugin_nonblocking() {
        let Some((mut app, dir, real_home)) = v16_plugin_env("add") else {
            return;
        };
        let ctx = egui::Context::default();
        v16_open_chain(&mut app, &ctx);
        // Wait for the async scan to land, then drive the real picker.
        let mut scanned = false;
        for _ in 0..800 {
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            if app.vst_scanned {
                scanned = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(scanned, "scan must land before the picker can list");
        let q = label_center(&mut app, &ctx, "+ VST plugin").expect("plugin picker");
        frame(&mut app, &ctx, vec![moved(q)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(q, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(q, egui::PointerButton::Primary, false)],
        );
        frame(&mut app, &ctx, vec![]); // popup paints next frame
        let d = galley_at(&mut app, &ctx, "Dragonfly")
            .map(|r| r.center())
            .expect("plugin row listed in picker");
        frame(&mut app, &ctx, vec![moved(d)]);
        let t0 = std::time::Instant::now();
        frame(
            &mut app,
            &ctx,
            vec![btn(d, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(d, egui::PointerButton::Primary, false)],
        );
        let click_frame = t0.elapsed();
        // THE pin: the click's frame is a queue-and-return — no plugin
        // ever instantiated on this thread — and the row is visibly
        // pending while the GUI thread does the work.
        assert!(
            click_frame < std::time::Duration::from_millis(150),
            "the add click blocked the frame loop for {:?}",
            click_frame
        );
        assert!(
            matches!(
                &app.fx_rows[0].first(),
                Some(ChainRow::Vst {
                    entry: None,
                    open_rx: Some(_),
                    ..
                })
            ),
            "add must return while the load is still in flight"
        );
        assert!(app.vst_note.contains("loading"), "{}", app.vst_note);
        // Now the entry lands, the note flips to loaded, and the plugin's
        // own GUI auto-opens (v0.10 rule, preserved on the async path).
        let mut loaded = false;
        for _ in 0..400 {
            frame(&mut app, &ctx, vec![]);
            logic_tick(&mut app);
            loaded = matches!(
                &app.fx_rows[0].first(),
                Some(ChainRow::Vst { entry: Some(_), .. })
            );
            if loaded {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(loaded, "async load must land via poll_vst_opens");
        assert!(app.vst_note.contains("loaded"), "{}", app.vst_note);
        std::env::set_var("HOME", &real_home);
        // Everything closes while the plugin file still exists.
        let editors: Vec<_> = app.fx_vst_editors.drain().collect();
        drop(editors);
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ================= v0.7 acceptance-criteria pins ======================
    // (requirements list: scratch slooper-v07/REQUIREMENTS.md)

    fn galley_at(app: &mut App, ctx: &egui::Context, needle: &str) -> Option<egui::Rect> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let hit = out.shapes.iter().find_map(|cs| {
            if let egui::epaint::Shape::Text(t) = &cs.shape {
                if t.galley.text().contains(needle) {
                    // `galley.rect` is LOCAL to the shape; the absolute
                    // center is `t.pos + (local.center() - local.min)`.
                    // Adding then subtracting `t.pos` (the old code)
                    // collapsed to the raw local center and pointed every
                    // click at the screen origin.
                    let local = t.galley.rect;
                    return Some(t.pos + (local.center() - local.min));
                }
            }
            None
        });
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        hit.map(|c| egui::Rect::from_center_size(c, egui::vec2(6.0, 10.0)))
    }

    /// Biggest painted rect of an exact fill color.
    fn painted_rect(app: &mut App, ctx: &egui::Context, rgb: (u8, u8, u8)) -> Option<egui::Rect> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let rect = out
            .shapes
            .iter()
            .filter_map(|cs| {
                if let egui::epaint::Shape::Rect(r) = &cs.shape {
                    let (r0, g0, b0, a0) = r.fill.to_tuple();
                    if a0 == 255
                        && (r0 as i16 - rgb.0 as i16).abs() <= 3
                        && (g0 as i16 - rgb.1 as i16).abs() <= 3
                        && (b0 as i16 - rgb.2 as i16).abs() <= 3
                    {
                        return Some(r.rect);
                    }
                }
                None
            })
            .max_by(|a, b| {
                (a.width() * a.height())
                    .partial_cmp(&(b.width() * b.height()))
                    .unwrap()
            });
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        rect
    }

    /// ALL painted rects of an exact fill color (clip bodies).
    fn painted_rects(app: &mut App, ctx: &egui::Context, rgb: (u8, u8, u8)) -> Vec<egui::Rect> {
        // v18: park the pointer away from the grid before probing — the
        // hover feedback brightens the clip under the pointer, and this
        // probe always matches the BASE fill rgb.
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            events: vec![moved(egui::pos2(20.0, 18.0))],
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let mut rects: Vec<egui::Rect> = out
            .shapes
            .iter()
            .filter_map(|cs| {
                if let egui::epaint::Shape::Rect(r) = &cs.shape {
                    let (r0, g0, b0, a0) = r.fill.to_tuple();
                    if a0 >= 100
                        && (r0 as i16 - rgb.0 as i16).abs() <= 3
                        && (g0 as i16 - rgb.1 as i16).abs() <= 3
                        && (b0 as i16 - rgb.2 as i16).abs() <= 3
                        && r.rect.width() > 4.0
                    {
                        return Some(r.rect);
                    }
                }
                None
            })
            .collect();
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        rects.sort_by(|a, b| a.min.x.partial_cmp(&b.min.x).unwrap());
        rects
    }

    fn click_pt(app: &mut App, ctx: &egui::Context, p: egui::Pos2) {
        frame(
            app,
            ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(app, ctx, vec![btn(p, egui::PointerButton::Primary, false)]);
    }

    fn click_mod(app: &mut App, ctx: &egui::Context, p: egui::Pos2, m: egui::Modifiers) {
        frame(
            app,
            ctx,
            vec![
                egui::Event::ModifiersChanged(m),
                moved(p),
                btn(p, egui::PointerButton::Primary, true),
            ],
        );
        frame(
            app,
            ctx,
            vec![
                btn(p, egui::PointerButton::Primary, false),
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ],
        );
    }

    fn drag_pt(app: &mut App, ctx: &egui::Context, from: egui::Pos2, to: egui::Pos2) {
        frame(
            app,
            ctx,
            vec![moved(from), btn(from, egui::PointerButton::Primary, true)],
        );
        // A 1 px nudge starts the drag: the first motion frame past the
        // press only arms egui's drag machinery, and canvases that commit
        // every drag frame (piano roll) silently swallow a big first jump.
        frame(app, ctx, vec![moved(from + egui::vec2(1.0, 0.0))]);
        let mid = from + (to - from) * 0.5;
        frame(app, ctx, vec![moved(mid)]);
        frame(app, ctx, vec![moved(to)]);
        // Land on the endpoint once more so handlers that commit with a
        // frame of lag still see the true destination.
        frame(app, ctx, vec![moved(to)]);
        frame(app, ctx, vec![btn(to, egui::PointerButton::Primary, false)]);
    }

    /// Open the piano roll through the real toolbar button; returns the
    /// painted canvas rect.
    fn open_roll_headless(app: &mut App, ctx: &egui::Context) -> egui::Rect {
        frame(app, ctx, vec![]);
        let b = galley_at(app, ctx, "Piano roll").expect("piano roll toolbar button");
        let p = b.center();
        click_pt(app, ctx, p);
        assert!(app.piano_roll.is_some(), "toolbar button opens the roll");
        for _ in 0..24 {
            frame(app, ctx, vec![]);
        }
        painted_rect(app, ctx, (22, 22, 26)).expect("roll canvas painted")
    }

    /// AC1.3, AC1.4, AC1.5, AC1.6, AC1.8.
    #[test]
    fn v07_roll_fills_scrolls_gates_and_closes() {
        let mut app = headless_app("v07rollnav");
        let ctx = egui::Context::default();
        let canvas = open_roll_headless(&mut app, &ctx);

        // AC1.3 the window fills most of the screen.
        assert!(
            canvas.width() > 1440.0 * 0.8 && canvas.height() > 853.0 * 0.55,
            "roll must fill the GUI, got {canvas:?}"
        );

        // AC1.4 the roll follows the MAIN metronome bpm (no own control).
        app.metro_bpm = String::from("97");
        frame(&mut app, &ctx, vec![]);
        assert_eq!(
            app.piano_roll.as_ref().unwrap().pat.bpm,
            97,
            "roll bpm = main bpm"
        );

        // AC1.6 both scrollbars move the view. Make content scrollable.
        {
            let r = app.piano_roll.as_mut().unwrap();
            r.pat.len_ticks = 7680;
            r.px_per_beat = 400.0;
            r.scroll_sec = 0.0;
            r.vscroll_px = 0.0;
        }
        frame(&mut app, &ctx, vec![]);
        let c = painted_rect(&mut app, &ctx, (22, 22, 26)).expect("canvas");
        click_pt(&mut app, &ctx, egui::pos2(c.max.x - 5.0, c.min.y + 12.0));
        assert!(
            app.piano_roll.as_ref().unwrap().vscroll_px > 0.0,
            "vertical scrollbar click scrolls the pitch view"
        );
        click_pt(&mut app, &ctx, egui::pos2(c.max.x - 40.0, c.max.y - 5.0));
        assert!(
            app.piano_roll.as_ref().unwrap().scroll_sec > 0.1,
            "horizontal scrollbar click scrolls the time view"
        );

        // AC1.5 a click inside the roll focuses it; space must NOT start
        // the transport behind the roll.
        let before = app.backend.engine.transport_on();
        click_pt(&mut app, &ctx, egui::pos2(c.min.x + 300.0, c.min.y + 60.0));
        assert!(
            app.piano_roll.as_ref().unwrap().focused,
            "click focuses the roll"
        );
        frame_keys(
            &mut app,
            &ctx,
            vec![key_ev(egui::Key::Space, egui::Modifiers::NONE)],
        );
        assert_eq!(
            app.backend.engine.transport_on(),
            before,
            "space is swallowed while the roll owns the keyboard"
        );

        // AC1.8 the title-bar close button closes ONLY the roll.
        // The X lives at the right end of the title bar — anchor on the
        // stored full-frame rect (title bar included); hard-coded screen
        // coordinates drifted the moment the window layout moved.
        let mut closed = false;
        let wr = app
            .piano_roll
            .as_ref()
            .unwrap()
            .win_rect
            .expect("roll frame rect");
        for dx in [6.0f32, 12.0, 18.0, 24.0] {
            for dy in [11.0f32, 16.0] {
                click_pt(&mut app, &ctx, egui::pos2(wr.max.x - dx, wr.min.y + dy));
                if app.piano_roll.is_none() {
                    closed = true;
                    break;
                }
            }
            if closed {
                break;
            }
        }
        assert!(closed, "the window X closes the piano roll alone");
        frame(&mut app, &ctx, vec![]);
        frame(&mut app, &ctx, vec![]); // app survives the close
    }

    /// AC1.7, AC7.2, AC8.4 (piano roll).
    #[test]
    fn v07_roll_edge_resize_group_move_and_clipboard() {
        let mut app = headless_app("v07rolle");
        let ctx = egui::Context::default();
        let _canvas = open_roll_headless(&mut app, &ctx);
        app.snap_on = false; // sample-granular gestures for exact deltas
        let ppb = 400.0f32; // px/beat at 120 bpm
        {
            let r = app.piano_roll.as_mut().unwrap();
            r.pat.bpm = 120;
            r.pat.len_ticks = 1920;
            r.px_per_beat = ppb;
            r.row_h = 9.0;
            r.scroll_sec = 0.0;
            r.vscroll_px = 0.0;
            r.pat.notes = vec![
                crate::midipat::Note {
                    ch: 0,
                    note: 48,
                    vel: 100,
                    press: 0,
                    start: 480,
                    len: 480,
                },
                crate::midipat::Note {
                    ch: 0,
                    note: 50,
                    vel: 100,
                    press: 0,
                    start: 960,
                    len: 480,
                },
            ];
        }
        frame(&mut app, &ctx, vec![]);
        let c = painted_rect(&mut app, &ctx, (22, 22, 26)).expect("canvas");
        let gx0 = c.min.x + 58.0;
        let x_of = |t_ticks: u32| gx0 + (t_ticks as f64 / 480.0 * ppb as f64) as f32;
        let y_of = |n: i32| c.max.y - 10.0 - n as f32 * 9.0 - 9.0;

        // Focus the roll: click the title bar (does not draw a note).
        // Anchor on the stored frame rect — a shape-scan title position
        // goes stale as soon as the window's first-use auto-placement
        // settles, and the click then lands outside the moved window.
        let wr = app
            .piano_roll
            .as_ref()
            .unwrap()
            .win_rect
            .expect("roll frame rect");
        click_pt(
            &mut app,
            &ctx,
            egui::pos2(wr.min.x + wr.width() * 0.5, wr.min.y + 10.0),
        );
        assert!(
            app.piano_roll.as_ref().unwrap().focused,
            "title click focuses the roll"
        );

        // AC8.4 click / ctrl+click / shift+click / marquee box on notes.
        let b0 = egui::pos2(x_of(480) + 40.0, y_of(48) + 4.0);
        let b1 = egui::pos2(x_of(960) + 40.0, y_of(50) + 4.0);
        click_pt(&mut app, &ctx, b0);
        assert_eq!(
            app.piano_roll.as_ref().unwrap().sel_notes,
            vec![0],
            "plain click selects one note"
        );
        click_mod(&mut app, &ctx, b1, egui::Modifiers::COMMAND);
        assert_eq!(
            app.piano_roll.as_ref().unwrap().sel_notes.len(),
            2,
            "ctrl+click toggles in"
        );
        click_mod(&mut app, &ctx, b0, egui::Modifiers::SHIFT);
        assert_eq!(
            app.piano_roll.as_ref().unwrap().sel_notes.len(),
            2,
            "shift+click runs from anchor"
        );
        click_mod(&mut app, &ctx, b1, egui::Modifiers::COMMAND);
        assert_eq!(
            app.piano_roll.as_ref().unwrap().sel_notes.len(),
            1,
            "ctrl+click toggles out"
        );
        // Ctrl+drag a rubber band on empty canvas replaces the selection.
        let from = egui::pos2(gx0 + 10.0, y_of(60) + 4.0);
        let to = egui::pos2(x_of(1440) + 20.0, y_of(46) + 4.0);
        frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                moved(from),
                btn(from, egui::PointerButton::Primary, true),
            ],
        );
        frame(&mut app, &ctx, vec![moved(from + (to - from) * 0.5)]);
        frame(
            &mut app,
            &ctx,
            vec![
                moved(to),
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(to, egui::PointerButton::Primary, false)],
        );
        assert_eq!(
            app.piano_roll.as_ref().unwrap().sel_notes.len(),
            2,
            "marquee box selects every intersected note"
        );

        // AC8.4 Ctrl+A selects all, AC7.2 copy + paste at the playhead.
        // `i.modifiers` only updates from ModifiersChanged — an inline
        // Key.modifiers alone never arms the command modifier.
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::A, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(
            app.piano_roll.as_ref().unwrap().sel_notes.len(),
            2,
            "Ctrl+A selects all"
        );
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::C, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(
            app.roll_clip.as_ref().map(|v| v.len()),
            Some(2),
            "Ctrl+C copies the notes"
        );
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::V, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(
            app.piano_roll.as_ref().unwrap().pat.notes.len(),
            4,
            "Ctrl+V pastes at the playhead"
        );

        // AC1.7 drag the RIGHT edge of note 0 outward: len grows, start
        // fixed (loop-area-handle style). Press 2 px INSIDE the boundary
        // — exactly on it falls outside the note's half-open x range.
        let n0 = app.piano_roll.as_ref().unwrap().pat.notes[0];
        let pe = egui::pos2(x_of(n0.start + n0.len) - 2.0, y_of(n0.note as i32) + 4.0);
        drag_pt(&mut app, &ctx, pe, pe + egui::vec2(80.0, 0.0));
        let after = app.piano_roll.as_ref().unwrap().pat.notes[0];
        assert_eq!(after.start, n0.start, "tail drag pins the start");
        let dl = after.len as f64 - n0.len as f64;
        assert!(
            (dl - 96.0).abs() < 10.0,
            "80px tail drag \u{2248} +96 ticks, got {dl}"
        );

        // AC1.7 head drag: start moves, end pinned.
        let n0 = after;
        let ph = egui::pos2(x_of(n0.start) + 2.0, y_of(n0.note as i32) + 4.0);
        drag_pt(&mut app, &ctx, ph, ph + egui::vec2(40.0, 0.0));
        let after2 = app.piano_roll.as_ref().unwrap().pat.notes[0];
        assert_eq!(
            after2.start + after2.len,
            n0.start + n0.len,
            "head drag pins the end"
        );
        assert!(after2.start > n0.start, "head drag moves the start right");

        // AC8.4 group move: Ctrl+A then drag one note body — EVERY selected
        // note shifts by the same snapped delta.
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::A, egui::Modifiers::COMMAND),
            ],
        );
        let before: Vec<u32> = app
            .piano_roll
            .as_ref()
            .unwrap()
            .pat
            .notes
            .iter()
            .map(|n| n.start)
            .collect();
        let nb = app.piano_roll.as_ref().unwrap().pat.notes[0];
        let pb = egui::pos2(
            x_of(nb.start) + (((nb.len as f64 / 960.0) * 800.0) * 0.5) as f32,
            y_of(nb.note as i32) + 4.0,
        );
        drag_pt(&mut app, &ctx, pb, pb + egui::vec2(160.0, 0.0));
        let movedv: Vec<u32> = app
            .piano_roll
            .as_ref()
            .unwrap()
            .pat
            .notes
            .iter()
            .map(|n| n.start)
            .collect();
        let d0 = movedv[0] as i64 - before[0] as i64;
        assert!(
            (d0 - 192).abs() < 10,
            "160px group drag \u{2248} +192 ticks, got {d0}"
        );
        for (b, m) in before.iter().zip(movedv.iter()) {
            assert_eq!(*m as i64 - *b as i64, d0, "group moves rigidly");
        }
    }

    /// AC5.1-AC5.3 (main GUI clip edges) + AC6.1/6.2 (snap toggle).
    #[test]
    fn v07_clip_edge_drag_and_snap_toggle() {
        let mut app = headless_app("v07trim");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        // A synthetic 12 s asset so edge drags have room to move.
        let data: std::sync::Arc<[f32]> = vec![0.25f32; 44_100 * 12].into();
        let a = app.assets.len() as u32;
        app.assets.push(crate::assets::Asset::audio(
            a,
            "long".into(),
            std::path::PathBuf::from("/tmp/long-test.wav"),
            data,
            false,
        ));
        let alen = app.asset_len(a);
        assert!(alen > 300_000, "test asset must be long enough, got {alen}");
        app.px_per_sec = 40.0; // 12 s asset = 480 px, fits the viewport
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        app.select_one((0, 0, 0));
        app.last_click_track = Some(0);
        app.snap_on = false;
        frame(&mut app, &ctx, vec![]);
        // Selected clip body color. NOTE: selection is shown by a
        // COL_CLIP_SEL *outline*; the body fill stays COL_CLIP·0.45
        // either way, so geometry is anchored on the fill.
        let sel_rgb = {
            let c = crate::ui::COL_CLIP.linear_multiply(0.45);
            let (r, g, b, _) = c.to_tuple();
            (r, g, b)
        };
        // The body fill is translucent (COL_CLIP·0.45 keeps alpha ~115),
        // so match through painted_rects (alpha-tolerant) and take the
        // biggest hit — `painted_rect`'s opaque-only filter never sees it.
        let r0 = painted_rects(&mut app, &ctx, sel_rgb)
            .into_iter()
            .max_by(|a, b| a.area().total_cmp(&b.area()))
            .expect("selected clip painted");
        {
            // The selection outline must be painted in COL_CLIP_SEL.
            let (sr, sg, sb, _) = crate::ui::COL_CLIP_SEL.to_tuple();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let outlined = out.shapes.iter().any(|cs| match &cs.shape {
                egui::epaint::Shape::Rect(r) => {
                    let (r0, g0, b0, _) = r.stroke.color.to_tuple();
                    r0.abs_diff(sr) <= 3 && g0.abs_diff(sg) <= 3 && b0.abs_diff(sb) <= 3
                }
                _ => false,
            });
            assert!(outlined, "selected clip carries a COL_CLIP_SEL outline");
        }
        let cy = r0.center().y;
        macro_rules! clp {
            () => {
                app.arr.tracks[0].lanes[0].clips[0]
            };
        }
        let span0 = clp!().end(alen) - clp!().pos;

        // AC5.2 trim from the tail (right edge inward). At 160 px/s a
        // 200 px pull removes ~60 k samples, comfortably past the 30 k
        // assertion floor (the old 58 px drag only cut ~17 k).
        drag_pt(
            &mut app,
            &ctx,
            egui::pos2(r0.max.x - 2.0, cy),
            egui::pos2(r0.max.x - 200.0, cy),
        );
        let c1 = clp!();
        assert!(
            c1.end(alen) - c1.pos < span0 - 30_000 && c1.cut == 0,
            "tail drag shortens and keeps the head: {c1:?}"
        );

        // AC5.2 trim from the head (left edge inward): pos AND cut move.
        // v18: park the pointer OFF the clip before probing painted
        // rects — a hovering pointer brightens the clip (hover feedback)
        // and this probe matches the BASE rgb.
        frame(&mut app, &ctx, vec![moved(egui::pos2(20.0, 18.0))]);
        frame(&mut app, &ctx, vec![]);
        let r1 = painted_rects(&mut app, &ctx, sel_rgb)
            .into_iter()
            .max_by(|a, b| a.area().total_cmp(&b.area()))
            .expect("clip after trim");
        drag_pt(
            &mut app,
            &ctx,
            egui::pos2(r1.min.x + 2.0, cy),
            egui::pos2(r1.min.x + 60.0, cy),
        );
        let c2 = clp!();
        assert!(
            c2.pos > c1.pos && c2.cut > 0 && c2.end(alen) == c1.end(alen),
            "head drag cuts into the audio: {c2:?} vs {c1:?}"
        );

        // AC5.3 head back leftward: re-exposes the cut audio.
        frame(&mut app, &ctx, vec![]);
        let r2 = painted_rects(&mut app, &ctx, sel_rgb)
            .into_iter()
            .max_by(|a, b| a.area().total_cmp(&b.area()))
            .expect("clip after head trim");
        drag_pt(
            &mut app,
            &ctx,
            egui::pos2(r2.min.x + 2.0, cy),
            egui::pos2(r2.min.x - 40.0, cy),
        );
        let c3 = clp!();
        assert!(c3.cut < c2.cut, "outward head drag re-exposes audio");

        // AC5.3 tail PAST the asset end: silent extension.
        frame(&mut app, &ctx, vec![]);
        let r3 = painted_rects(&mut app, &ctx, sel_rgb)
            .into_iter()
            .max_by(|a, b| a.area().total_cmp(&b.area()))
            .expect("clip before extend");
        // +240 px, not +200: at 160 px/s the release must land PAST the
        // asset end (200 px hits it exactly → play_len returns to None).
        drag_pt(
            &mut app,
            &ctx,
            egui::pos2(r3.max.x - 2.0, cy),
            egui::pos2(r3.max.x + 240.0, cy),
        );
        let c4 = clp!();
        let avail4 = alen - c4.cut;
        assert!(
            c4.play_len == Some(c4.end(alen) - c4.pos) && c4.end(alen) - c4.pos > avail4,
            "silence extension stores play_len > available audio: {c4:?}"
        );

        // AC6.1 the snap checkbox lives in the grid header next to the
        // subdivision combo; clicking it flips snap_on; AC6.2 OFF makes the
        // snap unit exactly one sample.
        app.snap_on = true;
        frame(&mut app, &ctx, vec![]);
        let unit_on = app.snap_unit();
        assert!(unit_on > 1.0, "snap on = subdivision unit");
        let lbl = galley_at(&mut app, &ctx, "snap").expect("snap label in header");
        // The clickable box sits AFTER the 4-glyph "snap" label (its own
        // label is ""), so aim right of the text, not at its middle.
        click_pt(
            &mut app,
            &ctx,
            egui::pos2(lbl.center().x + 20.0, lbl.center().y),
        );
        assert!(!app.snap_on, "header checkbox toggles snap off");
        assert_eq!(app.snap_unit(), 1.0, "snap off = one sample");
    }

    /// AC8.1-AC8.3 (main GUI multi-selection).
    #[test]
    fn v07_main_grid_multi_select() {
        let mut app = headless_app("v07msel");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        // Three 40 k-sample clips spread along lane 0 with empty gaps
        // around them. NOTE: Clip::new's second arg is the POSITION, so
        // the old "length" numbers stacked all three at the same x; the
        // timeline length goes in play_len.
        app.arr.insert(
            0,
            0,
            Clip {
                asset: a,
                pos: 60_000,
                cut: 0,
                play_len: Some(40_000),
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            },
        );
        app.arr.insert(
            0,
            0,
            Clip {
                asset: a,
                pos: 130_000,
                cut: 0,
                play_len: Some(40_000),
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            },
        );
        app.arr.insert(
            0,
            0,
            Clip {
                asset: a,
                pos: 200_000,
                cut: 0,
                play_len: Some(40_000),
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            },
        );
        frame(&mut app, &ctx, vec![]);
        let body = {
            let c = COL_CLIP.linear_multiply(0.45);
            let (r, g, b, _) = c.to_tuple();
            (r, g, b)
        };
        let mut rects = painted_rects(&mut app, &ctx, body);
        assert!(rects.len() >= 3, "three clips painted: {}", rects.len());
        // Left-to-right order: the click/marquee steps below index the
        // clips along the lane, not by painted area.
        rects.sort_by(|a, b| a.min.x.total_cmp(&b.min.x));
        let rects = &rects[..3];

        // AC8.1 plain click selects one, Ctrl+click adds.
        click_pt(&mut app, &ctx, rects[0].center());
        assert_eq!(app.sel_clips.len(), 1, "plain click = one clip");
        click_mod(&mut app, &ctx, rects[2].center(), egui::Modifiers::COMMAND);
        assert_eq!(
            app.sel_clips.len(),
            2,
            "ctrl+click toggles a second clip in"
        );
        click_mod(&mut app, &ctx, rects[2].center(), egui::Modifiers::COMMAND);
        assert!(app.sel_clips.len() == 1, "ctrl+click again toggles it out");
        click_mod(&mut app, &ctx, rects[2].center(), egui::Modifiers::COMMAND);

        // AC8.2 shift+click selects the same-lane run from the anchor.
        click_mod(&mut app, &ctx, rects[0].center(), egui::Modifiers::SHIFT);
        assert_eq!(
            app.sel_clips.len(),
            3,
            "shift+click extends to the whole run"
        );

        // AC8.3 dragging a box on the empty lane area selects every clip
        // it intersects.
        let y = rects[1].center().y;
        let from = egui::pos2(rects[0].min.x - 60.0, y);
        let to = egui::pos2(rects[2].max.x + 40.0, y);
        drag_pt(&mut app, &ctx, from, to);
        assert_eq!(
            app.sel_clips.len(),
            3,
            "selection box selects all intersected clips"
        );
    }

    /// AC3.1 the sampler root note renders as standard note notation.
    #[test]
    fn v07_sampler_root_note_shows_note_names() {
        let mut app = headless_app("v07root");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        app.set_track_synth_stem(0, "sampler");
        app.synth_win = Some(0);
        frame(&mut app, &ctx, vec![]);
        fn is_note(s: &str) -> bool {
            let t = s.trim();
            let mut ch = t.chars();
            let Some(a) = ch.next() else { return false };
            a.is_ascii_uppercase() && matches!(a, 'A'..='G') && {
                let rest: String = ch.collect();
                let (acc, num): (String, String) = rest.chars().partition(|c| *c == '#');
                !num.is_empty()
                    && num.chars().all(|c| c.is_ascii_digit())
                    && acc.chars().all(|c| c == '#')
            }
        }
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let g: Vec<String> = out
            .shapes
            .iter()
            .filter_map(|cs| {
                if let egui::epaint::Shape::Text(t) = &cs.shape {
                    let s = t.galley.text().trim().to_string();
                    if is_note(&s) {
                        return Some(s);
                    }
                }
                None
            })
            .collect();
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        assert!(
            g.iter().any(|s| s == "C3"),
            "root-note slider must paint 'C3' (default 48), galleys: {g:?}"
        );
    }

    /// v0.9: bare S splits the selected clip at the playhead; the clip
    /// property strip (gain / fade in / fade out) paints for exactly one
    /// selection and hides otherwise.
    #[test]
    fn v09_ui_split_gain_strip() {
        fn has_text(app: &mut App, _ctx: &egui::Context, needle: &str) -> bool {
            // Fresh context: egui skips `run` on a warm context that sees no
            // new input, which would yield empty shapes. The App itself holds
            // all state, so a cold context paints the same frame.
            let ctx = egui::Context::default();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            // Cold context emits a font-atlas delta on its first frame; this
            // probe renders for text only, so discard it (epaint asserts on
            // dropping an unapplied delta otherwise).
            out.textures_delta.clear();
            let texts: Vec<String> = out
                .shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    egui::epaint::Shape::Text(t) => Some(t.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            texts.iter().any(|x| x.contains(needle))
        }

        let mut app = headless_app("v09split");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        let data: std::sync::Arc<[f32]> = vec![0.25f32; 96_000].into();
        let a = app.assets.len() as u32;
        app.assets.push(crate::assets::Asset::audio(
            a,
            "sine".into(),
            std::path::PathBuf::from("/tmp/v09-sine.wav"),
            data,
            false,
        ));
        app.arr.insert(0, 0, Clip::new(a, 0));
        app.select_one((0, 0, 0));
        app.last_click_track = Some(0);
        app.backend.engine.seek_to(48_000);
        frame_keys(&mut app, &ctx, vec![]);

        // Property strip for exactly one selected clip.
        assert!(
            has_text(&mut app, &ctx, "clip: sine"),
            "strip names the clip"
        );
        assert!(
            has_text(&mut app, &ctx, "fade in"),
            "fade-in slider present"
        );
        assert!(
            has_text(&mut app, &ctx, "fade out"),
            "fade-out slider present"
        );

        // S splits at the playhead.
        frame_keys(
            &mut app,
            &ctx,
            vec![key_ev(egui::Key::S, egui::Modifiers::NONE)],
        );
        let lane = &app.arr.tracks[0].lanes[0];
        assert_eq!(lane.clips.len(), 2, "S split the clip");
        assert_eq!(lane.clips[1].pos, 48_000);
        assert_eq!(lane.clips[1].cut, 48_000, "right half resumes the source");
        assert_eq!(lane.clips[0].play_len, Some(48_000));

        // Playhead at a clip boundary is not inside anything: no-op.
        frame_keys(
            &mut app,
            &ctx,
            vec![key_ev(egui::Key::S, egui::Modifiers::NONE)],
        );
        assert_eq!(
            app.arr.tracks[0].lanes[0].clips.len(),
            2,
            "edge is not a split"
        );

        // No selection: the strip is gone.
        app.sel_clips.clear();
        frame_keys(&mut app, &ctx, vec![]);
        assert!(
            !has_text(&mut app, &ctx, "clip: sine"),
            "strip hides without a selection"
        );
    }

    // ===================== v0.10 acceptance pins =====================

    /// AC7.1/7.3: the track count now travels 1..=128 through config and
    /// project files, clamped at the engine capacity (never 16).
    #[test]
    fn v10_settings_track_count_caps_at_128() {
        let mut app = headless_app("v10-nt");
        assert_eq!(app.n_tracks, 16, "default unchanged");
        app.n_tracks = 128;
        assert_eq!(app.build_project("v10cap".into()).n_tracks, Some(128));
        app.do_save("v10cap");
        app.n_tracks = 4;
        let path = crate::project::project_path(&app.projects_dir(), "v10cap");
        app.do_load(&path);
        assert_eq!(app.n_tracks, 128, "saved capacity must restore");

        // Above capacity clamps down to MAX_LOOPS, not to the old 16.
        let mut pr = app.build_project("v10over".into());
        pr.n_tracks = Some(4096);
        app.n_tracks = 3;
        let p2 = crate::project::project_path(&app.projects_dir(), "v10over");
        crate::project::save(&p2, &pr).expect("save over");
        app.do_load(&p2);
        assert_eq!(app.n_tracks, MAX_LOOPS, "4096 must clamp to capacity");
    }

    /// AC7.1: a clip on a hidden track (> 16) survives save/load and stays
    /// silent until the capacity exposes it — capacity gates the GRID, the
    /// engine plays what's flagged on, like always.
    #[test]
    fn v10_tracks_beyond_16_roundtrip_and_play() {
        let mut app = headless_app("v10-hidden");
        app.n_tracks = 40;
        // Direct asset injection (rack scan is async; the test needs the
        // sample table synchronously, like the render tests do).
        let data: std::sync::Arc<[f32]> = std::sync::Arc::from(vec![0.5f32; 6000]);
        app.assets.push(Asset::audio(
            0,
            "tone".into(),
            std::path::PathBuf::from("/tmp/tone.wav"),
            data,
            false,
        ));
        app.arr.insert(33, 0, Clip::new(0, 0));
        app.push_project();
        assert!(app.arr.tracks[33].lanes.iter().any(|l| !l.clips.is_empty()));
        // Null-audio backends drop commands (no manager thread), so mirror
        // the project onto the engine handle directly — exactly what the
        // manager does with SetProject.
        app.backend.engine.set_project(
            std::sync::Arc::new(
                app.assets
                    .iter()
                    .map(|a| a.data.clone())
                    .collect::<Vec<_>>(),
            ),
            std::sync::Arc::new(app.arr.clone()),
            app.total_len(),
        );
        let out = app.backend.engine.render_project().expect("render");
        assert!(
            out.iter().any(|x| x.abs() > 0.05),
            "clip on track 34 sounds"
        );
    }

    /// AC6.2/8.4 UI side: the rack's audition state resets when the
    /// transport takes over, so a browse-click can never double over real
    /// playback.
    #[test]
    fn v10_transport_clears_audition_state() {
        let mut app = headless_app("v10-aud");
        app.preview_playing = Some("kick.wav".into());
        // Simulate the engine announcing transport start.
        let _ = app.backend.engine.preview_active();
        // Arm an audition, then do what the manager does on transport-on
        // (null-audio backends drop commands, so call the engine contract
        // directly; the PlayAll/StopAll wiring itself is covered e2e):
        // auditions must be gone before real playback starts.
        let v: std::sync::Arc<[f32]> = std::sync::Arc::from(vec![0.1f32; 4800]);
        app.backend.engine.play_preview(1, v);
        assert!(app.backend.engine.preview_active(), "audition armed");
        app.preview_playing = Some("kick.wav".into());
        app.backend.engine.stop_preview();
        assert!(
            !app.backend.engine.preview_active(),
            "transport-on must clear audition voices"
        );
    }

    /// AC2.1: binding a plugin instrument opens the plugin's OWN window
    /// straight away (no slider window step). Needs a real plugin + X, so
    /// it follows the existing SLOOPER_VST_SMOKE gate.
    #[test]
    fn v10_plugin_synth_opens_its_own_gui() {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" {
            eprintln!("skip: set SLOOPER_VST_SMOKE=1 to run");
            return;
        }
        let home = std::env::var("HOME").unwrap_or_default();
        // Wolf Shaper: native Linux VST2 whose editor returns the "true"
        // (1) convention on effEditOpen — exactly what used to be refused.
        let p = std::path::PathBuf::from(&home).join(".vst/wolf-shaper-vst.so");
        if !p.is_file() {
            eprintln!("skip: wolf-shaper not installed");
            return;
        }
        // (Opened standalone first: pins the SAME has_editor answer the UI
        // will see, and proves the plugin loads before asserting routing.)
        let load = crate::vsthost::open(&p, crate::vsthost::VstKind::V2, 48_000, None)
            .expect("open wolf-shaper");
        let has_editor = load.entry.has_editor();
        drop(load);
        let mut app = headless_app("v10-gui");
        let stem = vst_synth_encode(crate::vsthost::VstKind::V2, &p.to_string_lossy());
        app.set_track_synth_stem(0, &stem);
        // EXACTLY ONE surface takes over a plugin binding: the plugin's
        // own window when its editor is available, the slider window as
        // the fallback when it is not. Never both, never neither.
        let editor = app.vst_editors.contains_key(&0);
        let sliders = app.synth_win == Some(0);
        assert!(
            editor ^ sliders,
            "binding must pick exactly one surface (editor {editor}, sliders {sliders}); status: {}",
            app.status
        );
        if has_editor {
            assert!(
                editor,
                "an editor-capable plugin must open its OWN window, not sliders; status: {}",
                app.status
            );
        }
        // Close it again: the app must be able to tear the editor down.
        app.vst_editors.remove(&0);
    }

    /// AC2.2/2.3: script synths never open editor windows (sliders stay
    /// theirs), and plugin EDITORS for chain slots toggle through the
    /// plugin window first, sliders only as fallback.
    #[test]
    fn v10_script_synth_keeps_sliders_plugin_gets_own_window() {
        let mut app = headless_app("v10-fallback");
        app.set_track_synth_stem(1, "sampler");
        assert!(
            app.vst_editors.is_empty() && app.fx_vst_editors.is_empty() && app.synth_win.is_none(),
            "a script synth opened a plugin window somehow: {}",
            app.status
        );
        assert!(app.track_synth[1].as_deref() == Some("sampler"));
    }

    /// AC8.4: clicking a rack row that is auditioning stops ONLY that
    /// asset's preview voices; other assets keep playing (the engine half
    /// is pinned by v10_preview_voices_retire_and_stop_clears — here the
    /// UI's per-id routing and highlight bookkeeping).
    #[test]
    fn v10_rack_toggle_stops_only_that_asset() {
        let mut app = headless_app("v10-racktoggle");
        let mk = |name: &str| {
            crate::assets::Asset::audio(
                0,
                name.into(),
                std::path::PathBuf::from(format!("/tmp/{name}.wav")),
                std::sync::Arc::from(vec![0.4f32; 4800]),
                false,
            )
        };
        app.assets.push(mk("kick"));
        app.assets.push(mk("hat"));
        app.sample_rate = 48_000;
        app.preview_enabled = true;

        // Arm both on the engine directly (the null backend has no manager),
        // mirroring what preview_asset's commands do on a live engine.
        let a = app.assets[0].data.clone();
        let b = app.assets[1].data.clone();
        app.backend.engine.play_preview(0, a.clone());
        app.backend.engine.play_preview(0, a); // repeat stacks
        app.backend.engine.play_preview(1, b);

        // Audition kick through the UI, then hat through the UI.
        app.preview_asset(0);
        assert_eq!(
            app.preview_playing.as_deref(),
            Some("/tmp/kick.wav".to_string().as_str())
        );
        app.preview_asset(1);
        assert_eq!(
            app.preview_playing.as_deref(),
            Some("/tmp/hat.wav".to_string().as_str())
        );

        // Toggle kick OFF (click while its highlight is current after the
        // hat finished flag-wise is not the flow; simulate the row click
        // for the CURRENTLY highlighted asset):
        app.preview_asset(1);
        assert!(
            app.preview_playing.is_none(),
            "re-click stops the highlight"
        );

        // Engine-side per-id isolation (what the toggle command routes to):
        app.backend.engine.stop_preview_id(0);
        let snap = app.backend.engine.preview_snapshot();
        assert!(snap.iter().all(|(_v, id, _)| *id != 0), "kick voices gone");
        assert_eq!(snap.len(), 1, "hat voice untouched by the kick toggle");
        app.backend.engine.stop_preview();
    }

    /// v10.2 "deleting assets crashes to desktop" sweep: delete every
    /// asset, in several orders, while views that hold asset ids stay
    /// open — the piano roll editing one of them, clips referencing the
    /// rest, selection hot — running real frames between removals so any
    /// stale-id index (the class of bug: ids ARE indices) panics here.
    #[test]
    fn v102_delete_asset_stress_with_views_open() {
        use std::path::PathBuf;
        let mut app = headless_app("delstress");
        let ctx = egui::Context::default();
        let samples = PathBuf::from(std::env::var("XDG_CONFIG_HOME").unwrap()).join("samples");
        let kick = samples.join("kick.wav");
        for n in ["snare.wav", "hat.wav"] {
            let d = samples.join(n);
            if !d.exists() {
                std::fs::copy(&kick, &d).unwrap();
            }
        }
        let mut pat = Pattern::new(120);
        pat.len_ticks = 4 * PPQ as u32;
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
        });
        pat.save_mid(&samples.join("roll.mid")).unwrap();
        let a0 = app.ensure_asset(&kick).unwrap();
        let a1 = app.ensure_asset(&samples.join("snare.wav")).unwrap();
        let a2 = app.ensure_asset(&samples.join("hat.wav")).unwrap();
        let m = app.ensure_asset(&samples.join("roll.mid")).unwrap();
        assert_eq!(app.assets.len(), 4);
        for (lane, asset) in [(0usize, a0), (1, a1), (2, a2), (3, m)] {
            app.arr
                .insert(0, lane, Clip::new(asset as u32, (lane as u64) * 4800));
        }
        let sel =
            app.arr
                .tracks
                .iter()
                .enumerate()
                .flat_map(|(t, tr)| {
                    tr.lanes.iter().enumerate().flat_map(move |(k, l)| {
                        l.clips.iter().enumerate().map(move |(c, _)| (t, k, c))
                    })
                })
                .find(|(t, k, c)| app.arr.tracks[*t].lanes[*k].clips[*c].asset as usize == m)
                .unwrap();
        app.sel_clips = vec![sel];
        app.sel_asset = Some(a2);

        let btn = {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1440.0, 853.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
            let hit = out.shapes.iter().find_map(|cs| {
                if let egui::epaint::Shape::Text(t) = &cs.shape {
                    if t.galley.text().contains("Piano roll") {
                        return Some(egui::Rect::from_min_size(t.pos, egui::vec2(4.0, 12.0)));
                    }
                }
                None
            });
            let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
            drop(prims);
            out.textures_delta.clear();
            hit.expect("Piano roll button").center()
        };
        frame(&mut app, &ctx, vec![moved(btn), btn_btn(btn, true)]);
        frame(&mut app, &ctx, vec![btn_btn(btn, false)]);
        assert!(app.piano_roll.is_some(), "roll opens");
        assert_eq!(
            app.piano_roll.as_ref().unwrap().edit_asset,
            Some(m as u32),
            "roll edits the MIDI asset"
        );
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }

        // Delete the asset the roll is editing WHILE it is open.
        app.remove_asset_at(m);
        for _ in 0..3 {
            frame(&mut app, &ctx, vec![]);
        }
        assert_eq!(app.assets.len(), 3, "the deleted asset is gone");
        for tr in app.arr.tracks.iter() {
            for l in tr.lanes.iter() {
                for c in l.clips.iter() {
                    assert!(
                        (c.asset as usize) < app.assets.len(),
                        "clip holds a dead id"
                    );
                }
            }
        }
        if let Some(r) = app.piano_roll.as_ref() {
            assert!(
                r.edit_asset
                    .is_none_or(|id| (id as usize) < app.assets.len()),
                "roll kept a stale edit_asset id after delete: {:?}",
                r.edit_asset
            );
        }
        if let Some(r) = app.piano_roll.as_mut() {
            let id = r.edit_asset.unwrap_or(0);
            let mut p = r.pat.clone();
            app.update_piano_roll_asset(&mut p, id);
        }
        while !app.assets.is_empty() {
            let k = app.assets.len() - 1;
            app.remove_asset_at(k);
            frame(&mut app, &ctx, vec![]);
        }
        assert_eq!(app.assets.len(), 0);
    }

    /// v10.2 pin — the v0.10 "plugin opens its GUI when bound" standard,
    /// wired end to end through the App (not just the editor unit):
    /// binding a VST2 stem must instantiate on the shared GUI thread,
    /// open the plugin's own editor window, and keep the slider window
    /// CLOSED (editor-first). With no plugins/display installed the VST2
    /// half skips; a script synth must never open an editor at all.
    #[test]
    fn v102_bind_synth_opens_plugin_editor() {
        let mut app = headless_app("bindgui");
        // Script synth: never an editor, never the slider fallback.
        app.set_track_synth_stem(0, "piano");
        assert!(
            app.vst_editors.is_empty(),
            "a script synth opened an editor"
        );
        assert_ne!(app.synth_win, Some(0), "slider window for a script synth");
        assert!(app.track_synth_inst[0].is_some(), "script instance bound");

        let plugin = std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(".vst/wolf-shaper-vst.so");
        if std::env::var_os("SLOOPER_VST_SMOKE").is_none() || !plugin.exists() {
            eprintln!("skip: SLOOPER_VST_SMOKE unset or no wolf-shaper");
            return;
        }
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skip: no X display");
            return;
        }
        let stem = format!("vst2:{}", plugin.display());
        app.set_track_synth_stem(0, &stem);
        // Editor-first: bound, editor open, NO slider window.
        assert!(
            app.track_synth_inst[0].is_some(),
            "plugin instance bound: {}",
            app.status
        );
        assert!(
            app.vst_editors.contains_key(&0),
            "editor did not open at bind time: status {:?}",
            app.status
        );
        assert_ne!(
            app.synth_win,
            Some(0),
            "slider window opened beside the editor"
        );
        // The window must still be alive after frames (watchdog keeps it).
        let ctx = egui::Context::default();
        for _ in 0..6 {
            frame(&mut app, &ctx, vec![]);
        }
        assert!(
            app.vst_editors.contains_key(&0),
            "editor vanished after bind"
        );
        // Unbinding retires the editor cleanly.
        app.set_track_synth_stem(0, "");
        assert!(app.vst_editors.is_empty(), "editor left open after unbind");
    }

    // ================= v17: SYNTHS picker window ======================
    // (requirement list: scratch slooper-v17/REQUIREMENTS.md — a window,
    // not a ComboBox: shows the synth folders, lists every synth kind-
    // tagged ([rust] script vs [vst2]/[vst3] plugin), BORDER on active.)

    /// Paint one frame; return (galley, rect) for every synth-list row
    /// and every ACTIVE-marker border rect.
    fn synth_window_shapes(
        app: &mut App,
        ctx: &egui::Context,
    ) -> (Vec<(String, egui::Rect)>, Vec<egui::Rect>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let mut texts = Vec::new();
        let mut borders = Vec::new();
        for cs in &out.shapes {
            match &cs.shape {
                egui::epaint::Shape::Text(t) => {
                    let s = t.galley.text().to_string();
                    if s.contains("[rust]") || s.contains("[vst3]") || s.contains("[vst2]") {
                        texts.push((s, egui::Rect::from_min_size(t.pos, t.galley.rect.size())));
                    }
                }
                egui::epaint::Shape::Rect(r)
                    if (r.stroke.width - 1.6).abs() < 0.01 =>
                {
                    // The ACTIVE border color, possibly dimmed by the
                    // window's fade-in animation (shapes are painted at
                    // the layer's animated alpha) — compare un-premultiplied.
                    let p = r.stroke.color.to_array();
                    if p[3] > 10
                        && (p[0] as u32 * 255 / p[3] as u32).abs_diff(120) <= 12
                        && (p[1] as u32 * 255 / p[3] as u32).abs_diff(190) <= 14
                        && (p[2] as u32 * 255 / p[3] as u32).abs_diff(255) <= 14
                    {
                        borders.push(r.rect);
                    }
                }
                _ => {}
            }
        }
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        (texts, borders)
    }

    fn fake_vsti() -> crate::vsthost::VstEntry {
        crate::vsthost::VstEntry {
            path: std::path::PathBuf::from("/nonexistent/fake-synth.vst3"),
            kind: crate::vsthost::VstKind::V3,
            name: "FakeSynth".into(),
            vendor: "nosoft".into(),
            instrument: true,
            error: None,
        }
    }

    /// v17: the ♪ chip on a mixer strip opens the SYNTHS window (the old
    /// ComboBox is gone).
    #[test]
    fn v17_synth_chip_opens_picker_window() {
        let mut app = headless_app("synthpick");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let m = label_center(&mut app, &ctx, "\u{2026} more").expect("more button on the strip");
        frame(&mut app, &ctx, vec![moved(m)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(m, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(m, egui::PointerButton::Primary, false)],
        );
        let c = label_center(&mut app, &ctx, "\u{266a} piano").expect("synth chip in the popup");
        frame(&mut app, &ctx, vec![moved(c)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(c, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(c, egui::PointerButton::Primary, false)],
        );
        assert_eq!(
            app.synth_pick,
            Some(0),
            "the chip opens the SYNTHS window"
        );
    }

    /// v17: the window lists script synths as `[rust]` and scanned plugin
    /// instruments as `[vst3]`, and draws exactly one border — around the
    /// ACTIVE synth (default: piano).
    #[test]
    fn v17_synth_window_lists_kinds_and_borders_active() {
        let mut app = headless_app("synthwin");
        let ctx = egui::Context::default();
        app.vst_scanned = true; // no live scan in this pin
        app.vst_scan = vec![fake_vsti()];
        app.synth_pick = Some(0);
        frame(&mut app, &ctx, vec![]);
        let (texts, borders) = synth_window_shapes(&mut app, &ctx);
        let has = |needle: &str| texts.iter().any(|(s, _)| s.contains(needle));
        assert!(has("[rust]  piano"), "default synth listed as rust script");
        assert!(has("sampler"), "script synth listed");
        assert!(has("[vst3]  FakeSynth"), "plugin synth listed kind-tagged");
        assert_eq!(borders.len(), 1, "exactly one ACTIVE border");
        let piano = texts
            .iter()
            .find(|(s, _)| s.contains("piano"))
            .map(|(_, r)| *r)
            .expect("piano row");
        assert!(
            borders[0].contains(piano.center()),
            "border marks the ACTIVE (default piano) row"
        );
    }

    /// v17: clicking a plugin row BINDS it to the track, and the border
    /// moves with it (active marker follows selection). The fake path
    /// fails to load — the binding itself must still persist (the picker
    /// never hides a broken-but-chosen synth; the status line reports it).
    #[test]
    fn v17_synth_window_click_binds_and_moves_border() {
        let mut app = headless_app("synthbind");
        let ctx = egui::Context::default();
        app.vst_scanned = true;
        app.vst_scan = vec![fake_vsti()];
        app.synth_pick = Some(0);
        frame(&mut app, &ctx, vec![]);
        let d = galley_at(&mut app, &ctx, "FakeSynth")
            .map(|r| r.center())
            .expect("FakeSynth row painted");
        frame(&mut app, &ctx, vec![moved(d)]);
        frame(
            &mut app,
            &ctx,
            vec![btn(d, egui::PointerButton::Primary, true)],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(d, egui::PointerButton::Primary, false)],
        );
        assert_eq!(
            app.track_synth[0].as_deref(),
            Some("vst3:/nonexistent/fake-synth.vst3"),
            "row click binds the plugin synth (status: {})",
            app.status
        );
        let (texts, borders) = synth_window_shapes(&mut app, &ctx);
        assert_eq!(borders.len(), 1, "still exactly one ACTIVE border");
        let row = texts
            .iter()
            .find(|(s, _)| s.contains("FakeSynth"))
            .map(|(_, r)| *r)
            .expect("FakeSynth row");
        assert!(
            borders[0].contains(row.center()),
            "border moved to the newly-active plugin row"
        );
    }

    // ================= v0.13 acceptance-criteria pins =====================
    // (requirements list: scratch slooper-v13/REQUIREMENTS.md)

    /// OS file manager drops arrive as `dropped_files` on the raw input
    /// (winit fills `path`; `bytes` only for unsaved buffers).
    #[derive(Debug)]
    struct DroppedWav(std::path::PathBuf);
    impl egui::DroppedFile for DroppedWav {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|e| e.to_string())
        }
    }

    fn frame_files(
        app: &mut App,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        files: Vec<std::path::PathBuf>,
    ) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            events,
            dropped_files: files
                .into_iter()
                .map(|p| std::sync::Arc::new(DroppedWav(p)) as egui::DroppedFileHandle)
                .collect(),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
    }

    /// Real window drag: grab a window's title bar and park it clear of
    /// the rack panel so rack rows and drop zone are both clickable.
    fn park_window(app: &mut App, ctx: &egui::Context, title_needle: &str, to: egui::Vec2) {
        let title = galley_at(app, ctx, title_needle).expect("window title visible");
        let from = title.center();
        frame(app, ctx, vec![moved(from)]);
        frame(
            app,
            ctx,
            vec![moved(from), btn(from, egui::PointerButton::Primary, true)],
        );
        let mid = from + to * 0.5;
        frame(app, ctx, vec![moved(mid)]);
        let dst = from + to;
        frame(app, ctx, vec![moved(dst)]);
        frame(
            app,
            ctx,
            vec![moved(dst), btn(dst, egui::PointerButton::Primary, false)],
        );
        frame(app, ctx, vec![]);
    }

    /// AC3.1: a WAV dragged in from the FILE MANAGER switches the live
    /// sampler patch — decoded, registered, the engine's host table updated,
    /// asset param moved — with no restart. This is the exact user report:
    /// before v0.13 only in-app drags were handled, and `ensure_asset`'s
    /// audio branch never pushed the rack table, so the new sample stayed
    /// invisible to the audio thread until an app restart.
    #[test]
    fn v13_ui_os_file_drop_swaps_sampler_patch_live() {
        let mut app = headless_app("v13osdrop");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        app.set_track_synth_stem(0, "sampler");
        app.synth_win = Some(0);
        frame(&mut app, &ctx, vec![]);
        park_window(
            &mut app,
            &ctx,
            "sampler \u{b7} track 1",
            egui::vec2(560.0, 280.0),
        );

        // Give the sampler an initial patch: click kick in the rack
        // (rack rows show the file STEM — "kick", not "kick.wav").
        let kick = galley_at(&mut app, &ctx, "kick").expect("kick rack row");
        frame(&mut app, &ctx, vec![moved(kick.center())]);
        frame(
            &mut app,
            &ctx,
            vec![
                moved(kick.center()),
                btn(kick.center(), egui::PointerButton::Primary, true),
            ],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(kick.center(), egui::PointerButton::Primary, false)],
        );
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.sel_asset, Some(0), "kick.wav clicked: {}", app.status);

        // A brand-new wav on disk, outside every rack folder.
        let dir = std::env::temp_dir()
            .join("sp-uity-v13osdrop")
            .join("incoming");
        std::fs::create_dir_all(&dir).unwrap();
        let zap = dir.join("zap.wav");
        let data: Vec<f32> = (0..24_000).map(|i| (i as f32 * 0.3).sin() * 0.4).collect();
        crate::audio_io::export(&zap, 48000, &data, crate::audio_io::ExportFormat::Wav).unwrap();

        // The OS drops it onto the app while the sampler window is open.
        frame_files(&mut app, &ctx, vec![], vec![zap]);

        // AC3.1 (UI half): asset registered from the dropped file, live
        // patch switched, editor reflects the new sample. The audio-thread
        // half — that a SetProject asset swap replaces the SOUND inside a
        // running sampler — is pinned end-to-end by e2e
        // `e2e_v13_sampler_patch_swap_live`; headless null-audio drains no
        // command queue, so nothing here could observe it.
        assert_eq!(
            app.assets.len(),
            2,
            "dropped wav became a rack asset: {}",
            app.status
        );
        assert_eq!(app.assets[1].name, "zap");
        assert_eq!(
            app.assets[1].data.len(),
            24_000,
            "dropped file decoded at 48 k"
        );
        let inst = match app.track_synth_inst[0].as_deref() {
            Some(SynthHandle::Script(i)) => i.clone(),
            _ => panic!("sampler instance gone"),
        };
        assert_eq!(
            inst.values()[0] as usize,
            1,
            "live patch switched to the dropped file"
        );
        assert!(
            app.status.contains("zap"),
            "status names the dropped patch: {}",
            app.status
        );
    }

    /// AC3.2 regression pin: the IN-APP drag (rack row → sampler drop zone)
    /// still switches the patch, real press/move/release through frame().
    #[test]
    fn v13_ui_internal_drag_onto_sampler_drop_zone() {
        let mut app = headless_app("v13idrag");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        app.set_track_synth_stem(0, "sampler");
        app.synth_win = Some(0);
        frame(&mut app, &ctx, vec![]);
        park_window(
            &mut app,
            &ctx,
            "sampler \u{b7} track 1",
            egui::vec2(560.0, 280.0),
        );

        // Rack folder (the one headless_app configured) gains a second
        // wav; press its "scan" button to pick it up.
        let samples = std::env::temp_dir()
            .join("sp-uity-v13idrag")
            .join("samples");
        std::fs::create_dir_all(&samples).unwrap();
        let snare = samples.join("snare.wav");
        if !snare.exists() {
            let d: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.11).sin() * 0.2).collect();
            crate::audio_io::export(&snare, 48000, &d, crate::audio_io::ExportFormat::Wav).unwrap();
        }
        let scan = galley_at(&mut app, &ctx, "scan").expect("scan button");
        frame(&mut app, &ctx, vec![moved(scan.center())]);
        frame(
            &mut app,
            &ctx,
            vec![
                moved(scan.center()),
                btn(scan.center(), egui::PointerButton::Primary, true),
            ],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(scan.center(), egui::PointerButton::Primary, false)],
        );
        frame(&mut app, &ctx, vec![]);
        assert!(
            app.rack[0].files.iter().any(|r| r.name == "snare"),
            "scan found snare"
        );

        // Click kick first (it becomes asset 0 — the patch param's
        // default — so the drag below flips 0 -> 1, unambiguous).
        let kick = galley_at(&mut app, &ctx, "kick").expect("kick rack row");
        frame(&mut app, &ctx, vec![moved(kick.center())]);
        frame(
            &mut app,
            &ctx,
            vec![
                moved(kick.center()),
                btn(kick.center(), egui::PointerButton::Primary, true),
            ],
        );
        frame(
            &mut app,
            &ctx,
            vec![btn(kick.center(), egui::PointerButton::Primary, false)],
        );
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.sel_asset, Some(0), "kick.wav row clicked");

        // Real drag: press snare's rack row, exceed the dnd threshold,
        // hover the sampler drop zone, release.
        let zone = galley_at(&mut app, &ctx, "drop zone").expect("drop zone visible");
        let snare_row = galley_at(&mut app, &ctx, "snare")
            .expect("snare row")
            .center();
        frame(&mut app, &ctx, vec![moved(snare_row)]);
        frame(
            &mut app,
            &ctx,
            vec![
                moved(snare_row),
                btn(snare_row, egui::PointerButton::Primary, true),
            ],
        );
        frame(
            &mut app,
            &ctx,
            vec![moved(snare_row + egui::vec2(90.0, 0.0))],
        );
        assert!(
            egui::DragAndDrop::has_payload_of_type::<usize>(&ctx),
            "dragging a rack row must arm the asset drag payload"
        );
        frame(&mut app, &ctx, vec![moved(zone.center())]);
        frame(
            &mut app,
            &ctx,
            vec![
                moved(zone.center()),
                btn(zone.center(), egui::PointerButton::Primary, true),
            ],
        );
        frame(
            &mut app,
            &ctx,
            vec![
                moved(zone.center()),
                btn(zone.center(), egui::PointerButton::Primary, false),
            ],
        );
        frame(&mut app, &ctx, vec![]);

        let inst = match app.track_synth_inst[0].as_deref() {
            Some(SynthHandle::Script(i)) => i.clone(),
            _ => panic!("no sampler instance"),
        };
        assert_eq!(
                inst.values()[0] as usize,
                1,
                "dragging snare.wav onto the drop zone must set the sampler's asset param (status {:?})",
                app.status
            );
    }

    // ================= v18 acceptance pins: core-workflow sweep ============
    // (requirements list: scratch slooper-v18/REQUIREMENTS.md)

    fn n_clips(a: &App) -> usize {
        a.arr
            .tracks
            .iter()
            .map(|t| t.lanes.iter().map(|l| l.clips.len()).sum::<usize>())
            .sum()
    }

    /// Every placement in the arrangement as (track, lane, index, clip).
    fn clip_ids(a: &App) -> Vec<(usize, usize, usize, crate::arrange::Clip)> {
        let mut v = Vec::new();
        for (t, tr) in a.arr.tracks.iter().enumerate() {
            for (k, ln) in tr.lanes.iter().enumerate() {
                for (c, cl) in ln.clips.iter().enumerate() {
                    v.push((t, k, c, *cl));
                }
            }
        }
        v
    }

    fn write_test_wav(dir: &std::path::Path, name: &str, n: usize, f: f32) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        let data: Vec<f32> = (0..n).map(|i| (i as f32 * f).sin() * 0.4).collect();
        crate::audio_io::export(&p, 48000, &data, crate::audio_io::ExportFormat::Wav).unwrap();
        p
    }

    /// Right-click press + release at p (separate frames, pointer armed).
    fn rmb(app: &mut App, ctx: &egui::Context, p: egui::Pos2) {
        frame(
            app,
            ctx,
            vec![moved(p), btn(p, egui::PointerButton::Secondary, true)],
        );
        frame(
            app,
            ctx,
            vec![moved(p), btn(p, egui::PointerButton::Secondary, false)],
        );
    }

    /// Left-click the painted galley whose text equals `needle`.
    fn click_label(app: &mut App, ctx: &egui::Context, needle: &str) {
        let r = galley_at(app, ctx, needle).unwrap_or_else(|| {
            panic!("galley {needle:?} not painted (status {:?})", app.status)
        });
        let p = r.center();
        frame(app, ctx, vec![moved(p)]);
        frame(
            app,
            ctx,
            vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
        );
        frame(app, ctx, vec![btn(p, egui::PointerButton::Primary, false)]);
    }

    /// All galley centers equal to `needle`, in paint order.
    fn galleys_all(app: &mut App, ctx: &egui::Context, needle: &str) -> Vec<egui::Pos2> {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 853.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.ui_root(ui));
        let mut hits = Vec::new();
        for cs in &out.shapes {
            if let egui::epaint::Shape::Text(t) = &cs.shape {
                if t.galley.text() == needle {
                    hits.push(t.pos + t.galley.rect.center().to_vec2());
                }
            }
        }
        let prims = ctx.tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        drop(prims);
        out.textures_delta.clear();
        hits
    }

    // ---- R1: OS file drag & drop into the MAIN window ---------------------

    /// R1a: a WAV dropped from the FILE MANAGER onto a track lane decodes,
    /// registers, and lands as a selected clip at the drop position. Before
    /// v18 OS drops were consumed ONLY by the sampler drop zone, so dropping
    /// a sample anywhere else in the app silently did nothing.
    #[test]
    fn v18_osdrop_wav_on_grid_places_clip() {
        let mut app = headless_app("v18dropg");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let wav = write_test_wav(
            &std::env::temp_dir().join("sp-uity-v18dropg").join("in"),
            "zap.wav",
            24_000,
            0.2,
        );
        let n0 = app.assets.len();
        let p = egui::pos2(500.0, 240.0); // lane strip of track 2 (DnD-pin geometry)
        frame_files(&mut app, &ctx, vec![moved(p)], vec![wav]);
        frame(&mut app, &ctx, vec![moved(p)]);
        assert_eq!(app.assets.len(), n0 + 1, "dropped wav imported: {}", app.status);
        assert_eq!(n_clips(&app), 1, "drop placed a clip: {}", app.status);
        assert!(app.status.contains("track 2"), "status names the track: {}", app.status);
        assert!(!app.sel_clips.is_empty(), "placed clip is selected");
        assert_eq!(app.sel_asset, Some(app.assets.len() - 1), "and rack-selected");
    }

    /// R1b: a drop anywhere else (here: the rack panel) imports the asset —
    /// the sample is in the project, no restart, no folder dance.
    #[test]
    fn v18_osdrop_on_rack_imports_asset() {
        let mut app = headless_app("v18droprack");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let wav = write_test_wav(
            &std::env::temp_dir().join("sp-uity-v18droprack").join("in"),
            "blip.wav",
            12_000,
            0.1,
        );
        let n0 = app.assets.len();
        frame_files(&mut app, &ctx, vec![moved(egui::pos2(120.0, 400.0))], vec![wav]);
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.assets.len(), n0 + 1, "rack drop imported: {}", app.status);
        assert_eq!(n_clips(&app), 0, "rack drop places no clip");
        assert_eq!(app.sel_asset, Some(app.assets.len() - 1), "imported asset selected");
    }

    /// R1c: a .mid dropped on a lane places a MIDI clip.
    #[test]
    fn v18_osdrop_midi_file_places_clip() {
        let mut app = headless_app("v18dropmid");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let dir = std::env::temp_dir().join("sp-uity-v18dropmid").join("in");
        std::fs::create_dir_all(&dir).unwrap();
        let mid = dir.join("line.mid");
        let mut pat = crate::midipat::Pattern::new(120);
        pat.len_ticks = 4 * 480;
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note: 60,
            vel: 100,
            press: 0,
            start: 0,
            len: 240,
        });
        pat.save_mid(&mid).unwrap();
        frame_files(&mut app, &ctx, vec![moved(egui::pos2(500.0, 240.0))], vec![mid]);
        frame(&mut app, &ctx, vec![]);
        let ids = clip_ids(&app);
        assert_eq!(ids.len(), 1, "midi drop placed one clip: {}", app.status);
        assert!(
            app.assets[ids[0].3.asset as usize].pattern.is_some(),
            "placed clip carries a pattern"
        );
    }

    /// R1d: an unsupported file is refused with a clear status, no panic.
    #[test]
    fn v18_osdrop_unsupported_file_reports() {
        let mut app = headless_app("v18dropbad");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let dir = std::env::temp_dir().join("sp-uity-v18dropbad").join("in");
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("notes.txt");
        std::fs::write(&txt, "not audio").unwrap();
        let n0 = app.assets.len();
        frame_files(&mut app, &ctx, vec![moved(egui::pos2(500.0, 240.0))], vec![txt]);
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.assets.len(), n0, "nothing imported");
        assert_eq!(n_clips(&app), 0, "no clip");
        assert!(
            app.status.contains("unsupported"),
            "clear refusal message: {:?}",
            app.status
        );
    }

    /// R1e: a DIRECTORY dropped on the rack becomes a new rack folder
    /// (its files visible immediately) — the "+ folder" dialog without
    /// the dialog.
    #[test]
    fn v18_osdrop_folder_adds_rack_folder() {
        let mut app = headless_app("v18dropdir");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let dir = std::env::temp_dir().join("sp-uity-v18dropdir").join("newpack");
        write_test_wav(&dir, "hat.wav", 6_000, 0.4);
        let n0 = app.rack.len();
        frame_files(&mut app, &ctx, vec![moved(egui::pos2(120.0, 400.0))], vec![dir]);
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.rack.len(), n0 + 1, "folder added to the rack: {}", app.status);
        let last = app.rack.last().unwrap();
        assert_eq!(last.files.len(), 1, "its files scanned");
    }

    // ---- R2: deleting clips -----------------------------------------------

    /// R2: Delete/Backspace keys remove exactly the selected clips; an
    /// automation clip deletes too (asset survives); repeat presses and a
    /// stale selection after remove_asset_at must not panic.
    #[test]
    fn v18_delete_key_removes_clips_and_survives_abuse() {
        let mut app = headless_app("v18delkey");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        app.arr.insert(0, 0, Clip::new(a, 0));
        app.arr.insert(0, 0, Clip::new(a, 96_000));
        app.create_flat_auto_asset(1, crate::automation::TARGET_TRACK_VOL, 0.5);
        let before = n_clips(&app);
        assert_eq!(before, 3, "2 audio clips + 1 automation clip");
        app.select_one((0, 0, 1));
        frame_keys(&mut app, &ctx, vec![key_ev(egui::Key::Delete, Default::default())]);
        assert_eq!(n_clips(&app), before - 1, "Delete key removed the clip: {}", app.status);
        assert_eq!(
            clip_ids(&app).iter().filter(|(t, _, _, _)| *t == 0).count(),
            1,
            "the RIGHT clip went (index 1, not the first)"
        );
        let (at, ak, ac, _c) = clip_ids(&app)
            .into_iter()
            .find(|(_, _, _, c)| {
                app.assets.get(c.asset as usize).is_some_and(|x| x.is_auto())
            })
            .expect("automation clip placed");
        let n_assets = app.assets.len();
        app.select_one((at, ak, ac));
        frame_keys(&mut app, &ctx, vec![key_ev(egui::Key::Backspace, Default::default())]);
        assert_eq!(n_clips(&app), before - 2, "Backspace deleted the automation clip");
        assert_eq!(app.assets.len(), n_assets, "automation ASSET stays in the rack");
        // A stray key with no selection must no-op, not delete "whatever".
        assert!(app.sel_clips.is_empty());
        frame_keys(&mut app, &ctx, vec![key_ev(egui::Key::Delete, Default::default())]);
        assert_eq!(n_clips(&app), before - 2);
        // remove_asset_at cleared the selection — later Delete is a no-op.
        app.select_one((0, 0, 0));
        app.remove_asset_at(a as usize);
        frame_keys(&mut app, &ctx, vec![key_ev(egui::Key::Delete, Default::default())]);
        assert!(app.sel_clips.is_empty());
    }

    // ---- R3: automation creation by right-click ----------------------------

    /// R3a: right-clicking the BPM box opens the mini menu; "Create
    /// automation asset" produces an AUTO asset + clip at the playhead on
    /// the owning track, and a second creation gets its own asset (auto-2,
    /// never overwritten).
    #[test]
    fn v18_rmb_tempo_creates_automation_asset_twice() {
        let mut app = headless_app("v18rmbtempo");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let n0 = app.assets.len();
        let p = app.bpm_rect.expect("bpm rect painted").center();
        rmb(&mut app, &ctx, p);
        click_label(&mut app, &ctx, "Create automation asset");
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.assets.len(), n0 + 1, "AUTO asset created: {}", app.status);
        assert!(app.assets[n0].is_auto(), "kind = automation");
        let ids = clip_ids(&app);
        assert_eq!(ids.len(), 1, "automation clip placed: {}", app.status);
        assert_eq!(ids[0].0, 0, "placed on the selected track");
        assert_eq!(app.assets[n0].name, "auto-1");
        // Second creation: its own asset, never overwrite the first. The
        // rect is re-read because the top bar's layout belongs to the app
        // (a v18 pin — it used to JITTER with status text; a real user
        // aims at the CURRENTLY painted box).
        let p = app.bpm_rect.expect("bpm rect still painted").center();
        rmb(&mut app, &ctx, p);
        click_label(&mut app, &ctx, "Create automation asset");
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.assets.len(), n0 + 2, "second asset: {}", app.status);
        assert_eq!(app.assets[n0 + 1].name, "auto-2", "auto-2 next to auto-1");
    }

    /// R3a-2: the same menu from a TRACK volume readout in the mixer binds
    /// the clip to that track's volume target.
    #[test]
    fn v18_rmb_track_vol_creates_auto_for_that_target() {
        let mut app = headless_app("v18rmbvol");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        // Paint order in the mixer: master strip first, then track 1.
        let readout = format!("{:>5}", db_num(1.0));
        let hits = galleys_all(&mut app, &ctx, &readout);
        let p = *hits
            .get(1)
            .unwrap_or_else(|| panic!("expected master + track-1 dB readouts, got {hits:?}"));
        let n0 = app.assets.len();
        rmb(&mut app, &ctx, p);
        click_label(&mut app, &ctx, "Create automation asset");
        frame(&mut app, &ctx, vec![]);
        assert_eq!(app.assets.len(), n0 + 1, "track-vol menu creates: {}", app.status);
        let auto = app.assets[n0].automation.as_ref().expect("automation attached");
        assert_eq!(auto.target, crate::automation::TARGET_TRACK_VOL);
    }

    /// R3b: in the automation canvas, clicking empty space ADDS a node and
    /// right-clicking a node deletes it (the tooltip promises exactly this).
    #[test]
    fn v18_auto_canvas_click_adds_rmb_deletes_node() {
        let mut app = headless_app("v18autocanvas");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        app.open_auto_editor(None);
        frame(&mut app, &ctx, vec![]);
        let n0 = app.auto_edit.as_ref().unwrap().nodes.len();
        // Find the curve canvas the proven v15 way: a click inside adds
        // exactly one node (same scan, same coordinates).
        let mut cp = None;
        'outer: for y in (380i32..=560).step_by(6) {
            for x in (260i32..=700).step_by(40) {
                let p = egui::pos2(x as f32, y as f32);
                frame(
                    &mut app,
                    &ctx,
                    vec![moved(p), btn(p, egui::PointerButton::Primary, true)],
                );
                frame(
                    &mut app,
                    &ctx,
                    vec![btn(p, egui::PointerButton::Primary, false)],
                );
                let n = app.auto_edit.as_ref().unwrap().nodes.len();
                if n == n0 + 1 {
                    cp = Some(p);
                    break 'outer;
                }
                assert!(n == n0, "click never deletes nodes");
            }
        }
        let p = cp.expect("click empty canvas adds a node");
        // Right-click the NEW node (it sits exactly under p): it vanishes.
        rmb(&mut app, &ctx, p);
        frame(&mut app, &ctx, vec![]);
        assert_eq!(
            app.auto_edit
                .as_ref()
                .expect("editor stays open")
                .nodes
                .len(),
            n0,
            "right-click deleted the node"
        );
    }

    // ---- R4: clipboard ------------------------------------------------------

    /// R4a: Ctrl+X is a real cut (copy + delete), Ctrl+V brings the clip
    /// back — driven with REAL key events through the shortcut pipeline.
    #[test]
    fn v18_ctrlx_cut_key_then_paste() {
        let mut app = headless_app("v18cutkey");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        app.select_one((0, 0, 0));
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::X, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(n_clips(&app), 0, "Ctrl+X deleted the clip");
        assert_eq!(app.clipboard.as_ref().map(|c| c.items.len()), Some(1), "and copied it");
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::V, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(n_clips(&app), 1, "Ctrl+V pasted it back: {}", app.status);
    }

    /// R4b: copy -> remove_asset_at -> paste must not panic and must not
    /// place a clip pointing at the dead id (the clipboard entries drop).
    #[test]
    fn v18_copy_delete_asset_paste_is_safe() {
        let mut app = headless_app("v18cdead");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        app.select_one((0, 0, 0));
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::C, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(app.clipboard.as_ref().map(|c| c.items.len()), Some(1));
        app.remove_asset_at(a as usize);
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::V, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(n_clips(&app), 0, "no ghost clip from the dead asset");
    }

    /// R4c: after a REAL click on track 3's grid, Ctrl+V lands on track 3.
    #[test]
    fn v18_paste_lands_on_last_clicked_track() {
        let mut app = headless_app("v18pastetgt");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        app.arr.insert(0, 0, Clip::new(a, 48_000));
        app.select_one((0, 0, 0));
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::C, egui::Modifiers::COMMAND),
            ],
        );
        // Real left-click on track 3's EMPTY grid area (x far right of the
        // 3 s clip at 160 px/s). Settle a frame first so the props-strip
        // shift is baked into grid_anchor before geometry is sampled.
        frame(&mut app, &ctx, vec![]);
        let top3 = app.grid_anchor - app.vscroll + 2.0 * (app.track_h(2) + TRACK_SEP_HIT);
        let p = egui::pos2(1100.0, top3 + app.track_h(2) * 0.5);
        frame(&mut app, &ctx, vec![moved(p)]);
        frame(&mut app, &ctx, vec![moved(p), btn(p, egui::PointerButton::Primary, true)]);
        frame(&mut app, &ctx, vec![btn(p, egui::PointerButton::Primary, false)]);
        assert_eq!(app.last_click_track, Some(2), "grid click remembered track 3");
        frame_keys(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::V, egui::Modifiers::COMMAND),
            ],
        );
        let ids = clip_ids(&app);
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().any(|(t, _, _, _)| *t == 2), "paste landed on track 3: {ids:?}");
    }

    // ---- R5/R6: drag gestures vs. destructive edits -------------------------

    /// R5a: dragging a selected clip DOWN moves it to the track's child
    /// lane, the selection follows, and a following plain click still works
    /// (no stuck gesture).
    #[test]
    fn v18_vertical_clip_drag_moves_lane_and_reclick_works() {
        let mut app = headless_app("v18vertdrag");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        // Two overlapping clips on track 2: the second opens a child lane.
        app.arr.insert(1, 0, Clip::new(a, 0));
        let lane = app.arr.place_auto(1, Clip::new(a, 0), app.asset_len(a));
        assert_eq!(lane, 1, "overlap opened the child lane");
        app.select_one((1, 0, 0));
        // Settle one frame so the clip-props strip exists BEFORE geometry
        // is sampled — the strip shifts the whole grid down.
        frame(&mut app, &ctx, vec![]);
        let top1 = app.grid_anchor - app.vscroll + app.track_h(0) + TRACK_SEP_HIT;
        let lane_h = app.track_h(1) / 2.0;
        let grab = egui::pos2(500.0, top1 + lane_h * 0.5);
        let drop = egui::pos2(500.0, top1 + lane_h * 1.5);
        frame(&mut app, &ctx, vec![moved(grab)]);
        frame(&mut app, &ctx, vec![moved(grab), btn(grab, egui::PointerButton::Primary, true)]);
        let mid = egui::pos2(500.0, (grab.y + drop.y) * 0.5);
        frame(&mut app, &ctx, vec![moved(mid)]);
        frame(&mut app, &ctx, vec![moved(drop)]);
        frame(&mut app, &ctx, vec![moved(drop), btn(drop, egui::PointerButton::Primary, false)]);
        frame(&mut app, &ctx, vec![]);
        let ids = clip_ids(&app);
        assert_eq!(ids.iter().filter(|(t, k, _, _)| *t == 1 && *k == 1).count(), 2, "both clips now share the child lane: {ids:?}");
        assert!(app.sel_clips.iter().all(|(t, k, _)| *t == 1 && *k == 1), "selection followed the lane");
        // Plain click afterwards still selects (the reaper must be
        // clean) — click the CHILD LANE the clips now live in; lane 0 is
        // empty after the drag, and clicking empty grid correctly clears.
        let click = egui::pos2(500.0, top1 + lane_h * 1.5);
        frame(&mut app, &ctx, vec![moved(click)]);
        frame(&mut app, &ctx, vec![moved(click), btn(click, egui::PointerButton::Primary, true)]);
        frame(&mut app, &ctx, vec![btn(click, egui::PointerButton::Primary, false)]);
        assert_eq!(app.sel_clips.len(), 1, "click after drag selects again: {:?}", app.sel_clips);
    }

    /// R5b: cutting the selected clip WHILE an edge-resize drag is armed
    /// (button still down) must not panic on the next pointer move.
    #[test]
    fn v18_cut_during_armed_edge_resize_does_not_panic() {
        let mut app = headless_app("v18cutresize");
        let ctx = egui::Context::default();
        frame_keys(&mut app, &ctx, vec![]);
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        app.arr.insert(0, 0, Clip::new(a, 0));
        let top0 = app.grid_anchor - app.vscroll;
        // Right edge of the 3 s clip at the default zoom.
        let edge_x = 270.0 + 3.0 * app.px_per_sec;
        let grab = egui::pos2(edge_x - 2.0, top0 + app.track_h(0) * 0.5);
        frame(&mut app, &ctx, vec![moved(grab)]);
        frame(&mut app, &ctx, vec![moved(grab), btn(grab, egui::PointerButton::Primary, true)]);
        let moved_to = egui::pos2(edge_x + 14.0, top0 + app.track_h(0) * 0.5);
        frame(&mut app, &ctx, vec![moved(moved_to)]);
        assert!(app.clip_resize.is_some(), "edge resize armed");
        // Ctrl+X while the button is still down: the clip dies under us.
        frame_keys(
            &mut app,
            &ctx,
            vec![
                moved(moved_to),
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key_ev(egui::Key::X, egui::Modifiers::COMMAND),
            ],
        );
        assert_eq!(n_clips(&app), 0, "cut happened");
        // Keep moving, then release: the dead gesture must not be touched.
        frame_keys(&mut app, &ctx, vec![moved(egui::pos2(edge_x + 40.0, moved_to.y))]);
        frame(&mut app, &ctx, vec![btn(egui::pos2(edge_x + 40.0, moved_to.y), egui::PointerButton::Primary, false)]);
        frame(&mut app, &ctx, vec![]);
    }

    /// R6: right-clicking the grid MID-marquee (primary still down) opens a
    /// menu without panic, and the arrangement survives the whole sequence.
    #[test]
    fn v18_rmb_mid_marquee_is_safe() {
        let mut app = headless_app("v18rmbmarquee");
        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![]);
        let kick = app.rack[0].files[0].path.clone();
        let a = app.ensure_asset(std::path::Path::new(&kick)).unwrap() as u32;
        app.arr.insert(0, 0, Clip::new(a, 0));
        let top0 = app.grid_anchor - app.vscroll;
        let p0 = egui::pos2(900.0, top0 + app.track_h(0) * 0.5);
        frame(&mut app, &ctx, vec![moved(p0)]);
        frame(&mut app, &ctx, vec![moved(p0), btn(p0, egui::PointerButton::Primary, true)]);
        let p1 = egui::pos2(980.0, top0 + app.track_h(0) * 0.5);
        frame(&mut app, &ctx, vec![moved(p1)]);
        // Secondary press + release while the primary is still held.
        frame(&mut app, &ctx, vec![btn(p1, egui::PointerButton::Secondary, true)]);
        frame(&mut app, &ctx, vec![btn(p1, egui::PointerButton::Secondary, false)]);
        frame(&mut app, &ctx, vec![moved(p1)]);
        // Release primary somewhere else; idle frames settle everything.
        frame(&mut app, &ctx, vec![btn(p0, egui::PointerButton::Primary, false)]);
        for _ in 0..4 {
            frame(&mut app, &ctx, vec![]);
        }
        assert_eq!(n_clips(&app), 1, "nothing was destroyed en route");
    }
}
