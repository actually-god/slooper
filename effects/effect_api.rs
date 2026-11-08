// ============================================================ slooper ==
// effect_api.rs — the slooper effect ABI, version 1.
//
// THIS FILE IS NOT AUTHORED BY HAND AS A STANDALONE PROGRAM. It is a shared
// contract, written once into every effects folder by slooper itself and
// embedded in the binary (src/effect_api.rs is the source of truth). Effect
// scripts pull it in with:
//
//     include!("effect_api.rs");
//
// because that resolves next to the script, where slooper keeps a fresh
// copy. Never edit the copy inside your effects folder — if slooper
// rewrites it (or the version number below changes), your effect adapts.
//
// ---------------------------------------------------------------------------
// What an effect is
// ---------------------------------------------------------------------------
// An effect script is one standalone Rust file. slooper compiles each
// valid `*.rs` file in the effects folder with:
//
//     rustc --edition 2021 --crate-type cdylib <file>
//
// loads the result, and calls your functions per audio sample, on the audio
// thread, for every loop that has the effect enabled. Per loop, per effect
// slot, an independent *instance* is created via `create` and torn down via
// `destroy`; each instance owns whatever state it needs (counters, delay
// lines, filters …).
//
// ---------------------------------------------------------------------------
// REQUIRED pieces (an effect without these is not valid)
// ---------------------------------------------------------------------------
// 1. `pub static DESC: EffectInfoV1` — the description block below.
//    `name` is the only field you must give real text: it is what the GUI
//    shows next to the toggle and in the loop row.
// 2. `#[no_mangle] pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1`
//    — the entry point slooper looks for. One line, see the template.
// 3. The three functions wired into DESC:
//    - `create`     (sample_rate) -> state pointer. Called once per enabled
//                   instance. Return core::ptr::null_mut() to signal failure
//                   (the effect then passes audio through).
//    - `destroy`    (state)       — free what `create` allocated.
//    - `process`    (state, input_sample, params) -> output_sample.
//                   Called once per sample on the audio thread. MUST be
//                   allocation-free, must never block (no locks, I/O, sleep),
//                   and must never panic. `params` points to `n_params`
//                   f32 values (the sliders; read them, never write).
//
// ---------------------------------------------------------------------------
// OPTIONAL pieces
// ---------------------------------------------------------------------------
// - `author`, `description`: shown as hover text in the effects window.
//   Use core::ptr::null() when you have nothing to show.
// - Parameters: up to SLOOPER_MAX_PARAMS entries in the `params` array.
//   Each one becomes a labelled slider in the effect's AUTO child window,
//   clamped to [min, max], seeded with `default`. To ADD a parameter:
//   append an `EffectParamV1` entry, raise `n_params`, and read
//   `*params.add(K)` in `process`.
//   To REMOVE one: delete the entry, lower `n_params`, drop its use.
//   Keep the array indices and the `process` reads in sync — slooper
//   validates `n_params <= SLOOPER_MAX_PARAMS` but cannot know which
//   index you meant. A slider with min == max renders inert; that is fine.
// - A CUSTOM child GUI (curve displays, drag handles, live meters): export
//   the optional `slooper_effect_gui` symbol — see the GUI contract at
//   the bottom of this file. Everything else stays exactly as-is, so old
//   scripts are untouched by it.
//
// ---------------------------------------------------------------------------
// How to check your effect is VALID
// ---------------------------------------------------------------------------
// Slooper marks a script valid only when ALL of this succeeds:
//   * it compiles with the command above (no `extern crate`s beyond std/core;
//     include! of effect_api.rs must resolve — keep the script in the folder),
//   * the library exports `slooper_effect_info`,
//   * DESC.abi == SLOOPER_EFFECT_ABI, name is non-null UTF-8 text,
//     n_params <= SLOOPER_MAX_PARAMS, and all three function pointers are
//     non-null.
// You can dry-run the compile yourself in the effects folder:
//     rustc --edition 2021 --crate-type cdylib my_effect.rs
// and see every rejection reason in the effects window (invalid files are
// listed with the compiler/loader error). Inside slooper, toggling the
// effect on a loop proves `create` + `process` run live.
// ---------------------------------------------------------------------------

/// ABI version. Bumped only when this struct layout or the callback contract
/// changes incompatibly; slooper refuses effects built for another version.
pub const SLOOPER_EFFECT_ABI: u32 = 1;

/// Hard cap on params per effect (keeps the realtime param snapshot fixed
/// size; the auto GUI will not show more). Effects with a CUSTOM child GUI
/// (see the GUI contract at the bottom of this file) lay params out however
/// they like — the cap is on param count, not on widgets.
pub const SLOOPER_MAX_PARAMS: usize = 32;

/// One slider the auto child window rides per param: label + range + default.
#[repr(C)]
pub struct EffectParamV1 {
    /// Display label (required, non-null).
    pub label: *const core::ffi::c_char,
    /// Lowest allowed value; also the floor passed to `process`.
    pub min: f32,
    /// Highest allowed value. Must be >= `min`.
    pub max: f32,
    /// Value used when the effect is first enabled on a loop.
    pub default: f32,
}

/// The whole effect, as a C-ABI vtable. Plain data + plain function pointers
/// only — nothing from outside core/std may cross this boundary.
#[repr(C)]
pub struct EffectInfoV1 {
    /// Must be [`SLOOPER_EFFECT_ABI`].
    pub abi: u32,
    /// Display name in the GUI (REQUIRED, non-null).
    pub name: *const core::ffi::c_char,
    /// Optional author line (or null).
    pub author: *const core::ffi::c_char,
    /// Optional one-paragraph description (or null).
    pub description: *const core::ffi::c_char,
    /// Number of valid entries in `params` (0..=SLOOPER_MAX_PARAMS).
    pub n_params: u32,
    /// Param descriptors — null exactly when `n_params == 0`.
    pub params: *const EffectParamV1,
    /// Per-instance constructor. `sample_rate` is the engine rate in Hz (may
    /// be 0.0 when the audio backend was not up yet — size state lazily or
    /// guard divisions). Return null to fail (effect then passes audio
    /// through and shows an error).
    pub create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    /// Per-instance destructor. Always called once per successful `create`.
    pub destroy: extern "C" fn(state: *mut core::ffi::c_void),
    /// Per-sample processing on the audio thread: one mono sample in, one
    /// out. `params` holds `n_params` f32 values, updated lock-free by the
    /// GUI at audio-block boundaries. Keep it cheap: this runs at the full
    /// sample rate while the loop plays.
    pub process:
        extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32,
}

// ---------------------------------------------------------------------------
// ABI v2: sidechain input
// ---------------------------------------------------------------------------
// A v2 effect is a v1 effect that ALSO can consume a per-track sidechain
// signal (SC): the summed audio of other tracks the user routed to this
// track's strip (mixer: "sidechain in"). Sidechaining is how one track
// keys another's dynamics — a kick ducking a pad, a vocal keying a reverb.
//
// A v2 script exports ONE extra symbol alongside the usual v1 one:
//
//     #[no_mangle]
//     pub extern "C" fn slooper_effect_info_v2() -> *const EffectInfoV2 { &DESC2 }
//
// The loader tries that symbol first and falls back to
// `slooper_effect_info` (v1) forever — existing scripts never change.
// When the host has no SC feed routed to the track, `sc` is exactly 0.0.
//
// Feed rules (docs/SIDECHAIN.md has the full picture):
//   * `sc` is MONO — the post-chain sum of every source track selected on
//     this strip, taken from the PREVIOUS render block (one block of
//     detection latency, ~21 ms at 1024/48k; standard practice, inaudible
//     on any envelope).
//   * SC never loops within a block, so mutual sends can never self-oscillate.
//   * Silence on the SC bus is a hard 0.0 — gate on `sc != 0.0` only if a
//     plugin-semantics "no key means bypass" is wanted; envelope followers
//     just see silence.
pub const SLOOPER_EFFECT_ABI_V2: u32 = 2;

/// A v1 layout with one extra entry point appended. The first fields are
/// bit-identical to [`EffectInfoV1`] (a v2 struct may be read by an older
/// host as valid v1 — the extra pointer at the end is never touched).
#[repr(C)]
pub struct EffectInfoV2 {
    /// Must be [`SLOOPER_EFFECT_ABI_V2`].
    pub abi: u32,
    pub name: *const core::ffi::c_char,
    pub author: *const core::ffi::c_char,
    pub description: *const core::ffi::c_char,
    pub n_params: u32,
    pub params: *const EffectParamV1,
    pub create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    pub destroy: extern "C" fn(state: *mut core::ffi::c_void),
    pub process:
        extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32,
    /// Same as `process`, plus the sidechain sample for THIS output sample.
    /// A v2 effect MUST do its real work here; `process` is kept as the
    /// graceful fallback (return `input`) for hosts that predate v2.
    pub process_sc:
        extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32, sc: f32) -> f32,
}

// ---------------------------------------------------------------------------
// ABI v3: stereo field
// ---------------------------------------------------------------------------
// A v3 effect is a v2 effect that ALSO processes a stereo pair. Export a
// third entry point beside the other two:
//
//     #[no_mangle]
//     pub extern "C" fn slooper_effect_info_v3() -> *const EffectInfoV3 { &DESC3 }
//
// The loader tries v3 first, then v2, then v1 — existing scripts never
// change. The v3 prefix is bit-identical to v2, so a v2 host still reads
// a v3 struct as a valid v2 one.
//
// What the host guarantees (tier-3 rule, mirror of the v2 contract):
//   * The chain bus turns STEREO only in the sense that `process_st`
//     receives a left and a right sample and returns a left plus a right
//     (through `out_r`). Sources upstream are MONO: the first slot of a
//     chain always sees l == r, and a mono stage is fed the mid
//     (l+r)*0.5 and has its output replace BOTH sides.
//   * The pair the effect emits survives to the track pan (dual-mono
//     law: l*pan_l into the mix left, r*pan_r into the mix right), so a
//     spreader/panner's image is real — but only in the LIVE mix.
//     Offline renders and preview bounces are mono: they feed the stereo
//     slot (mid, mid) and keep its LEFT output (documented fold).
//   * `sc` keeps its full v2 meaning (0.0 when no send is routed).
//   * A v3 effect MUST still export v1 `process` (return `input`) and,
//     in practice, a real `process_sc` too — every older entry stays a
//     valid graceful fallback.
//   * Time-based math stays SAMPLE-exact: block sizes run ~1 ms (RT) to
//     seconds (preview bounce).
pub const SLOOPER_EFFECT_ABI_V3: u32 = 3;

/// A v2 layout with one extra entry point appended (prefix bit-identical
/// to [`EffectInfoV2`]).
#[repr(C)]
pub struct EffectInfoV3 {
    /// Must be [`SLOOPER_EFFECT_ABI_V3`].
    pub abi: u32,
    pub name: *const core::ffi::c_char,
    pub author: *const core::ffi::c_char,
    pub description: *const core::ffi::c_char,
    pub n_params: u32,
    pub params: *const EffectParamV1,
    pub create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    pub destroy: extern "C" fn(state: *mut core::ffi::c_void),
    pub process:
        extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32,
    pub process_sc:
        extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32, sc: f32) -> f32,
    /// Stereo step. `l`/`r` are this sample's chain pair, `sc` the v2
    /// sidechain sample. Return the LEFT output; write the RIGHT through
    /// `out_r` (never null — the host passes a real address). Do not
    /// assume l == r: your own output pair feeds the NEXT slot's input.
    pub process_st: extern "C" fn(
        state: *mut core::ffi::c_void,
        l: f32,
        r: f32,
        params: *const f32,
        sc: f32,
        out_r: *mut f32,
    ) -> f32,
}

// ---------------------------------------------------------------------------
// ABI v4: transport sync + param display metadata
// ---------------------------------------------------------------------------
// A v4 effect is a v3 effect that ALSO receives transport/song position
// pushes and (optionally) publishes display metadata for its sliders.
// Export a fourth entry point beside the other three:
//
//     #[no_mangle]
//     pub extern "C" fn slooper_effect_info_v4() -> *const EffectInfoV4 { &DESC4 }
//
// The loader tries v4 first, then v3, v2, v1 — existing scripts never
// change. The v4 prefix is bit-identical to v3, so a v3 host still reads
// a v4 struct as a valid v3 one. Because the v3 prefix is REQUIRED, every
// v4 script implements all four entry points — a script that wants ONLY
// the metadata (no real stereo processing) implements `process_st` as the
// documented mono graceful fallback (process the mid, return it for both
// sides) and sets the appended `stereo` flag to 0.
//
// Transport sync (the Scratcher/Gross-Beat-class capability):
//   * When `set_sync` is non-null the host calls it ONCE PER AUDIO BLOCK,
//     on the audio thread, before any `process`/`process_st` sample call
//     of that block, with a `SyncInfoV1` describing the block start.
//     Offline renders and preview bounces push it once per chain pass
//     with `playing = 1` and the pass's window origin as `pos_samples`.
//   * The push marks the block START only. Between pushes, a tempo-synced
//     script advances its own phase SAMPLE-EXACTLY:
//         beats_per_sample = bpm / (60.0 * sample_rate)
//     — at `set_sync` store `self.beats = s.pos_beats; self.in_block = 0`,
//     in process: `let b = self.beats + self.in_block as f64 * bps;
//     self.in_block += 1;`. Block sizes run ~1 ms (RT) to seconds
//     (offline passes), and the self-advance keeps every scheduling
//     sample-exact across the whole pass.
//   * `playing == 0` means the transport is stopped: freeze your phase
//     (do NOT self-advance), and re-anchor whenever a push arrives.
//     Positions are timeline samples from project origin; a loop wrap or
//     seek shows up as a jump in `pos_samples` on the next push.
//
// Param metadata: `param_meta` points at `n_params` `ParamMetaV1`
// entries (null = none, everything default). It changes ONLY how the
// host's AUTO slider window looks and snaps; scripted GUIs draw whatever
// they like, and `process` must still quantize mode params itself (the
// labels mirror the quantization, they do not create it).
pub const SLOOPER_EFFECT_ABI_V4: u32 = 4;

/// `SyncInfoV1::abi` value (versioned independently of the effect ABI).
pub const SLOOPER_SYNC_ABI: u32 = 1;

/// One transport/song position push. Plain data; valid only for the
/// duration of the `set_sync` call — COPY what you need into your state.
#[repr(C)]
pub struct SyncInfoV1 {
    /// Must be [`SLOOPER_SYNC_ABI`].
    pub abi: u32,
    /// 1 = the transport is running for this block; 0 = stopped (the
    /// host still pushes when stopped/seeking, so you can re-anchor).
    pub playing: u32,
    /// Engine sample rate in Hz (0.0 only before the backend is up).
    pub sample_rate: f64,
    /// Project tempo in BPM (the metro/automation tempo — automation
    /// tempo clips change it live).
    pub bpm: f64,
    /// Time-signature numerator (beats per bar, default 4).
    pub beats_per_bar: f64,
    /// Timeline position of THIS BLOCK'S FIRST SAMPLE, samples from
    /// project origin (never negative).
    pub pos_samples: i64,
    /// Same position in musical beats: pos_samples * bpm / (60 * sr).
    pub pos_beats: f64,
}

// `SyncInfoV1` unit conventions for `ParamMetaV1::unit`.
pub const SLOOPER_UNIT_NONE: u32 = 0;
pub const SLOOPER_UNIT_PERCENT: u32 = 1;
pub const SLOOPER_UNIT_DB: u32 = 2;
pub const SLOOPER_UNIT_HZ: u32 = 3;
pub const SLOOPER_UNIT_MS: u32 = 4;
pub const SLOOPER_UNIT_SEMITONES: u32 = 5;
pub const SLOOPER_UNIT_BEATS: u32 = 6;
pub const SLOOPER_UNIT_TIMES: u32 = 7; // multiplier, rendered "1.50x"
pub const SLOOPER_UNIT_SAMPLES: u32 = 8;
pub const SLOOPER_UNIT_SECONDS: u32 = 9;

/// Display metadata for ONE slider (the auto window only).
#[repr(C)]
pub struct ParamMetaV1 {
    /// One of the `SLOOPER_UNIT_*` constants (0 = plain number).
    pub unit: u32,
    /// Reserved, write 0.
    pub flags: u32,
    /// Non-zero: the auto window snaps the slider to `min + k*step` and
    /// scripts MUST quantize identically inside `process`, since an auto
    /// window can never be the only quantizer.
    pub step: f32,
    /// Enum labels, or null. With `n_labels = N >= 1` the param is an
    /// enum of N equally spaced values across [min, max] (N = 1 or
    /// min == max pins one value); label `i` names value
    /// `min + i*(max-min)/(N-1)`. The auto window draws a picker and
    /// shows the label instead of a number.
    pub labels: *const *const core::ffi::c_char,
    /// Number of valid entries in `labels` (max 32).
    pub n_labels: u32,
}

/// A v3 layout with the sync/metadata entries appended (prefix
/// bit-identical to [`EffectInfoV3`]).
#[repr(C)]
pub struct EffectInfoV4 {
    /// Must be [`SLOOPER_EFFECT_ABI_V4`].
    pub abi: u32,
    pub name: *const core::ffi::c_char,
    pub author: *const core::ffi::c_char,
    pub description: *const core::ffi::c_char,
    pub n_params: u32,
    pub params: *const EffectParamV1,
    pub create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    pub destroy: extern "C" fn(state: *mut core::ffi::c_void),
    pub process:
        extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32,
    pub process_sc:
        extern "C" fn(state: *mut core::ffi::c_void, input: f32, params: *const f32, sc: f32) -> f32,
    pub process_st: extern "C" fn(
        state: *mut core::ffi::c_void,
        l: f32,
        r: f32,
        params: *const f32,
        sc: f32,
        out_r: *mut f32,
    ) -> f32,
    /// 1 = `process_st` really processes the pair (the mixer/UI shows
    /// the (ST) tag, the bundle gate checks decorrelation); 0 = the mono
    /// graceful fallback (no tag — do not claim stereo you do not do).
    pub stereo: u32,
    /// Block-rate transport push, called on the audio thread BEFORE any
    /// sample call of the block (null = no sync; metadata-only v4 is a
    /// legal effect). See the v4 header for the self-advance contract.
    pub set_sync: Option<extern "C" fn(state: *mut core::ffi::c_void, sync: *const SyncInfoV1)>,
    /// Per-param display metadata, `n_params` entries, or null.
    pub param_meta: *const ParamMetaV1,
}

// ---------------------------------------------------------------------------
// OPTIONAL: the child GUI (editor window) contract
// ---------------------------------------------------------------------------
// By default slooper gives every effect a child window with one slider
// per param — effects window / loop-row button → window, no GUI code
// needed. An effect that wants a CUSTOM editor exports ONE extra symbol:
//
//     #[no_mangle]
//     pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 { &GUI }
//
// with a `GUI: EffectGuiV1` static (its own version field:
// SLOOPER_EFFECT_GUI_ABI). Effects without this symbol keep the auto
// slider window; everything else in this file is unchanged for them.
//
// The child GUI is IMMEDIATE MODE, like the primitives below suggest:
// slooper calls your `draw` once per frame while the window is open.
// You receive
//   * `gs`     — your own per-window state (from `gui_create`), and
//   * `state`  — the AUDIO instance state (the same pointer `create`
//                returned for this loop's instance), so you can show live
//                data `process` publishes, and
//   * `frame`  — window size, mouse, time — and a `GuiApiV1` vtable of
//                drawing/interaction calls. Call them at the coordinates
//                you want (origin = canvas top-left, y down, points).
// Interaction returns 1 the frame the user changed something; you then
// push new values with `api.set_param(k, v)` — the same params `process`
// reads, so audio follows automatically (and slooper persists them).
//
// GUI-thread rules (relaxed vs. process, but real):
//   * Allocation, format!, Vec are OK here (60 fps UI code).
//   * NEVER block, and NEVER free instance state from here.
//   * `state` is owned by the audio thread: read/write ONLY through
//     atomics you placed in it (levels for a spectrum, a "gui open" flag
//     …). The editor holds an instance reference for the whole window
//     lifetime, so the pointer stays valid until `gui_destroy` returns.
//   * Keep drawing inside 0..frame.width / 0..frame.height.
// ---------------------------------------------------------------------------

/// Child-GUI ABI version (independent of the audio ABI above).
pub const SLOOPER_EFFECT_GUI_ABI: u32 = 1;

/// Everything one `draw` call knows about the current frame. Plain data.
#[repr(C)]
pub struct GuiFrameV1 {
    /// Canvas size in points; lay out relative to these to stay resizable.
    pub width: f32,
    pub height: f32,
    /// Pointer position in canvas coordinates (valid when `mouse_inside`).
    pub mouse_x: f32,
    pub mouse_y: f32,
    /// 1 while the pointer is over the canvas / button pressed / pressed
    /// this frame / released this frame (left button).
    pub mouse_inside: u32,
    pub mouse_down: u32,
    pub mouse_pressed: u32,
    pub mouse_released: u32,
    /// Seconds since the window opened, and the previous frame's duration
    /// (clamped to sane values) — drive animations and envelope decays.
    pub time: f64,
    pub dt: f64,
    /// Frames drawn so far in this window.
    pub frame: u64,
    /// Audio engine sample rate in Hz (0.0 until the backend is up).
    pub sample_rate: f64,
}

/// Immediate-mode canvas + param access. Every function takes the `ctx`
/// from [`GuiApiV1::ctx`] as its first argument — pass it through
/// untouched. Coordinates are canvas points (origin top-left, y down).
/// Colors are RGBA floats 0..1.
#[repr(C)]
pub struct GuiApiV1 {
    /// Must be [`SLOOPER_EFFECT_GUI_ABI`] (checked for you).
    pub abi: u32,
    /// Per-frame context token. Pass it as the first argument of EVERY
    /// call below: `(api.line)(api.ctx, …)`. Valid only during `draw`.
    pub ctx: *mut core::ffi::c_void,
    // ---- pure drawing (no interaction, drawn this frame only) ----
    /// One line segment.
    pub line: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x0: f32, y0: f32, x1: f32, y1: f32,
        width: f32, r: f32, g: f32, b: f32, a: f32,
    ),
    /// Polyline through `n` xy pairs (interleaved `[x0,y0,x1,y1,…]`);
    /// `closed` != 0 closes it (polygon outline).
    pub polyline: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        xy: *const f32, n: u32, closed: u32,
        width: f32, r: f32, g: f32, b: f32, a: f32,
    ),
    /// Filled / outlined rectangle at (x, y) with size (w, h).
    pub rect_filled: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, w: f32, h: f32, r: f32, g: f32, b: f32, a: f32,
    ),
    pub rect_stroke: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, w: f32, h: f32,
        width: f32, r: f32, g: f32, b: f32, a: f32,
    ),
    /// Text at (x, y), `size` points tall; `align` 0=left 1=center 2=right
    /// around x, vertically centred on y. UTF-8.
    pub text: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, size: f32, align: u32,
        r: f32, g: f32, b: f32, a: f32, s: *const core::ffi::c_char,
    ),
    /// Width of `s` in points at `size` — lay out labels without guessing.
    pub text_width: extern "C" fn(
        ctx: *mut core::ffi::c_void, size: f32, s: *const core::ffi::c_char,
    ) -> f32,
    // ---- interaction widgets (return 1 the frame the user changed) ----
    /// Horizontal slider `w × h` at (x, y), label baked in. `curve`:
    /// 1.0 linear, 0.0 logarithmic (needs min > 0), else exponent.
    /// Writes the new value through `value` when it returns 1.
    pub slider: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, w: f32, h: f32, label: *const core::ffi::c_char,
        value: *mut f32, min: f32, max: f32, curve: f32,
    ) -> u32,
    /// Round knob of diameter `size` at (x, y) (drag up/down to edit;
    /// label under it). Same value mechanics as `slider`.
    pub knob: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, size: f32, label: *const core::ffi::c_char,
        value: *mut f32, min: f32, max: f32, curve: f32,
    ) -> u32,
    /// Push button `w` wide (auto height) at (x, y). Returns 1 when clicked.
    pub button: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, w: f32, label: *const core::ffi::c_char,
    ) -> u32,
    /// Checkbox at (x, y) — TOP-LEFT anchor like `button`: the box starts
    /// at x and the widget is centred inside a 20 px row band starting at
    /// y, so it lines up with buttons placed at the same y. `on` != 0
    /// draws checked, toggled writes 0/1.
    pub checkbox: extern "C" fn(
        ctx: *mut core::ffi::c_void,
        x: f32, y: f32, label: *const core::ffi::c_char, on: *mut f32,
    ) -> u32,
    // ---- audio param access (the ONLY channel to `process`) ----
    /// Current value of param `k` (0..`n_params`); 0.0 out of range.
    pub get_param: extern "C" fn(ctx: *mut core::ffi::c_void, k: u32) -> f32,
    /// Ride param `k` (clamped to its declared range by slooper;
    /// remembers the change for persistence).
    pub set_param: extern "C" fn(ctx: *mut core::ffi::c_void, k: u32, v: f32),
    /// How many params this effect has.
    pub n_params: u32,
}

/// The optional editor window, as a C-ABI vtable. Like `EffectInfoV1`:
/// plain data + plain function pointers, no Rust types cross the boundary.
#[repr(C)]
pub struct EffectGuiV1 {
    /// Must be [`SLOOPER_EFFECT_GUI_ABI`].
    pub abi: u32,
    /// Preferred canvas size in points (0 = let slooper choose ~480×320).
    pub want_width: f32,
    pub want_height: f32,
    /// Per-open-window constructor (called when the child window opens;
    /// NOT per loop — one editor window at a time). Null to fall back to
    /// the auto slider window. Return null to fail (auto window used).
    pub create: extern "C" fn(sample_rate: f64) -> *mut core::ffi::c_void,
    /// Draw one frame. `gs` is `create`'s state, `state` the audio
    /// instance state of the loop being edited (never free it here!),
    /// `frame` the frame info, `api` valid for this call only.
    pub draw: extern "C" fn(
        gs: *mut core::ffi::c_void,
        state: *mut core::ffi::c_void,
        frame: *const GuiFrameV1,
        api: *const GuiApiV1,
    ),
    /// Window closed. Free what `create` allocated. The audio instance is
    /// guaranteed alive until this returns.
    pub destroy: extern "C" fn(gs: *mut core::ffi::c_void),
}

// Compile-time prefix proof (this file is compiled into BOTH the host and
// every script — the offsets below are enforced on both sides, so a layout
// drift in any generation is a compile error, never a silent misread).
const _: () = assert!(core::mem::offset_of!(EffectInfoV2, process_sc) > core::mem::offset_of!(EffectInfoV2, process));
const _: () = assert!(core::mem::offset_of!(EffectInfoV3, process_st) > core::mem::offset_of!(EffectInfoV3, process_sc));
const _: () = assert!(core::mem::size_of::<EffectInfoV2>()
    == core::mem::offset_of!(EffectInfoV3, process_st));
const _: () = assert!(core::mem::offset_of!(EffectInfoV4, process_st)
    == core::mem::offset_of!(EffectInfoV3, process_st));
const _: () = assert!(core::mem::offset_of!(EffectInfoV4, stereo)
    == core::mem::size_of::<EffectInfoV3>());
const _: () = assert!(core::mem::offset_of!(EffectInfoV4, set_sync)
    == core::mem::offset_of!(EffectInfoV4, stereo) + 8);
const _: () = assert!(core::mem::offset_of!(EffectInfoV4, param_meta)
    == core::mem::offset_of!(EffectInfoV4, set_sync) + 8);

// The description blocks live in read-only statics and are only ever shared
// one way (effect -> loader); nothing in Rust mutates them after the library
// is loaded, so cross-thread sharing is exactly the contract.
unsafe impl Sync for EffectParamV1 {}
unsafe impl Sync for EffectInfoV1 {}
unsafe impl Sync for EffectInfoV2 {}
unsafe impl Sync for EffectInfoV3 {}
unsafe impl Sync for EffectInfoV4 {}
unsafe impl Sync for SyncInfoV1 {}
unsafe impl Sync for ParamMetaV1 {}
unsafe impl Sync for GuiApiV1 {}
unsafe impl Sync for EffectGuiV1 {}
