//! W9: the Windows MIDI input bus (WINDOWS-PORT program).
//!
//! On Linux slooper's ONLY MIDI source is the JACK `midi_in` port — every
//! remote port auto-links to it (`jack::rescan_midi_sources`) and Linux stays
//! that way permanently. Windows has no JACK: WinMM device callbacks
//! (midir's winmm backend, driven from `audio/wasapi.rs`) push raw MIDI
//! events into a [`MidiInQueue`], and the WASAPI RT loop drains it once per
//! block into `Proc::process_block` — the same `(frame, bytes)` contract the
//! JACK shim fulfills (W4).
//!
//! W12b sub-block timing: every event carries WinMM's message timestamp
//! (device-clock microseconds; midir multiplies `dwTimestamp` ms by 1000).
//! That clock shares no origin with the audio clock, so [`spread`] anchors
//! the FIRST meaningful stamp of each drained batch at frame 0 (zero extra
//! latency versus W9's all-at-block-start behavior) and spaces the rest of
//! the batch by their measured µs distance — strums, CC runs and dense
//! sequences crossing a block boundary keep their relative timing inside the
//! block. Drivers that supply no stamps (all-zero) fall back to the W9
//! behavior; see [`spread`] for every degradation rule.
//!
//! Why a hand-rolled ring instead of a channel: the producer is one callback
//! thread PER WinMM device (multi-producer) while the consumer is the RT
//! thread, and the RT path may not lock or allocate. Producers allocate the
//! event buffer once on their own thread and hand ownership to a ring slot;
//! the consumer MOVES it into the block's event vector — allocation count
//! per event stays exactly one `Vec`, like the JACK shim, just paid off the
//! RT thread.
//!
//! Lock discipline (Vyukov-style sequence-stamped MPMC ring):
//! * Claims are taken with a CAS loop on the write counter and only cover
//!   slots the consumer has provably vacated; a claim of index `i` is
//!   followed by exactly one slot write + `stamp = i + 1` release-store.
//!   A refused claim (ring full) never touches the counter at all, so the
//!   consumer never waits on a phantom and the oldest unread event is never
//!   clobbered (overflow drops the NEWEST, always the freshest data).
//! * The consumer advances `read` only over slots whose stamp says this
//!   generation landed. A producer stalled between claim and stamp (killed
//!   callback thread) is tolerated for [`MidiInQueue::STALL_SKIP`]
//!   consecutive drain cycles and then skipped (tombstoned, counted as a
//!   drop), so the bus can never wedge the audio thread.
//! * Known theoretical race (accepted for MIDI): a producer stalled for a
//!   skip, then revived, stamps an already-passed generation with its old
//!   value; it could overwrite a FUTURE generation's bytes only if it
//!   wakes within exactly one wrap of that generation's window. A
//!   single stale event, no memory unsafety (slot access stays inside the
//!   protocol), cost of a full epoch queue not justified.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

/// Ring capacity (power of two). Full-rate MIDI is ~350 events/s worst
/// case; 1024 slots is a ~3 s backlog — overflow means the RT thread was
/// starved for seconds, not a traffic problem.
const CAP: usize = 1024;
const MASK: usize = CAP - 1;

struct Slot {
    /// Written by the generation's producer before its stamp lands; taken
    /// by the single consumer after. `(stamp_us, bytes)` — WinMM device-
    /// clock microseconds from the callback (0 = driver supplied none).
    /// `None` = tombstone (overflow/skip).
    data: UnsafeCell<Option<(u64, Box<[u8]>)>>,
    /// `i + 1` once claim `i` has fully written slot `i % CAP`.
    stamp: AtomicU64,
}

/// Lock-free multi-producer / single-consumer ring of raw MIDI events.
pub struct MidiInQueue {
    slots: Box<[Slot]>,
    write: AtomicU64,
    /// Single consumer owns it; atomic only so `&self` methods can touch it.
    read: AtomicU64,
    /// Consecutive drain cycles that stopped on this same unstamped slot.
    stalled: AtomicU32,
    stall_idx: AtomicU64,
    /// Events dropped because the ring was full (producers count; the
    /// RT-side `Backend::midi_drops` covers GUI-queue drops).
    drops: AtomicU64,
}

// Access protocol is documented above: one writer per claimed generation,
// one reader, handoffs published through stamp acquire/release pairs.
unsafe impl Send for MidiInQueue {}
unsafe impl Sync for MidiInQueue {}

impl MidiInQueue {
    /// Drain cycles an unstamped (claimed-but-unwritten) slot stalls the
    /// consumer before it is skipped as dead producer. 256 blocks is well
    /// under one audio buffer at any sane block size.
    pub const STALL_SKIP: u32 = 256;

    pub fn new() -> Arc<Self> {
        Arc::new(MidiInQueue {
            slots: (0..CAP)
                .map(|_| Slot {
                    data: UnsafeCell::new(None),
                    stamp: AtomicU64::new(0),
                })
                .collect(),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            stalled: AtomicU32::new(0),
            stall_idx: AtomicU64::new(0),
            drops: AtomicU64::new(0),
        })
    }

    /// Producer side (any thread): claim a slot and write `bytes` with the
    /// callback's device-clock `stamp_us` (0 when the backend offers no
    /// timestamp). Returns
    /// `false` (claim refused, ring untouched) when the ring is full — the
    /// caller owns the drop count. The claim is a CAS loop precisely BECAUSE
    /// of that refusal: a `fetch_add` claim for an event that does not fit
    /// would either wedge the consumer (unstamped gap) or force a slot write
    /// clobbering the oldest UNREAD event. The allocation happens HERE, on
    /// the producer thread, never in the RT consumer.
    pub fn push(&self, bytes: &[u8], stamp_us: u64) -> bool {
        let mut i = self.write.load(Ordering::Relaxed);
        loop {
            if i.saturating_sub(self.read.load(Ordering::Acquire)) >= CAP as u64 {
                self.drops.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            match self
                .write
                .compare_exchange_weak(i, i + 1, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(cur) => i = cur,
            }
        }
        let slot = &self.slots[i as usize & MASK];
        // SAFETY: the CAS gave this call the unique claim on generation `i`;
        // the fits-check above proved the consumer has already passed
        // generation `i - CAP`, so this slot is free.
        let cell = unsafe { &mut *slot.data.get() };
        *cell = Some((stamp_us, Box::from(bytes)));
        slot.stamp.store(i + 1, Ordering::Release);
        true
    }

    /// Consumer side (the RT thread only): move everything that landed into
    /// `out` as `(frame, bytes)` — frame 0 until [`spread`] converts the
    /// batch's timestamps — pushing each event's device-clock `stamp_us`
    /// (µs) in lockstep onto `stamps`, so `stamps.len()` always equals the
    /// number of events this drain added. Never blocks:
    /// a slot claimed but not yet stamped ends the drain, unless the same
    /// slot has stalled [`Self::STALL_SKIP`] cycles — then it is skipped as
    /// a dead producer and counted as one drop.
    pub fn drain_into(
        &self,
        out: &mut Vec<(u32, Vec<u8>)>,
        stamps: &mut Vec<u64>,
    ) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let mut r = self.read.load(Ordering::Relaxed);
        let mut n = 0usize;
        while r < w {
            let slot = &self.slots[r as usize & MASK];
            let want = r + 1;
            let stamped = slot.stamp.load(Ordering::Acquire) == want;
            if !stamped {
                if r == self.stall_idx.load(Ordering::Relaxed) {
                    if self.stalled.fetch_add(1, Ordering::Relaxed) + 1 < Self::STALL_SKIP {
                        break;
                    }
                    // Dead producer: tombstone this generation and move on.
                    self.stalled.store(0, Ordering::Relaxed);
                    self.stall_idx.store(0, Ordering::Relaxed);
                    self.drops.fetch_add(1, Ordering::Relaxed);
                    slot.stamp.store(want, Ordering::Release);
                } else {
                    self.stall_idx.store(r, Ordering::Relaxed);
                    self.stalled.store(1, Ordering::Relaxed);
                    break;
                }
            } else {
                self.stalled.store(0, Ordering::Relaxed);
            }
            // SAFETY: single consumer; the producer finished writing before
            // its stamp became visible (Acquire above pairs with its
            // Release store).
            if let Some((ts, b)) = unsafe { &mut *slot.data.get() }.take() {
                out.push((0, b.into_vec()));
                stamps.push(ts);
                n += 1;
            }
            r += 1;
        }
        self.read.store(r, Ordering::Release);
        n
    }

    /// Events claimed but not yet consumed (approximate; producers are
    /// live). For UI/test introspection, never for logic.
    pub fn pending(&self) -> usize {
        (self.write.load(Ordering::Relaxed).saturating_sub(self.read.load(Ordering::Relaxed)))
            .min(CAP as u64) as usize
    }

    /// Ring-full / dead-producer drops since start.
    pub fn drops(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }

    /// Test hook: claim the next generation WITHOUT writing/stamping it —
    /// simulates a producer frozen mid-push (see the stall-skip pin).
    #[cfg(test)]
    pub(crate) fn debug_claim_stalled(&self) -> u64 {
        self.write.fetch_add(1, Ordering::AcqRel)
    }
}

/// The midir callback payload: a queue handle plus the shared
/// `Backend::midi_drops` counter, so ring overflows surface in the same
/// place as full GUI event queues. `Clone + Send` — one per open device.
#[derive(Clone)]
pub struct MidiInProducer {
    queue: Arc<MidiInQueue>,
    drops: Arc<AtomicU64>,
}

impl MidiInProducer {
    pub fn new(queue: Arc<MidiInQueue>, drops: Arc<AtomicU64>) -> Self {
        MidiInProducer { queue, drops }
    }

    /// One raw MIDI message from a WinMM callback thread. `stamp_us` is the
    /// backend's device-clock timestamp in microseconds (midir's callback
    /// first argument; 0 = none) — [`spread`] turns these into sub-block
    /// frame offsets on the RT side.
    pub fn on_bytes(&self, bytes: &[u8], stamp_us: u64) {
        if !self.queue.push(bytes, stamp_us) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Deduplicate display labels the way the WASAPI device list does:
/// `"A", "A", "A"` -> `"A", "A [2]", "A [3]"`. WinMM names repeat whenever
/// a device exposes several ports; the suffixed label is what the picker
/// round-trips through `Cmd::SetMidiInputs`, so enumeration and open must
/// both run in the same device order (see `wasapi::open_midi_devices`).
pub fn dedupe_labels(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for mut n in names {
        // Count earlier labels that ARE this base ("A") or already carry a
        // suffix of it ("A [2]") — an exact-match count would re-emit
        // "A [2]" for the third raw "A".
        let hits = out
            .iter()
            .filter(|o| **o == n || o.starts_with(&format!("{n} [")))
            .count();
        if let Some(k) = hits.checked_sub(1) {
            n = format!("{n} [{}]", k + 2);
        }
        out.push(n);
    }
    out
}

/// The devices to open: the available ones whose label is selected, in
/// enumeration order. Selections that are not currently available are
/// simply absent — the config keeps them, so a device that was only
/// unplugged momentarily reappears in the list and re-Apply picks it up
/// again (same discipline as the WASAPI audio port picker).
pub fn open_set(available: &[String], selected: &[String]) -> Vec<String> {
    available
        .iter()
        .filter(|n| selected.contains(n))
        .cloned()
        .collect()
}

/// W12b: convert a drained batch's device-clock µs stamps into sub-block
/// frame offsets, writing `evs[i].0` in place (pairing is positional; a
/// missing stamp reads as 0).
///
/// The WinMM clock shares no origin with the audio clock, so the mapping is
/// batch-relative: the FIRST non-zero stamp anchors at frame 0 — the batch
/// still starts exactly where W9 played it (no latency added) — and each
/// following event lands `µs distance from the anchor × sr` frames later
/// (distances resolve at ms granularity — the WinMM clock's own unit),
/// clamped to `n_frames - 1` (a batch longer than the block collapses onto
/// its last frame; nothing is dropped). Degradation rules, each keeping
/// offsets NON-DECREASING in arrival order (the engine stages in arrival
/// order and sorts):
/// * no events / `sr_hz == 0` / `n_frames == 0` → no-op (every frame stays 0);
/// * ALL stamps 0 → no-op: an unstamped driver keeps the W9 block-start
///   behavior exactly;
/// * leading 0-stamps stay at frame 0; the anchor is the first real stamp;
/// * deltas are computed in ms space with u32 wrap arithmetic (WinMM stamps
///   are wrapping u32 milliseconds — a 49.7-day-lifetime wrap lands as the
///   SMALL correct delta);
/// * a delta larger than an hour (backwards jump, or two device clocks
///   interleaved into one batch) keeps the PREVIOUS event's offset and does
///   NOT advance the anchor: one bad stamp can only ever collapse events
///   onto the same frame, never reorder them.
pub fn spread(evs: &mut [(u32, Vec<u8>)], stamps: &[u64], sr_hz: u32, n_frames: u32) {
    if evs.is_empty() || sr_hz == 0 || n_frames == 0 {
        return;
    }
    // More than an hour inside ONE drain batch means a clock mismatch, not
    // real spacing. (Drains run every audio block — milliseconds apart.)
    const MAX_SPAN_MS: u64 = 3_600_000;
    let cap = n_frames - 1;
    let anchor = match stamps.iter().take(evs.len()).position(|&t| t != 0) {
        Some(i) => i,
        None => return, // unstamped driver: legacy block-start behavior
    };
    let mut prev = stamps[anchor];
    let mut cum_ms = 0u64;
    for (i, ev) in evs.iter_mut().enumerate().skip(anchor + 1) {
        let ts = stamps.get(i).copied().unwrap_or(0);
        if ts != 0 {
            // Wrap-correct ms delta on the underlying u32 ms clock (midir
            // multiplies the DWORD stamp by 1000, so µs/1000 is exact).
            let d_ms = ((ts / 1000) as u32).wrapping_sub((prev / 1000) as u32) as u64;
            if d_ms <= MAX_SPAN_MS {
                cum_ms += d_ms;
                prev = ts;
            }
        }
        ev.0 = ((cum_ms * sr_hz as u64) / 1000).min(cap as u64) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w9_ring_roundtrip_stamps_block_start() {
        let q = MidiInQueue::new();
        assert!(q.push(&[0x90, 60, 100], 0));
        assert!(q.push(&[0xC0, 5], 0));
        assert!(q.push(&[0xF0, 0x01, 0xF7], 0)); // a sysex chunk rides the same ring
        let mut evs = Vec::new();
        let mut ts = Vec::new();
        assert_eq!(q.drain_into(&mut evs, &mut ts), 3);
        assert_eq!(
            evs,
            vec![
                (0u32, vec![0x90u8, 60, 100]),
                (0, vec![0xC0, 5]),
                (0, vec![0xF0, 1, 0xF7]),
            ],
            "order preserved, every event stamped at block start (frame 0)"
        );
        assert_eq!(ts, vec![0u64, 0, 0], "unstamped producers drain unstamped");
        let mut evs2 = Vec::new();
        let mut ts2 = Vec::new();
        assert_eq!(q.drain_into(&mut evs2, &mut ts2), 0);
        assert_eq!(q.pending(), 0);
    }

    #[test]
    fn w9_ring_overflow_drops_newest_and_keeps_flow() {
        let q = MidiInQueue::new();
        for i in 0..CAP {
            assert!(q.push(&[0xB0, (i % 250) as u8, 1], 0), "slot {i} must fit");
        }
        assert!(!q.push(&[0xB0, 250, 1], 0), "CAP+1 must report full");
        assert_eq!(q.drops(), 1);
        let mut evs = Vec::new();
        let mut ts = Vec::new();
        assert_eq!(q.drain_into(&mut evs, &mut ts), CAP);
        assert_eq!(evs.first().unwrap().1, vec![0xB0u8, 0, 1]);
        assert_eq!(evs.last().unwrap().1, vec![0xB0u8, ((CAP - 1) % 250) as u8, 1]);
        assert_eq!(evs.iter().filter(|e| e.1[1] == 250).count(), 0, "newest was dropped");
        assert!(q.push(&[0x90, 1, 1], 0), "flow resumes once the consumer drained");
    }

    #[test]
    fn w9_ring_mpmc_threads_no_loss_no_dupes() {
        let q = MidiInQueue::new();
        const THREADS: u8 = 4;
        const PER: u16 = 250; // * THREADS kept far below CAP*wrap safety
        let qs: Vec<Arc<MidiInQueue>> = (0..THREADS).map(|_| q.clone()).collect();
        std::thread::scope(|s| {
            for (t, q) in (0..THREADS).zip(qs) {
                s.spawn(move || {
                    for i in 0..PER {
                        // 3 bytes: [thread, hi(counter), lo(counter)]
                        let n = u16::from_le_bytes([i as u8, 0]);
                        let _ = q.push(&[t, (n >> 8) as u8, n as u8], 0);
                    }
                });
            }
        });
        let mut evs = Vec::new();
        let mut ts = Vec::new();
        assert_eq!(q.drain_into(&mut evs, &mut ts), (THREADS as usize) * (PER as usize));
        let mut seen = vec![0u32; (THREADS as usize) * (PER as usize)];
        for (_, b) in &evs {
            let idx = b[0] as usize * PER as usize + u16::from_be_bytes([b[1], b[2]]) as usize;
            seen[idx] += 1;
        }
        assert!(
            seen.iter().all(|&c| c == 1),
            "every event exactly once: {} slots empty, {} duplicated",
            seen.iter().filter(|&&c| c == 0).count(),
            seen.iter().filter(|&&c| c > 1).count()
        );
        assert_eq!(q.drops(), 0);
    }

    #[test]
    fn w9_ring_skips_dead_producer_after_patience() {
        let q = MidiInQueue::new();
        let dead = q.debug_claim_stalled(); // claim 0: never written
        assert!(q.push(&[0x90, 64, 111], 0)); // claim 1: written
        let mut evs = Vec::new();
        let mut ts = Vec::new();
        // The live event behind the stall must NOT jump the queue early...
        for _ in 0..MidiInQueue::STALL_SKIP - 1 {
            assert_eq!(q.drain_into(&mut evs, &mut ts), 0, "must wait on the claimed slot");
        }
        // ... and must arrive on the skip drain, counted as one drop.
        assert_eq!(q.drain_into(&mut evs, &mut ts), 1);
        assert_eq!(evs[0].1, vec![0x90u8, 64, 111]);
        assert_eq!(ts[0], 0);
        assert_eq!(q.drops(), 1, "the dead producer's event counts once");
        assert_eq!(q.pending(), 0);
        assert_eq!(dead, 0);
        // The bus keeps working afterwards.
        assert!(q.push(&[0x80, 64, 0], 0));
        let mut more = Vec::new();
        let mut more_ts = Vec::new();
        assert_eq!(q.drain_into(&mut more, &mut more_ts), 1);
    }

    #[test]
    fn w9_producer_counts_ring_overflows_into_drops() {
        let q = MidiInQueue::new();
        let drops = Arc::new(AtomicU64::new(0));
        let prod = MidiInProducer::new(q.clone(), drops.clone());
        for i in 0..CAP {
            prod.on_bytes(&[0xB0, (i % 250) as u8, 1], 0);
        }
        prod.on_bytes(&[0xB0, 200, 1], 0);
        prod.on_bytes(&[0xF8], 0);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(q.drops(), 2);
    }

    #[test]
    fn w9_open_set_semantics() {
        let avail: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let sel: Vec<String> = ["C", "GONE", "A"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            open_set(&avail, &sel),
            vec!["A".to_string(), "C".to_string()],
            "enumeration order wins; stale selections simply vanish until the device returns"
        );
        assert_eq!(open_set(&avail, &[]).len(), 0);
        assert_eq!(open_set(&[], &sel).len(), 0);
    }

    #[test]
    fn w9_dedupe_labels_shape() {
        let names = ["A", "A", "B", "A"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            dedupe_labels(names),
            vec![
                "A".to_string(),
                "A [2]".to_string(),
                "B".to_string(),
                "A [3]".to_string()
            ],
            "labels are unique so the picker selection round-trips to one device"
        );
    }

    // ---- W12b: sub-block MIDI timing -------------------------------------

    fn stamped_batch(stamps: &[u64]) -> Vec<(u32, Vec<u8>)> {
        stamps.iter().map(|_| (0u32, vec![0x90u8, 60, 100])).collect()
    }
    fn frames(evs: &[(u32, Vec<u8>)]) -> Vec<u32> {
        evs.iter().map(|e| e.0).collect()
    }

    #[test]
    fn w12b_ring_carries_stamp_per_event_in_order() {
        let q = MidiInQueue::new();
        assert!(q.push(&[0x90, 60, 100], 1_000_000));
        assert!(q.push(&[0x90, 61, 100], 0)); // a no-stamp device in the same ring
        assert!(q.push(&[0xB0, 7, 100], 1_005_000));
        let mut evs = Vec::new();
        let mut ts = Vec::new();
        assert_eq!(q.drain_into(&mut evs, &mut ts), 3);
        assert_eq!(
            ts,
            vec![1_000_000u64, 0, 1_005_000],
            "stamps ride in arrival order, paired positionally with the events"
        );
        assert_eq!(evs[1].1, vec![0x90u8, 61, 100]);
    }

    #[test]
    fn w12b_spread_spaces_a_batch_by_device_clock() {
        // 48 kHz, block 256: 1 ms = 48 frames. Anchor (first stamp) = 0.
        // WinMM stamps are whole-ms carrier values (midir ms * 1000).
        let stamps = [100_000u64, 105_000, 112_000]; // 5 ms and 12 ms after the anchor
        let mut evs = stamped_batch(&stamps);
        spread(&mut evs, &stamps, 48_000, 1024);
        assert_eq!(frames(&evs), vec![0, 240, 576]);
    }

    #[test]
    fn w12b_spread_unstamped_and_degenerate_inputs_stay_at_block_start() {
        let stamps = [0u64, 0, 0];
        let mut evs = stamped_batch(&stamps);
        spread(&mut evs, &stamps, 48_000, 256);
        assert_eq!(frames(&evs), vec![0, 0, 0], "driver without stamps: W9 behavior exactly");
        // Degenerate clocks are no-ops, never a panic or a wild offset.
        let real = [1_000u64, 5_000];
        let mut e1 = stamped_batch(&real);
        spread(&mut e1, &real, 0, 256);
        assert_eq!(frames(&e1), vec![0, 0], "sr 0 (NULL engine) cannot convert");
        let mut e2 = stamped_batch(&real);
        spread(&mut e2, &real, 48_000, 0);
        assert_eq!(frames(&e2), vec![0, 0], "zero-length block has no sub-block space");
        let mut e3: Vec<(u32, Vec<u8>)> = Vec::new();
        spread(&mut e3, &[], 48_000, 256); // empty batch: no-op
    }

    #[test]
    fn w12b_spread_is_wrap_correct_on_the_u32_ms_clock() {
        // The underlying WinMM clock crosses its 2^32 ms wrap (≈49.7 days)
        // between the two events: 8 ms before the wrap, then 2 ms after.
        let stamps = [((u32::MAX - 7) as u64) * 1000, 2_000];
        let mut evs = stamped_batch(&stamps);
        spread(&mut evs, &stamps, 48_000, 1024);
        assert_eq!(
            frames(&evs),
            vec![0, 480],
            "10 ms across the wrap = 480 frames, not a 24.8-day jump"
        );
    }

    #[test]
    fn w12b_spread_backwards_stamp_collapses_never_reorders() {
        // Two device clocks interleaved into one batch (or a driver glitch):
        // the bad delta keeps the previous offset and does NOT poison the
        // anchor — the chain re-measures from the last good stamp.
        let stamps = [20_000u64, 30_000, 25_000, 32_000];
        let mut evs = stamped_batch(&stamps);
        spread(&mut evs, &stamps, 48_000, 1024);
        assert_eq!(frames(&evs), vec![0, 480, 480, 576]);
        let f = frames(&evs);
        assert!(f.windows(2).all(|w| w[0] <= w[1]), "offsets never decrease");
    }

    #[test]
    fn w12b_spread_clamps_overrun_onto_the_last_frame() {
        // A batch spanning three blocks (RT hiccup / idle stall): everything
        // past the block clamps onto its last frame — nothing dropped, order
        // kept (process_block applies the same n-1 ceiling).
        let stamps = [1_000u64, 21_000, 41_000, 61_000]; // 0, 20, 40, 60 ms
        let mut evs = stamped_batch(&stamps);
        spread(&mut evs, &stamps, 48_000, 256); // block = 256 frames ≈ 5.3 ms
        assert_eq!(frames(&evs), vec![0, 255, 255, 255]);
    }

    #[test]
    fn w12b_spread_leading_unstamped_events_stay_at_frame_zero() {
        let stamps = [0u64, 10_000, 12_000];
        let mut evs = stamped_batch(&stamps);
        spread(&mut evs, &stamps, 48_000, 256);
        assert_eq!(
            frames(&evs),
            vec![0, 0, 96],
            "anchor = first REAL stamp; earlier unknowns stay at the block start"
        );
    }
}
