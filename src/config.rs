//! Persistent configuration: rebindable keyboard shortcuts, MIDI bindings,
//! per-track mixer levels, metronome/grid settings and folder locations.
//!
//! Stored as JSON at `$XDG_CONFIG_HOME/slooper/config.json` (falling back to
//! `~/.config/slooper/config.json`). A missing or corrupt file yields
//! defaults; saving is best-effort (UI keeps working if the write fails).
//! Unknown fields are ignored and removed fields (root loops, the old
//! metronome-quantize toggle) simply drop out, so configs from any older
//! build load cleanly.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::midi::{MidiAction, MidiBind};

/// One saved MIDI binding: an action plus its trigger (None = unbound).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiBinding {
    pub action: MidiAction,
    pub bind: Option<MidiBind>,
}

/// v29 R7: a MIDI control bound to one VALUE (mixer knob, plugin param)
/// for the CURRENT PROJECT — unlike action slots, these live in the
/// project file and in the session config, and are managed from the
/// automation manager's bindings rack (plugin setups are per project).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ValueBind {
    /// Automation-domain target id: "track.vol", "master.vol",
    /// "fx:0:2", "synth:1", ... (see crate::automation).
    pub target: String,
    /// Owner track (ignored for master.* targets).
    pub track: u32,
    /// The bound control's identity (message type + channel + number).
    pub bind: crate::midi::MidiBind,
    /// 0 = KNOB: raw lo..hi scales onto the target's range.
    /// 1 = SWITCH: a matching message toggles the (boolean) value.
    pub kind: u8,
    /// Raw minimum / maximum captured by the knob wizard
    /// (lo > hi = inverted travel, corrected on apply).
    pub lo: u8,
    pub hi: u8,
}

/// v29 R5: one drum-machine pad: a 1-note impulse instrument.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DrumPadSave {
    pub name: String,
    /// Asset FILE the pad fires (empty = silent until picked). Stored as
    /// a path so the project can relink it after reload.
    pub asset: String,
    /// Default paint velocity.
    pub vel: u8,
    pub muted: bool,
}

/// v30 R1: the drum machine PATTERN (the pattern builder): a velocity
/// grid of `steps` columns x pad rows. The pad SOUNDS (the kit) are NOT
/// here any more — every track's synth module owns its own kit, so each
/// synth can carry a different patch. `pads` remains only as a migration
/// source for v29 files.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DrumSave {
    /// v30: legacy (v29 global kit). Read once on load to seed the target
    /// track's kit; saved as an empty vec from now on.
    #[serde(default)]
    pub pads: Vec<DrumPadSave>,
    #[serde(default = "default_drum_steps")]
    pub steps: u32,
    /// v30 R1: grid subdivision in STEPS PER BEAT (4 = 16th notes, the
    /// historical default). Steps per bar = metronome beats * sub.
    #[serde(default = "default_drum_sub")]
    pub sub: u32,
    /// grid[pad][step]: 0 = off, 1..=127 = hit velocity.
    #[serde(default)]
    pub grid: Vec<Vec<u8>>,
    /// Arrangement track the pattern commits to.
    #[serde(default)]
    pub track: u32,
}

fn default_drum_steps() -> u32 {
    16
}
fn default_drum_sub() -> u32 {
    4
}

/// v30 R1: the stock drum kit (pattern builder default): 8 silent pads
/// with generic instrument names. An empty saved kit entry always
/// means THIS.
pub fn default_drum_kit() -> Vec<DrumPadSave> {
    ["Kick", "Snare", "Hat", "Open", "Clap", "Tom", "Cow", "Perc"]
        .iter()
        .map(|n| DrumPadSave {
            name: n.to_string(),
            asset: String::new(),
            vel: 100,
            muted: false,
        })
        .collect()
}

/// v51: one sampler key-map slot: a rack asset (stored as a PATH like the
/// drum kit, so projects can re-link after a reload) plus the root note
/// whose pitch the sample already has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SamplerSlotSave {
    pub asset: String,
    pub root: u8,
}

/// v51: one track's multi-sample keyboard map for the bundled sampler.
/// `mode`: 0 = hard cutoff — zones switch at the halfway point, a note
/// exactly halfway plays the LOWER sample; 1 = crossfade — the two
/// nearest samples are bent to the note and mixed by inverse distance.
/// An empty `slots` on a map that EXISTS means silent (NOT the legacy
/// single-asset params); no map entry at all keeps the params rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SamplerMapSave {
    #[serde(default)]
    pub slots: Vec<SamplerSlotSave>,
    #[serde(default)]
    pub mode: u8,
}

/// Axis calibration for one continuous (knob/slider) binding slot: the raw
/// controller values captured when the user swept the control to its minimum
/// and maximum. Values between are scaled onto the program's 0..=127 space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiAxisCal {
    pub slot: u16,
    pub lo: u8,
    pub hi: u8,
}

/// A track's VST insert: plugin path + kind, plus normalized slider values
/// in load order. If the plugin file is gone at next start, the track
/// simply comes up without the insert.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VstSave {
    pub path: String,
    /// "VST2" or "VST3".
    pub kind: String,
    /// Display name (UI only; the plugin file is authoritative).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub params: Vec<f32>,
}

/// One enabled effect in a track's saved chain (script stem + slider values,
/// in chain order). The stem is the key: dropping scripts are always safe.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackFxSave {
    pub stem: String,
    #[serde(default)]
    pub params: Vec<f32>,
}

/// One slot of a track's mixer chain (see `Config::track_chain`). Tagged so
/// a chain mixes effect scripts and hosted plugins in any order.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ChainSave {
    /// Bundled/user effect script by stem.
    Fx {
        stem: String,
        #[serde(default)]
        params: Vec<f32>,
    },
    /// In-process hosted plugin (VST2/VST3/CLAP).
    Vst {
        path: String,
        /// "VST2", "VST3" or "CLAP".
        kind: String,
        /// CLAP only: plugin id inside a multi-plugin file.
        #[serde(default)]
        plugin_id: Option<String>,
        /// Display name (UI only; the plugin file is authoritative).
        #[serde(default)]
        name: String,
        #[serde(default)]
        params: Vec<f32>,
        /// v22: plugin state chunk, base64 (VST3 + chunk-capable hosts).
        /// Older files simply lack the key and load with `None`.
        #[serde(default)]
        state: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// base64 (standard alphabet, padded) — hand-rolled so plugin state can ride
// in JSON/config without pulling a dependency (offline AUR builds).
// ---------------------------------------------------------------------------

const B64_T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(v: &[u8]) -> String {
    let mut s = String::with_capacity(v.len().div_ceil(3) * 4);
    for c in v.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        s.push(B64_T[(n >> 18 & 63) as usize] as char);
        s.push(B64_T[(n >> 12 & 63) as usize] as char);
        s.push(if c.len() > 1 {
            B64_T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        s.push(if c.len() > 2 {
            B64_T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    s
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for ch in s.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// v29: sub-folder display order for one rack folder (see
/// [`Config::rack_dir_order`]).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RackDirOrder {
    pub folder: String,
    pub dirs: Vec<String>,
}

/// v30 R6: geometry of the user-movable side windows (asset rack, MIDI
/// bindings, MIDI monitor). Each entry is `[x, y, w, h]` of the window's
/// outer rect; all-zero = never moved/resized (built-in default).
/// v30-batch-2 also carries the mixer (`mixer`) and the DOCK table:
/// `dock[i] = [docked (0/1), edge (0 left / 1 right / 2 bottom), order]`
/// for window i (0 rack, 1 bindings, 2 monitor, 3 mixer). Windows snap
/// into those edge slots and may be pulled back out to float freely.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SideWins {
    pub rack: [f32; 4],
    pub binds: [f32; 4],
    pub monitor: [f32; 4],
    pub mixer: [f32; 4],
    pub dock: [[i16; 3]; 4],
    /// v47 R1: ACTIVE-tab flag per primary window. Docked windows sharing
    /// (edge, order) form one TAB GROUP (a shared slot with a tab strip);
    /// exactly one member of a group keeps this flag set — the one whose
    /// body is painted. (Dynamic windows carry the same flag in `DynDock`.)
    #[serde(default)]
    pub prim_tab: [bool; 4],
}

/// v35: dock/rect state for ONE dynamic window class instance — the
/// plugin-class windows that join the dock system. `kind` is the DynKind
/// key: "params" | "drums" | "roll" | "auto" | "chain:<track>" |
/// "synth:<track>" | "synths:<track>". `dock` uses the same encoding as
/// SideWins: [docked (0/1), edge (0 left / 1 right / 2 bottom / 3 top),
/// order along the edge].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DynDock {
    pub kind: String,
    pub dock: [i16; 3],
    pub rect: [f32; 4],
    /// v47 R1: active-tab flag (see `SideWins::prim_tab`).
    #[serde(default)]
    pub tab_act: bool,
}

impl Default for DynDock {
    fn default() -> Self {
        DynDock {
            kind: String::new(),
            dock: [0, 1, 0],
            rect: [0.0; 4],
            tab_act: false,
        }
    }
}

/// v47 R2: a NAMED window-layout template (a "view template"): all
/// side-window geometry, dock slots and tab groups, every registered
/// dynamic window's geometry, and the list of dynamic-window KINDS that
/// were open at capture time. Applying restores geometry for everything
/// and visibility only for windows that can be re-opened generically
/// (drums, master, chain/synth pickers); plugin-specific windows
/// (params, roll, automation, FX editor) need a live target and open only
/// through their own command — but land in their saved slot when they do.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WinLayout {
    pub name: String,
    pub side: SideWins,
    pub dyns: Vec<DynDock>,
    pub open: Vec<String>,
}

impl WinLayout {
    /// Kind keys that applying may open/close generically.
    pub fn reopenable(kind_head: &str) -> bool {
        matches!(kind_head, "drums" | "master" | "chain" | "synth" | "synths")
    }
}

impl Default for SideWins {
    fn default() -> Self {
        SideWins {
            rack: [0.0; 4],
            binds: [0.0; 4],
            monitor: [0.0; 4],
            mixer: [0.0; 4],
            // Classic layout: rack left, mixer flush right, bindings just
            // left of the mixer, monitor along the bottom — ALL DOCKED.
            dock: default_dock(),
            prim_tab: [false; 4],
        }
    }
}

/// Classic docked layout (v30-batch-2 default): 0 = docked flag,
/// edge 0 left / 1 right / 2 bottom; order 0 = closest to the screen
/// edge within its side. [rack L, bindings right-inner, monitor bottom,
/// mixer right-outer].
pub fn default_dock() -> [[i16; 3]; 4] {
    // Classic layout, spread so the docks can't eat the canvas: rack LEFT,
    // mixer flush RIGHT (its strips align with the track rows), bindings
    // and monitor share the BOTTOM edge side by side. Same-edge panels
    // stack ALONG the edge (ranks order them); the canvas keeps 348..1152.
    [[1, 0, 0], [1, 2, 1], [1, 2, 0], [1, 1, 0]]
}

/// Everything Settings persists.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)] // partial / hand-edited files keep the rest at defaults
pub struct Config {
    /// Keyboard shortcuts as key names (e.g. `"R"`, `"Space"`), one per entry
    /// of `MidiAction::ALL` (same order); null = unbound. Tables from older
    /// layouts are remapped on load (`midi::migrate_shortcuts`).
    #[serde(default)]
    pub shortcuts: Vec<Option<String>>,
    /// MIDI bindings (only bound actions are stored).
    #[serde(default)]
    pub midi: Vec<MidiBinding>,
    /// Axis calibrations for continuous bindings (knob min/max sweep).
    /// Slots without an entry use the controller's full 0..=127 travel.
    #[serde(default)]
    pub midi_axis: Vec<MidiAxisCal>,
    /// Last applied audio port selections — restored on the next launch so the
    /// user doesn't have to re-check the same ports every time.
    #[serde(default)]
    pub ports_in: Vec<String>,
    #[serde(default)]
    pub ports_out: Vec<String>,
    /// Stereo routing for `ports_out` (same order): bit 0 = left bus,
    /// bit 1 = right bus (3 = mono downmix). Missing/short vectors fall
    /// back to 3 — pre-stereo configs keep their exact mono behaviour.
    #[serde(default)]
    pub ports_out_lr: Vec<u8>,
    /// GUI zoom factor (egui `zoom_factor`): 1.0 = default UI size, >1 larger.
    #[serde(default = "default_gui_scale")]
    pub gui_scale: f32,
    /// Latency compensation in milliseconds: each fresh recording discards
    /// this much input at its start, so the button click that started the
    /// take lands exactly at the take's first sample. Monitoring and the
    /// output mix are never delayed.
    #[serde(default)]
    pub latency_ms: f32,
    /// Per-track playback volume, MIDI-knob style (0..=127; 0 = -inf, 127 =
    /// 0 dB). Missing entries fall back to 127.
    #[serde(default)]
    pub trk_vol: Vec<u8>,
    /// Per-track grid block height in points, written when the user drags a
    /// separation bar (0 = automatic: lanes × lane height). Missing entries
    /// fall back to automatic.
    #[serde(default)]
    pub trk_h: Vec<f32>,
    /// Per-track gain trim, MIDI-knob style (0..=127; 0 = -inf, 80 = 0 dB,
    /// 127 = +10 dB). Missing entries fall back to 80 (unity).
    #[serde(default)]
    pub trk_gain: Vec<u8>,
    /// Per-track stereo pan, MIDI-knob style (0..=127; 0 = hard left,
    /// 64 = centre, 127 = hard right). Missing entries fall back to 64.
    #[serde(default)]
    pub trk_pan: Vec<u8>,
    /// Metronome tempo in BPM — also the tempo of the project grid.
    #[serde(default = "default_metro_bpm")]
    pub metro_bpm: u32,
    /// Metronome click subdivision as clicks per beat (1 = quarter, 2 = eighth).
    #[serde(default = "default_metro_sub")]
    pub metro_sub: u32,
    /// Metronome beats per measure (time-signature numerator); the first
    /// quarter note of every measure gets the downbeat tone and defines the
    /// measure the timeline grid (and the ruler) is drawn on.
    #[serde(default = "default_metro_num")]
    pub metro_num: u32,
    /// Quantize grid: evenly-spaced steps per beat (index into the UI's
    /// SUBDIVISIONS table; see `quantize::unit_samples`).
    #[serde(default = "default_grid_sub")]
    pub q_sub: u32,
    /// Drag/drop snap grid: evenly-spaced steps per beat.
    #[serde(default = "default_grid_sub")]
    pub snap_sub: u32,
    /// Snap freshly recorded takes' placement to the quantize grid.
    #[serde(default = "default_true")]
    pub auto_quantize: bool,
    /// Timeline loop toggle: wrap the playhead at the end of the loop
    /// region (see `loop_mode` below).
    #[serde(default = "default_true")]
    pub loop_tl: bool,
    /// Loop region mode: 0 = area (dragged handles on the ruler),
    /// 1 = current longest (0 → last clip end), 2 = whole song.
    #[serde(default = "default_loop_mode")]
    pub loop_mode: usize,
    /// Area loop bounds in samples (mode 0; end 0 = "not set yet" → 2 bars
    /// once the sample rate is known).
    #[serde(default)]
    pub loop_area_start: u64,
    #[serde(default)]
    pub loop_area_end: u64,
    /// Master output volume, MIDI range (0 = -inf, 115 = 0 dB, 127 = +6 dB
    /// — the same `vol_amp` mapping every track volume uses).
    #[serde(default = "default_master_vol")]
    pub master_vol: u8,
    /// Live input monitoring master switch (Settings checkbox). Default on.
    #[serde(default = "default_true")]
    pub monitor_on: bool,
    /// v32: live MIDI input feeds the synthesizers (Settings checkbox;
    /// default on). Off means MIDI drives only bindings/recording/monitor.
    #[serde(default = "default_true")]
    pub midi_live: bool,
    /// Audition rack entries on click (asset preview). Default on.
    #[serde(default = "default_true")]
    pub preview_on: bool,
    /// Preview length in seconds: entries shorter than this play in full,
    /// longer ones are auditioned from the start for exactly this long.
    #[serde(default = "default_preview_secs")]
    pub preview_secs: f32,
    /// Track whose instrument + mixer chain auditions MIDI (piano-roll
    /// "preview through track"). `None` = the default synth script.
    #[serde(default)]
    pub preview_track: Option<u32>,
    /// Grid snap on/off (main header checkbox). True = placement snaps to
    /// the subdivision grid; false = one sample (the minimum resolution).
    #[serde(default = "default_true")]
    pub snap_on: bool,
    /// v34 R1: the automation manager's OWN snap selector. The manager
    /// used to be beat-snapped in bars mode and free otherwise, with no
    /// way to say "snap my nodes to 1/8".
    #[serde(default)]
    pub auto_snap_on: bool,
    /// The automation manager's subdivision (same u32 grid values as
    /// `snap_sub`; independent of the main grid's choice).
    #[serde(default = "default_grid_sub")]
    pub auto_snap_sub: u32,
    /// False only in configs written before 0.3.0, where volume was
    /// MIDI-linear (127 = 0 dB). `load` converts the stored faders once.
    #[serde(default)]
    pub vol_scale_v2: bool,
    /// Where `.slooper` project files live (empty = `~/slooper-projects`).
    #[serde(default)]
    pub project_dir: String,
    /// v34 R8: last folder each picker opened, keyed by picker
    /// ("rack", "rack-browse", "export", "effects", "vst", "projects").
    /// Every folder/file dialog should open WHERE THE USER LAST WAS, and
    /// the map also travels with the project (see Project::dirs).
    #[serde(default)]
    pub dir_memo: std::collections::HashMap<String, String>,
    /// Asset rack folders, in order. The first entry is the *primary* folder:
    /// recordings are written there.
    #[serde(default)]
    pub rack_folders: Vec<String>,
    /// v29: collapsed rack sub-folder groups, `"<folder path>\u{1}<dir>"`
    /// (the view state that used to be session-only).
    #[serde(default)]
    pub rack_collapsed: std::collections::BTreeSet<String>,
    /// v29: per-folder sub-folder display order: `[(folder path, [dirs...])]`.
    /// Dirs not listed sort alphabetically after the listed ones. The FIRST
    /// listed (or first alphabetical, if none are) sub-folder of the primary
    /// folder is where recordings are dropped.
    #[serde(default)]
    pub rack_dir_order: Vec<RackDirOrder>,
    /// v50 R5: explicit recordings target directory (absolute path, any
    /// rack root or sub-folder). "" = the legacy implicit rule (primary
    /// folder's top sub-folder). Set from a folder's right-click menu.
    #[serde(default)]
    pub record_target: String,
    /// v29: group the rack by sub-folder (true) instead of by asset type
    /// (false, the historical default).
    #[serde(default)]
    pub rack_group_by_dir: bool,
    /// v29: per-track recording sources `[audio, midi, auto]` — which of
    /// the three the track's single REC button arms. Defaults to audio.
    #[serde(default)]
    pub rec_src: Vec<[bool; 3]>,
    /// v42 "p": per-track PREVIEW arming (kept beside `rec_src` but a
    /// separate field so older configs deserialize untouched).
    #[serde(default)]
    pub rec_prev: Vec<bool>,
    /// v42: per-track LOOP-record ticks (parallel to rec_src, same trick).
    #[serde(default)]
    pub rec_loop: Vec<bool>,
    /// v29 R7: session copy of the project's MIDI-value bindings (the
    /// project file is authoritative on load; this keeps a bare session).
    #[serde(default)]
    pub value_binds: Vec<ValueBind>,
    /// v29 R5: drum machine state (session copy; project file wins).
    #[serde(default)]
    pub drum: Option<DrumSave>,
    /// v30 R1: per-track drum kits (index = track, index = pad row).
    /// An EMPTY entry = the stock 8-pad kit (never written to disk).
    #[serde(default)]
    pub drum_kits: Vec<Vec<DrumPadSave>>,
    /// v51: per-track sampler key maps (index = track). `None` = no map
    /// (the sampler's single-asset params rule); `Some` = the multi-sample
    /// map, even with zero slots (= silence).
    #[serde(default)]
    pub sampler_maps: Vec<Option<SamplerMapSave>>,
    /// v29 R5: was the drum-machine window open?
    #[serde(default)]
    pub show_drums: bool,
    /// v39: was the MASTER OUT meter panel open? Defaults OPEN (the new
    /// master section is the front door for loudness metering).
    #[serde(default = "default_true")]
    pub show_master: bool,
    /// Effects folder ("" = default: `effects/` next to the installed
    /// package). The folder is created and seeded on first use.
    #[serde(default)]
    pub fx_dir: String,
    /// Per-track effect chains (legacy format, pre-VST-in-chain).
    /// only when `track_chain` is absent; saves write `track_chain` only.
    #[serde(default)]
    pub track_fx: Vec<Vec<TrackFxSave>>,
    /// Per-track mixer chains: index = track, entries = chain slots in
    /// audio order. Scripted effects and VST plugins are one unified list —
    /// a plugin can sit anywhere in the chain (the mixer-chain model).
    #[serde(default)]
    pub track_chain: Vec<Vec<ChainSave>>,
    /// v32 R5: sidechain send mask — bit k of `sc_send[t]` means "track k's
    /// audio keys track t's effect chain" (mixer SC picker). The live copy
    /// rides the engine atomics (`Engine::set_sc_send`).
    #[serde(default)]
    pub sc_send: Vec<u128>,
    /// Extra VST search folders (user-set; the standard system folders are
    /// always scanned on top of these).
    #[serde(default)]
    pub vst_dirs: Vec<String>,
    /// v23: editor crash-guard cache — plugin path → (file mtime seconds,
    /// editor safe to open). Written by the editor probe (see
    /// `vsthost::probe_editor`); an entry whose mtime no longer matches the
    /// plugin file is stale and re-probes, so a plugin update re-arms its
    /// window automatically. Absent = never probed.
    #[serde(default)]
    pub editor_probe: std::collections::BTreeMap<String, (u64, bool)>,
    /// Per-track VST insert (legacy single-insert format; folded into
    /// `track_chain` on load, never written again).
    #[serde(default)]
    pub track_vst: Vec<Option<VstSave>>,
    /// Per-track synth bindings: script stem ("" / None = default piano)
    /// plus saved slider values, index = track. Tracks beyond the
    /// configured count keep their bindings untouched.
    #[serde(default)]
    pub track_synth: Vec<Option<String>>,
    #[serde(default)]
    pub track_synth_params: Vec<Vec<f32>>,
    /// Per-track synth state chunk (base64; index = track). v22: plugin
    /// patches are REAL state now — the v20 "session-only" rule threw away
    /// every change a user made inside a plugin GUI.
    #[serde(default)]
    pub track_synth_states: Vec<Option<String>>,
    /// Number of tracks the program exposes (clamped 1..=16). Per-track
    /// levels and bindings are kept for ALL 16 slots regardless, so raising
    /// the count again restores everything that was configured while a
    /// lower count hid the track.
    #[serde(default = "default_n_tracks")]
    pub n_tracks: usize,
    /// v25: panel visibility (Settings ▸ GUI). Each dock can be hidden for
    /// a cleaner window; all three default to visible.
    #[serde(default = "default_true")]
    pub show_rack: bool,
    #[serde(default = "default_true")]
    pub show_mixer: bool,
    /// v52 R5: true (default) = today's behavior — while docked left/right
    /// the mixer mirrors the main GUI's track heights and shares its scroll.
    /// False: mixer strips keep a UNIFORM height, start at the panel top and
    /// own their scroll even when docked — the mixer becomes its own view.
    #[serde(default = "default_true")]
    pub mixer_follow_tracks: bool,
    #[serde(default = "default_true")]
    pub show_midi: bool,
    /// v30 R4: the project-value-bindings window now belongs to the MAIN
    /// GUI (it used to exist only while the automation manager was open).
    #[serde(default = "default_true")]
    pub show_binds: bool,
    /// v30 R6: last-used rects [x, y, w, h] of the free side windows.
    /// A zeroed entry means "use the built-in dock footprint".
    #[serde(default)]
    pub side_wins: SideWins,
    /// v35: docked/floating state of the DYNAMIC windows (mixer chain,
    /// parameters, synth sliders, drums, roll, automation, SYNTHS picker
    /// — per track where applicable). Kinds absent here start floating.
    #[serde(default)]
    pub dyn_docks: Vec<DynDock>,
    /// v47 R2: named window-layout templates (Windows menu): capture,
    /// apply, delete. Each holds the complete dock/float/tab geometry.
    #[serde(default)]
    pub win_layouts: Vec<WinLayout>,
    /// v25: panel/window theme — "dark" (default) or "light". The
    /// arrangement canvas and piano roll keep their fixed studio palette
    /// either way (contrast for editing is intentional).
    #[serde(default = "default_theme")]
    pub theme: String,
    /// v42/MPE: MIDI Polyphonic Expression. When set, pitch-bend (0xEx)
    /// and per-note aftertouch (0xAx) from the input port drive track
    /// instruments per channel/voice. Default OFF — classic single-channel
    /// rigs keep byte-identical behavior.
    #[serde(default)]
    pub mpe: bool,
    /// W8b (Windows): open plugin editors INSIDE the main window (child
    /// HWND under the winit window, slooper-drawn chrome) instead of as
    /// free top-level windows. Ignored on Linux — editors there are always
    /// their own windows (`x11::embed_requested` is permanently false).
    /// `SLOOPER_WIN_EMBED` still forces it on without touching the config
    /// (power users / CI).
    #[serde(default)]
    pub win_editor_embed: bool,
    /// W9 (Windows): the WinMM MIDI input devices to open at launch
    /// (exact labels from `Evt::MidiInputs`; the Ports picker writes it).
    /// Linux never reads it — JACK MIDI auto-connects everything.
    #[serde(default)]
    pub win_midi_inputs: Vec<String>,
    /// W12f (Windows): host audio backend preference — "auto" (native JACK
    /// when its DLL is discoverable, else WASAPI), "wasapi", or "jack".
    /// Values/precedence resolved by `audio::resolve_backend` (env
    /// `SLOOPER_AUDIO_BACKEND` wins). Linux never reads it — JACK is THE
    /// backend there.
    #[serde(default = "default_audio_backend")]
    pub win_audio_backend: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            shortcuts: Vec::new(),
            midi: Vec::new(),
            midi_axis: Vec::new(),
            ports_in: Vec::new(),
            ports_out: Vec::new(),
            ports_out_lr: Vec::new(),
            gui_scale: default_gui_scale(),
            latency_ms: 0.0,
            trk_vol: Vec::new(),
            trk_h: Vec::new(),
            trk_gain: Vec::new(),
            trk_pan: Vec::new(),
            n_tracks: default_n_tracks(),
            metro_bpm: default_metro_bpm(),
            metro_sub: default_metro_sub(),
            metro_num: default_metro_num(),
            q_sub: default_grid_sub(),
            snap_sub: default_grid_sub(),
            auto_snap_on: false,
            auto_snap_sub: default_grid_sub(),
            dir_memo: Default::default(),
            auto_quantize: default_true(),
            loop_tl: default_true(),
            loop_mode: default_loop_mode(),
            loop_area_start: 0,
            loop_area_end: 0,
            master_vol: default_master_vol(),
            monitor_on: true,
            midi_live: true,
            preview_on: true,
            preview_secs: default_preview_secs(),
            preview_track: None,
            snap_on: true,
            vol_scale_v2: true,
            project_dir: String::new(),
            rack_folders: Vec::new(),
            rack_collapsed: Default::default(),
            rack_dir_order: Vec::new(),
            record_target: String::new(),
            rack_group_by_dir: false,
            rec_src: vec![[true, false, false]; 8],
            rec_prev: Vec::new(),
            rec_loop: Vec::new(),
            value_binds: Vec::new(),
            drum: None,
            drum_kits: Vec::new(),
            sampler_maps: Vec::new(),
            show_drums: false,
            show_master: true,
            fx_dir: String::new(),
            track_fx: Vec::new(),
            track_chain: Vec::new(),
            sc_send: Vec::new(),
            vst_dirs: Vec::new(),
            editor_probe: Default::default(),
            track_vst: Vec::new(),
            track_synth: Vec::new(),
            track_synth_params: Vec::new(),
            track_synth_states: Vec::new(),
            show_rack: true,
            show_mixer: true,
            mixer_follow_tracks: true,
            show_midi: true,
            show_binds: true,
            side_wins: SideWins::default(),
            dyn_docks: Vec::new(),
            win_layouts: Vec::new(),
            theme: default_theme(),
            mpe: false,
            win_editor_embed: false,
            win_midi_inputs: Vec::new(),
            win_audio_backend: default_audio_backend(),
        }
    }
}

/// W12f: "auto" = native JACK if its DLL is discoverable, else WASAPI.
fn default_audio_backend() -> String {
    "auto".into()
}

fn default_gui_scale() -> f32 {
    1.0
}

fn default_theme() -> String {
    "dark".into()
}

/// "Current longest" is the default loop region mode (index 1 of
/// `["area", "current longest", "whole song"]`).
fn default_loop_mode() -> usize {
    1
}

/// Master volume default = 0 dB on the `vol_amp` MIDI mapping.
fn default_master_vol() -> u8 {
    115
}

/// Asset-preview length default: 5 seconds.
fn default_preview_secs() -> f32 {
    5.0
}

/// Track-count default: the historical fixed 16 tracks.
fn default_n_tracks() -> usize {
    16
}

/// Convert a pre-0.3 MIDI-linear fader (0 = silence, 127 = 0 dB, plain
/// linear amplitude) onto the current dB-scale MIDI value. Shared by the
/// config migration and the v1 project loader.
pub fn vol_midi_from_legacy(v: u8) -> u8 {
    if v == 0 {
        return 0;
    }
    let db = 20.0 * (v as f32 / 127.0).log10();
    crate::ui::vol_midi(db)
}

fn default_metro_bpm() -> u32 {
    120
}

fn default_metro_sub() -> u32 {
    1
}

fn default_metro_num() -> u32 {
    4
}

fn default_grid_sub() -> u32 {
    4
}

fn default_true() -> bool {
    true
}

impl Config {
    /// `paths::config_dir()/config.json` — `$XDG_CONFIG_HOME/slooper/`
    /// (or `~/.config/...`) on Linux, `%APPDATA%\slooper\` on Windows
    /// (WINDOWS-PORT W2; resolution lives in [`crate::paths`]).
    pub fn path() -> PathBuf {
        crate::paths::config_dir().join("config.json")
    }

    /// Effective mixer chains: `track_chain` when present, else migrated
    /// from the legacy split (`track_fx` scripts + trailing `track_vst`).
    pub fn chain(&self) -> Vec<Vec<ChainSave>> {
        if !self.track_chain.is_empty() {
            return self.track_chain.clone();
        }
        let n = self.track_fx.len().max(self.track_vst.len());
        (0..n)
            .map(|i| {
                let mut v: Vec<ChainSave> = self
                    .track_fx
                    .get(i)
                    .map(|s| {
                        s.iter()
                            .map(|f| ChainSave::Fx {
                                stem: f.stem.clone(),
                                params: f.params.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(Some(vs)) = self.track_vst.get(i) {
                    v.push(ChainSave::Vst {
                        plugin_id: None,
                        path: vs.path.clone(),
                        kind: vs.kind.clone(),
                        name: vs.name.clone(),
                        params: vs.params.clone(),
                        state: None,
                    });
                }
                v
            })
            .collect()
    }

    /// Load the config, falling back to defaults on any error.
    pub fn load() -> Self {
        let raw = std::fs::read_to_string(Self::path());
        let mut cfg: Self = match &raw {
            Ok(s) => match serde_json::from_str(s) {
                Ok(c) => c,
                Err(e) => {
                    // A torn/invalid write must never be silently replaced:
                    // keep it next to the config for recovery, start fresh.
                    let _ = std::fs::rename(Self::path(), Self::path().with_extension("corrupt"));
                    eprintln!(
                        "[slooper] config invalid ({e}) — saved as config.corrupt, using defaults"
                    );
                    return Self::default();
                }
            },
            Err(_) => return Self::default(),
        };
        if !cfg.vol_scale_v2 {
            // Pre-0.3 config: volumes were MIDI-linear (127 = 0 dB).
            cfg.master_vol = vol_midi_from_legacy(cfg.master_vol);
            for v in &mut cfg.trk_vol {
                *v = vol_midi_from_legacy(*v);
            }
            cfg.vol_scale_v2 = true;
            cfg.save().ok();
        }
        // W12d (WINDOWS-PORT): a config written on the other OS may carry
        // absolute paths that are legal *relative* junk here (C:\… on Linux,
        // /home/… on Windows) — create-or-seed consumers would write into
        // them. Drop those shapes and fall back to this OS's defaults; the
        // next normal save persists the cleaned config (the other OS's
        // folders are unrecoverable by syntax anyway — its pickers refill).
        let foreign = cfg.drop_foreign_paths();
        if foreign > 0 {
            eprintln!(
                "[slooper] config: dropped {foreign} path(s) of the other OS — this OS's defaults apply"
            );
        }
        cfg
    }

    /// Save the config; creates the directory if needed.
    pub fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&p, s).map_err(|e| e.to_string())
    }

    /// Where `.slooper` files live: the configured folder, or the
    /// `slooper-projects` folder in the user's home when unset.
    pub fn projects_dir(&self) -> PathBuf {
        if self.project_dir.trim().is_empty() {
            crate::paths::default_projects_dir()
        } else {
            PathBuf::from(self.project_dir.trim())
        }
    }

    /// MIDI binding for `action`, if any.
    pub fn midi_bind(&self, a: MidiAction) -> Option<MidiBind> {
        self.midi
            .iter()
            .find(|b| b.action == a)
            .and_then(|b| b.bind)
    }

    /// W12d (WINDOWS-PORT): drop every user path whose syntax belongs to
    /// the *other* OS — see [`crate::paths::path_is_foreign`] for why such
    /// strings are legal junk on the receiving OS. `Config::load` calls
    /// this on every launch; a same-OS config never matches (return 0).
    ///
    /// Fields that feed create-or-seed consumers get cleaned:
    /// `project_dir`/`fx_dir` are blanked (⇒ per-OS defaults),
    /// `rack_folders`/`vst_dirs` lose the foreign entries (an emptied rack
    /// list falls back to the samples root like a first launch would),
    /// and `dir_memo` forgets folders this OS cannot open. The inert
    /// view caches (`editor_probe`, `rack_collapsed`, `rack_dir_order`)
    /// are deliberately NOT touched — their stale keys can never hit and
    /// dropping them would break the round trip back to the owning OS.
    /// Returns the number of dropped/blanked entries.
    pub fn drop_foreign_paths(&mut self) -> usize {
        let mut n = 0usize;
        for f in [&mut self.project_dir, &mut self.fx_dir] {
            if crate::paths::path_is_foreign(f) {
                f.clear();
                n += 1;
            }
        }
        for v in [&mut self.rack_folders, &mut self.vst_dirs] {
            let before = v.len();
            v.retain(|p| !crate::paths::path_is_foreign(p));
            n += before - v.len();
        }
        let before = self.dir_memo.len();
        self.dir_memo
            .retain(|_, v| !crate::paths::path_is_foreign(v));
        n += before - self.dir_memo.len();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_volume_faders_migrate_to_db_scale() {
        use crate::ui::VOL_UNITY_MIDI;
        // Old scale: 127 = 0 dB, linear amplitude. New: 115 = 0 dB.
        assert_eq!(vol_midi_from_legacy(127), VOL_UNITY_MIDI);
        assert_eq!(vol_midi_from_legacy(0), 0);
        assert_eq!(vol_midi_from_legacy(64), 104); // -5.94 dB
                                                   // Monotone and never exceeds unity below 127.
        let mut prev = 0u8;
        for v in 0..=127u8 {
            let m = vol_midi_from_legacy(v);
            assert!(m >= prev, "not monotone at {v}");
            prev = m;
        }
    }

    #[test]
    fn w8b_win_editor_embed_defaults_off_and_parses() {
        // W8b p2-a: the embed switch must be additive-safe for every
        // existing config file on disk (missing key = today's behavior).
        assert!(!Config::default().win_editor_embed);
        let legacy: Config = serde_json::from_str("{}").unwrap();
        assert!(
            !legacy.win_editor_embed,
            "old configs parse with the flag off"
        );
        let on: Config = serde_json::from_str(r#"{"win_editor_embed": true}"#).unwrap();
        assert!(on.win_editor_embed);
    }

    #[test]
    fn w9_win_midi_inputs_defaults_empty_and_roundtrips() {
        // W9: additive-safe like the W8b flag; the selection is a label
        // list that must survive save/load verbatim (labels carry " [2]"
        // suffixes — a mangling round trip opens the wrong device).
        assert!(Config::default().win_midi_inputs.is_empty());
        let legacy: Config = serde_json::from_str("{}").unwrap();
        assert!(
            legacy.win_midi_inputs.is_empty(),
            "old configs parse with nothing selected"
        );
        let json = r#"{"win_midi_inputs": ["KeyStage 61", "KeyStage 61 [2]"]}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.win_midi_inputs,
            vec!["KeyStage 61".to_string(), "KeyStage 61 [2]".to_string()]
        );
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            back.win_midi_inputs, cfg.win_midi_inputs,
            "round-trips verbatim"
        );
    }

    /// W12f: `win_audio_backend` is additive-safe — old configs default to
    /// "auto" (native JACK when a DLL is found, else the shipped WASAPI),
    /// and an explicit value round-trips verbatim (the picker writes exact
    /// words; a mangling round trip would silently re-switch backends).
    #[test]
    fn w12f_win_audio_backend_defaults_auto_and_roundtrips() {
        assert_eq!(Config::default().win_audio_backend, "auto");
        let legacy: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(
            legacy.win_audio_backend, "auto",
            "old configs default to auto"
        );
        let cfg: Config = serde_json::from_str(r#"{"win_audio_backend":"jack"}"#).unwrap();
        assert_eq!(cfg.win_audio_backend, "jack");
        let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.win_audio_backend, "jack", "round-trips verbatim");
    }

    /// Restores `SLOOPER_CONFIG_DIR` and drops the sandbox even when an
    /// assertion panics mid-test (leaked config dirs poison every later
    /// test that calls `Config::load`).
    struct ConfigSandbox(PathBuf);

    impl ConfigSandbox {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("sp-w12d-{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("SLOOPER_CONFIG_DIR", &dir);
            ConfigSandbox(dir)
        }
    }

    impl Drop for ConfigSandbox {
        fn drop(&mut self) {
            std::env::remove_var("SLOOPER_CONFIG_DIR");
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn w12d_path_shape_foreign_rules() {
        // W12d: the syntax detector behind the config round trip. Every OS
        // asserts its own truth (runs in both CI lanes).
        use crate::paths::path_is_foreign;
        // Never foreign: empty/blank, relative names of every kind, and
        // this OS's absolute paths.
        assert!(!path_is_foreign(""));
        assert!(!path_is_foreign("   "));
        assert!(!path_is_foreign("relative/dir"));
        assert!(!path_is_foreign("C:foo"), "drive-RELATIVE is not claimed");
        if cfg!(windows) {
            assert!(!path_is_foreign(r"C:\Users\studio\rack"));
            assert!(!path_is_foreign("C:/Users/studio/rack"));
            assert!(!path_is_foreign(r"\\srv\share\rack"), "UNC is native");
            assert!(
                path_is_foreign("/home/studio/rack"),
                "Unix root = drive junk"
            );
            assert!(path_is_foreign("  /tmp/x  "), "checked after trim");
        } else {
            assert!(!path_is_foreign("/home/studio/rack"));
            assert!(!path_is_foreign(r"back\slash-name"), "legal POSIX name");
            assert!(path_is_foreign(r"C:\Users\studio\rack"), "drive root");
            assert!(path_is_foreign("C:/Users/studio/rack"));
            assert!(path_is_foreign("C:"), "bare drive root");
            assert!(
                path_is_foreign(r"\\srv\share\rack"),
                "UNC cannot exist here"
            );
        }
    }

    #[test]
    fn w12d_verbatim_other_os_json_parses_then_filters() {
        // The JSON layer itself is OS-neutral: a Windows-written config
        // parses losslessly ANYWHERE (paths untouched until the load-level
        // filter). Escaping matches what serde on Windows actually writes.
        let win = r#"{"project_dir":"C:\\Users\\studio\\proj","fx_dir":"C:\\Users\\studio\\fx","rack_folders":["C:\\rack","D:\\takes"],"vst_dirs":["D:\\VST2"],"dir_memo":{"export":"C:\\out"},"metro_bpm":96,"win_midi_inputs":["KeyStage 61"]}"#;
        let cfg: Config = serde_json::from_str(win).expect("other-OS config must parse");
        assert_eq!(cfg.fx_dir, r"C:\Users\studio\fx", "parse is verbatim");
        assert_eq!(cfg.rack_folders.len(), 2);
        let mut cfg = cfg;
        let dropped = cfg.drop_foreign_paths();
        if cfg!(windows) {
            assert_eq!(dropped, 0, "a Windows config is native on Windows");
            assert_eq!(cfg.fx_dir, r"C:\Users\studio\fx");
            assert_eq!(cfg.rack_folders.len(), 2);
        } else {
            // project_dir + fx_dir + 2 rack folders + 1 vst dir + 1 memo
            assert_eq!(dropped, 6, "every drive path counted");
            assert!(cfg.project_dir.is_empty() && cfg.fx_dir.is_empty());
            assert!(cfg.rack_folders.is_empty() && cfg.vst_dirs.is_empty());
            assert!(!cfg.dir_memo.contains_key("export"));
            assert_eq!(cfg.metro_bpm, 96, "non-path fields untouched");
            assert_eq!(cfg.win_midi_inputs, ["KeyStage 61"]);
        }
    }

    #[test]
    fn w12d_load_end_to_end_keeps_local_drops_foreign() {
        // The real entry point: a config file on disk (written from the
        // other OS, carrying BOTH path shapes as values) loads with
        // exactly the foreign ones gone and ours — including relative
        // entries — intact. The fx_dir assert guards the worst junk-write
        // consumer: prepare_effects_dir seeds bundled scripts into it.
        let _sb = ConfigSandbox::new("load");
        let (foreign, local) = if cfg!(windows) {
            (
                "/home/studio/rack".to_string(),
                "C:\\Users\\studio\\rack".to_string(),
            )
        } else {
            (
                "C:\\Users\\studio\\rack".to_string(),
                "/tmp/w12d-rack".to_string(),
            )
        };
        let cfg0 = Config {
            project_dir: foreign.clone(),
            fx_dir: foreign.clone(),
            rack_folders: vec![foreign.clone(), local.clone(), "relative-pack".into()],
            vst_dirs: vec![foreign.clone()],
            dir_memo: [
                ("export".to_string(), foreign.clone()),
                ("rack".to_string(), local.clone()),
            ]
            .into_iter()
            .collect(),
            editor_probe: [(foreign.clone(), (1234, true))].into_iter().collect(),
            rack_collapsed: [format!("{foreign}\u{1}kit-a")].into_iter().collect(),
            ..Config::default()
        };
        cfg0.save().unwrap();
        let cfg = Config::load();
        assert!(cfg.project_dir.is_empty(), "foreign project_dir dropped");
        assert!(
            cfg.fx_dir.is_empty(),
            "fx_dir must NEVER seed into foreign here"
        );
        assert_eq!(
            cfg.rack_folders,
            vec![local.clone(), "relative-pack".to_string()],
            "local + relative rack folders survive"
        );
        assert!(cfg.vst_dirs.is_empty());
        assert_eq!(
            cfg.dir_memo.get("rack").map(String::as_str),
            Some(local.as_str())
        );
        assert!(!cfg.dir_memo.contains_key("export"));
        // Inert view caches stay verbatim (their keys can never hit here,
        // and dropping them would break the round trip back).
        assert!(cfg.editor_probe.contains_key(&foreign));
        assert!(cfg.rack_collapsed.contains(&format!("{foreign}\u{1}kit-a")));
        // Deterministic across loads (load does not half-persist shapes).
        let again = Config::load();
        assert_eq!(again.rack_folders, cfg.rack_folders);
    }

    #[test]
    fn config_roundtrip() {
        let cfg = Config {
            mpe: true,
            win_editor_embed: true,
            auto_snap_on: false,
            auto_snap_sub: 8,
            dir_memo: Default::default(),
            n_tracks: 16,
            shortcuts: vec![Some("R".into()), None, Some("Space".into())],
            midi: vec![MidiBinding {
                action: MidiAction::RecLoop(0),
                bind: Some(MidiBind {
                    ty: 0,
                    ch: 1,
                    d1: 60,
                }),
            }],
            midi_axis: Vec::new(),
            editor_probe: Default::default(),
            ports_in: vec!["remote:out_1".into()],
            ports_out: vec!["remote:in_1".into(), "remote:in_2".into()],
            ports_out_lr: vec![1, 2],
            gui_scale: 1.25,
            latency_ms: 12.5,
            metro_bpm: 96,
            metro_sub: 3,
            metro_num: 7,
            q_sub: 6,
            snap_sub: 2,
            auto_quantize: false,
            loop_tl: false,
            loop_mode: 1,
            loop_area_start: 24,
            loop_area_end: 192_000,
            master_vol: 100,
            monitor_on: false,
            midi_live: false,
            preview_on: false,
            preview_secs: 2.5,
            preview_track: Some(3),
            snap_on: false,
            vol_scale_v2: true,
            project_dir: "/tmp/projects".into(),
            rack_folders: vec!["/tmp/samples".into(), "/tmp/takes".into()],
            rack_collapsed: ["\u{1}kit-a".to_string()].into_iter().collect(),
            rack_dir_order: vec![RackDirOrder {
                folder: "/tmp/samples".into(),
                dirs: vec!["kick".into(), "snare".into()],
            }],
            rack_group_by_dir: true,
            rec_src: vec![[true, true, false], [false, false, true]],
            rec_prev: vec![true],
            rec_loop: vec![true],
            value_binds: vec![ValueBind {
                target: "track.vol".into(),
                track: 1,
                bind: crate::midi::MidiBind {
                    ty: 1,
                    ch: 0,
                    d1: 7,
                },
                kind: 0,
                lo: 9,
                hi: 118,
            }],
            drum: Some(DrumSave {
                pads: vec![
                    DrumPadSave {
                        name: "Kick".into(),
                        asset: "/s/kick.wav".into(),
                        vel: 100,
                        muted: false,
                    },
                    DrumPadSave {
                        name: "Hat".into(),
                        asset: "".into(),
                        vel: 80,
                        muted: true,
                    },
                ],
                steps: 16,
                sub: 3,
                grid: vec![vec![0u8; 16], vec![90, 0, 0, 0]],
                track: 2,
            }),
            drum_kits: vec![vec![DrumPadSave {
                name: "Boom".into(),
                asset: "/s/boom.wav".into(),
                vel: 70,
                muted: false,
            }]],
            show_drums: true,
            show_master: false, // exercise the non-default round-trip
            fx_dir: "/tmp/effects".into(),
            track_chain: Vec::new(), // exercise the legacy track_fx/track_vst migration
            sc_send: Vec::new(),
            track_fx: vec![vec![TrackFxSave {
                stem: "template".into(),
                params: vec![0.5, 4.0],
            }]],
            trk_vol: vec![127, 100],
            trk_h: vec![120.0],
            trk_gain: vec![80, 60],
            trk_pan: vec![64, 20],
            track_synth: vec![None, Some("piano".into())],
            track_synth_params: vec![vec![], vec![0.2]],
            track_synth_states: vec![],
            vst_dirs: vec!["/tmp/vst".into()],
            track_vst: vec![
                None,
                Some(VstSave {
                    path: "/tmp/vst/thing.vst3".into(),
                    kind: "VST3".into(),
                    name: "Thing".into(),
                    params: vec![0.4],
                }),
            ],
            show_rack: false,
            show_mixer: true,
            show_midi: false,
            show_binds: true,
            side_wins: crate::config::SideWins {
                rack: [1.0, 2.0, 300.0, 400.0],
                binds: [0.0; 4],
                monitor: [0.0; 4],
                mixer: [5.0, 6.0, 250.0, 600.0],
                dock: [[0, 0, 0], [1, 1, 1], [1, 2, 0], [0, 1, 0]],
                prim_tab: [false, true, false, false],
            },
            dyn_docks: vec![crate::config::DynDock {
                kind: "chain:1".into(),
                dock: [1, 3, 0],
                rect: [4.0, 5.0, 440.0, 300.0],
                tab_act: true,
            }],
            theme: "light".into(),
            ..Default::default()
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.shortcuts, cfg.shortcuts);
        assert_eq!(back.midi, cfg.midi);
        assert_eq!(
            back.dyn_docks, cfg.dyn_docks,
            "v35 dynamic dock state round-trips"
        );
        assert!((back.gui_scale - 1.25).abs() < f32::EPSILON);
        assert!((back.latency_ms - 12.5).abs() < f32::EPSILON);
        assert_eq!(back.metro_bpm, 96);
        assert_eq!(back.metro_sub, 3);
        assert_eq!(back.metro_num, 7);
        assert_eq!(back.q_sub, 6);
        assert_eq!(back.snap_sub, 2);
        assert!(!back.monitor_on, "monitoring toggle round-trips");
        assert!(!back.auto_quantize && !back.loop_tl);
        assert_eq!(back.project_dir, "/tmp/projects");
        assert_eq!(back.rack_folders, cfg.rack_folders);
        assert_eq!(back.ports_in, cfg.ports_in);
        assert_eq!(back.ports_out, cfg.ports_out);
        assert_eq!(back.ports_out_lr, cfg.ports_out_lr);
        assert_eq!(back.trk_vol, cfg.trk_vol);
        assert_eq!(back.vst_dirs, cfg.vst_dirs);
        assert_eq!(back.track_vst.len(), 2);
        let v = back.track_vst[1].clone().unwrap();
        assert_eq!(
            (v.path.as_str(), v.kind.as_str(), v.params.as_slice()),
            ("/tmp/vst/thing.vst3", "VST3", &[0.4f32][..])
        );
        assert_eq!(back.trk_gain, cfg.trk_gain);
        assert_eq!(back.trk_pan, cfg.trk_pan);
        assert!(!back.preview_on, "preview toggle round-trips");
        assert!(back.win_editor_embed, "W8b embed flag round-trips");
        assert!((back.preview_secs - 2.5).abs() < f32::EPSILON);
        assert_eq!(back.preview_track, Some(3), "preview track round-trips");
        assert!(!back.snap_on, "snap toggle round-trips");
        assert_eq!(back.fx_dir, "/tmp/effects");
        // v25: panel visibility + theme ride the config file.
        assert!(!back.show_rack && back.show_mixer && !back.show_midi);
        assert_eq!(back.theme, "light");
        assert_eq!(back.track_fx[0][0].stem, "template");
        assert_eq!(back.track_fx[0][0].params, vec![0.5, 4.0]);
    }

    #[test]
    fn defaults_on_corrupt() {
        let back: Config = serde_json::from_str("{not json").unwrap_or_default();
        assert_eq!(back.q_sub, 4);
        assert!(back.shortcuts.is_empty());
    }

    #[test]
    fn missing_fields_default_and_removed_fields_tolerated() {
        // An old config: removed fields (root_loops, q_metro) must be
        // ignored, new ones must land on their documented defaults.
        let back: Config =
            serde_json::from_str(r#"{"root_loops": 3, "q_metro": true, "unknown": 1}"#).unwrap();
        assert_eq!(back.q_sub, 4);
        assert_eq!(back.snap_sub, 4);
        assert!(
            back.auto_quantize,
            "recordings snap to the grid unless unticked"
        );
        assert!(back.loop_tl, "the timeline loops by default");
        assert!(back.preview_on, "asset preview is on by default");
        assert_eq!(back.preview_secs, 5.0, "default preview length is 5 s");
        // v25: an older config (no keys) = every panel visible, dark theme.
        assert!(back.show_rack && back.show_mixer && back.show_midi);
        assert_eq!(back.theme, "dark");
        assert!(back.midi.is_empty());
        // Per-track levels are #[serde(default)] — old configs load as empty
        // and App::new falls them back to 127 (vol) / 80 (gain) / 64 (pan).
        assert!(back.trk_vol.is_empty());
        assert!(back.trk_gain.is_empty());
        assert!(back.trk_pan.is_empty());
        assert!(back.ports_out_lr.is_empty());
        assert!(back.rack_folders.is_empty());
    }

    #[test]
    fn removed_midi_actions_become_reserved() {
        // A persisted binding naming an action this build removed must not
        // poison the whole config load.
        let back: Config = serde_json::from_str(
            r#"{"midi":[{"action":"OdSelected","bind":{"ty":0,"ch":0,"d1":60}},
                       {"action":{"RecLoop":3},"bind":null}]}"#,
        )
        .expect("old action names still deserialize");
        assert_eq!(back.midi[0].action, MidiAction::Reserved);
        assert_eq!(back.midi[1].action, MidiAction::RecLoop(3));
    }

    #[test]
    fn projects_dir_fallback() {
        let mut cfg = Config::default();
        assert!(cfg.projects_dir().ends_with("slooper-projects"));
        cfg.project_dir = "/srv/music".into();
        assert_eq!(cfg.projects_dir(), PathBuf::from("/srv/music"));
    }

    /// The config-flip workflow (hand-edited files) must not silently
    /// reset every field that is absent from the JSON.
    #[test]
    fn partial_config_keeps_defaults() {
        let cfg: Config = serde_json::from_str(
            r#"{"show_drums": true, "dyn_docks": [{"kind":"drums","dock":[1,1,1],"rect":[0.0,0.0,0.0,0.0]}]}"#,
        )
        .unwrap();
        assert!(cfg.show_drums, "present field honored");
        assert_eq!(cfg.dyn_docks.len(), 1, "seeded dynamic dock state");
        assert_eq!(cfg.theme, "dark", "absent fields at default");
        assert_eq!(cfg.n_tracks, Config::default().n_tracks);
    }

    /// v22 pin: plugin state chunks ride base64 through the JSON saves —
    /// xattr storage vanished silently on vfat/external drives, which is
    /// how plugin edits "went back to default" after closing the GUI.
    /// v51: sampler key maps ride the config session and survive old
    /// files (missing field = empty, legacy rules).
    #[test]
    fn v51_sampler_maps_config_roundtrip() {
        let c = Config {
            sampler_maps: vec![Some(SamplerMapSave {
                slots: vec![SamplerSlotSave {
                    asset: "/x/kick.wav".into(),
                    root: 48,
                }],
                mode: 1,
            })],
            ..Default::default()
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        let m = back.sampler_maps[0].as_ref().expect("map survives");
        assert_eq!((m.mode, m.slots.len(), m.slots[0].root), (1, 1, 48));
        assert_eq!(m.slots[0].asset, "/x/kick.wav");
    }

    #[test]
    fn v22_b64_state_roundtrip() {
        let raw: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let enc = b64_encode(&raw);
        assert!(
            !enc.contains('\n') && !enc.contains(' '),
            "state must survive as one JSON string"
        );
        assert_eq!(b64_decode(&enc).unwrap(), raw);
        assert_eq!(b64_encode(&[]), "");
        assert_eq!(b64_decode("").unwrap(), Vec::<u8>::new());
        assert!(
            b64_decode("!!!").is_none(),
            "garbage is rejected, not trusted"
        );
    }

    /// v23 pin: the editor crash-guard cache (path → (mtime, safe))
    /// rides the config file, so a plugin that crashes THIS display
    /// server stays on the parameter window across restarts — and an
    /// older config without the key still loads (empty cache = probe
    /// again, never ban from memory alone).
    #[test]
    fn v23_editor_probe_cache_roundtrip() {
        let mut cfg = Config::default();
        cfg.editor_probe
            .insert("/host/ADLplug.so".into(), (1_700_000_000, false));
        cfg.editor_probe
            .insert("/host/wolf-shaper.vst3".into(), (1_700_000_001, true));
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.editor_probe.get("/host/ADLplug.so"),
            Some(&(1_700_000_000, false))
        );
        let old: Config = serde_json::from_str(r#"{"n_tracks":8}"#).unwrap();
        assert!(old.editor_probe.is_empty(), "pre-v23 config loads clean");
    }

    /// v34 pin (R3): CLAP rows persist kind "CLAP" AND the in-file plugin
    /// id; pre-CLAP project files (no field) must still deserialize.
    #[test]
    fn v34_chainsave_clap_carries_plugin_id() {
        let c = ChainSave::Vst {
            path: "/home/u/.clap/Camshaft.clap".into(),
            kind: "CLAP".into(),
            plugin_id: Some("com.azuriteid.camshaft.comppressor".into()),
            name: "CamComp".into(),
            params: vec![0.5],
            state: Some(b64_encode(b"F1 0.25")),
        };
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"CLAP\""), "{j}");
        let back: ChainSave = serde_json::from_str(&j).unwrap();
        match back {
            ChainSave::Vst {
                kind,
                plugin_id,
                state,
                ..
            } => {
                assert_eq!(kind, "CLAP");
                assert_eq!(
                    plugin_id.as_deref(),
                    Some("com.azuriteid.camshaft.comppressor")
                );
                assert_eq!(
                    state.as_deref().and_then(b64_decode),
                    Some(b"F1 0.25".to_vec())
                );
            }
            _ => panic!("variant lost"),
        }
        let old: ChainSave = serde_json::from_str(
            r#"{ "type": "vst", "path": "/x.vst3", "kind": "VST3", "name": "n",
                 "params": [], "state": null }"#,
        )
        .unwrap();
        match old {
            ChainSave::Vst { plugin_id, .. } => assert!(plugin_id.is_none()),
            _ => panic!("variant lost"),
        }
    }

    /// v22 pin: `ChainSave::Vst` persists the state chunk (project saves),
    /// and pre-v22 files without the key load as `None`.
    #[test]
    fn v22_chainsave_vst_state_roundtrip() {
        let c = ChainSave::Vst {
            plugin_id: None,
            path: "/host/thing.vst3".into(),
            kind: "VST3".into(),
            name: "Thing".into(),
            params: vec![0.25, 0.75],
            state: Some(b64_encode(b"plugin-chunk")),
        };
        let j = serde_json::to_string(&c).unwrap();
        let back: ChainSave = serde_json::from_str(&j).unwrap();
        match back {
            ChainSave::Vst { state, params, .. } => {
                assert_eq!(
                    state.as_deref().and_then(b64_decode),
                    Some(b"plugin-chunk".to_vec())
                );
                assert_eq!(params, vec![0.25, 0.75]);
            }
            _ => panic!("variant lost"),
        }
        let old = r#"{"type":"vst","path":"/p","kind":"VST2","name":"","params":[0.5]}"#;
        match serde_json::from_str::<ChainSave>(old).unwrap() {
            ChainSave::Vst { state, .. } => assert!(state.is_none(), "old files load state=None"),
            _ => unreachable!(),
        }
    }
}
