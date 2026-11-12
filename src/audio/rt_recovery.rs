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
}
