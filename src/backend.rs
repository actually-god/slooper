//! Audio backend contract — what a platform layer must provide so the
//! engine core can run (WINDOWS-PORT W4).
//!
//! # The shape of the system
//!
//! slooper splits the realtime world into two halves with a fixed set of
//! seams between them:
//!
//! * **Engine core (backend-neutral)** — `Proc::process_block`
//!   (`audio.rs`): transport, clip mixing, per-track effect chains, synths,
//!   MIDI rendering, take capture, meters, and every playhead/atomic update.
//!   It knows nothing about JACK, WASAPI, or any device: per block it gets
//!   `n` frames, owned MIDI events, and buffers through [`BlockIo`].
//! * **Backend shim (per OS)** — builds the buffers/events for one block and
//!   calls `process_block` exactly once from its RT callback. Today: the
//!   `jack::ProcessHandler` impl in `audio.rs` (Linux, primary). W5 adds a
//!   WASAPI shim (Windows). A shim contains NO mix/transport logic — if you
//!   are about to write any there, move it into `process_block` instead.
//! * **Manager thread contract (identical for every backend)** — the
//!   `Backend` manager thread in `audio.rs` owns the device handle/client
//!   and the GUI-facing channels: commands flow in on the `Cmd` mpsc
//!   (`Cmd::Apply` rewrites the input/output slot masks that
//!   `process_block` reads; project/metro/transport commands are engine
//!   atomics or table updates), events flow back on `Evt` (bounded —
//!   `try_send`, drops counted). The GUI (`ui.rs`) only ever talks to this
//!   contract, so it is backend-agnostic by construction: adding a backend
//!   must not change `Cmd`/`Evt` shapes.
//!
//! # What a backend must guarantee per block
//!
//! 1. Call `Proc::process_block(n, midi, io)` exactly once per cycle, on
//!    the RT thread, before publishing output.
//! 2. `n` = frames in this block. `n == 0` is legal (consumes the MIDI
//!    events, skips audio).
//! 3. `midi`: raw MIDI received *during this block*, in arrival order,
//!    each with its block-relative sample offset (`0..n`, clamped inside).
//!    `process_block` consumes the vector.
//! 4. [`BlockIo`]: for each enabled slot, the block's f32 buffer. Input
//!    buffers must hold this block's live audio; output buffers receive the
//!    mix (the core WRITES every enabled output slot each non-idle block —
//!    it fully overwrites, never accumulates). Returning a null pointer for
//!    a slot excludes that slot, exactly like a disconnected JACK port.
//! 5. RT rules inside the callback (all inherited by `process_block`):
//!    no blocking locks (engine tables refresh via `try_lock` + generation
//!    counters), no IO, no panics, allocations only the bounded per-event
//!    byte vectors handed over in `midi`.
//! 6. NO_START ethos: no device is an error at startup (`Evt::Unavailable`
//!    tells the GUI); an empty patch stays silent — the backend must not
//!    fabricate or auto-connect devices.
//!
//! The Linux behaviour is the reference: `audio.rs` `impl ProcessHandler
//! for Proc` is the model shim (collect frames/MIDI/buffers, hand off,
//! return). Bit-truth of the core across backend refactors is gated by
//! `scripts/porting/w4capture.rs` (deterministic live-capture A/B).

/// One block's live buffers, supplied by the backend shim.
///
/// Slot index = the fixed pool index shared with the manager thread's
/// `Cmd::Apply` masks (`mask_in`/`mask_out_l`/`mask_out_r` atomics). A null
/// return means the slot has no buffer this cycle; the core then treats the
/// slot as not connected for this block. Implementations must be
/// side-effect free w.r.t. engine state and RT-safe (they are called from
/// the realtime callback, once per enabled slot per block).
pub trait BlockIo {
    /// Live input samples for `slot` (read-only for the core), length >=
    /// the block's frame count; null = not connected.
    fn in_buf(&self, slot: usize) -> *const f32;
    /// Output mix destination for `slot`, fully overwritten by the core;
    /// null = not connected.
    fn out_buf(&self, slot: usize) -> *mut f32;
}
