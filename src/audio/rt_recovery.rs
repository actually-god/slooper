//! Lost-stream recovery decision table for the WASAPI backend
//! (WINDOWS-PORT W12a).
//!
//! The RT thread classifies its own death (see `wasapi.rs`): a stop-event
//! break stores NO fault, a failed wait on our own handles or an
//! `AUDCLNT_E_DEVICE_INVALIDATED` from the device stores `FAULT_DEVICE`,
//! any other stream error stores `FAULT_STREAM`. The manager thread's
//! `idle` hook reaps the dead thread and asks THIS pure table what to do:
//!
//! * `Reapply` — every endpoint of the last live patch enumerates again:
//!   re-Apply it automatically (replugging a USB DAC resumes audio with no
//!   click). Bounded by a budget: a flaky device must not death-loop at
//!   idle cadence.
//! * `DeviceGone` — an endpoint left the system: a Notice explains; retrying
//!   against a vanished endpoint is pointless.
//! * `GaveUp` — device present, retries exhausted: stop trying, say so.
//!
//! Pure and OS-neutral so the Linux suite pins the ENTIRE table (and the
//! Windows CI runs the same pins — the OS split is only in who reads it).

/// The last patch `Cmd::Apply` brought up: `(ins, outs)` exactly as
/// `ManagerBackend::apply` receives it.
pub type Patch = (Vec<String>, Vec<(String, u8)>);

/// RT thread died via the stop event (or nothing died).
pub const FAULT_NONE: u32 = 0;
/// Generic stream error (GetBuffer/GetNextPacketSize/… failed).
pub const FAULT_STREAM: u32 = 1;
/// The endpoint was invalidated (device unplugged / taken exclusive /
/// our own wait handles died).
pub const FAULT_DEVICE: u32 = 2;

/// Auto-reapplies allowed per fault streak.
pub const RETRY_BUDGET: u32 = 2;
/// A stream that stayed up this long resets the budget: a device that
/// survives and then dies is a NEW story, not a death loop.
pub const SURVIVED_SECS: u64 = 10;

/// What the manager's idle hook does after reaping a dead RT thread.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum IdleRecovery {
    /// Do nothing (clean stop, or no recorded patch to recover).
    Nothing,
    /// Re-Apply this exact patch once (budget already checked).
    Reapply(Patch),
    /// A patched device left the system: Notice the user.
    DeviceGone,
    /// Device present but retries exhausted: stop, Notice the user.
    GaveUp,
}

/// The whole recovery decision, pure so every arm is pinnable on Linux.
/// `still_present` = every endpoint label of `last` resolves in the CURRENT
/// enumeration; `budget` = reaps remaining (caller resets it on survival).
pub fn plan(fault: u32, last: Option<&Patch>, still_present: bool, budget: u32) -> IdleRecovery {
    if fault == FAULT_NONE {
        return IdleRecovery::Nothing; // stop event: nothing failed
    }
    let Some(patch) = last else {
        return IdleRecovery::Nothing; // never applied (or explicit silence)
    };
    if !still_present {
        return IdleRecovery::DeviceGone; // never retry a vanished endpoint
    }
    if budget > 0 {
        IdleRecovery::Reapply(patch.clone())
    } else {
        IdleRecovery::GaveUp
    }
}

// ---------- v118: JACK server-loss (card t_0bba9aaa) ----------
//
// The Linux twin of the WASAPI contract above. The jack backend's manager
// `idle` hook owns the state; this table decides, exactly like `plan()`
// does for WASAPI. Same OS-neutral rationale: the Linux suite pins the
// ENTIRE table, and Windows CI runs the same pins.

/// Full client rebuilds allowed per loss event against a server that IS
/// reachable but refuses setup. Server-ABSENT attempts never consume this:
/// PipeWire restarts are routine desktop events, and audio must come back
/// by itself when the server does (no "restart slooper" for a hiccup).
pub const JACK_REBUILD_BUDGET: u32 = 3;
/// Minimum spacing between reconnect attempts, in ms. The manager idle
/// ticks every 250 ms; without pacing we would hammer Client::new at the
/// dead server at 4 Hz.
pub const JACK_PROBE_MS: u64 = 1_000;
/// Silent window after which a client is declared lost, in ms. Both
/// libjack and PipeWire-JACK keep dispatching process() to an ACTIVE
/// client even with zero connections and idle transport (measured on
/// HunterPC: 47 blocks/s @ 48k/1024), so a stopped block counter is
/// unambiguous death, not idleness. The primary death signal is the
/// client lib's own shutdown callback; this is the backstop for the
/// hard-kill case that never fires it.
pub const JACK_STALE_MS: u64 = 2_000;

/// What the jack idle hook does on one dead-state tick.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum JackRecovery {
    /// Stay quiet: pacing window not elapsed, or given up (only a user
    /// `Cmd::Apply` rearms via `jack_rearm`).
    Waiting,
    /// Attempt a full client rebuild + patch replay NOW. The glue runs it
    /// and reports the outcome through `jack_rebuild_failed`/`jack_revive`.
    Rebuild,
}

/// Per-loss-event reconnect bookkeeping. All `*_ms` fields are
/// milliseconds on ONE clock origin chosen by the caller (the session's
/// boot instant).
#[derive(Debug)]
pub struct JackLoss {
    /// Rebuild attempts that failed on a reachable server (the budget).
    pub rebuilds: u32,
    /// Budget exhausted: stop attempting until `jack_rearm`.
    pub gave_up: bool,
    /// Attempt pacing: ms clock of the last `Rebuild` dispatch.
    pub last_try_ms: u64,
    /// Last process-callback counter observed, and when it last moved.
    pub live_ticks: u64,
    pub last_live_ms: u64,
}

impl JackLoss {
    pub fn new(now_ms: u64) -> JackLoss {
        JackLoss {
            rebuilds: 0,
            gave_up: false,
            last_try_ms: 0,
            live_ticks: 0,
            last_live_ms: now_ms,
        }
    }
}

/// Liveness rule: alive iff the process counter moved since last check,
/// or the silence has not yet reached the stale window. The glue updates
/// `prev_*` only when the counter moves, so a frozen counter ages the
/// silence every tick.
pub fn jack_alive(prev_ticks: u64, now_ticks: u64, prev_ms: u64, now_ms: u64) -> bool {
    now_ticks != prev_ticks || now_ms.saturating_sub(prev_ms) < JACK_STALE_MS
}

/// Dead-state tick: never attempt after giving up; otherwise dispatch a
/// Rebuild once the pacing window elapsed (claiming the slot now).
pub fn jack_loss_step(loss: &mut JackLoss, now_ms: u64) -> JackRecovery {
    if loss.gave_up {
        return JackRecovery::Waiting;
    }
    if now_ms.saturating_sub(loss.last_try_ms) < JACK_PROBE_MS {
        return JackRecovery::Waiting;
    }
    loss.last_try_ms = now_ms;
    JackRecovery::Rebuild
}

/// Record a failed rebuild. `server_up == false` (Client::new itself
/// failed → the server is NOT there) costs pacing only and never burns
/// budget. A failure WITH a reachable server costs one attempt and
/// returns true exactly at the gave-up transition (the glue announces
/// that once — it is the "restart slooper" message).
pub fn jack_rebuild_failed(loss: &mut JackLoss, server_up: bool, now_ms: u64) -> bool {
    loss.last_try_ms = now_ms;
    if !server_up {
        return false;
    }
    loss.rebuilds += 1;
    if loss.rebuilds >= JACK_REBUILD_BUDGET {
        loss.gave_up = true;
        return true;
    }
    false
}

/// A successful rebuild ends the loss event: fresh budget, liveness
/// re-seeded from the NEW client's counter (which keeps counting across
/// rebuilds — never reset, so old/low counters can only look alive).
pub fn jack_revive(loss: &mut JackLoss, ticks: u64, now_ms: u64) {
    loss.rebuilds = 0;
    loss.gave_up = false;
    loss.live_ticks = ticks;
    loss.last_live_ms = now_ms;
}

/// An explicit user `Cmd::Apply` outranks a give-up (and re-arms the
/// budget): the patch is remembered by the glue and replayed by the next
/// rebuild. `last_try_ms` is set so the NEXT idle tick (<= 250 ms away)
/// already sees the pacing window elapsed.
pub fn jack_rearm(loss: &mut JackLoss, now_ms: u64) {
    loss.rebuilds = 0;
    loss.gave_up = false;
    loss.last_try_ms = now_ms.saturating_sub(JACK_PROBE_MS);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch() -> Patch {
        (
            vec!["Mic (L)".to_string()],
            vec![("DAC (L)".to_string(), 0), ("DAC (R)".to_string(), 1)],
        )
    }

    #[test]
    fn w12a_plan_clean_stop_is_silent() {
        // Even with a live patch and budget, a stop-event death (fault 0)
        // must never trigger a re-Apply — the user stopped it, not the wire.
        let p = patch();
        assert_eq!(
            plan(FAULT_NONE, Some(&p), true, RETRY_BUDGET),
            IdleRecovery::Nothing
        );
    }

    #[test]
    fn w12a_plan_device_fault_reapplies_while_present() {
        let p = patch();
        let got = plan(FAULT_DEVICE, Some(&p), true, RETRY_BUDGET);
        assert_eq!(got, IdleRecovery::Reapply(p.clone()));
        // The Reapply carries the EXACT patch (in and out lists).
        if let IdleRecovery::Reapply(q) = got {
            assert_eq!(q.0, vec!["Mic (L)".to_string()]);
            assert_eq!(q.1.len(), 2);
            assert_eq!(q.1[1], ("DAC (R)".to_string(), 1));
        } else {
            panic!("expected Reapply");
        }
    }

    #[test]
    fn w12a_plan_stream_fault_retries_too() {
        // Recovery is not keyed on the fault class: a generic stream death
        // on a still-present device re-Applys as well.
        let p = patch();
        assert_eq!(
            plan(FAULT_STREAM, Some(&p), true, 1),
            IdleRecovery::Reapply(p.clone())
        );
    }

    #[test]
    fn w12a_plan_vanished_device_announces_not_retries() {
        let p = patch();
        // Budget present or not: a gone endpoint is never retried, and the
        // message arm does not depend on the budget.
        assert_eq!(
            plan(FAULT_DEVICE, Some(&p), false, RETRY_BUDGET),
            IdleRecovery::DeviceGone
        );
        assert_eq!(
            plan(FAULT_STREAM, Some(&p), false, 0),
            IdleRecovery::DeviceGone
        );
    }

    #[test]
    fn w12a_plan_budget_exhaustion_gives_up() {
        let p = patch();
        assert_eq!(plan(FAULT_DEVICE, Some(&p), true, 0), IdleRecovery::GaveUp);
    }

    #[test]
    fn w12a_plan_no_history_nothing() {
        // Faulty death without a recorded patch (never applied, or an
        // explicit silence patch) recovers nothing.
        assert_eq!(
            plan(FAULT_DEVICE, None, true, RETRY_BUDGET),
            IdleRecovery::Nothing
        );
        assert_eq!(plan(FAULT_STREAM, None, false, 0), IdleRecovery::Nothing);
    }

    #[test]
    fn w12a_recovery_policy_constants() {
        // The shipped policy values (docs/WINDOWS-PORT.md W12a): at most
        // two auto-reapplies per streak; a stream that survived 10 s
        // starts a fresh story. Change here = deliberate re-decision.
        assert_eq!(RETRY_BUDGET, 2);
        assert_eq!(SURVIVED_SECS, 10);
        assert_eq!((FAULT_NONE, FAULT_STREAM, FAULT_DEVICE), (0, 1, 2));
    }

    // ---- v118: JACK server-loss table (card t_0bba9aaa) ----

    #[test]
    fn v118_jack_liveness_rule_window() {
        // Counter MOVED: alive, however long the previous silence was —
        // the glue re-seeds prev_ms exactly on this condition.
        assert!(jack_alive(10, 11, 0, 999_999));
        // Frozen counter INSIDE the stale window: still alive (a server
        // under a graph rebuild may skip a couple of cycles).
        assert!(jack_alive(10, 10, 5_000, 5_000 + JACK_STALE_MS - 1));
        // Frozen AT/PAST the window: declared lost. Boundary inclusive.
        assert!(!jack_alive(10, 10, 5_000, 5_000 + JACK_STALE_MS));
        assert!(!jack_alive(10, 10, 5_000, 9_999_999));
    }

    #[test]
    fn v118_jack_loss_pacing() {
        let mut l = JackLoss::new(10_000);
        assert_eq!(l.last_live_ms, 10_000);
        assert!(!l.gave_up);
        // A loss event starts patient-but-prompt: last_try 0 means the
        // first dead-state tick dispatches immediately.
        assert_eq!(jack_loss_step(&mut l, 10_000), JackRecovery::Rebuild);
        assert_eq!(l.last_try_ms, 10_000);
        // The 250 ms idle cadence must NOT hammer the down server.
        assert_eq!(jack_loss_step(&mut l, 10_250), JackRecovery::Waiting);
        assert_eq!(
            jack_loss_step(&mut l, 10_000 + JACK_PROBE_MS - 1),
            JackRecovery::Waiting
        );
        assert_eq!(
            jack_loss_step(&mut l, 10_000 + JACK_PROBE_MS),
            JackRecovery::Rebuild
        );
        assert_eq!(l.last_try_ms, 10_000 + JACK_PROBE_MS);
    }

    #[test]
    fn v118_jack_rebuild_budget_asymmetry() {
        // The policy heart: a MISSING server has no budget (audio must
        // return by itself when PipeWire comes back); a REFUSING one is
        // bounded so a broken install cannot spin forever.
        let mut l = JackLoss::new(0);
        for i in 0..30 {
            assert!(!jack_rebuild_failed(&mut l, false, i));
        }
        assert_eq!(l.rebuilds, 0);
        assert!(!l.gave_up);
        // Present-but-refusing: consumes one per failure, announces the
        // transition exactly once.
        assert!(!jack_rebuild_failed(&mut l, true, 100));
        assert!(!jack_rebuild_failed(&mut l, true, 200));
        assert!(jack_rebuild_failed(&mut l, true, 300));
        assert!(l.gave_up);
        // Given up: never attempt again on plain ticks.
        assert_eq!(jack_loss_step(&mut l, 999_999), JackRecovery::Waiting);
        // A user Apply rearms: budget clean, first attempt immediate.
        jack_rearm(&mut l, 1_000_000);
        assert!(!l.gave_up);
        assert_eq!(l.rebuilds, 0);
        assert_eq!(jack_loss_step(&mut l, 1_000_000), JackRecovery::Rebuild);
        // A successful rebuild ends the story: fresh budget, liveness
        // re-seeded from the new client's counter.
        jack_revive(&mut l, 4242, 1_000_250);
        assert!(!l.gave_up);
        assert_eq!(l.rebuilds, 0);
        assert_eq!(l.live_ticks, 4242);
        assert_eq!(l.last_live_ms, 1_000_250);
    }

    #[test]
    fn v118_jack_loss_policy_constants() {
        // Shipped policy values (this card's decision): three rebuilds
        // per loss event against a present server; reconnect probe spaced
        // at one second; two seconds of process silence = declared lost.
        // Change here = deliberate re-decision.
        assert_eq!(JACK_REBUILD_BUDGET, 3);
        assert_eq!(JACK_PROBE_MS, 1_000);
        assert_eq!(JACK_STALE_MS, 2_000);
    }
}
