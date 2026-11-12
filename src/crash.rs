//! W12e: the Windows fatal-crash story (docs/WINDOWS-PORT.md, W12).
//!
//! A GUI-subsystem Windows exe (W11: `windows_subsystem = "windows"`,
//! `strip = true`) that dies leaves the user with NOTHING — no console,
//! no stderr, no coredumpctl. This module is that missing artifact:
//!
//! * Hard faults (AV, stack guard, C++ throw escaping every guard):
//!   a `SetUnhandledExceptionFilter` handler writes a real **minidump**
//!   (`MiniDumpWriteDump`, exception stream included — WinDbg opens it and
//!   lands on the faulting frame) plus a small text note, shows a message
//!   box naming the folder, then terminates.
//! * Rust panics (main's hook aborts — see the v42 note there): the hook
//!   calls [`note_panic`] first, which appends the panic + a captured
//!   backtrace to a text note. No box (a stray panic inside a test or
//!   probe child must never pop a modal on an automation run), no dump
//!   (the panic machinery already destroyed the interesting stack —
//!   the note's backtrace *is* the story).
//!
//! Linux keeps coredumpctl forensics untouched: every write path here is
//! `#[cfg(windows)]`, and [`install`] / [`note_panic`] are structurally
//! inert no-ops on Linux (pinned by `w12e_linux_path_is_inert` — the W5
//! audio-split rule: Linux behavior is never changed by the port).
//!
//! Artifacts live in `paths::crash_dir()` (`SLOOPER_CRASH_DIR` overrides —
//! the W12d `%APPDATA%` bleed rule extended to crash files). The folder is
//! created lazily by the writers only. `SLOOPER_CRASH_QUIET=1` suppresses
//! the message box for unattended runs.
//!
//! `SLOOPER_CRASH_ME=panic|fault` deliberately trips each path at startup
//! (after the filter is armed) — the on-hardware/CI self-test of this whole
//! story, and what the child-process pins drive. The words are parsed on
//! every OS ([`crash_me_mode`]) but acted on only by the Windows
//! [`self_test`], which `install()` calls; the Linux twin never even reads
//! the var, so no Linux run or test can be killed by it.
//!
//! Probe children (W7 scan probes, v23 editor probes) never reach
//! [`install`] — main dispatches them before it — so a deliberately
//! crash-testing probe loads no handler and spams no dumps.
//!
//! Symbolization: the shipping exe keeps symbols (release.yml `winrel`
//! profile, W12e) so dumps resolve module+symbol without a PDB search.
//! Docs: docs/INSTALL.md (Windows crash section).

use std::path::PathBuf;

use crate::paths;

/// Env var: suppress the crash message box (unattended / CI).
pub const CRASH_QUIET_ENV: &str = "SLOOPER_CRASH_QUIET";

/// How many crash *events* (stem = timestamp+code) to keep before the
/// oldest are pruned. Each event is a `.txt` note plus — hard faults only —
/// a few-MB `.dmp`, so 10 events stay a rounding error on disk.
pub const CRASH_KEEP: usize = 10;

/// Where this run would write crash artifacts. Pure resolution, no IO —
/// see [`paths::crash_dir`].
pub fn crash_dir() -> PathBuf {
    paths::crash_dir()
}

/// File stem for one crash event: `slooper-crash-<epoch-seconds>-<code>`.
/// The fixed-width epoch keeps lexicographic order == chronological order;
/// `<code>` is the NTSTATUS hex of a hard fault (e.g. `c0000005`) or the
/// literal `panic`. Split from the writer so Linux pins exercise the same
/// naming the Windows writer uses.
pub fn crash_stem(epoch_secs: u64, code: &str) -> String {
    format!("slooper-crash-{epoch_secs}-{code}")
}

/// Which of `names` (basenames sharing our prefix, any extension) belong to
/// crash events BEYOND the newest `keep` stems. Newest-first walk over a
/// lexicographic sort; a stem counts once, so its `.dmp` and `.txt` are
/// kept or dropped together. Pure — pinned on Linux by
/// `w12e_prune_keeps_newest_events`.
pub fn prune_plan(names: &[String], keep: usize) -> Vec<String> {
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort_unstable_by(|a, b| b.cmp(a)); // newest first
    let mut seen_stems: Vec<&str> = Vec::new();
    let mut doomed = Vec::new();
    for n in sorted {
        // Foreign names (no recognizable stem) are never touched.
        if let Some(stem) = stem_of(n) {
            if !seen_stems.contains(&stem) {
                seen_stems.push(stem);
            }
            if seen_stems.len() > keep {
                doomed.push(n.clone());
            }
        }
    }
    doomed
}

/// The `slooper-crash-…` stem of a basename (epoch+code), or None if the
/// name is not ours (extension stripped). A malformed name is treated as
/// foreign (conservative — prune never touches it).
fn stem_of(name: &str) -> Option<&str> {
    if let Some(rest) = name.strip_prefix("slooper-crash-") {
        // The stem ends at the extension; epoch and code contain none.
        let core = match rest.rfind('.') {
            Some(dot) => &rest[..dot],
            None => rest,
        };
        // stem = "<10+ digit epoch>-<code>"
        let dash = core.find('-')?;
        let epoch = &core[..dash];
        if epoch.len() >= 10 && epoch.bytes().all(|b| b.is_ascii_digit()) && core.len() > dash + 1 {
            Some(core)
        } else {
            None
        }
    } else {
        None
    }
}

/// `1700000000 -> "2023-11-14 22:13:20 UTC"` — the days-from-civil
/// algorithm (Howard Hinnant's civil_from_days, inverted month by
/// arithmetic, no crates). Pinned against known epochs; used only inside
/// crash notes, so a monotone-but-unix clock is fine (no chrono dependency
/// and no new Windows API in a crash path).
pub fn utc_stamp(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = (epoch_secs % 86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 -> proleptic Gregorian (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Wall-clock seconds for artifact names. Only the Windows writers use it
/// (Linux is inert), hence the gate — an ungated fn trips the zero-warning
/// clippy gate on Linux as dead code.
#[cfg(windows)]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Env var: deliberately crash THIS process at startup — the self-test of
/// the story above (`SLOOPER_CRASH_ME=panic` or `=fault`). The switch is
/// parsed on every OS ([`crash_me_mode`], so the parsing is pinned on Linux)
/// but only [`self_test`] — Windows-only, called from [`install`] — acts on
/// it, and `install()` is a Linux no-op, so the Linux build can never trip
/// it and `SLOOPER_CRASH_ME` there is inert junk in the environment.
pub const CRASH_ME_ENV: &str = "SLOOPER_CRASH_ME";

/// What [`CRASH_ME_ENV`] asks for: `Some("panic")`, `Some("fault")` or
/// `None`. Anything else (empty, unknown word) = no self-test.
pub fn crash_me_mode(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<&'static str> {
    let v = env(CRASH_ME_ENV)?;
    match v.to_string_lossy().as_ref() {
        "panic" => Some("panic"),
        "fault" => Some("fault"),
        _ => None,
    }
}

/// Act on [`crash_me_mode`]. Windows-only: Linux never reaches here because
/// [`install`] is a no-op there. `panic` exercises the note path (hook →
/// `note_panic` → v42 abort); `fault` forces a real access violation so the
/// unhandled-exception filter writes a dump and a note. The read is
/// `read_volatile` so optimization cannot fold the fault away.
#[cfg(windows)]
pub fn self_test(env: impl Fn(&str) -> Option<std::ffi::OsString>) {
    match crash_me_mode(env) {
        Some("panic") => panic!("SLOOPER_CRASH_ME=panic — self-test of the panic note path"),
        Some(_) => {
            unsafe {
                std::ptr::read_volatile(std::ptr::null::<u8>());
            }
        }
        None => {}
    }
}

/// Linux twin of [`self_test`]: structurally inert (never even reads the
/// env var), so `SLOOPER_CRASH_ME` cannot kill a Linux run or a test.
#[cfg(not(windows))]
pub fn self_test(_env: impl Fn(&str) -> Option<std::ffi::OsString>) {}

/// Register the fatal-fault handler. Windows: becomes the process
/// unhandled-exception filter (last resort — the plugin FFI firewall and
/// any SEH `__try` still get first refusal, exactly as intended).
/// Linux: the whole thing is a no-op — coredumpctl stays the forensics
/// path and this module must not create files or hooks here (pinned).
#[cfg(windows)]
pub fn install() {
    use windows::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter;
    unsafe {
        // Returning the previous filter (we replace whatever was there —
        // normally none) is fine to drop.
        let _ = SetUnhandledExceptionFilter(Some(uef));
    }
    // The self-test must be armed AFTER the filter: `=fault` then lands in
    // our handler instead of the default WER dialog.
    self_test(|k| std::env::var_os(k));
}

/// Linux inert twin — see this module's docs and `w12e_linux_path_is_inert`.
#[cfg(not(windows))]
pub fn install() {}

/// Main's panic hook calls this BEFORE printing/aborting (cfg-gated at the
/// call site). Formats the payload and delegates to [`note_crash`].
#[cfg(windows)]
pub fn note_panic(info: &std::panic::PanicHookInfo) {
    let loc = info
        .location()
        .map(|l| format!("{}:{}", l.file(), l.line()))
        .unwrap_or_else(|| "<unknown location>".into());
    let msg = info
        .payload_as_str()
        .unwrap_or("<non-&str panic payload>")
        .to_string();
    let bt = std::backtrace::Backtrace::force_capture();
    note_crash(
        "panic",
        &format!("panic at {loc}: {msg}\n\nbacktrace:\n{bt}"),
    );
}

/// Linux twin: structurally inert (pinned).
#[cfg(not(windows))]
pub fn note_panic(_info: &std::panic::PanicHookInfo) {}

/// Write one text note into the crash folder and prune old events.
/// `title` becomes the stem code segment (`panic`, or the exception-code
/// hex). This is the function both the panic hook and the hard-fault
/// handler use, and what the functional pins exercise on Windows.
/// Linux: no-op, creates nothing (pinned by `w12e_linux_path_is_inert`).
#[cfg(windows)]
pub fn note_crash(title: &str, detail: &str) {
    let dir = crash_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return; // nowhere to be loud about — best effort only
    }
    write_note_at(&dir, title, detail);
    prune_dir(&dir);
}

/// Linux twin: pure no-op (never creates the folder, never writes).
#[cfg(not(windows))]
pub fn note_crash(_title: &str, _detail: &str) {}

/// Whether the hard-fault message box must be suppressed (unattended /
/// CI runs set the env var).
pub fn quiet_suppressed(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    env(CRASH_QUIET_ENV)
        .filter(|v| !v.is_empty())
        .is_some()
}

// ---------------------------------------------------------------------------
// Windows internals
// ---------------------------------------------------------------------------

#[cfg(windows)]
const PREFIX: &str = "slooper-crash-";

/// One event's text note at `dir`, collision-suffixed. Split from the env
/// read so the functional pins drive it against a sandbox folder directly.
#[cfg(windows)]
fn write_note_at(dir: &std::path::Path, title: &str, detail: &str) -> Option<std::path::PathBuf> {
    let ts = now_secs();
    let base = crash_stem(ts, title);
    let mut name = format!("{base}.txt");
    let mut n = 1u32;
    while dir.join(&name).exists() {
        name = format!("{base}-{n}.txt");
        n += 1;
    }
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let text = format!(
        "slooper {} crash note — {} ({ts})\nexe: {exe}\n\n{detail}\n",
        env!("CARGO_PKG_VERSION"),
        utc_stamp(ts),
    );
    std::fs::write(dir.join(&name), text).ok()?;
    Some(dir.join(name))
}

/// Prune crash events beyond [`CRASH_KEEP`] inside `dir` (notes AND dumps
/// share the stem, so an event disappears whole).
#[cfg(windows)]
fn prune_dir(dir: &std::path::Path) {
    let names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with(PREFIX))
                .collect()
        })
        .unwrap_or_default();
    let doomed = prune_plan(&names, CRASH_KEEP);
    for d in doomed {
        let _ = std::fs::remove_file(dir.join(&d));
    }
}

#[cfg(windows)]
fn exception_hex(
    pointers: Option<*const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS>,
) -> String {
    unsafe {
        pointers
            .filter(|p| !p.is_null())
            .and_then(|p| {
                let rec = (*p).ExceptionRecord;
                if rec.is_null() {
                    None
                } else {
                    Some(format!("{:08x}", (*rec).ExceptionCode.0))
                }
            })
            .unwrap_or_else(|| "fault".to_string())
    }
}

/// Hard-fault handler (see [`install`]). Runs on the faulting thread with
/// the process already dead-in-the-water: write everything we can, tell
/// the user where it is, terminate. Re-entrancy is guarded — if the dump
/// writer itself faults, the flag stays set and the second call falls
/// straight through to the handler.
#[cfg(windows)]
unsafe extern "system" fn uef(
    pointers: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    use windows::Win32::System::Diagnostics::Debug::{
        EXCEPTION_EXECUTE_HANDLER, EXCEPTION_POINTERS,
    };

    static WRITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WRITING.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return EXCEPTION_EXECUTE_HANDLER; // faulted inside the crash writer
    }

    let ptr: *const EXCEPTION_POINTERS = pointers;
    let code = exception_hex(if ptr.is_null() { None } else { Some(ptr) });
    let ts = now_secs();
    let dir = crash_dir();
    let _ = std::fs::create_dir_all(&dir);

    // Minidump FIRST: it is the artifact that cannot be rebuilt from logs.
    let dump_ok = write_dump_at(&dir, ts, &code, ptr).is_some();
    let addr = unsafe {
        if ptr.is_null() || (*ptr).ExceptionRecord.is_null() {
            std::ptr::null_mut()
        } else {
            (*(*ptr).ExceptionRecord).ExceptionAddress
        }
    };
    write_note_at(
        &dir,
        &code,
        &format!(
            "unhandled exception code {code} at address {addr:p}; minidump written: {dump_ok}"
        ),
    );
    prune_dir(&dir);

    if !quiet_suppressed(|k| std::env::var_os(k)) {
        announce_crash(&dir.join(crash_stem(ts, &code)));
    }
    // Leave the process to the handler — no WER dialog (we just were it).
    EXCEPTION_EXECUTE_HANDLER
}

/// `MiniDumpWriteDump` for the current process into `dir`, returning the
/// dump path. `pointers` may be null (state-only dump — what the pins
/// drive); with the real EXCEPTION_POINTERS the exception stream lands in
/// the dump and debuggers open directly on the faulting frame.
#[cfg(windows)]
fn write_dump_at(
    dir: &std::path::Path,
    ts: u64,
    code: &str,
    pointers: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> Option<std::path::PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use windows::core::BOOL;
    use windows::Win32::System::Diagnostics::Debug::{
        MiniDumpWriteDump, MiniDumpNormal, MiniDumpWithIndirectlyReferencedMemory,
        MINIDUMP_EXCEPTION_INFORMATION, EXCEPTION_POINTERS,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};

    let base = crash_stem(ts, code);
    let mut name = format!("{base}.dmp");
    let mut n = 1u32;
    while dir.join(&name).exists() {
        name = format!("{base}-{n}.dmp");
        n += 1;
    }
    let file = std::fs::File::create(dir.join(&name)).ok()?;
    let dump_type = MiniDumpNormal | MiniDumpWithIndirectlyReferencedMemory;
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    let exc_info = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: tid,
        ExceptionPointers: pointers as *mut EXCEPTION_POINTERS,
        ClientPointers: BOOL(1),
    };
    let r = unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            windows::Win32::Foundation::HANDLE(file.as_raw_handle()),
            dump_type,
            if pointers.is_null() {
                None
            } else {
                Some(&exc_info as *const _)
            },
            None,
            None,
        )
    };
    drop(file);
    match r {
        Ok(()) => Some(dir.join(name)),
        Err(_) => {
            let _ = std::fs::remove_file(dir.join(&name));
            None
        }
    }
}

/// Tell the user their DAW died and where the receipts are. A modal box is
/// right here: the process is terminating anyway and a GUI-subsystem app
/// has no other mouth. Suppressed by SLOOPER_CRASH_QUIET.
#[cfg(windows)]
fn announce_crash(stem: &std::path::Path) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let text = format!(
        "slooper hit a fatal error and wrote a crash report next to:\n{}\n\n\
         (SLOOPER_CRASH_QUIET=1 silences this dialog.)",
        stem.display()
    );
    let wtext: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let wcap: Vec<u16> = "slooper".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(wtext.as_ptr()),
            PCWSTR(wcap.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CrashEnvSandbox {
        saved: Option<String>,
    }
    impl CrashEnvSandbox {
        fn set(v: &str) -> Self {
            let s = CrashEnvSandbox {
                saved: std::env::var(paths::CRASH_DIR_ENV).ok(),
            };
            std::env::set_var(paths::CRASH_DIR_ENV, v);
            s
        }
    }
    impl Drop for CrashEnvSandbox {
        fn drop(&mut self) {
            match &self.saved {
                Some(v) => std::env::set_var(paths::CRASH_DIR_ENV, v),
                None => std::env::remove_var(paths::CRASH_DIR_ENV),
            }
        }
    }

    fn read_src(p: &str) -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(p);
        std::fs::read_to_string(&root)
            .unwrap_or_else(|e| panic!("read {}: {e}", root.display()))
    }

    // ---- pure logic (every OS asserts its own truth) ---------------------

    #[test]
    fn w12e_stem_names_sort_chronologically() {
        // The fixed-width epoch is what makes prune's lexicographic walk
        // chronological; a variable-width name would break pruning order.
        let a = crash_stem(1_700_000_000, "panic");
        let b = crash_stem(1_760_000_000, "c0000005");
        assert_eq!(a, "slooper-crash-1700000000-panic");
        assert!(a < b, "{a} vs {b}");
    }

    #[test]
    fn w12e_prune_keeps_newest_events() {
        // 12 events x (txt + dmp) + one foreign file: keep newest 10 whole,
        // drop the 2 oldest whole, never touch the foreign name.
        let mut names = vec!["notes.txt".to_string(), "slooper-crash-junk".to_string()];
        for i in 0..12u64 {
            let ts = 1_700_000_000 + i * 100;
            names.push(format!("slooper-crash-{ts}-c0000005.txt"));
            names.push(format!("slooper-crash-{ts}-c0000005.dmp"));
        }
        let doomed = prune_plan(&names, 10);
        assert_eq!(
            doomed.len(),
            4,
            "must drop exactly the 2 oldest events: {doomed:?}"
        );
        let oldest0 = format!("slooper-crash-{}-c0000005", 1_700_000_000);
        let oldest1 = format!("slooper-crash-{}-c0000005", 1_700_000_100);
        assert!(doomed
            .iter()
            .all(|d| d.starts_with(&oldest0) || d.starts_with(&oldest1)));
        assert!(doomed
            .iter()
            .all(|d| !(d.contains("1700000200") || d.contains("1700001100"))));
        assert!(!doomed.iter().any(|d| d == "notes.txt"));
        // keep >= events: nothing doomed
        assert!(prune_plan(&names, 12).is_empty());
    }

    #[test]
    fn w12e_utc_stamp_known_epochs() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_stamp(1_700_000_000), "2023-11-14 22:13:20 UTC");
        // leap-day + century-era edges (known dates, hand-verified):
        assert_eq!(utc_stamp(1_709_208_109), "2024-02-29 12:01:49 UTC");
        assert_eq!(utc_stamp(1_893_456_000), "2030-01-01 00:00:00 UTC");
    }

    #[test]
    fn w12e_crash_dir_honors_env_override() {
        let _s = CrashEnvSandbox::set("/tmp/w12e-sandbox");
        assert_eq!(crash_dir(), PathBuf::from("/tmp/w12e-sandbox"));
    }

    #[test]
    fn w12e_crash_me_mode_accepts_only_the_two_words() {
        let env_with = |v: &'static str| move |k: &str| {
            if k == CRASH_ME_ENV {
                Some(std::ffi::OsString::from(v))
            } else {
                None
            }
        };
        assert_eq!(crash_me_mode(env_with("panic")), Some("panic"));
        assert_eq!(crash_me_mode(env_with("fault")), Some("fault"));
        // Anything else is inert junk — a typo must NOT kill a user's DAW.
        assert_eq!(crash_me_mode(env_with("")), None);
        assert_eq!(crash_me_mode(env_with("Panic")), None);
        assert_eq!(crash_me_mode(env_with("1")), None);
        assert_eq!(crash_me_mode(|_k| None), None);
        // Linux contract: with BOTH words the inert twin acts on neither —
        // it does not even read the env var. (Windows runs these words for
        // real in the child pins below.)
        #[cfg(not(windows))]
        {
            self_test(env_with("fault"));
            self_test(env_with("panic"));
        }
    }

    #[test]
    #[cfg(unix)]
    fn w12e_linux_path_is_inert() {
        // Linux contract: install() registers nothing, note_crash() writes
        // nothing and must not even CREATE the folder — slooper's Linux
        // forensics stay 100 % coredumpctl.
        let dir = std::env::temp_dir().join(format!("w12e-inert-{}", std::process::id()));
        let _s = CrashEnvSandbox::set(dir.to_str().unwrap());
        install();
        note_crash("panic", "inert check");
        assert!(!dir.exists(), "Linux crash path touched the filesystem");
        // Quiet-flag logic is OS-neutral and exercised here:
        assert!(quiet_suppressed(|k| if k == CRASH_QUIET_ENV {
            Some(std::ffi::OsString::from("1"))
        } else {
            None
        }));
        assert!(!quiet_suppressed(|_k| None));
    }

    // ---- source consistency (both OSes; unique-call-site shapes) ---------

    /// Byte offset of the first **active** (non-commented) line containing
    /// `call`. A plain `src.contains(needle)` reads commented-out and
    /// doc-commented call sites as live wiring — proven: deleting the
    /// `note_panic` call but leaving it commented kept the old
    /// contains-pin GREEN. Every "main must call X" pin goes through this.
    fn active_call(src: &str, call: &str) -> Option<usize> {
        let mut off = 0usize;
        for line in src.lines() {
            let t = line.trim_start();
            if line.contains(call) && !t.starts_with("//") && !t.starts_with("///") {
                return Some(off);
            }
            off += line.len() + 1;
        }
        None
    }

    #[test]
    fn w12e_main_wires_the_guard_early() {
        let m = read_src("src/main.rs");
        // Unique shapes (trap lesson): exact ACTIVE call lines + an ordering
        // assert — the handler must exist BEFORE the audio backend can
        // start, but AFTER the probe-child dispatches so probes stay
        // handler-free.
        let i_install = active_call(&m, "slooper::crash::install();")
            .expect("main must install the crash guard (active, not commented)");
        let i_backend = m
            .find("let backend = slooper::audio::Backend::start();")
            .unwrap();
        let i_probe = m.find("probe_child_main(kind, &path)").unwrap();
        assert!(
            i_probe < i_install && i_install < i_backend,
            "install must sit between probe dispatch and Backend::start"
        );
        // The panic hook must log BEFORE its eprintln+abort (a console-less
        // GUI subsystem app loses anything printed-and-dropped after abort).
        let i_hook = active_call(&m, "slooper::crash::note_panic(info);")
            .expect("main's panic hook must note_panic (active, not commented)");
        let i_abort = m[i_hook..]
            .find("std::process::abort();")
            .map(|j| i_hook + j)
            .expect("hook still aborts (v42 rule)");
        assert!(i_hook < i_abort);
    }

    #[test]
    fn w12e_linux_twins_are_structurally_inert() {
        // The Linux install()/note_crash() bodies are empty — sabotage-
        // proving that behavior can only live Windows-side.
        let s = read_src("src/crash.rs");
        let install = s
            .find("#[cfg(not(windows))]\npub fn install() {}")
            .expect("Linux install must be a bare no-op");
        let note = s
            .find("#[cfg(not(windows))]\npub fn note_crash(_title: &str, _detail: &str) {}")
            .expect("Linux note_crash must be a bare no-op");
        assert!(install < note); // both exist, nothing else in their bodies
    }

    #[test]
    fn w12e_hard_fault_terminates_after_the_dump() {
        // The UEF writes, then terminates the process with
        // EXCEPTION_EXECUTE_HANDLER — never CONTINUE_SEARCH (that hands a
        // dead process to WER, which would stack its dialog on top of
        // ours). Pin the handler's FINAL EXPRESSION, sliced to the fn body
        // only: a whole-module `contains` check reads this very test's
        // needle strings and passes vacuously (self-reference trap).
        let s = read_src("src/crash.rs");
        let start = s
            .find("unsafe extern \"system\" fn uef(")
            .expect("uef handler exists");
        let after = &s[start..];
        // A column-0 `}` at line start closes the fn (inner blocks are
        // indented) — that boundary cuts the tests module out of `body`.
        let close = after.find("\n}\n").expect("uef closes at column 0");
        let body = &after[..close];
        assert!(
            body.trim_end().ends_with("EXCEPTION_EXECUTE_HANDLER"),
            "uef must TERMINATE with EXCEPTION_EXECUTE_HANDLER, got: …{}",
            &body[body.len().saturating_sub(120)..]
        );
        assert!(
            !body.contains("EXCEPTION_CONTINUE_SEARCH"),
            "uef must not defer to WER"
        );
        // And the dump precedes the note+box (the dump is the artifact).
        let i_dump = body.find("write_dump_at(").expect("dump write");
        let i_note = body.find("write_note_at(").expect("note write");
        let i_box = body.find("announce_crash(").expect("user announcement");
        assert!(i_dump < i_note && i_note < i_box, "dump -> note -> box");
    }

    // ---- Windows-only functional (runs on the windows-native CI lane) ----

    /// The APP executable to re-exec for the self-test pins. ONLY the
    /// explicitly staged copy (`SLOOPER_PROBE_EXE`, which the CI
    /// windows-native job points at a copy of the app exe taken BEFORE
    /// `cargo test` replaced that path with the libtest harness — W10 lore).
    /// The build.rs-baked path is deliberately NOT a fallback here: execing
    /// the harness would run the test suite instead of the app.
    #[cfg(windows)]
    fn app_child_exe() -> Option<std::path::PathBuf> {
        let p = std::env::var_os("SLOOPER_PROBE_EXE")?;
        if p.is_empty() || !std::path::Path::new(&p).is_file() {
            return None;
        }
        Some(std::path::PathBuf::from(p))
    }

    /// Run the app with the self-test switch: returns (died, stderr). The
    /// child gets a SANDBOXED crash dir + config dir (the W12d bleed rule)
    /// and the quiet flag (no modal box on a CI runner). Bounded wait: a
    /// child that never dies is a FAILURE, not a hang.
    #[cfg(windows)]
    fn run_self_test(exe: &std::path::Path, mode: &str, dir: &std::path::Path) -> (bool, String) {
        use std::process::{Command, Stdio};
        let mut child = Command::new(exe)
            .env(CRASH_ME_ENV, mode)
            .env(paths::CRASH_DIR_ENV, dir)
            .env(CRASH_QUIET_ENV, "1")
            .env("SLOOPER_CONFIG_DIR", dir.join("cfg"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn slooper.exe for the self-test");
        let t0 = std::time::Instant::now();
        let mut exited = false;
        while t0.elapsed() < std::time::Duration::from_secs(90) {
            if child.try_wait().expect("try_wait").is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let out = if exited {
            child.wait_with_output().expect("collect stderr")
        } else {
            let _ = child.kill();
            panic!("self-test child never died ({mode}) — the guard did not fire");
        };
        (!out.status.success(), String::from_utf8_lossy(&out.stderr).to_string())
    }

    #[cfg(windows)]
    fn crash_files(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("crash dir {} missing: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("slooper-crash-"))
            .collect();
        v.sort();
        v
    }

    #[cfg(windows)]
    #[test]
    fn w12e_windows_self_test_panic_writes_only_a_note() {
        let Some(exe) = app_child_exe() else {
            eprintln!("skip: SLOOPER_PROBE_EXE (staged app exe) unset");
            return;
        };
        let dir = std::env::temp_dir().join(format!("w12e-self-panic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (died, stderr) = run_self_test(&exe, "panic", &dir);
        assert!(died, "the panic self-test must end the process");
        // Identity: the message came from OUR tripwire, not from some other
        // binary behind SLOOPER_PROBE_EXE.
        assert!(
            stderr.contains("SLOOPER_CRASH_ME=panic"),
            "the app itself must panic: {stderr}"
        );
        let names = crash_files(&dir);
        assert_eq!(names.len(), 1, "exactly one artifact: {names:?}");
        assert!(names[0].ends_with("-panic.txt"), "{names:?}");
        let body = std::fs::read_to_string(dir.join(&names[0])).unwrap();
        assert!(
            body.contains("backtrace:"),
            "the note carries the backtrace: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn w12e_windows_self_test_fault_writes_a_real_minidump() {
        let Some(exe) = app_child_exe() else {
            eprintln!("skip: SLOOPER_PROBE_EXE (staged app exe) unset");
            return;
        };
        let dir = std::env::temp_dir().join(format!("w12e-self-fault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (died, _stderr) = run_self_test(&exe, "fault", &dir);
        assert!(died, "the fault self-test must end the process");
        let names = crash_files(&dir);
        let Some(dmp) = names.iter().find(|n| n.ends_with(".dmp")) else {
            panic!("no .dmp written: {names:?}");
        };
        let Some(txt) = names.iter().find(|n| n.ends_with(".txt")) else {
            panic!("no .txt note written: {names:?}");
        };
        // A real access violation = STATUS_ACCESS_VIOLATION in the stem…
        assert!(dmp.contains("c0000005"), "stem must carry the NTSTATUS: {dmp}");
        // …and one EVENT: dump and note share the stem, so they age together.
        assert_eq!(
            dmp.trim_end_matches(".dmp"),
            txt.trim_end_matches(".txt"),
            "{dmp} vs {txt}"
        );
        let bytes = std::fs::read(dir.join(dmp)).unwrap();
        assert!(bytes.len() > 4096, "minidump suspiciously small: {}", bytes.len());
        assert_eq!(&bytes[..4], b"MDMP", "not a minidump");
        let body = std::fs::read_to_string(dir.join(txt)).unwrap();
        assert!(
            body.contains("minidump written: true"),
            "the note must report the dump: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn w12e_windows_note_and_dump_are_written() {
        let dir = std::env::temp_dir().join(format!("w12e-win-{}", std::process::id()));
        let _s = CrashEnvSandbox::set(dir.to_str().unwrap());
        note_crash("panic", "functional pin detail line");
        let files: Vec<String> = std::fs::read_dir(&dir)
            .expect("crash folder must exist after a note write")
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        assert_eq!(files.len(), 1, "one note: {files:?}");
        assert!(files[0].starts_with("slooper-crash-") && files[0].ends_with("-panic.txt"));
        let body = std::fs::read_to_string(dir.join(&files[0])).unwrap();
        assert!(body.contains("functional pin detail line"));
        assert!(body.contains(env!("CARGO_PKG_VERSION")));
        // Same-second second write must collide-suffix, not clobber:
        note_crash("panic", "second");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn w12e_windows_minidump_carries_mdmp_magic() {
        // A real MiniDumpWriteDump against our own process with NO exception
        // record (null EXCEPTION_POINTERS is the documented state-only
        // form): the file must materialize as a valid MDMP.
        let dir = std::env::temp_dir().join(format!("w12e-dmp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_dump_at(&dir, 1_700_000_000, "c0000005", std::ptr::null()).expect("dump");
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 1024, "minidump suspiciously small: {}", bytes.len());
        assert_eq!(&bytes[..4], b"MDMP");
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("c0000005"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn w12e_windows_prunes_oldest_events_on_disk() {
        let dir = std::env::temp_dir().join(format!("w12e-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Plant CRASH_KEEP+3 events (txt+dmp each) with old epochs.
        let planted = CRASH_KEEP + 3;
        for i in 0..planted {
            let ts = 1_000_000_000 + i as u64 * 1000;
            let stem = crash_stem(ts, "c0000005");
            std::fs::write(dir.join(format!("{stem}.txt")), "old").unwrap();
            std::fs::write(dir.join(format!("{stem}.dmp")), "old").unwrap();
        }
        let _s = CrashEnvSandbox::set(dir.to_str().unwrap());
        note_crash("panic", "the fresh event triggers pruning");
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        // keep newest CRASH_KEEP events; the fresh note is #1.
        assert_eq!(
            left.iter().filter(|n| n.ends_with(".txt")).count(),
            CRASH_KEEP,
            "pruning missed: {left:?}"
        );
        assert_eq!(
            left.iter().filter(|n| n.ends_with(".dmp")).count(),
            CRASH_KEEP - 1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
