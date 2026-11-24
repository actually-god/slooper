// ============================================================ slooper ==
// template.rs — starter effect for the slooper effect framework.
//
// Copy this file inside your effects folder, rename it, and edit. Every
// effect is a single standalone Rust file that slooper compiles and
// loads at runtime. This template starts as an audible-but-subtle tremolo
// (an amplitude wobble); with depth = 0 it is an exact pass-through, so
// swap its guts for your own algorithm and keep the scaffolding.
//
// ---------------------------------------------------------------------------
// THE FIVE THINGS AN EFFECT NEEDS (see effect_api.rs for the full contract)
// ---------------------------------------------------------------------------
// 1. `include!("effect_api.rs");` — pulls in the shared ABI structs. The
//    file lives next to this script (slooper writes it there); keep this
//    script inside the effects folder or the include stops resolving.
// 2. A state struct — everything this effect remembers per loop-instance
//    (one instance per loop that enables it; loops never share state).
// 3. Three `extern "C"` functions: create / destroy / process.
// 4. A `DESC: EffectInfoV1` static wiring it together — the ONLY required
//    field with content is `name` (shown in the GUI). author/description
//    are optional hover text; null when unused.
// 5. The `slooper_effect_info` entry point — one fixed line; do not edit.
//
// That is all a v1 effect needs. What an effect CAN also get is below.
//
// ---------------------------------------------------------------------------
// THE OPTIONAL LADDER — v2 / v3 / v4 / custom GUI (skeleton at the bottom)
// ---------------------------------------------------------------------------
// Each rung exports ONE extra symbol and keeps the older ones working; the
// loader probes v4, v3, v2, v1 and runs the NEWEST table it finds, so an
// effect can grow without breaking older hosts:
//   * v2 — sidechain key. Export `slooper_effect_info_v2`; the real work
//     moves to `process_sc(state, input, params, sc)`, where `sc` is the
//     key signal other tracks route to this strip (0.0 when nothing is).
//   * v3 — stereo pair. Export `slooper_effect_info_v3`; the real work
//     moves to `process_st(state, l, r, params, sc, out_r)`: return LEFT,
//     write RIGHT through `out_r`. Your output pair survives to the track
//     pan, so panners/spreaders get a real image (offline renders fold
//     mono — see the v3 header in effect_api.rs).
//   * v4 — transport sync + param display metadata. Export
//     `slooper_effect_info_v4`: `set_sync` pushes a `SyncInfoV1` once per
//     audio block (bpm, bar position — for sample-exact tempo-locked
//     modulation), and `param_meta: ParamMetaV1` per slider declares its
//     unit (`SLOOPER_UNIT_HZ`/`_DB`/`_MS`/`_PERCENT`/`_TIMES`…), an
//     integer/value `step` snap, and enum `labels` the auto window draws
//     as a picker. Metadata only changes the LOOKS — `process` must
//     quantize mode/step params itself, identically to the labels.
//     A v4 table also carries the v2/v3 entries plus a `stereo` flag:
//     declare 1 only if `process_st` truly decorrelates the pair.
//   * GUI — custom child editor window. Export `slooper_effect_gui` with
//     an `EffectGuiV1` (knobs/sliders/meters through `GuiApiV1`); without
//     it slooper draws the auto slider window — nothing else changes.
// The bottom of this file carries a FULL, COMPILING v4 + GUI version of
// this very tremolo as a commented skeleton. Uncomment it to adopt (or
// just copy the pieces you need); read effect_api.rs for each contract.
//
// ---------------------------------------------------------------------------
// ADDING / REMOVING SETTINGS (sliders the user rides per loop)
// ---------------------------------------------------------------------------
// Each slider = one EffectParamV1 entry in DESC.params: label, min, max,
// default. The GUI renders them (in order) on the loop row. Read them in
// `process` as `*params.add(K)` where K is the entry index. Adding a slider:
//   1. append an EffectParamV1 { .. } to the array below,
//   2. raise `n_params` in DESC,
//   3. read the new index in `process`.
// Removing: the reverse. Values arrive pre-clamped to [min, max]; the engine
// updates them between audio blocks, never mid-`process`. Up to
// SLOOPER_MAX_PARAMS (32) total.
//
// ---------------------------------------------------------------------------
// HOW SLOOPER DECIDES THIS FILE IS "VALID"
// ---------------------------------------------------------------------------
// - it compiles:  rustc --edition 2021 --crate-type cdylib template.rs
//   (run that inside the effects folder to check without restarting; only
//   std/core are available — no external crates),
// - the library exports slooper_effect_info (and, for v2..v4, the newest
//   slooper_effect_info_vN you declared — the loader probes newest-first),
// - DESC.abi matches the newest SLOOPER_EFFECT_ABI* you export, name
//   non-empty, n_params <= SLOOPER_MAX_PARAMS (32), all fn pointers set.
// Anything else (typo, bad type, missing entry point) and the effects
// window lists the file in red with the compiler/loader error — the app and
// every other effect stay fine. Re-edit and hit Rescan.
//
// ---------------------------------------------------------------------------
// REALTIME RULES (process runs on the audio thread, every sample)
// ---------------------------------------------------------------------------
// No allocations, no locks, no I/O, no panicking, keep the math small.
// `create`/`destroy` may allocate. Read `sample_rate` in `create` to size
// time-based state (it can be 0.0 when the backend was not up yet — the
// effect is rebuilt with the real rate once audio starts, but guard maths
// against 0 just in case).
// ---------------------------------------------------------------------------

// The shared ABI (structs, constants, validity rules) — written next to
// this file by slooper on every start.
include!("effect_api.rs");

// ---------------------------------------------------------------------------
// 2. State — one per enabled (loop, effect-slot) instance.
// ---------------------------------------------------------------------------
struct Tremolo {
    /// Engine sample rate captured at create (Hz; 0 until known).
    sr: f64,
    /// LFO phase in radians, advanced by `process`.
    phase: f64,
}

/// Index of each slider below — keep in sync with DESC.n_params.
const P_DEPTH: usize = 0; // 0..1   tremolo depth (0 = pass-through)
const P_RATE: usize = 1; //  0.1..20 Hz wobble speed
const P_MIX: usize = 2; //  0..1   dry/wet (1 = fully wobbled)
const P_OUT: usize = 3; //  0..2   output trim after the effect

// ---------------------------------------------------------------------------
// 3a. create — allocate instance state. Return null to fail.
// ---------------------------------------------------------------------------
extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    // Box into the heap; the raw pointer is the effect's opaque state.
    Box::into_raw(Box::new(Tremolo { sr: sample_rate.max(1.0), phase: 0.0 }))
        as *mut core::ffi::c_void
}

// ---------------------------------------------------------------------------
// 3b. destroy — free what create allocated (called once per instance).
// ---------------------------------------------------------------------------
extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        // Reclaim the Box from create; dropping it frees the memory.
        drop(unsafe { Box::from_raw(state as *mut Tremolo) });
    }
}

// ---------------------------------------------------------------------------
// 3c. process — the audio thread hot path, one mono sample at a time.
//     `params` points at the slider values, indexed as declared in DESC.
// ---------------------------------------------------------------------------
extern "C" fn process(
    state: *mut core::ffi::c_void,
    input: f32,
    params: *const f32,
) -> f32 {
    if state.is_null() {
        return input; // never happen (engine drops null instances) — be safe
    }
    let st = unsafe { &mut *(state as *mut Tremolo) };

    // Slider values (pre-clamped by the engine).
    let depth = unsafe { *params.add(P_DEPTH) }.clamp(0.0, 1.0);
    let rate = unsafe { *params.add(P_RATE) }.clamp(0.1, 20.0);
    let mix = unsafe { *params.add(P_MIX) }.clamp(0.0, 1.0);
    let out = unsafe { *params.add(P_OUT) }.clamp(0.0, 2.0);

    // Depth 0 => exact pass-through: skip the oscillator entirely.
    if depth <= 0.0 || mix <= 0.0 {
        return input * out;
    }

    // Advance the LFO (one 32-bit sin per sample is fine for a looper;
    // keep heavier math out of here or amortise it across samples).
    let phase_step = core::f64::consts::TAU * rate as f64 / st.sr;
    st.phase = (st.phase + phase_step) % core::f64::consts::TAU;
    let lfo = 1.0 - depth as f32 * (0.5 - 0.5 * (st.phase as f32).cos());

    // Dry/wet blend, then output trim.
    (input * (1.0 - mix) + input * lfo * mix) * out
}

// ---------------------------------------------------------------------------
// 4. Description — what the GUI shows. REQUIRED fields: name + the three
//    functions. Set author/description to null to omit them.
// ---------------------------------------------------------------------------
static PARAMS: [EffectParamV1; 4] = [
    EffectParamV1 { label: c"depth".as_ptr(), min: 0.0, max: 1.0, default: 0.25 },
    EffectParamV1 { label: c"rate".as_ptr(), min: 0.1, max: 20.0, default: 4.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
    EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"Template Tremolo".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Starter effect: depth 0 is an exact pass-through. Copy template.rs and swap the maths in process().".as_ptr(),
    n_params: 4,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

// ---------------------------------------------------------------------------
// 5. Entry point — fixed signature; slooper's loader looks this symbol up
//    by name. Never rename, never make it generic, never add args.
// ---------------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}

// ===========================================================================
// THE OPTIONAL LADDER IN CODE — a full v4 + custom-GUI version of THIS
// tremolo, kept as a commented skeleton so the v1 starter stays the default.
// ===========================================================================
// It demonstrates every rung at once: bpm-locked LFO (`set_sync`),
// sidechain-keyed depth (`process_sc`), true stereo spread (`process_st`
// with `stereo: 1`), an enum picker + units + step snap (`param_meta`),
// and a knob editor (`slooper_effect_gui`). Exporting it next to the v1
// DESC above is legal and how the loader is designed: v4-capable hosts
// pick the newest symbol, the v1 table stays as the graceful fallback.
//
// Everything between SKELETON-START and SKELETON-END is real Rust behind
// `// `: a test (fx::tests::v144_template_v4_skeleton_compiles_and_loads)
// strips the markers, compiles the file with the block uncommented, and
// loads it through the real loader — so this skeleton cannot rot quietly
// the way the old v1-only docs did.
//
// SKELETON-START ----------------------------------------------------------------
// // ---------------------------------------------------------------------------
// // ABI v4 table — the SAME tremolo, grown: bpm-locked wobble (set_sync),
// // sidechain-keyed depth (process_sc), true stereo (process_st), and
// // slider metadata (units / enum picker / step snap). A v4 host exports all
// // four symbols; its loader probes v4, v3, v2, v1 and uses the newest, so
// // the v1 DESC above stays untouched and older hosts still work.
// // ---------------------------------------------------------------------------
//
// use core::ffi::c_void; // shorthand for the signatures below
//
// /// v4 state. Older tables are never re-read through the new struct: each
// /// exported table pairs with its own create/destroy/process functions.
// struct Tremolo4 {
//     sr: f64,
//     bpm: f64,
//     /// Transport beats at the last `set_sync` push (the block anchor).
//     beats: f64,
//     /// Samples consumed since that anchor — the sample-exact self-clock.
//     in_block: u32,
//     playing: bool,
// }
//
// extern "C" fn create4(sample_rate: f64) -> *mut c_void {
//     Box::into_raw(Box::new(Tremolo4 {
//         sr: sample_rate.max(1.0),
//         bpm: 120.0,
//         beats: 0.0,
//         in_block: 0,
//         playing: false,
//     })) as *mut c_void
// }
//
// extern "C" fn destroy4(state: *mut c_void) {
//     if !state.is_null() {
//         drop(unsafe { Box::from_raw(state as *mut Tremolo4) });
//     }
// }
//
// /// v4 `set_sync`: ONE push per audio block, on the audio thread, BEFORE
// /// the block's samples. `SyncInfoV1` is only valid during this call —
// /// COPY what you need; self-advance per sample between pushes.
// extern "C" fn set_sync4(state: *mut c_void, sync: *const SyncInfoV1) {
//     if state.is_null() || sync.is_null() {
//         return;
//     }
//     let st = unsafe { &mut *(state as *mut Tremolo4) };
//     let s = unsafe { &*sync };
//     st.sr = if s.sample_rate > 0.0 { s.sample_rate } else { st.sr };
//     st.bpm = s.bpm.max(1.0);
//     if s.playing != 0 {
//         st.beats = s.pos_beats; // re-anchor at the block start
//         st.in_block = 0;
//         st.playing = true;
//     } else {
//         st.playing = false; // stopped/seeking: freeze the phase
//     }
// }
//
// /// Index of each slider (same order as PARAMS4 below).
// const P4_DEPTH: usize = 0; // 0..100 % wobble depth
// const P4_RATE: usize = 1; //  0.25..8 wobbles per BEAT (bpm-locked)
// const P4_SHAPE: usize = 2; // 0 = sine, 1 = triangle (enum picker below)
// const P4_OUT: usize = 3; //  0..2x output trim, stepped by 0.25
//
// impl Tremolo4 {
//     /// Beat position of the NEXT sample; call ONCE per sample — a stereo
//     /// stage must feed both channels the same position.
//     fn tick(&mut self) -> f64 {
//         if !self.playing {
//             return self.beats;
//         }
//         let bps = self.bpm / (60.0 * self.sr);
//         let b = self.beats + self.in_block as f64 * bps;
//         self.in_block += 1;
//         b
//     }
// }
//
// /// The shared maths at transport beat `b`. `phase_off` offsets the LFO.
// fn wob(b: f64, params: *const f32, phase_off: f64, sc: f32, input: f32) -> f32 {
//     let depth = (unsafe { *params.add(P4_DEPTH) } * 0.01).clamp(0.0, 1.0);
//     let rate = unsafe { *params.add(P4_RATE) }.clamp(0.25, 8.0);
//     // The enum picker offers exactly {0, 1} — quantize like it snaps.
//     let tri = unsafe { *params.add(P4_SHAPE) } > 0.5;
//     // Mirror the declared step = 0.25 (a custom GUI could feed any value,
//     // so process is the one quantizer that always runs).
//     let out = (unsafe { *params.add(P4_OUT) } / 0.25).round() * 0.25;
//     // Sidechain key: |sc| raises the depth (0.0 when no send is routed).
//     let depth = depth.max(sc.abs().clamp(0.0, 1.0));
//
//     let mut p = (b * rate as f64 + phase_off) % 1.0;
//     if p < 0.0 {
//         p += 1.0;
//     }
//     // Triangle in [-1, 1] starting where the cosine starts (value 1).
//     let wave = if tri {
//         4.0 * (p - 0.5).abs() - 1.0
//     } else {
//         (core::f64::consts::TAU * p).cos()
//     };
//     input * (1.0 - depth * (0.5 - 0.5 * wave as f32)) * out
// }
//
// /// v1-shaped graceful fallback (hosts that predate the v4 table).
// extern "C" fn process4(state: *mut c_void, input: f32, params: *const f32) -> f32 {
//     if state.is_null() {
//         return input;
//     }
//     let st = unsafe { &mut *(state as *mut Tremolo4) };
//     let b = st.tick();
//     wob(b, params, 0.0, 0.0, input)
// }
//
// /// v2 entry point: real work, with `sc` = this track's sidechain sample.
// extern "C" fn process_sc4(
//     state: *mut c_void,
//     input: f32,
//     params: *const f32,
//     sc: f32,
// ) -> f32 {
//     if state.is_null() {
//         return input;
//     }
//     let st = unsafe { &mut *(state as *mut Tremolo4) };
//     let b = st.tick();
//     wob(b, params, 0.0, sc, input)
// }
//
// /// v3 entry point: the pair. The beat is read ONCE and feeds both
// /// channels; RIGHT's wobble lags a quarter-beat, so the output pair is
// /// genuinely decorrelated — that is what `stereo: 1` claims.
// extern "C" fn process_st4(
//     state: *mut c_void,
//     l: f32,
//     r: f32,
//     params: *const f32,
//     sc: f32,
//     out_r: *mut f32,
// ) -> f32 {
//     if state.is_null() {
//         unsafe { *out_r = r };
//         return l;
//     }
//     let st = unsafe { &mut *(state as *mut Tremolo4) };
//     let b = st.tick();
//     let left = wob(b, params, 0.0, sc, l);
//     unsafe { *out_r = wob(b, params, 0.25, sc, r) };
//     left
// }
//
// /// PARAMS4: same sliders, but depth now rides 0..100 % (see META4).
// static PARAMS4: [EffectParamV1; 4] = [
//     EffectParamV1 { label: c"depth".as_ptr(), min: 0.0, max: 100.0, default: 25.0 },
//     EffectParamV1 { label: c"wob/beat".as_ptr(), min: 0.25, max: 8.0, default: 4.0 },
//     EffectParamV1 { label: c"shape".as_ptr(), min: 0.0, max: 1.0, default: 0.0 },
//     EffectParamV1 { label: c"out".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
// ];
//
// // Enum labels for `shape` (ABI v4 meta): two equally-spaced values across
// // [0, 1] = {0, 1}, matching the `> 0.5` quantization in wob() above.
// // (The wrapper gives the array of raw pointers its Sync impl, same idiom
// // as the stock multiband/parametric_eq effects.)
// struct Labels<const N: usize>([*const core::ffi::c_char; N]);
// unsafe impl<const N: usize> Sync for Labels<N> {}
// static SHAPE_LABELS: Labels<2> = Labels([c"sine".as_ptr(), c"tri".as_ptr()]);
//
// static META4: [ParamMetaV1; 4] = [
//     // "25 %" — the slider rides 0..100 so the unit renders honestly.
//     ParamMetaV1 { unit: SLOOPER_UNIT_PERCENT, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
//     // plain number — the label ("wob/beat") already carries the meaning.
//     ParamMetaV1 { unit: SLOOPER_UNIT_NONE, flags: 0, step: 0.0, labels: core::ptr::null(), n_labels: 0 },
//     // picker: the auto window shows "sine"/"tri" instead of a number.
//     ParamMetaV1 { unit: SLOOPER_UNIT_NONE, flags: 0, step: 0.0, labels: SHAPE_LABELS.0.as_ptr(), n_labels: 2 },
//     // "1.50x", snapped to quarter steps (quantized identically in wob).
//     ParamMetaV1 { unit: SLOOPER_UNIT_TIMES, flags: 0, step: 0.25, labels: core::ptr::null(), n_labels: 0 },
// ];
//
// pub static DESC4: EffectInfoV4 = EffectInfoV4 {
//     abi: SLOOPER_EFFECT_ABI_V4,
//     name: c"Template Tremolo v4".as_ptr(),
//     author: c"slooper".as_ptr(),
//     description:
//         c"Bpm-locked tremolo with sidechain-keyed depth, stereo spread, metadata pickers and a knob GUI.".as_ptr(),
//     n_params: 4,
//     params: PARAMS4.as_ptr(),
//     create: create4,
//     destroy: destroy4,
//     process: process4,
//     process_sc: Some(process_sc4),
//     process_st: Some(process_st4),
//     // 1 = process_st really decorrelates (quarter-beat offset). If yours
//     // is a mid-in/both-out stub, declare 0 and skip the claim.
//     stereo: 1,
//     set_sync: Some(set_sync4),
//     param_meta: META4.as_ptr(),
// };
//
// #[no_mangle]
// pub extern "C" fn slooper_effect_info_v4() -> *const EffectInfoV4 {
//     &DESC4
// }
//
// // ---------------------------------------------------------------------------
// // Custom child GUI (optional symbol) — knobs + picker + button instead of
// // the auto slider list. GUI-thread rules (effect_api.rs, GUI contract):
// // allocation and formatting are fine, blocking is not, and the AUDIO
// // instance `state` may only be touched through atomics you put in it.
// // This editor draws params only, so it never derefs `state`.
// // ---------------------------------------------------------------------------
// struct Gs4 {
//     /// Seconds-stamp of the last "defaults" click (per-window state:
//     /// `create`d when the window opens, freed by `destroy`).
//     flash: f64,
// }
//
// extern "C" fn gui_create4(_sample_rate: f64) -> *mut c_void {
//     Box::into_raw(Box::new(Gs4 { flash: -1.0 })) as *mut c_void
// }
//
// extern "C" fn gui_draw4(
//     gs: *mut c_void,
//     _state: *mut c_void,
//     frame: *const GuiFrameV1,
//     api: *const GuiApiV1,
// ) {
//     if gs.is_null() || frame.is_null() || api.is_null() {
//         return;
//     }
//     let gs = unsafe { &mut *(gs as *mut Gs4) };
//     let fr = unsafe { &*frame };
//     let api = unsafe { &*api };
//     let c = api.ctx;
//
//     {
//         (api.text)(c, 14.0, 14.0, 13.0, 0, 0.85, 0.90, 1.0, 1.0,
//             c"Template Tremolo v4".as_ptr());
//         (api.text)(c, 14.0, 30.0, 11.0, 0, 0.55, 0.60, 0.70, 1.0,
//             c"bpm-locked - keyed by sidechain - stereo".as_ptr());
//
//         // Knob x 3 — the host adds the live-value hover hint and the
//         // mouse cursors for free; keep labels short (value goes behind).
//         let mut v = (api.get_param)(c, 0);
//         if (api.knob)(c, 26.0, 46.0, 58.0, c"depth".as_ptr(), &mut v, 0.0, 100.0, 1.0) == 1 {
//             (api.set_param)(c, 0, v);
//         }
//         let mut v = (api.get_param)(c, 1);
//         // curve 0.0 = logarithmic (min > 0) — right for a rate slider.
//         if (api.knob)(c, 106.0, 46.0, 58.0, c"wob/beat".as_ptr(), &mut v, 0.25, 8.0, 0.0) == 1 {
//             (api.set_param)(c, 1, v);
//         }
//         let mut v = (api.get_param)(c, 3);
//         if (api.knob)(c, 186.0, 46.0, 58.0, c"out".as_ptr(), &mut v, 0.0, 2.0, 1.0) == 1 {
//             (api.set_param)(c, 3, v);
//         }
//
//         // The enum param as a checkbox (two-value picker): quantize the
//         // way `shape` is quantized in wob().
//         let mut sh = (api.get_param)(c, 2);
//         if (api.checkbox)(c, 26.0, 122.0, c"tri LFO".as_ptr(), &mut sh) == 1 {
//             (api.set_param)(c, 2, if sh > 0.5 { 1.0 } else { 0.0 });
//         }
//
//         if (api.button)(c, 150.0, 120.0, 94.0, c"defaults".as_ptr()) == 1 {
//             (api.set_param)(c, 0, 25.0);
//             (api.set_param)(c, 1, 4.0);
//             (api.set_param)(c, 2, 0.0);
//             (api.set_param)(c, 3, 1.0);
//             gs.flash = fr.time;
//         }
//         if fr.time - gs.flash < 0.6 {
//             (api.text)(c, 250.0, 126.0, 11.0, 0, 0.40, 0.90, 0.55, 1.0,
//                 c"reset".as_ptr());
//         }
//     }
// }
//
// extern "C" fn gui_destroy4(gs: *mut c_void) {
//     if !gs.is_null() {
//         drop(unsafe { Box::from_raw(gs as *mut Gs4) });
//     }
// }
//
// static GUI4: EffectGuiV1 = EffectGuiV1 {
//     abi: SLOOPER_EFFECT_GUI_ABI,
//     want_width: 300.0,
//     want_height: 150.0,
//     create: gui_create4,
//     draw: gui_draw4,
//     destroy: gui_destroy4,
// };
//
// #[no_mangle]
// pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 {
//     &GUI4
// }
// SKELETON-END ------------------------------------------------------------------
