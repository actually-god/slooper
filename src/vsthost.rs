//! VST hosting: discover plugins in user-set VST folders, instantiate them,
//! and run them through realtime stage entries.
//!
//! One [`RtStageEntry`] per track holds the plugin plus ring buffers:
//! slooper's block size is whatever the JACK callback hands us, while both
//! plugin APIs (and the VST3 realtime runner especially) want a fixed
//! processing size, so audio is accumulated and fed to the plugin in fixed
//! [`VST_BLOCK`]-sized chunks. Tracks are mono in slooper: the mono signal
//! is upfed to every plugin input channel and the output bus is averaged
//! back to mono.
//!
//! Threading discipline (mirrors `vst3_host::realtime` rules):
//! * Instances are created on the UI thread and moved (as
//!   `Arc<RtStageEntry>`) into the engine, where the audio thread processes
//!   them. `process`/`flush` use the entry's interior mutability and are
//!   audio-thread-only (same shape as the scripted-FX instances).
//! * A replaced entry is *held* by the engine until the UI thread drains
//!   the holding list, so plugin destructors — and VST3 COM teardown, which
//!   the runner hands off rather than running in place — execute on the
//!   creator (UI) thread.
//! * Parameters ride [`VstParams`] (normalized 0..=1 atomics shared with
//!   the UI). VST3 changes are additionally pushed into the runner's
//!   command ring ([`RtStageEntry::set_param`]); VST2 has no ring, so the
//!   audio thread polls the atomics and calls `set_parameter` at the block
//!   boundary.
//! * [`Backend`] wraps raw plugin handles; the `unsafe impl Send` documents
//!   why moving the *owner* between threads is sound.

use std::cell::UnsafeCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Fixed processing granularity handed to plugins (samples).
pub const VST_BLOCK: usize = 512;
/// Number of sliders the VST panel exposes per plugin.
pub const VST_MAX_PARAMS: usize = 64;
/// Ring capacity: covers the largest plausible JACK callback plus slop.
/// The rings never grow past this (oldest samples are dropped on overflow),
/// which keeps the audio thread allocation-free after load.
const RING_CAP: usize = 1 << 16;

/// Realtime f32 ring (v16 pipeline pass): fixed capacity, memcpy in/out.
/// The previous per-sample `VecDeque` churn (push/pop/Option-unwrap per
/// sample, 3 rings per plugin) showed up as pointless audio-thread work;
/// this version moves whole blocks and never allocates or unwraps.
/// Overflow keeps the NEWEST samples and reports how many oldest were
/// dropped, so the echo ring can stay history-consistent (v0.6 behavior).
struct FxRing {
    buf: Vec<f32>,
    head: usize,
    count: usize,
}

impl FxRing {
    fn with_capacity(cap: usize) -> Self {
        Self {
            buf: vec![0.0f32; cap],
            head: 0,
            count: 0,
        }
    }
    #[inline]
    fn len(&self) -> usize {
        self.count
    }
    #[inline]
    fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }
    /// Append a block; returns how many oldest samples overflowed away.
    fn push_slice(&mut self, s: &[f32]) -> usize {
        let cap = self.buf.len();
        if s.len() >= cap {
            // Bigger than the whole ring: keep its newest window.
            let start = s.len() - cap;
            self.buf.copy_from_slice(&s[start..]);
            let dropped = self.count + start;
            self.head = 0;
            self.count = cap;
            return dropped;
        }
        let over = self.count + s.len();
        let dropped = over.saturating_sub(cap);
        if dropped > 0 {
            self.head = (self.head + dropped) % cap;
            self.count -= dropped;
        }
        let start = (self.head + self.count) % cap;
        let first = (cap - start).min(s.len());
        self.buf[start..start + first].copy_from_slice(&s[..first]);
        if s.len() > first {
            self.buf[..s.len() - first].copy_from_slice(&s[first..]);
        }
        self.count += s.len();
        dropped
    }
    /// Drop `n` oldest samples (max `count`).
    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.count);
        self.head = (self.head + n) % self.buf.len();
        self.count -= n;
    }
    /// Copy out up to `dst.len()` samples; returns how many were taken.
    fn drain_into(&mut self, dst: &mut [f32]) -> usize {
        let cap = self.buf.len();
        let n = self.count.min(dst.len());
        let first = (cap - self.head).min(n);
        dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        if n > first {
            dst[first..n].copy_from_slice(&self.buf[..n - first]);
        }
        self.head = (self.head + n) % cap;
        self.count -= n;
        n
    }
}

// ---------------------------------------------------------------- kinds ---

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VstKind {
    V2,
    V3,
}

impl VstKind {
    pub fn tag(self) -> &'static str {
        match self {
            VstKind::V2 => "VST2",
            VstKind::V3 => "VST3",
        }
    }
    /// Round-trip through config strings.
    pub fn parse(s: &str) -> Option<VstKind> {
        match s {
            "VST2" => Some(VstKind::V2),
            "VST3" => Some(VstKind::V3),
            _ => None,
        }
    }
}

/// One plugin found while scanning the VST folders.
#[derive(Clone, Debug)]
pub struct VstEntry {
    pub path: PathBuf,
    pub kind: VstKind,
    pub name: String,
    pub vendor: String,
    /// Plugin takes no audio inputs (VST2 `inputs == 0`, VST3 empty input
    /// buses): it generates sound from MIDI, so the UI offers it in the
    /// per-track synthesizer slot as well as the effects chain.
    pub instrument: bool,
    /// Set when the file looked like a plugin but failed to probe; the
    /// picker shows it greyed out instead of silently hiding broken files.
    pub error: Option<String>,
}

/// 0-based MIDI channel index -> vst3-host channel enum (no `from_index` there).
const MIDI_CHANNELS: [vst3_host::midi::MidiChannel; 16] = [
    vst3_host::midi::MidiChannel::Ch1,
    vst3_host::midi::MidiChannel::Ch2,
    vst3_host::midi::MidiChannel::Ch3,
    vst3_host::midi::MidiChannel::Ch4,
    vst3_host::midi::MidiChannel::Ch5,
    vst3_host::midi::MidiChannel::Ch6,
    vst3_host::midi::MidiChannel::Ch7,
    vst3_host::midi::MidiChannel::Ch8,
    vst3_host::midi::MidiChannel::Ch9,
    vst3_host::midi::MidiChannel::Ch10,
    vst3_host::midi::MidiChannel::Ch11,
    vst3_host::midi::MidiChannel::Ch12,
    vst3_host::midi::MidiChannel::Ch13,
    vst3_host::midi::MidiChannel::Ch14,
    vst3_host::midi::MidiChannel::Ch15,
    vst3_host::midi::MidiChannel::Ch16,
];

/// Default VST search folders (the usual Linux conventions), existing ones
/// only. Users may add their own folders in the VST window.
pub fn default_vst_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(&home).join(".vst3"));
        dirs.push(PathBuf::from(&home).join(".vst"));
        // Wine prefixes: the folders common Windows installers drop
        // plugins into, so a .dll placed there is found (and yabridged)
        // with zero configuration.
        let win = [
            "drive_c/Program Files/Steinberg/VSTPlugins",
            "drive_c/Program Files/Common Files/VSTPlugins",
            "drive_c/Program Files/VSTPlugins",
            "drive_c/VSTPlugins",
        ];
        for base in ["wine", ".wine"] {
            for w in win {
                dirs.push(PathBuf::from(&home).join(base).join(w));
            }
        }
    }
    dirs.push(PathBuf::from("/usr/lib/vst3"));
    dirs.push(PathBuf::from("/usr/lib/vst"));
    dirs.push(PathBuf::from("/usr/lib64/vst3"));
    dirs.push(PathBuf::from("/usr/local/lib/vst3"));
    dirs.push(PathBuf::from("/usr/local/lib/vst"));
    if let Ok(extra) = std::env::var("VST_PATH") {
        for p in extra.split(':') {
            if !p.is_empty() {
                dirs.push(PathBuf::from(p));
            }
        }
    }
    dirs.retain(|d| d.is_dir());
    dirs
}

/// Scan the given folders for VST2/VST3 plugins. Each folder is searched
/// directly plus one level of subdirectories (the "label pack" layout).
/// VST3 probes load the factory without instantiating; VST2 probes
/// instantiate once (required for metadata) and drop again — both safe on
/// a UI thread but slow, so this runs only on explicit user rescans.
pub fn scan(dirs: &[PathBuf]) -> Vec<VstEntry> {
    // Production default: honour the wine-free-scan switch if set.
    scan_with(dirs, std::env::var("SLOOPER_SKIP_BRIDGED").is_ok())
}

/// `scan` with the yabridge-skip decision made explicit (unit tests pass
/// `skip_bridged = true` for local scans — instantiating a Wine wrapper
/// from a unit test risks aborting the process, see below — and `false`
/// for fixtures that must prove sidecar discovery works).
pub fn scan_with(dirs: &[PathBuf], skip_bridged: bool) -> Vec<VstEntry> {
    let mut out = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for root in dirs {
        if !root.is_dir() {
            continue;
        }
        let subdirs = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir());
        for dir in std::iter::once(root.clone()).chain(subdirs) {
            let entries = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for e in entries.flatten() {
                let p = e.path();
                if seen.iter().any(|s| s == &p) {
                    continue;
                }
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // v17: an already-instantiated, unchanged file is NOT probed
                // again — creating (and worse, dropping) a second instance of
                // a live library tears down the toolkit singletons the live
                // instance still uses (the Vitalium crash). Reuse its identity.
                if let Some(cached) = cached_entry(&p) {
                    seen.push(p.clone());
                    out.push(cached);
                    continue;
                }
                // yabridge wrappers dlopen straight into a Wine-host probe;
                // when the Wine side cannot come up, yabridge's chainloader
                // calls abort() INSIDE this process — unrecoverable.
                if skip_bridged && (name.contains(".dll.") || dir.to_string_lossy().contains("/yabridge/")) {
                    continue;
                }
                // v19.1: a real chainloader wrapper is guarded even in
                // production — if the probe says Wine cannot host, the entry
                // gets an error note instead of aborting the process.
                // Only for formats scan would actually dlopen: raw `.dll`s
                // inside yabridge folders stay invisible, as always.
                let probeable = name.ends_with(".vst3")
                    || (name.ends_with(".vst") && p.is_dir())
                    || (name.ends_with(".so") && !p.is_dir());
                if probeable && is_wine_bridged(&p) {
                    if let Err(why) = bridge_ok(bridge_needs32(&p)) {
                        seen.push(p.clone());
                        out.push(bridge_error_entry(&p, why));
                        continue;
                    }
                }
                if name.ends_with(".vst3") {
                    seen.push(p.clone());
                    out.push(probe_vst3(&p));
                } else if name.ends_with(".vst") && p.is_dir() {
                    // VST2 bundle layout (`Foo.vst/Contents/<arch>.so`) —
                    // what yabridge writes for Windows plugins and what
                    // many Linux packs ship. Probe the real library inside.
                    if let Some(so) = bundle_so(&p) {
                        seen.push(p.clone());
                        out.push(probe_vst2(&so));
                    }
                } else if name.ends_with(".so") && !p.is_dir() {
                    seen.push(p.clone());
                    out.push(probe_vst2(&p));
                }
            }
        }
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

/// Locate the loadable library inside a `Foo.vst` bundle directory.
fn bundle_so(dir: &Path) -> Option<PathBuf> {
    let c = dir.join("Contents");
    for name in ["x86-64.so", "x86_64-linux.so", "x86_64.so", "x86-linux.so"] {
        let p = c.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    // Unknown arch spelling: take the only .so if there is exactly one.
    let oks: Vec<PathBuf> = std::fs::read_dir(&c)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "so").unwrap_or(false))
        .collect();
    if oks.len() == 1 {
        Some(oks.into_iter().next().unwrap())
    } else {
        None
    }
}

/// True when the yabridge control tool is installed (Windows VST support).
pub fn yabridge_available() -> bool {
    yabridgectl_path().is_some()
}

fn yabridgectl_path() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        cands.push(PathBuf::from(&home).join(".local/bin/yabridgectl"));
    }
    cands.push(PathBuf::from("/usr/bin/yabridgectl"));
    cands.push(PathBuf::from("/usr/local/bin/yabridgectl"));
    cands.into_iter().find(|p| p.is_file())
}

/// v0.10 drop-in Windows VST support (yabridge + Wine), no configuration.
///
/// Scans the plugin folders for `.dll` files — Windows plugins the user
/// simply dropped in — registers each containing folder with yabridgectl
/// and runs `sync`, which writes native `.so` wrappers into `~/.vst` and
/// `~/.vst3` (the folders we already scan). Returns a one-line summary
/// when any wrapping happened, `None` when there was nothing to do or
/// yabridge is not installed. Never fails the caller's native scan.
pub fn yabridge_sync(roots: &[PathBuf]) -> Option<String> {
    let exe = yabridgectl_path()?;
    // Collect the folders that directly contain Windows plugin files.
    let mut dll_dirs: Vec<PathBuf> = Vec::new();
    for root in roots {
        let subdirs = std::fs::read_dir(root)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir());
        for dir in std::iter::once(root.clone()).chain(subdirs) {
            let has = std::fs::read_dir(&dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .any(|e| {
                    e.path()
                        .extension()
                        .map(|x| x.eq_ignore_ascii_case("dll"))
                        .unwrap_or(false)
                });
            if has && !dll_dirs.contains(&dir) {
                dll_dirs.push(dir);
            }
        }
    }
    if dll_dirs.is_empty() {
        return None;
    }
    let mut added = 0usize;
    for d in &dll_dirs {
        // `add` is idempotent in practice; errors (already known) are fine.
        let _ = std::process::Command::new(&exe)
            .arg("add")
            .arg(d)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        added += 1;
    }
    let out = std::process::Command::new(&exe)
        .arg("sync")
        .env("PATH", format!("{}:/usr/bin:/bin", exe.parent()?.display()))
        .output()
        .ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let line = text
        .lines()
        .rev()
        .find(|l| l.contains("Finished setting up"))
        .unwrap_or("yabridge sync done")
        .trim()
        .to_string();
    Some(format!(
        "yabridge: {added} Windows plugin folder(s) processed — {line}"
    ))
}

// ---------------------------------------------------------------------------
// v19.1 Wine-bridge guard. A yabridge chainloader calls abort() INSIDE this
// process when its Wine host cannot start (reproduced on the release install:
// desktop session without WINELOADER → WoW64-only Wine 11 → the 32-bit host
// dies `c000007b` → chainloader prints "The Wine host process has exited
// unexpectedly" and aborts the app). dlopen of the wrapper IS the abort —
// there is no error path after it. The only defense that works is asking
// Wine, BEFORE any dlopen, whether it can run its own host binary at all.
// ---------------------------------------------------------------------------

/// Fingerprint yabridge embeds in every chainloader wrapper it copies into
/// the plugin folders (its own debug/source path, survives stripping).
const CHAINLOADER_MAGIC: &[u8] = b"libyabridge-chainloader";

/// True when `p` contains yabridge chainloader magic bytes. Only the first
/// 512 KiB of small files is read — wrappers are ~89 KiB, real plugin
/// binaries are usually larger and never touched.
fn is_chainloader_file(p: &Path) -> bool {
    if p.metadata().map(|m| m.len() > 512 * 1024).unwrap_or(true) {
        return false;
    }
    std::fs::File::open(p)
        .and_then(|mut f| {
            use std::io::Read;
            let mut buf = vec![0u8; 512 * 1024];
            let n = f.read(&mut buf)?;
            buf.truncate(n);
            Ok(buf
                .windows(CHAINLOADER_MAGIC.len())
                .any(|w| w == CHAINLOADER_MAGIC))
        })
        .unwrap_or(false)
}

/// Every `.so` inside a bundle's `Contents/` (directly or one arch subdir
/// down) — covers both the VST3 `Contents/x86_64-linux/` and VST2
/// `Contents/x86-64.so` layouts. Used for the chainloader magic check only.
fn bundle_inner_libs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir.join("Contents")) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Ok(rd2) = std::fs::read_dir(&p) {
                out.extend(rd2.flatten().map(|e2| e2.path()).filter(|q| {
                    q.extension().map(|x| x == "so").unwrap_or(false)
                }));
            }
        } else if p.extension().map(|x| x == "so").unwrap_or(false) {
            out.push(p);
        }
    }
    out
}

/// The Windows `.dll` a wrapper sits next to: yabridge keeps `X.so`/
/// `X.dll.so`/`X.vst3` and `X.dll` side by side.
fn bridge_companion_dll(p: &Path) -> Option<PathBuf> {
    let name = p.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".so")
        .or_else(|| name.strip_suffix(".vst3"))
        .unwrap_or(name);
    let dll = if stem.ends_with(".dll") {
        stem.to_string()
    } else {
        format!("{stem}.dll")
    };
    let p = p.parent()?.join(dll);
    p.is_file().then_some(p)
}

/// PE `Machine` field of a Windows DLL (`None` when not a parseable PE):
/// 0x014c = i386, 0x8664 = x86-64.
fn pe_machine(dll: &Path) -> Option<u16> {
    let b = std::fs::read(dll).ok()?;
    if b.len() < 0x40 || &b[0..2] != b"MZ" {
        return None;
    }
    let off = u32::from_le_bytes([b[0x3c], b[0x3d], b[0x3e], b[0x3f]]) as usize;
    if off + 6 > b.len() || &b[off..off + 4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes([b[off + 4], b[off + 5]]))
}

/// Whether this wrapper needs a 32-bit-capable Wine host. Unknown PE → true:
/// guessing 64 wrongly means an in-process abort, guessing 32 wrongly means
/// a false "unavailable" note. The asymmetry decides.
fn bridge_needs32(p: &Path) -> bool {
    !matches!(
        bridge_companion_dll(p).as_deref().and_then(pe_machine),
        Some(0x8664)
    )
}

/// True when probing/loading `p` would dlopen a yabridge chainloader —
/// i.e. drag Wine into THIS process.
pub fn is_wine_bridged(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if p.to_string_lossy().contains("/yabridge/") {
        return true;
    }
    if name.ends_with(".vst3") || name.ends_with(".vst") {
        let libs = bundle_inner_libs(p);
        return libs.iter().any(|so| is_chainloader_file(so))
            || (name.contains(".dll.") && bridge_companion_dll(p).is_some());
    }
    if is_chainloader_file(p) {
        return true;
    }
    // Sidecar naming ALONE is not proof (the v10 fixture renames a real
    // Linux plugin that way): it only guards when the companion `.dll`
    // actually parses as a Windows binary.
    name.contains(".dll.")
        && bridge_companion_dll(p)
            .map(|d| pe_machine(&d).is_some())
            .unwrap_or(false)
}

/// The Wine loader the chainloader child would pick (yabridge honours
/// `WINELOADER`, else `wine` on PATH): probe THAT one, not some other wine.
/// `SLOOPER_WINE` overrides everything (test seam).
fn bridge_wine_loader() -> Option<PathBuf> {
    for var in ["SLOOPER_WINE", "WINELOADER"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let c = Path::new(dir).join("wine");
            if c.is_file() {
                return Some(c);
            }
        }
    }
    ["/usr/bin/wine", "/usr/local/bin/wine"]
        .into_iter()
        .map(PathBuf::from)
        .find(|c| c.is_file())
}

/// Where yabridge installed its host binaries (the wrappers exec these
/// under Wine). `SLOOPER_YABRIDGE_DIR` overrides (test seam).
fn bridge_host_exe(need32: bool) -> Option<PathBuf> {
    let names: [&str; 2] = if need32 {
        ["yabridge-host-32.exe.so", "yabridge-host-32.exe"]
    } else {
        ["yabridge-host.exe.so", "yabridge-host.exe"]
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(v) = std::env::var("SLOOPER_YABRIDGE_DIR") {
        dirs.push(PathBuf::from(v));
    }
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(x).join("yabridge"));
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/yabridge"));
    }
    dirs.extend(["/usr/share/yabridge", "/usr/lib/yabridge"].map(PathBuf::from));
    dirs.iter().find_map(|d| {
        names
            .iter()
            .find_map(|n| {
                let p = d.join(n);
                p.is_file().then_some(p)
            })
    })
}

/// Run the Wine host once, argument-free, and judge the loader from its
/// output. Success markers: the host banner/usage (Wine RAN the PE — the
/// host then exits with a usage error or keeps running on its socket, and
/// a still-alive host at the deadline counts too). A silent, quick death is
/// treated as broken: a loader that cannot even produce the host's banner
/// has no business hosting plugins.
fn bridge_probe(loader: &Path, exe: &Path, need32: bool) -> Result<(), String> {
    let out_path = std::env::temp_dir().join(format!(
        "slooper-wine-probe-{}-{}.log",
        std::process::id(),
        if need32 { "32" } else { "64" }
    ));
    let f = std::fs::File::create(&out_path).map_err(|e| format!("probe temp file: {e}"))?;
    let f2 = f.try_clone().ok();
    let mut child = std::process::Command::new(loader)
        .arg(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(f))
        .stderr(f2.map(std::process::Stdio::from).unwrap_or(std::process::Stdio::null()))
        .spawn()
        .map_err(|e| {
            format!("Wine loader '{}' could not be started: {e}", loader.display())
        })?;
    let start = std::time::Instant::now();
    let mut alive_at_deadline = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed().as_millis() >= 20_000 {
                    let _ = child.kill();
                    let _ = child.wait();
                    alive_at_deadline = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }
    let txt = std::fs::read_to_string(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    if txt.contains("host version") || txt.contains("Usage: yabridge-host") {
        return Ok(()); // Wine ran the host — the banner is the host itself talking
    }
    if alive_at_deadline {
        return Ok(()); // host process alive = Wine works, host waits on its socket
    }
    let why = if txt.contains("c000007b") {
        if need32 {
            "this Wine cannot run 32-bit Windows code (c000007b)"
        } else {
            "Wine cannot load its host binary (c000007b)"
        }
    } else if txt.contains("wineserver") {
        // Mixed Wine versions sharing one prefix: another build's wineserver
        // owns it right now. The REAL host load would hit the same wall —
        // surface it with the cure instead of a generic "cannot start".
        "a wineserver from a different Wine build holds the prefix — close all Windows-audio programs and retry"
    } else if txt.contains("failed to start") {
        "Wine failed to start its plugin host"
    } else {
        "Wine could not start its plugin host"
    };
    let tail: String = txt
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .join(" / ")
        .chars()
        .take(160)
        .collect();
    Err(if tail.is_empty() {
        why.to_string()
    } else {
        format!("{why} — last output: {tail}")
    })
}

/// Cached outcome of the two bitness probes; the env fingerprint invalidates
/// it, so tests (and fixed environments) re-probe instead of inheriting.
struct BridgeProbeResult {
    key: String,
    ok32: Result<(), String>,
    ok64: Result<(), String>,
}

fn bridge_probes() -> std::sync::MutexGuard<'static, Option<BridgeProbeResult>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<BridgeProbeResult>>> =
        std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Ask Wine — once per process, once per bitness — whether it can host
/// Windows plugins. `SLOOPER_BRIDGE_PROBE=0` trusts wrappers blindly
/// (power-user escape hatch), `SLOOPER_SKIP_BRIDGED=1` never touches them.
fn bridge_ok(need32: bool) -> Result<(), String> {
    if std::env::var("SLOOPER_BRIDGE_PROBE").map(|v| v == "0").unwrap_or(false) {
        return Ok(());
    }
    let key = [
        std::env::var("SLOOPER_WINE").unwrap_or_default(),
        std::env::var("WINELOADER").unwrap_or_default(),
        std::env::var("SLOOPER_YABRIDGE_DIR").unwrap_or_default(),
        std::env::var("PATH").unwrap_or_default(),
    ]
    .join("\u{1}");
    let mut slot = bridge_probes();
    if slot.as_ref().map(|c| c.key != key).unwrap_or(true) {
        let probe = |n32: bool| match (bridge_wine_loader(), bridge_host_exe(n32)) {
            (None, _) => Err("Wine not found (install Wine or set WINELOADER)".to_string()),
            (_, None) => Err("yabridge host binaries not found (run `yabridgectl sync`)".to_string()),
            (Some(w), Some(h)) => bridge_probe(&w, &h, n32),
        };
        *slot = Some(BridgeProbeResult {
            key,
            ok32: probe(true),
            ok64: probe(false),
        });
    }
    let c = slot.as_ref().expect("bridge probe cache just filled");
    if need32 {
        c.ok32.clone()
    } else {
        c.ok64.clone()
    }
}

/// Scan result for a bridged plugin the probe refused: a greyed "not
/// loaded" row naming what to do, never a crashed app.
fn bridge_error_entry(p: &Path, why: String) -> VstEntry {
    let name = base_label(p).trim_end_matches(".dll").to_string();
    let kind = if p.to_string_lossy().contains(".vst3") {
        VstKind::V3
    } else {
        VstKind::V2
    };
    VstEntry {
        path: p.to_path_buf(),
        kind,
        name: format!("{name} (Windows)"),
        vendor: "Wine".to_string(),
        instrument: false,
        error: Some(format!(
            "Wine bridge unavailable: {why}. Set WINELOADER to a Wine build that runs {}-bit plugins, then rescan",
            if bridge_needs32(p) { "32" } else { "64" }
        )),
    }
}

fn base_label(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .trim_end_matches(".vst3")
        .trim_end_matches(".so")
        .to_string()
}

fn probe_vst3(path: &Path) -> VstEntry {
    // Never re-instantiate a live/unchanged library (see `cached_entry`).
    if let Some(cached) = cached_entry(path) {
        return cached;
    }
    match vst3_host::discovery::get_plugin_info(path) {
        Ok(info) => {
            // Instrument or effect? Instantiate once (UI thread, scan only)
            // and look at the audio buses: a synth has no audio inputs.
            // v17: the probe instance is LEAKED, exactly like the VST2
            // probe's documented rule — dropping it fires `deleteInstance`
            // /terminate into the binary, and the one-shot process globals
            // it tears down (JUCE singletons, DPF connection points) belong
            // to every other instance of this plugin in the process: the
            // Vitalium crash (delete-while-active warnings, then SEGV).
            let mut instrument = false;
            if let Ok(p) = vst3_host::simple::load_plugin(path) {
                // Trust the plugin's declared category first: synths with a
                // feedback/sidechain bus (JS80P) declare `Instrument|Synth`
                // yet HAVE an audio input bus, and the old "no inputs" bus
                // heuristic shelved them into the FX list. Only fall back to
                // the bus shape when the category says nothing either way.
                let cat = p.info().category.to_ascii_lowercase();
                instrument = if cat.contains("instrument") || cat.contains("synth") {
                    true
                } else if cat.contains("fx") || cat.contains("effect") {
                    false
                } else {
                    p.bus_arrangements()
                        .ok()
                        .map(|b| b.inputs.is_empty())
                        .unwrap_or(false)
                };
                note_instantiated(
                    path,
                    VstKind::V3,
                    info.name.clone(),
                    info.vendor.clone(),
                    instrument,
                );
                std::mem::forget(p);
            }
            VstEntry {
                path: path.to_path_buf(),
                kind: VstKind::V3,
                name: info.name,
                vendor: info.vendor,
                instrument,
                error: None,
            }
        }
        Err(err) => VstEntry {
            path: path.to_path_buf(),
            kind: VstKind::V3,
            name: base_label(path),
            vendor: String::new(),
            instrument: false,
            error: Some(err.to_string()),
        },
    }
}

fn probe_vst2(path: &Path) -> VstEntry {
    use vst2::plugin::Plugin;
    // Never re-instantiate a live/unchanged library (see `cached_entry`).
    if let Some(cached) = cached_entry(path) {
        return cached;
    }
    let probed = vst2::host::PluginLoader::load(path, Arc::new(Mutex::new(StubHost)))
        .map(|mut l| l.instance());
    match probed {
        Ok(Ok(inst)) => {
            let info = inst.get_info();
            // NEVER drop the probe instance: PluginInstance::drop dispatches
            // Shutdown — VSTPluginMain(terminate) — into the plugin binary,
            // and toolkits with one-shot process globals (VSTGUI et al.) tear
            // down editor machinery every LATER instance still uses: the next
            // effEditOpen dereferences freed C++ statics and dies (SIGSEGV
            // mid-editor-open). Read the metadata, then leak the instance.
            note_instantiated(
                path,
                VstKind::V2,
                info.name.clone(),
                info.vendor.clone(),
                info.inputs == 0,
            );
            std::mem::forget(inst);
            VstEntry {
                path: path.to_path_buf(),
                kind: VstKind::V2,
                name: info.name,
                vendor: info.vendor,
                instrument: info.inputs == 0,
                error: None,
            }
        }
        Ok(Err(err)) | Err(err) => VstEntry {
            path: path.to_path_buf(),
            kind: VstKind::V2,
            name: base_label(path),
            vendor: String::new(),
            instrument: false,
            error: Some(err.to_string()),
        },
    }
}

// -------------------------------------------------------------- params ---

/// Slider metadata captured at load time (read from the plugin).
#[derive(Clone, Debug)]
pub struct VstParamDesc {
    /// VST3 parameter id, or VST2 parameter index.
    pub id: u32,
    pub name: String,
    /// Normalized (0..=1) default value.
    pub default: f32,
    /// False for VST3 read-only meters — the UI hides them.
    pub writable: bool,
}

/// Shared parameter cell: the UI writes, the audio thread reads (VST2) or
/// the UI additionally pushes through the runner ring (VST3). Values are
/// normalized 0..=1 for both plugin generations — the space both APIs use
/// for `setParameter`.
pub struct VstParams {
    slots: Vec<AtomicU32>,
    generation: AtomicU64,
}

impl VstParams {
    pub fn new(descs: &[VstParamDesc]) -> Arc<Self> {
        Arc::new(Self {
            slots: descs
                .iter()
                .map(|d| AtomicU32::new(d.default.to_bits()))
                .collect(),
            generation: AtomicU64::new(1),
        })
    }

    /// Mirror seeded with explicit live values (clamped to the slots we have
    /// descriptors for). Used when the plugin's *current* state — not its
    /// reported defaults — is the correct baseline; see `open_v3`.
    pub fn with_values(descs: &[VstParamDesc], values: &[f32]) -> Arc<Self> {
        Arc::new(Self {
            slots: descs
                .iter()
                .enumerate()
                .map(|(k, d)| {
                    let v = values.get(k).copied().unwrap_or(d.default).clamp(0.0, 1.0);
                    AtomicU32::new(v.to_bits())
                })
                .collect(),
            generation: AtomicU64::new(1),
        })
    }

    /// UI side: store a normalized value and bump the generation counter.
    pub fn set(&self, index: usize, value: f32) {
        if let Some(slot) = self.slots.get(index) {
            let v = value.clamp(0.0, 1.0);
            if slot.swap(v.to_bits(), Ordering::Relaxed) != v.to_bits() {
                self.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn get(&self, index: usize) -> f32 {
        self.slots
            .get(index)
            .map(|s| f32::from_bits(s.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

// -------------------------------------------------------------- backend ---

// Both backends wrap raw plugin handles (libloading instances, VST3 COM
// pointers), so neither auto-Sends. Ownership of a plugin moves
// create(UI) -> process(audio) -> held/reap(UI) in a strictly
// happens-before chain: exactly one thread owns the plugin at any moment
// and every destroy/dtor runs on the UI thread. VST3's IAudioProcessor is
// specified for precisely this pattern (created on the control thread,
// process() called from the audio thread, teardown off the audio thread —
// the realtime runner hands its plugin back on drop rather than destroying
// it in place), and a VST2 host that calls the dispatcher from one thread
// at a time follows the same contract.
struct RtBackend<T>(T);
unsafe impl<T> Send for RtBackend<T> {}

enum Backend {
    V3(RtBackend<vst3_host::realtime::RealtimePluginRunner>),
    V2(RtBackend<vst2::host::PluginInstance>),
}

// ------------------------------------------------------------ rt state ---

/// Mutable audio-thread state of a stage entry. Touched exclusively by
/// `process`/`flush` on the audio thread; wrapped in `UnsafeCell` so the
/// entry can live in an `Arc` shared with the engine table.
struct RtState {
    backend: Backend,
    /// VST2-only: 2-in/2-out host buffer bound to the staging vectors.
    hostbuf: Option<vst2::host::HostBuffer<f32>>,
    /// VST2-only: parameter object (`Sync`; called from the audio thread
    /// while polling — the VST2 `setParameter` contract).
    params_obj: Option<Arc<dyn vst2::plugin::PluginParameters>>,
    /// VST3-only: fixed-size runner buffers.
    v3_bufs: Option<vst3_host::audio::AudioBuffers>,
    v2_out_channels: usize,
    seen_gen: u64,
    last: Vec<f32>,
    // staging (all exactly VST_BLOCK frames, allocated at load)
    stage_l: Vec<f32>,
    stage_r: Vec<f32>,
    stage_ol: Vec<f32>,
    stage_or: Vec<f32>,
    in_ring: FxRing,
    out_ring: FxRing,
    /// Dry echo filling the startup window until the plugin (plus any
    /// latency it adds) produces its first samples.
    echo: FxRing,
    /// Channel voice messages fed for instrument use, delivered at the next
    /// internal 512-block boundary (audio-thread contract, like `process`).
    pending_midi: Vec<[u8; 3]>,
    /// VST2-only: stable heap slots for raw events — `api::Events` carries
    /// pointers, so the events must not move. Filled on first use, reused.
    // The Box is load-bearing (address stability), hence no Vec::into_boxed-slice style fix.
    #[allow(clippy::vec_box)]
    ev_pool: Vec<Box<vst2::api::Event>>,
    errors: u32,
}

// --------------------------------------------------------- stage entry ---

/// One live plugin shared between the UI (parameters, metadata) and the
/// audio thread (processing). Field order is load-bearing for drops:
/// `state` (whose VST3 runner hands the plugin to the control end) must
/// drop before `control`.
pub struct RtStageEntry {
    state: UnsafeCell<RtState>,
    /// VST3-only: UI-thread producer end of the runner's command ring.
    /// Never locked on the audio thread (zero contention).
    control: Mutex<Option<vst3_host::realtime::RtControl>>,
    params: Arc<VstParams>,
    descs: Vec<VstParamDesc>,
    name: String,
    /// Where this plugin was loaded from (re-opens offline render copies).
    spec: (PathBuf, VstKind),
    /// Whether the plugin advertises a custom editor window (VST2
    /// `effFlagsHasEditor`, VST3 `IComponent` view). Drives the "GUI"
    /// button; the slider panel remains the fallback for GUI-less plugins.
    has_editor: bool,
    /// VST2 only: raw `AEffect` dispatcher for editor hosting (see
    /// `src/vstedit.rs`). VST3 editors host a shadow instance instead —
    /// [`RtStageEntry::open_vst3_editor`].
    vst2: Option<Vst2Handle>,
}

/// Raw editor-dispatch handle for a loaded VST2 plugin: just the
/// `AEffect` address, so it can cross into the editor GUI thread.
/// Callers must close any editor before the plugin instance is dropped.
#[derive(Clone, Copy)]
pub struct Vst2Handle(*mut vst2::api::AEffect);
unsafe impl Send for Vst2Handle {}
unsafe impl Sync for Vst2Handle {}

impl Vst2Handle {
    pub(crate) fn from_instance(inst: &vst2::host::PluginInstance) -> Self {
        Self(inst.raw_effect())
    }
    /// Raw `AEffect::dispatcher` call with a VST2 opcode number.
    pub fn dispatch(
        &self,
        opcode: i32,
        index: i32,
        value: isize,
        ptr: *mut std::ffi::c_void,
        opt: f32,
    ) -> isize {
        unsafe {
            let ae = self.0;
            ((*ae).dispatcher)(ae, opcode, index, value, ptr, opt)
        }
    }
}

unsafe impl Send for RtStageEntry {}
unsafe impl Sync for RtStageEntry {}

/// What [`open`] hands back: the shared entry plus the UI-facing metadata
/// (also reachable through the entry).
pub struct VstLoad {
    pub entry: Arc<RtStageEntry>,
}

/// Process-wide plugin instantiation lock. dlopen-ing plugin code and
/// running a plugin's factory/editor bootstrap touch PROCESS-GLOBAL state
/// inside the plugin binary (toolkit singletons, lazy statics). A per-frame
/// VST scan instantiating plugins on the UI thread raced an editor thread's
/// effEditOpen on those statics and produced calls through garbage pointers
/// (SIGSEGV inside Dragonfly's editor open). Every entry point that creates
/// plugin instances or opens/closes plugin editors serializes on this lock.
static VST_LOADER: Mutex<()> = Mutex::new(());

/// Take the loader lock (poison-tolerant: a failed `open` must not
/// permanently deadlock plugin loading).
pub fn vst_loader_lock() -> std::sync::MutexGuard<'static, ()> {
    VST_LOADER.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- loaded libs
//
// v17 (the Vitalium crash): plugin binaries carry one-shot PROCESS-GLOBAL
// state (JUCE singletons, DPF connection points). Two host habits made that
// state fatal:
//   * `probe_vst3` DROPPED its probe instance on every scan —
//     `deleteInstance`/terminate while other instances of the same library
//     were live ("asked to delete component while audio processor still
//     active") and tore down singletons the live instance still used;
//   * `scan()` re-instantiated libraries that were ALREADY live (the bound
//     synth), hitting `juce_Singleton` on the second creation (one Rescan
//     with Vitalium bound printed ~57 000 JUCE assertion failures before
//     the first use-after-free).
// Fix: every successful instantiation records the file's identity here, and
// a scan that finds an unchanged, already-instantiated file REUSES the
// cached metadata instead of touching the library again. Probes never drop
// what they create (the VST2 rule, extended to VST3).

/// One already-instantiated library file.
struct CachedLib {
    mtime: std::time::SystemTime,
    len: u64,
    entry: VstEntry,
}

fn lib_cache() -> &'static Mutex<std::collections::BTreeMap<PathBuf, CachedLib>> {
    static CACHE: std::sync::OnceLock<Mutex<std::collections::BTreeMap<PathBuf, CachedLib>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Instantiations performed by this process (pin v17: a re-scan of a
/// live/unchanged library must NOT add one).
static INSTANTIATIONS: AtomicU64 = AtomicU64::new(0);

/// Instantiations performed so far — observability + test hook.
pub fn instantiation_count() -> u64 {
    INSTANTIATIONS.load(Ordering::Relaxed)
}

/// The real library file behind a scan target: a `.so` for itself, and for
/// a bundle directory (`.vst3` or `Foo.vst`) the plugin `.so` inside it —
/// a replaced plugin build rewrites THAT file, and only its identity tells
/// us the cached entry went stale.
fn lib_file_of(path: &Path) -> PathBuf {
    if path.is_file() {
        return path.to_path_buf();
    }
    let mut best: Option<(PathBuf, u64)> = None;
    let c = path.join("Contents");
    if c.is_dir() {
        for arch in std::fs::read_dir(&c).into_iter().flatten().flatten() {
            let p = arch.path();
            let cands: Vec<PathBuf> = if p.is_dir() {
                std::fs::read_dir(&p)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|f| f.extension().map(|x| x == "so").unwrap_or(false))
                    .collect()
            } else if p.extension().map(|x| x == "so").unwrap_or(false) {
                vec![p]
            } else {
                vec![]
            };
            for f in cands {
                let len = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
                if best.as_ref().map(|(_, l)| len > *l).unwrap_or(true) {
                    best = Some((f, len));
                }
            }
        }
    }
    best.map(|(f, _)| f).unwrap_or_else(|| path.to_path_buf())
}

/// File identity: a replaced build must be probed again.
fn file_sig(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let md = std::fs::metadata(lib_file_of(path)).ok()?;
    Some((md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH), md.len()))
}

/// Record a SUCCESSFUL instantiation (both opens, both probes). Failed
/// loads are NOT recorded: a broken file is probed again at the next Rescan.
fn note_instantiated(path: &Path, kind: VstKind, name: String, vendor: String, instrument: bool) {
    INSTANTIATIONS.fetch_add(1, Ordering::Relaxed);
    let Some((mtime, len)) = file_sig(path) else { return };
    let entry =
        VstEntry { path: path.to_path_buf(), kind, name, vendor, instrument, error: None };
    if let Ok(mut c) = lib_cache().lock() {
        c.insert(path.to_path_buf(), CachedLib { mtime, len, entry });
    }
}

/// The entry for a file already instantiated in this process, unchanged
/// since. `scan()` reuses it instead of instantiating the library again.
fn cached_entry(path: &Path) -> Option<VstEntry> {
    let (mtime, len) = file_sig(path)?;
    let c = lib_cache().lock().ok()?;
    let hit = c.get(path).filter(|c| c.mtime == mtime && c.len == len)?;
    Some(hit.entry.clone())
}


// ---------------------------------------------------------------- GUI thread
//
// ONE thread owns every plugin *control-plane* call: instantiation,
// editor open/idle/close, and editor teardown. Why this exists: plugin
// GUI toolkits bind their global event loop to the thread that first
// touches the plugin (JUCE's `MessageManager`, FLTK). When instantiating
// ran on the UI thread but `effEditOpen` ran on a per-editor thread,
// JUCE deferred every editor operation onto the (idle) load thread
// forever: the child window appeared created-and-sized but never mapped,
// and no VST2 GUI ever showed (the v10.2 "plugin GUIs don't work"
// report — verified on this machine for wolf-shaper and Dragonfly:
// the identical open becomes painted-at-first-idle when load
// and editOpen share a thread). Serializing instantiation AND all editor
// dispatch on this thread makes every toolkit's affinity assumption
// true by construction.
//
// CADENCE (v16 responsiveness pass): the loop's tick is ADAPTIVE. With
// editors open every tick dispatches one idle step, so the tick IS the
// plugin UI's input/framerate — the fixed 30 ms tick was measured at
// 27-33 Hz and reads as delayed mouse input and low editor framerate
// (JUCE and VST3 message queues want 4-10 ms service; Steinberg's own
// VST3 host runs its message loop continuously). With no editors open,
// the old 30 ms idle tick costs nothing.
type GuiJob = Box<dyn FnOnce() + Send + 'static>;

/// Pump tick while at least one plugin editor is open (ms): every tick is
/// one `effEditIdle` / `service_run_loop` step, so plugin UIs run at
/// ~1/this-rate input latency and repaint.
const EDITOR_TICK_MS: u64 = 6;
/// Pump tick with no editors open (ms): keeps the historical idle cost.
const IDLE_TICK_MS: u64 = 30;

/// Loop iterations completed by the GUI pump thread (observability pin:
/// sample the rate to verify the adaptive tick).
static GUI_HEARTBEAT: AtomicU64 = AtomicU64::new(0);

/// GUI-thread iterations so far; increases once per pump tick.
pub fn gui_heartbeat() -> u64 {
    GUI_HEARTBEAT.load(Ordering::Relaxed)
}

fn gui_tx() -> &'static std::sync::mpsc::Sender<GuiJob> {
    static TX: std::sync::OnceLock<std::sync::mpsc::Sender<GuiJob>> = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<GuiJob>();
        std::thread::Builder::new()
            .name("vst2-gui".into())
            .spawn(move || {
                let stats = std::env::var_os("SLOOPER_VST_STATS").is_some();
                let mut window = 0u32;
                let mut pump_ms = 0.0f64;
                let mut t_window = std::time::Instant::now();
                loop {
                    let tick = if crate::vstedit::editors_active() > 0 {
                        EDITOR_TICK_MS
                    } else {
                        IDLE_TICK_MS
                    };
                    match rx.recv_timeout(std::time::Duration::from_millis(tick)) {
                        Ok(job) => {
                            job();
                            while let Ok(job) = rx.try_recv() {
                                job();
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    // Editor idle pump for every open editor. Toolkits
                    // expect a steady effEditIdle; the fast tick only
                    // exists while editors are actually open.
                    let t0 = std::time::Instant::now();
                    crate::vstedit::pump_editors();
                    GUI_HEARTBEAT.fetch_add(1, Ordering::Relaxed);
                    if stats {
                        window += 1;
                        pump_ms += t0.elapsed().as_secs_f64() * 1000.0;
                        if window.is_multiple_of(100) {
                            let dt = t_window.elapsed().as_secs_f64().max(0.001);
                            eprintln!(
                                "[vst-stats] tick={tick}ms avg={:.2}ms rate={:.1}Hz editors={}",
                                pump_ms / 100.0,
                                100.0 / dt,
                                crate::vstedit::editors_active()
                            );
                            pump_ms = 0.0;
                            t_window = std::time::Instant::now();
                        }
                    }
                }
            })
            .expect("spawn vst2-gui thread");
        tx
    })
}

/// Run `f` on the VST GUI thread and block the caller for its result.
/// Never call from a GUI job (deadlock); no job ever does.
pub fn on_gui_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (rtx, rrx) = std::sync::mpsc::sync_channel(1);
    gui_tx()
        .send(Box::new(move || {
            let _ = rtx.send(f());
        }))
        .map_err(|_| "vst gui thread gone".to_string())?;
    rrx.recv().map_err(|_| "vst gui thread gone".to_string())
}

/// Queue `f` on the VST GUI thread WITHOUT waiting (v16: wakes the loop
/// immediately even mid-sleep; used for close hand-offs and async loads).
pub fn post(f: impl FnOnce() + Send + 'static) {
    let _ = gui_tx().send(Box::new(f));
}

/// v16: start a plugin load on the GUI thread and return immediately.
/// The caller polls the receiver (the UI thread must never block on an
/// instantiation: a Vitalium-class load measured 180 ms of frozen app).
pub fn open_async(
    path: &Path,
    kind: VstKind,
    sample_rate: u32,
    saved: Option<&[f32]>,
) -> std::sync::mpsc::Receiver<Result<Arc<RtStageEntry>, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let path = path.to_path_buf();
    let saved = saved.map(|s| s.to_vec());
    post(move || {
        // `open_inner` DIRECTLY: this closure already runs on the GUI
        // thread, and `open` would proxy its result back to this very
        // thread — waiting on ourselves = the v16 pump freeze this pin
        // caught ("picked plugin never loaded").
        let _ = tx.send(open_inner(&path, kind, sample_rate, saved.as_deref()).map(|l| l.entry));
    });
    rx
}

/// v16: run [`scan`] on the GUI thread, returning immediately. Loading
/// every plugin for metadata can take seconds; doing it on the UI thread
/// froze the app on the first chain-window open.
pub fn scan_async(dirs: Vec<PathBuf>) -> std::sync::mpsc::Receiver<Vec<VstEntry>> {
    let (tx, rx) = std::sync::mpsc::channel();
    post(move || {
        let _ = tx.send(scan(&dirs));
    });
    rx
}

/// Load a plugin (UI/manager/render thread entry point) — the actual
/// instantiation is proxied onto the shared VST GUI thread so toolkit
/// thread affinity binds there, where the editors also run. `saved`
/// (normalized, in `descs` order) overrides defaults.
pub fn open(
    path: &Path,
    kind: VstKind,
    sample_rate: u32,
    saved: Option<&[f32]>,
) -> Result<VstLoad, String> {
    let path = path.to_path_buf();
    let saved = saved.map(|s| s.to_vec());
    on_gui_thread(move || open_inner(&path, kind, sample_rate, saved.as_deref()))?
}

/// [`open`]'s body. Runs ON the VST GUI thread (the proxy above, or a
/// GUI-thread job). NEVER call the proxying [`open`] from a GUI job —
/// it would wait for a result posted to the blocked thread itself.
fn open_inner(
    path: &Path,
    kind: VstKind,
    sample_rate: u32,
    saved: Option<&[f32]>,
) -> Result<VstLoad, String> {
    // v19.1: never dlopen a chainloader wrapper whose Wine probe failed —
    // the dlopen itself is the abort(). Covers lazily (re)opened chain rows
    // saved in a project/config on a machine whose Wine since broke.
    if is_wine_bridged(path) {
        bridge_ok(bridge_needs32(path))
            .map_err(|w| format!("Windows plugin '{}' unavailable: {w}", path.display()))?;
    }
    let _loader = vst_loader_lock();
    let entry = match kind {
        VstKind::V3 => open_v3(path, sample_rate, saved)?,
        VstKind::V2 => open_v2(path, sample_rate, saved)?,
    };
    Ok(VstLoad {
        entry: Arc::new(entry),
    })
}

fn stage_buf() -> Vec<f32> {
    vec![0.0f32; VST_BLOCK]
}

fn apply_saved(params: &VstParams, descs: &[VstParamDesc], saved: Option<&[f32]>) {
    if let Some(vals) = saved {
        for (k, v) in vals.iter().enumerate() {
            if k < descs.len() && descs[k].writable {
                params.set(k, *v);
            }
        }
    }
}

/// The audio-thread half of a load, bundled so `assemble` stays readable.
struct RtParts {
    spec: (PathBuf, VstKind),
    backend: Backend,
    hostbuf: Option<vst2::host::HostBuffer<f32>>,
    params_obj: Option<Arc<dyn vst2::plugin::PluginParameters>>,
    v3_bufs: Option<vst3_host::audio::AudioBuffers>,
    control: Option<vst3_host::realtime::RtControl>,
    out_channels: usize,
}

fn assemble(
    name: String,
    parts: RtParts,
    params: Arc<VstParams>,
    descs: Vec<VstParamDesc>,
    has_editor: bool,
    vst2: Option<Vst2Handle>,
) -> RtStageEntry {
    let last: Vec<f32> = (0..descs.len()).map(|k| params.get(k)).collect();
    let seen_gen = params.generation();
    let state = RtState {
        backend: parts.backend,
        hostbuf: parts.hostbuf,
        params_obj: parts.params_obj,
        v3_bufs: parts.v3_bufs,
        v2_out_channels: parts.out_channels,
        seen_gen,
        last,
        stage_l: stage_buf(),
        stage_r: stage_buf(),
        stage_ol: stage_buf(),
        stage_or: stage_buf(),
        in_ring: FxRing::with_capacity(RING_CAP),
        out_ring: FxRing::with_capacity(RING_CAP),
        echo: FxRing::with_capacity(RING_CAP),
        pending_midi: Vec::new(),
        ev_pool: Vec::new(),
        errors: 0,
    };
    RtStageEntry {
        state: UnsafeCell::new(state),
        control: Mutex::new(parts.control),
        params,
        descs,
        name,
        spec: parts.spec,
        has_editor,
        vst2,
    }
}

fn open_v3(path: &Path, sample_rate: u32, saved: Option<&[f32]>) -> Result<RtStageEntry, String> {
    use vst3_host::{
        audio::{AudioBuffers, BusDirection, MediaType},
        realtime::RealtimePluginRunner,
        simple,
    };
    let mut plugin = simple::load_plugin_with_settings(path, sample_rate as f64, VST_BLOCK)
        .map_err(|e| format!("load: {e}"))?;
    // JUCE-wrapper plugins (Vitalium, SwankyAmp — distrho-ports builds) advertise
    // their buses with `active = 0` in `getBusInfo`, and the loader honours that.
    // An unactivated output bus renders NOTHING: `output_channel_count` collapses
    // to 0 and the plugin "loads fine" but is permanently silent. If we see zero
    // active output channels, activate every output bus and re-measure. Only in
    // that broken case — plugins that advertise correctly (Geonkick with its 16
    // auxiliary out buses!) keep their activation and channel accounting intact.
    if plugin.output_channel_count() == 0 {
        if let Ok(layout) = plugin.audio_bus_layout() {
            for i in 0..layout.outputs.len() as i32 {
                let _ = plugin.set_bus_active(MediaType::Audio, BusDirection::Output, i, true);
            }
        }
    }
    let ps = plugin
        .get_parameters()
        .map_err(|e| format!("params: {e}"))?;
    let descs: Vec<VstParamDesc> = ps
        .iter()
        .take(VST_MAX_PARAMS)
        .map(|p| VstParamDesc {
            id: p.id,
            name: p.name.clone(),
            default: p.default as f32,
            writable: !p.is_read_only,
        })
        .collect();
    // Seed the mirror from the plugin's CURRENT values, not its reported
    // defaults. Several synths (Dexed, JS80P) initialize from patch/program
    // data and advertise bogus `default_value` metadata (Output = 0.0): a
    // host that seeds from defaults and writes every param back silences
    // them the instant they load. Fresh open => write nothing; the plugin
    // already holds its own init state. Only a RESTORE (saved Some) pushes
    // values into the plugin, and only the saved ones.
    let current: Vec<f32> = ps
        .iter()
        .take(VST_MAX_PARAMS)
        .map(|p| p.value as f32)
        .collect();
    let params = VstParams::with_values(&descs, &current);
    apply_saved(&params, &descs, saved);
    if let Some(vals) = saved {
        for (k, d) in descs.iter().enumerate().take(vals.len()) {
            if d.writable {
                let _ = plugin.set_parameter(d.id, params.get(k) as f64);
            }
        }
    }
    let out_ch = plugin.output_channel_count().max(1);
    // Checked before the plugin moves into the realtime runner.
    let has_editor = plugin.has_editor();
    // v17: record this instantiation for the scan cache (see `cached_entry`).
    {
        let pinfo = vst3_host::discovery::get_plugin_info(path).ok();
        // Same rule as `probe_vst3`: declared category beats the bus-shape
        // heuristic (JS80P is a synth WITH an audio input bus).
        let instrument = pinfo
            .as_ref()
            .map(|i| {
                let cat = i.category.to_ascii_lowercase();
                if cat.contains("instrument") || cat.contains("synth") {
                    true
                } else if cat.contains("fx") || cat.contains("effect") {
                    false
                } else {
                    plugin
                        .audio_bus_layout()
                        .map(|l| l.inputs.is_empty())
                        .unwrap_or(false)
                }
            })
            .unwrap_or(false);
        let vendor = pinfo.map(|i| i.vendor).unwrap_or_default();
        note_instantiated(path, VstKind::V3, plugin_name(path), vendor, instrument);
    }
    plugin
        .start_processing()
        .map_err(|e| format!("start: {e}"))?;
    let (mut runner, control) = RealtimePluginRunner::new(plugin, 256);
    runner.start().map_err(|e| format!("run: {e}"))?;
    let bufs = AudioBuffers::new(out_ch, out_ch, VST_BLOCK, sample_rate as f64);
    Ok(assemble(
        plugin_name(path),
        RtParts {
            spec: (path.to_path_buf(), VstKind::V3),
            backend: Backend::V3(RtBackend(runner)),
            hostbuf: None,
            params_obj: None,
            v3_bufs: Some(bufs),
            control: Some(control),
            out_channels: out_ch,
        },
        params,
        descs,
        has_editor,
        None,
    ))
}

fn open_v2(path: &Path, sample_rate: u32, saved: Option<&[f32]>) -> Result<RtStageEntry, String> {
    use vst2::{
        host::{HostBuffer, PluginLoader},
        plugin::{Plugin, PluginParameters},
    };
    let mut loader = PluginLoader::load(path, Arc::new(Mutex::new(StubHost)))
        .map_err(|e| format!("load: {e}"))?;
    let mut inst = loader.instance().map_err(|e| format!("instance: {e}"))?;
    let info = inst.get_info();
    if info.inputs > 2 || info.outputs > 2 {
        return Err("multi-channel plugins are not supported".into());
    }
    // effOpen (opcode 0) — THE VST2 init handshake. The vendored loader
    // never sends it, and EFFECT plugins (distortion, reverb, ...) keep
    // their audio path closed until it arrives: every VST2 effect came back
    // as pure silence, muting whatever track it sat on. Instruments tolerate
    // its absence (that is why VST2 synths seemed fine). Verified against
    // wolf-shaper (0 → 0.5 peak) and Dragonfly.
    inst.init();
    inst.set_sample_rate(sample_rate as f32);
    inst.set_block_size(VST_BLOCK as i64);
    inst.resume();
    // v17: record this instantiation for the scan cache (see `cached_entry`).
    note_instantiated(
        path,
        VstKind::V2,
        info.name.to_string(),
        info.vendor.to_string(),
        info.inputs == 0,
    );
    let params_obj: Arc<dyn PluginParameters> = inst.get_parameter_object();
    let n = (info.parameters.max(0) as usize).min(VST_MAX_PARAMS);
    let descs: Vec<VstParamDesc> = (0..n)
        .map(|i| {
            let name = params_obj.get_parameter_name(i as i32);
            VstParamDesc {
                id: i as u32,
                name: if name.trim().is_empty() {
                    format!("Param {}", i + 1)
                } else {
                    name
                },
                default: params_obj.get_parameter(i as i32),
                writable: true,
            }
        })
        .collect();
    let params = VstParams::new(&descs);
    apply_saved(&params, &descs, saved);
    for (k, d) in descs.iter().enumerate() {
        if d.writable {
            params_obj.set_parameter(d.id as i32, params.get(k));
        }
    }
    let out_ch = info.outputs.clamp(1, 2) as usize;
    // Editor hosting: grab the raw effect handle + has-editor flag before
    // the instance moves into the realtime backend.
    let has_editor = unsafe { ((*inst.raw_effect()).flags & 1) != 0 };
    let vst2 = Some(Vst2Handle::from_instance(&inst));
    Ok(assemble(
        info.name,
        RtParts {
            spec: (path.to_path_buf(), VstKind::V2),
            backend: Backend::V2(RtBackend(inst)),
            hostbuf: Some(HostBuffer::new(2, 2)),
            params_obj: Some(params_obj),
            v3_bufs: None,
            control: None,
            out_channels: out_ch,
        },
        params,
        descs,
        has_editor,
        vst2,
    ))
}

fn plugin_name(path: &Path) -> String {
    base_label(path)
}

impl RtStageEntry {
    /// Plugin display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the plugin advertises a custom editor GUI.
    pub fn has_editor(&self) -> bool {
        self.has_editor
    }

    /// VST2 editor dispatch handle (`None` for VST3 — see docs/PLUGINS.md).
    pub fn vst2_handle(&self) -> Option<Vst2Handle> {
        self.vst2
    }

    /// Plugin file path this entry was loaded from.
    pub fn path(&self) -> &std::path::Path {
        &self.spec.0
    }

    /// (Path, kind) this entry was loaded from.
    pub fn spec(&self) -> &(PathBuf, VstKind) {
        &self.spec
    }

    /// Slider descriptors (UI).
    pub fn descs(&self) -> &[VstParamDesc] {
        &self.descs
    }

    /// Shared parameter cell (slider positions read from here).
    pub fn params(&self) -> &Arc<VstParams> {
        &self.params
    }

    /// Which plugin generation this entry hosts.
    pub fn kind(&self) -> VstKind {
        self.spec.1
    }

    /// v0.12: open this VST3 plugin's own editor window. The sounding
    /// instance is confined to the `vst3-host` realtime runner, so the
    /// window hosts a SHADOW instance of the same plugin (see
    /// `vstedit::Editor3`): never processes audio, primed from the live
    /// parameter cells, and its every edit writes back through those same
    /// cells — the slider path, generation bump and all. `sample_rate`
    /// must match the one the plugin was opened with.
    pub fn open_vst3_editor(
        &self,
        sample_rate: u32,
    ) -> Result<crate::vstedit::EditorWindow, String> {
        if !matches!(self.spec.1, VstKind::V3) {
            return Err("not a VST3 plugin".into());
        }
        if !self.has_editor {
            return Err("plugin advertises no editor window".into());
        }
        let path = self.spec.0.to_string_lossy().into_owned();
        let initial: Vec<(u32, f64)> = self
            .descs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.writable)
            .map(|(i, d)| (d.id, self.params.get(i) as f64))
            .collect();
        let ids: Vec<u32> = self.descs.iter().map(|d| d.id).collect();
        crate::vstedit::open_vst3_editor(
            &path,
            &self.name,
            sample_rate,
            initial,
            ids,
            self.params.clone(),
        )
    }

    /// UI side: update one parameter (normalized) and push it to the
    /// plugin (VST3 via the command ring; VST2 via the atomics the audio
    /// thread polls).
    pub fn set_param(&self, index: usize, value: f32) {
        self.params.set(index, value);
        if let Some(d) = self.descs.get(index) {
            let mut guard = self.control.lock().unwrap();
            if let Some(c) = guard.as_mut() {
                c.set_parameter(d.id, value.clamp(0.0, 1.0) as f64);
            }
        }
    }

    /// v0.11 R5 automation write: same effect as [`Self::set_param`] but
    /// NEVER blocks -- safe to call from the realtime thread. The atomic
    /// param mirror (which VST2's audio path polls directly) always takes
    /// the value; the VST3 controller push rides `try_lock`, and a lock
    /// contention just means the next write wins.
    pub fn set_param_rt(&self, index: usize, value: f32) {
        self.params.set(index, value);
        if let Some(d) = self.descs.get(index) {
            if let Ok(mut guard) = self.control.try_lock() {
                if let Some(c) = guard.as_mut() {
                    c.set_parameter(d.id, value.clamp(0.0, 1.0) as f64);
                }
            }
        }
    }

    /// Feed one channel voice message (raw status byte with channel low
    /// nibble). Audio thread only — same contract as [`Self::process`];
    /// the message is delivered at the start of the next internal block.
    pub fn feed_midi(&self, msg: [u8; 3]) {
        // SAFETY: audio-thread-only access to `state` (see `flush`).
        let s = unsafe { &mut *self.state.get() };
        if s.pending_midi.len() < 512 {
            s.pending_midi.push(msg);
        }
    }

    /// Note on (channel 0-15). Audio thread only.
    pub fn note_on(&self, ch: u8, note: u8, vel: u8) {
        self.feed_midi([0x90 | ch & 0x0F, note & 0x7F, vel.clamp(1, 127)]);
    }

    /// Note off (channel 0-15). Audio thread only.
    pub fn note_off(&self, ch: u8, note: u8) {
        self.feed_midi([0x80 | ch & 0x0F, note & 0x7F, 0]);
    }

    /// Control change (channel 0-15). Audio thread only.
    pub fn cc(&self, ch: u8, ctl: u8, val: u8) {
        self.feed_midi([0xB0 | ch & 0x0F, ctl & 0x7F, val & 0x7F]);
    }

    /// Channel pressure (0-127) — the pattern's expression lane. VST3
    /// delivery maps it to ChannelAftertouch; VST2 plugins take the raw
    /// 0xD0 message directly.
    pub fn press(&self, ch: u8, val: u8) {
        self.feed_midi([0xD0 | ch & 0x0F, val & 0x7F, 0]);
    }

    /// Panic switch: all-notes-off + all-sounds-off on every channel.
    /// Audio thread only.
    pub fn all_notes_off(&self) {
        for c in 0..16u8 {
            self.cc(c, 123, 0);
            self.cc(c, 120, 0);
        }
    }

    /// Drop all buffered audio. Audio thread only (called from the engine's
    /// seek handling).
    pub fn flush(&self) {
        // SAFETY: `process`/`flush` are only ever called from the audio
        // thread, so no other thread holds a reference into `state`.
        let s = unsafe { &mut *self.state.get() };
        s.in_ring.clear();
        s.out_ring.clear();
        s.echo.clear();
        s.pending_midi.clear();
    }

    /// Process one engine block: mono in, mono out, internally chunked to
    /// [`VST_BLOCK`]. Audio thread only.
    pub fn process(&self, input: &[f32], output: &mut [f32]) {
        // SAFETY: audio-thread-only access to `state` (see `flush`).
        let s = unsafe { &mut *self.state.get() };
        let n = input.len().min(output.len());

        self.poll_params(s);

        // Whole-block ring moves (v16 pipeline pass): no per-sample
        // push/pop/unwrap on the audio thread anymore.
        let dropped = s.in_ring.push_slice(&input[..n]);
        s.echo.push_slice(&input[..n]);
        if dropped > 0 {
            // History-consistent echo trim: the old code popped the echo in
            // lockstep with in-ring overflow.
            s.echo.drop_front(dropped);
        }

        while s.in_ring.len() >= VST_BLOCK {
            s.in_ring.drain_into(&mut s.stage_l);
            s.stage_r.copy_from_slice(&s.stage_l);
            s.stage_ol.fill(0.0);
            s.stage_or.fill(0.0);
            self.process_block(s);
            let got = s.stage_ol.len().min(VST_BLOCK);
            s.out_ring.push_slice(&s.stage_ol[..got]);
        }

        // Drain output; while the plugin has produced nothing (startup,
        // seek, latency window) the echo carries the dry signal through —
        // exactly the old per-sample rule, now per chunk.
        let mut off = 0usize;
        while off < n {
            let got = s.out_ring.drain_into(&mut output[off..n]);
            if got > 0 {
                off += got;
                continue;
            }
            let got = s.echo.drain_into(&mut output[off..n]);
            if got == 0 {
                break;
            }
            off += got;
        }
        if off < n {
            output[off..n].fill(0.0);
        }
    }

    /// Hand pending channel-voice messages to the plugin. VST2 gets raw
    /// `api::Events` (batches of 2 — the C struct's fixed pointer array);
    /// VST3 goes through the runner's command ring (pushes never block).
    fn deliver_midi(&self, s: &mut RtState) {
        if s.pending_midi.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut s.pending_midi);
        if matches!(s.backend, Backend::V3(_)) {
            // The UI holds this lock only for µs-long ring pushes; a brief
            // wait here is bounded well below any block period.
            if let Ok(mut g) = self.control.lock() {
                if let Some(c) = g.as_mut() {
                    use vst3_host::midi::MidiEvent;
                    for m in pending {
                        let ch = MIDI_CHANNELS[(m[0] & 0x0F) as usize];
                        let (d1, d2) = (m[1], m[2]);
                        let _ = match m[0] & 0xF0 {
                            0x90 if d2 > 0 => c.send_midi(MidiEvent::NoteOn {
                                channel: ch,
                                note: d1,
                                velocity: d2,
                            }),
                            0x80 | 0x90 => c.send_midi(MidiEvent::NoteOff {
                                channel: ch,
                                note: d1,
                                velocity: d2,
                            }),
                            0xB0 => c.send_midi(MidiEvent::ControlChange {
                                channel: ch,
                                controller: d1,
                                value: d2,
                            }),
                            0xC0 => c.send_midi(MidiEvent::ProgramChange {
                                channel: ch,
                                program: d1,
                            }),
                            0xD0 => c.send_midi(MidiEvent::ChannelAftertouch {
                                channel: ch,
                                pressure: d1,
                            }),
                            0xA0 => c.send_midi(MidiEvent::PolyAftertouch {
                                channel: ch,
                                note: d1,
                                pressure: d2,
                            }),
                            0xE0 => c.send_midi(MidiEvent::PitchBend {
                                channel: ch,
                                value: ((d2 as u16) << 7) | (d1 as u16),
                            }),
                            _ => false,
                        };
                    }
                }
            }
            return;
        }
        // VST2: the pool grows once, then every event is a reuse.
        while s.ev_pool.len() < 2 {
            // Zeroed raw event: a benign null record until overwritten.
            s.ev_pool.push(Box::new(unsafe { std::mem::zeroed() }));
        }
        let mut i = 0usize;
        while i < pending.len() {
            let take = 2usize.min(pending.len() - i);
            if let Backend::V2(inst) = &mut s.backend {
                use vst2::{api, plugin::Plugin};
                for (k, m) in pending[i..i + take].iter().enumerate() {
                    let me = api::MidiEvent {
                        event_type: api::EventType::Midi,
                        byte_size: std::mem::size_of::<api::MidiEvent>() as i32,
                        delta_frames: 0,
                        flags: 0,
                        note_length: 0,
                        note_offset: 0,
                        midi_data: [m[0], m[1], m[2]],
                        _midi_reserved: 0,
                        detune: 0,
                        note_off_velocity: 0,
                        _reserved1: 0,
                        _reserved2: 0,
                    };
                    // Identical layout — the crate's own transmute idiom.
                    let mut raw: api::Event = unsafe { std::mem::transmute(me) };
                    raw.event_type = api::EventType::Midi;
                    *s.ev_pool[k] = raw;
                }
                let events = api::Events {
                    num_events: take as i32,
                    _reserved: 0,
                    // Boxed slots keep the pointers stable; slot 1 is a
                    // dummy whenever `take == 1`.
                    events: [s.ev_pool[0].as_mut(), s.ev_pool[1].as_mut()],
                };
                inst.0.process_events(&events);
            }
            i += take;
        }
    }

    /// One VST_BLOCK-sized plugin call; the mono result lands in
    /// `stage_ol` (downmixed from the plugin's output bus). On error the
    /// block passes silence (and `errors` accumulates for diagnostics).
    fn process_block(&self, s: &mut RtState) {
        self.deliver_midi(s);
        match &mut s.backend {
            Backend::V3(runner) => {
                let mut ok = false;
                if let Some(bufs) = s.v3_bufs.as_mut() {
                    for ch in bufs.inputs.iter_mut() {
                        ch.copy_from_slice(&s.stage_l);
                    }
                    if runner.0.process(bufs).is_ok() && !bufs.outputs.is_empty() {
                        let n = VST_BLOCK;
                        if bufs.outputs.len() == 1 {
                            s.stage_ol.copy_from_slice(&bufs.outputs[0][..n]);
                        } else {
                            for i in 0..n {
                                s.stage_ol[i] = (bufs.outputs[0][i] + bufs.outputs[1][i]) * 0.5;
                            }
                        }
                        ok = true;
                    }
                }
                if !ok {
                    s.errors += 1;
                    s.stage_ol.fill(0.0);
                }
            }
            Backend::V2(inst) => {
                use vst2::plugin::Plugin;
                if let Some(hb) = s.hostbuf.as_mut() {
                    let stereo_out = s.v2_out_channels == 2;
                    let mut ab = hb.bind(
                        &[&s.stage_l[..], &s.stage_r[..]],
                        &mut [&mut s.stage_ol[..], &mut s.stage_or[..]],
                    );
                    inst.0.process(&mut ab);
                    if stereo_out {
                        for i in 0..VST_BLOCK {
                            s.stage_ol[i] = (s.stage_ol[i] + s.stage_or[i]) * 0.5;
                        }
                    }
                } else {
                    s.errors += 1;
                    s.stage_ol.fill(0.0);
                }
            }
        }
    }

    /// VST2 pulls parameter changes on the audio thread through its
    /// parameter object; VST3 cell writes (the shadow-editor drain lands
    /// here, `vstedit::route_param_edits3` → `VstParams::set`) are RELAYED
    /// into the runner's command ring on this same thread — pre-v20 this
    /// function silently ignored VST3, so every editor gesture died at the
    /// mirror and the plugin GUI changed nothing audible. The ring mutex is
    /// `try_lock`ed (RT rule): the UI holds it for µs-long pushes, and on
    /// contention `seen_gen` stays stale so the NEXT block retries the diff
    /// (drops are impossible in steady state, and a full ring retries too).
    fn poll_params(&self, s: &mut RtState) {
        let gen = self.params.generation();
        if gen == s.seen_gen {
            return;
        }
        if let Some(po) = &s.params_obj {
            s.seen_gen = gen;
            for (k, desc) in self.descs.iter().enumerate() {
                let v = self.params.get(k);
                if k < s.last.len() && (s.last[k] - v).abs() > f32::EPSILON {
                    s.last[k] = v;
                    po.set_parameter(desc.id as i32, v);
                }
            }
            return;
        }
        if matches!(s.backend, Backend::V3(_)) {
            if let Ok(mut g) = self.control.try_lock() {
                if let Some(c) = g.as_mut() {
                    let mut retry = false;
                    for (k, desc) in self.descs.iter().enumerate() {
                        let v = self.params.get(k);
                        if k < s.last.len() && (s.last[k] - v).abs() > f32::EPSILON {
                            if c.set_parameter(desc.id, v as f64) {
                                s.last[k] = v;
                            } else {
                                retry = true; // ring full: resend next block
                            }
                        }
                    }
                    if !retry {
                        s.seen_gen = gen;
                    }
                } else {
                    s.seen_gen = gen; // control retired: nothing to relay to
                }
            }
        } else {
            s.seen_gen = gen;
        }
    }
}

/// Minimal VST2 host: the crate calls back for identity, time info
/// (ignored) and automation (ignored — slooper's transport is the master).
struct StubHost;

impl vst2::host::Host for StubHost {
    fn get_info(&self) -> (isize, String, String) {
        (
            1,
            String::from("slooper"),
            String::from(env!("CARGO_PKG_VERSION")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v17 regression: `open_v3` must NOT seed from reported `default_value`
    /// metadata or write params back on a fresh (unsaved) open. Dexed and
    /// JS80P advertise `Output`/level defaults of 0.0 while carrying their
    /// real init state in patch data — writing defaults into them silences
    /// the plugin at load. The mirror seeds from CURRENT values instead;
    /// this pins the constructor that makes that possible. (The audible
    /// half of the regression is guarded by `every_local_instrument_makes_sound`.)
    #[test]
    fn v17_with_values_seeds_live_state_not_defaults() {
        let descs = vec![
            VstParamDesc {
                id: 0,
                name: "Output".into(),
                default: 0.0, // the bogus metadata Dexed reports
                writable: true,
            },
            VstParamDesc {
                id: 1,
                name: "Cutoff".into(),
                default: 0.0,
                writable: true,
            },
        ];
        // Plugin's actual state on fresh load: full output, filter open.
        let p = VstParams::with_values(&descs, &[1.0, 0.8, 0.5]);
        assert_eq!(p.get(0), 1.0, "live value beats reported default");
        assert_eq!(p.get(1), 0.8);
        // Missing trailing values fall back to the descriptor default,
        // and out-of-range values clamp like `set` does.
        let q = VstParams::with_values(&descs, &[2.0]);
        assert_eq!(q.get(0), 1.0, "clamped");
        assert_eq!(q.get(1), 0.0, "falls back to default");
    }

    #[test]
    fn params_store_and_signal() {
        let descs = vec![
            VstParamDesc {
                id: 0,
                name: "a".into(),
                default: 0.25,
                writable: true,
            },
            VstParamDesc {
                id: 1,
                name: "b".into(),
                default: 0.75,
                writable: true,
            },
        ];
        let p = VstParams::new(&descs);
        assert_eq!(p.get(0), 0.25);
        assert_eq!(p.len(), 2);
        let g = p.generation();
        p.set(0, 0.5);
        assert_eq!(p.get(0), 0.5);
        assert_eq!(p.generation(), g + 1);
        p.set(0, 0.5); // no-op does not bump
        assert_eq!(p.generation(), g + 1);
        p.set(1, 9.0); // clamped
        assert_eq!(p.get(1), 1.0);
    }

    #[test]
    fn kinds_round_trip() {
        for k in [VstKind::V2, VstKind::V3] {
            assert_eq!(VstKind::parse(k.tag()), Some(k));
        }
        assert_eq!(VstKind::parse("LV2"), None);
    }

    #[test]
    fn scan_tolerates_junk_dirs() {
        // A directory of only unrelated files yields no entries and no
        // panics.
        let tmp = std::env::temp_dir();
        let entries = scan(std::slice::from_ref(&tmp));
        for e in &entries {
            assert!(e.path.extension().is_some());
        }
    }

    /// v17 pin (Vitalium crash, host side): a `scan()` must NEVER create a
    /// second instance of a library this process already instantiated —
    /// the shared toolkit singletons (JUCE/DPF) do not survive it, and the
    /// old probe additionally DROPPED its instance (delete-while-active →
    /// use-after-free: 57 000 assertion failures per Rescan with Vitalium
    /// bound, then SIGSEGV). A re-scan of an unchanged, already-instantiated
    /// file must reuse the cached identity with ZERO new instantiations.
    #[test]
    fn v17_rescan_never_reinstantiate_live_library() {
        let home = std::env::var("HOME").unwrap_or_default();
        let src = std::path::PathBuf::from(format!("{home}/.vst/wolf-shaper-vst.so"));
        if !src.is_file() {
            eprintln!("skip: no wolf-shaper VST2 on this host");
            return;
        }
        let dir = std::env::temp_dir().join(format!("sp-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let so = dir.join("wolf-shaper-vst.so");
        std::fs::copy(&src, &so).unwrap();

        let n0 = instantiation_count();
        let first = scan(std::slice::from_ref(&dir));
        assert_eq!(instantiation_count(), n0 + 1, "first scan probes once");
        let second = scan(std::slice::from_ref(&dir));
        assert_eq!(
            instantiation_count(),
            n0 + 1,
            "RE-scan of an unchanged, already-probed library must not instantiate again"
        );
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].name, second[0].name);
        assert_eq!(first[0].instrument, second[0].instrument);
        assert!(!second[0].instrument, "wolf-shaper is an effect");

        // The user's crash sequence: BIND (a live instance now exists),
        // then re-scan the folder. Before the fix the re-scan re-probed the
        // live library and dropped the probe mid-flight.
        let load = open(&so, VstKind::V2, 48_000, None).expect("open wolf-shaper");
        assert_eq!(instantiation_count(), n0 + 2, "the open instantiates once");
        let third = scan(std::slice::from_ref(&dir));
        assert_eq!(
            instantiation_count(),
            n0 + 2,
            "re-scan while the library is BOUND must not instantiate at all"
        );
        assert_eq!(third[0].name, first[0].name);
        std::mem::forget(load.entry); // probe rule: never drop plugin instances
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v17 pin (the exact user symptom): a Vitalium instance bound to a
    /// track keeps making sound ACROSS a folder re-scan. Pre-fix the scan
    /// deleted shared plugin state under the live instance (delete-while-
    /// active warnings, then death). Skips without the plugin.
    #[test]
    fn v17_bound_instrument_survives_rescan() {
        let home = match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => return,
        };
        let bundle = std::path::PathBuf::from(format!("{home}/.vst3/vitalium.vst3"));
        if !bundle.is_dir() {
            eprintln!("skip: no vitalium on this host");
            return;
        }
        let load = open(&bundle, VstKind::V3, 48_000, None).expect("open vitalium");
        let entry = load.entry;
        let parent = bundle.parent().unwrap();
        // The crash window: scan the folder WHILE this instance is live.
        let entries = scan(&[parent.to_path_buf()]);
        let me = entries.iter().find(|e| e.path == bundle);
        assert!(me.is_some(), "the live plugin must still be listed");
        assert!(me.unwrap().instrument, "cached identity: instrument");
        // And it must STILL make sound.
        entry.note_on(0, 60, 110);
        let mut peak = 0.0f32;
        let mut out = vec![0f32; 512];
        let silence = [0f32; 512];
        for _ in 0..120 {
            entry.process(&silence, &mut out);
            for x in out.iter() {
                peak = peak.max(x.abs());
            }
        }
        assert!(
            peak > 1e-4,
            "bound vitalium alive after re-scan, peak {peak}"
        );
        std::mem::forget(entry);
    }

    /// v20 (user report "VST plugins don't apply changes — synths AND EQs,
    /// e.g. Surge XT Effects"): the VST3 editor hosts a SHADOW instance,
    /// and its drain (`vstedit::route_param_edits3`) deposits every
    /// performEdit into the shared cell mirror with a generation bump —
    /// it NEVER touches the runner's command ring. Pre-v20 `poll_params`
    /// applied the cells only for VST2 (`params_obj`), so every shadow-
    /// editor gesture died at the mirror: knobs and preset broadcasts
    /// changed nothing audible, while VST2 editors (editing the LIVE
    /// instance) worked — exactly the reported split. Pin: replay the
    /// drain call with Post Gain → 0 and require the processed output to
    /// go silent. (RED on HEAD.)
    #[test]
    fn v20_vst3_cell_write_reaches_sound() {
        let wolf = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".vst3/wolf-shaper.vst3");
        if !wolf.is_dir() {
            eprintln!("skip: no wolf-shaper.vst3 on this host");
            return;
        }
        let load = match open(&wolf, VstKind::V3, 48_000, None) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skip: wolf-shaper VST3 would not open ({e})");
                return;
            }
        };
        let entry = &load.entry;
        // Pre Gain has the strongest clean response on this build (probe:
        // norm 0→1 swings the noise peak ~2.2×); Post Gain 0 was disproven
        // as a mute control (bottoms out near −1 dB on this build).
        let pre = match entry
            .descs()
            .iter()
            .position(|d| d.writable && d.name == "Pre Gain")
        {
            Some(k) => k,
            None => {
                eprintln!("skip: no writable \"Pre Gain\" param");
                return;
            }
        };
        // Deterministic noise (LCG — no rand dependency, reproducible).
        let mut st = 0x2545_F491_u32;
        let mut noise = vec![0f32; 512];
        for x in noise.iter_mut() {
            st = st.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *x = ((st >> 8) as f32 / (1 << 24) as f32) * 2.0 - 1.0;
        }
        let measure = |e: &super::RtStageEntry, blocks: usize| -> f32 {
            let mut peak = 0f32;
            let mut out = [0f32; 512];
            for _ in 0..blocks {
                e.process(&noise, &mut out);
                peak = peak.max(out.iter().fold(0f32, |m, x| m.max(x.abs())));
            }
            peak
        };
        // Let the plugin (and its latency) settle before measuring.
        let before = measure(entry, 24);
        assert!(before > 1e-3, "baseline noise through wolf-shaper is silent");
        // THE exact editor-drain call shape: (id, normalized) → cell — and
        // NOTHING else. Both directions must move the audio: on HEAD both
        // peaks sat equal (measured 1.0341 vs 1.0453 on the old assertion).
        let ids: Vec<u32> = entry.descs().iter().map(|d| d.id).collect();
        let pid = entry.descs()[pre].id;
        crate::vstedit::route_param_edits3(vec![(pid, 0.0)], &ids, &entry.params);
        assert_eq!(entry.params.get(pre), 0.0, "drain wrote the cell");
        let p_lo = measure(entry, 24);
        crate::vstedit::route_param_edits3(vec![(pid, 1.0)], &ids, &entry.params);
        let p_hi = measure(entry, 24);
        assert!(
            p_hi > p_lo * 1.5,
            "cell-only writes (the shadow-editor drain path) never reached \
             the live VST3 runner: peak(lo) {p_lo:.4} vs peak(hi) {p_hi:.4} \
             (baseline {before:.4})"
        );
    }

    /// End-to-end against the local plugin folders when present: every
    /// backend contract — open, ring processing across block sizes,
    /// parameter push — runs for real. Silently skips when no plugins are
    /// installed (CI).
    /// Local-audio-test filter. Two classes of installed plugin must not be
    /// driven by the unit suite:
    ///  * yabridge wrappers (`…/yabridge/…`): instantiating one spawns a
    ///    Wine host — slow, and it ABORTS the whole test process when no
    ///    usable wine loader is configured. Windows plugins are covered by
    ///    `examples/plugin_audit`, not by unit tests.
    ///  * Modular/patchless bundles (Cardinal family, Stochas): they are
    ///    silent until the user loads a patch/sequence — silence is their
    ///    correct default state, not a dead output bus.
    fn local_audio_test_drivable(e: &super::VstEntry) -> bool {
        if e.path.to_string_lossy().contains("/yabridge/") {
            return false;
        }
        const PATCHLESS: &[&str] = &[
            "Cardinal.vst3",
            "CardinalSynth.vst3",
            "CardinalFX.vst3",
            "Stochas.vst3",
        ];
        !PATCHLESS.contains(&e.path.file_name().and_then(|s| s.to_str()).unwrap_or(""))
    }

    #[test]
    fn smoke_local_plugins_if_present() {
        let found = scan_with(&default_vst_dirs(), true);
        let mut ran = 0;
        for e in found
            .iter()
            .filter(|e| e.error.is_none() && local_audio_test_drivable(e))
            .take(3)
        {
            for kind in [VstKind::V2, VstKind::V3] {
                let Ok(load) = open(&e.path, kind, 48_000, None) else {
                    continue;
                };
                // Deliberately ragged block sizes: exercises the rings.
                for size in [512usize, 1000, 100, 512] {
                    let input = vec![0.1f32; size];
                    let mut output = vec![0.0f32; size];
                    load.entry.process(&input, &mut output);
                    assert_eq!(output.len(), size);
                    assert!(output.iter().all(|v| v.is_finite()));
                }
                load.entry.set_param(0, 0.33);
                let input = vec![0.0f32; 512];
                let mut output = vec![0.0f32; 512];
                for _ in 0..8 {
                    load.entry.process(&input, &mut output);
                }
                assert!(output.iter().all(|v| v.is_finite()));
                drop(load); // held-reap teardown path
                ran += 1;
            }
        }
        // Informational, not an assertion: CI has no plugins installed.
        let _ = ran;
    }

    /// Every installed instrument VST3 must actually SOUND when fed a
    /// note. Regression pin for the JUCE-wrapper silent-bus bug:
    /// distrho-ports builds (Vitalium et al.) advertise their buses with
    /// `active = 0`; a host that honours that renders nothing, and the
    /// plugin silently never plays as a track instrument. `open` must leave
    /// every loadable instrument on an activated output bus.
    #[test]
    fn every_local_instrument_makes_sound() {
        let found = scan_with(&default_vst_dirs(), true);
        let mut checked = 0;
        for e in found
            .iter()
            .filter(|e| e.error.is_none() && e.instrument && local_audio_test_drivable(e))
        {
            if !e.path.extension().is_some_and(|x| x == "vst3") {
                continue; // VST2 instruments are covered by the smoke test
            }
            let Ok(load) = open(&e.path, VstKind::V3, 48_000, None) else {
                continue;
            };
            load.entry.note_on(0, 60, 110);
            let mut peak = 0.0f32;
            // ~1 second at 48k in ring-sized chunks.
            for _ in 0..96 {
                let mut out = [0.0f32; 512];
                load.entry.process(&[0.0f32; 512], &mut out);
                peak = peak.max(out.iter().fold(0.0f32, |m, x| m.max(x.abs())));
            }
            load.entry.all_notes_off();
            assert!(
                peak > 1e-4,
                "{}: instrument VST3 produced silence from a held note \
                 (unactivated output bus?)",
                e.path.display()
            );
            checked += 1;
        }
        // Informational: CI has no instruments installed.
        let _ = checked;
    }

    /// v0.12 coverage hole: `every_local_instrument_makes_sound` pins the
    /// instrument path, but NO test ever fed audio through an installed
    /// VST3 EFFECT — the user's "silence in my effect chain" report had no
    /// matrix behind it. Feed noise through every local VST3 effect; a
    /// plugin that swallows all input (dead output bus, failed activate)
    /// fails here with the path named.
    #[test]
    fn v12_local_vst3_effects_pass_audio() {
        let found = scan_with(&default_vst_dirs(), true);
        let mut checked = 0;
        for e in found
            .iter()
            .filter(|e| e.error.is_none() && !e.instrument && local_audio_test_drivable(e))
        {
            if !e.path.extension().is_some_and(|x| x == "vst3") {
                continue; // VST2 effects are pinned by v10_local_vst2_effect_passes_audio
            }
            let Ok(load) = open(&e.path, VstKind::V3, 48_000, None) else {
                continue;
            };
            let mut rng = 12345u32;
            let mut peak = 0.0f32;
            for _ in 0..60 {
                let mut input = [0.0f32; 512];
                for x in input.iter_mut() {
                    rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    *x = (((rng >> 16) as f32) / 32768.0 - 1.0) * 0.25;
                }
                let mut out = [0.0f32; 512];
                load.entry.process(&input, &mut out);
                peak = peak.max(out.iter().fold(0.0f32, |m, x| m.max(x.abs())));
            }
            assert!(
                peak > 1e-4,
                "{}: VST3 effect swallowed all input (silent output bus?)",
                e.path.display()
            );
            checked += 1;
        }
        // Informational: CI has no effects installed.
        let _ = checked;
    }

    // ===================== v0.10 acceptance pins =====================

    /// AC2.4: drop-in install layout. A folder may contain loose `.so`
    /// plugins, `.vst` / `.vst3` bundles, and yabridge sidecars (the
    /// `foo.dll.so` wrappers yabridgectl generates for Windows `.dll`s —
    /// they are ordinary Linux `.so`s to the loader). scan() must find
    /// every native-loadable form; the raw `.dll` itself is registered
    /// with yabridgectl (yabridge_sync), never dlopen'd.
    #[test]
    fn v10_scan_finds_yabridge_wrappers_and_dlls() {
        let dir = std::env::temp_dir().join(format!("sp-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let home = std::env::var("HOME").unwrap_or_default();
        let src = std::path::PathBuf::from(&home).join(".vst/wolf-shaper-vst.so");
        if !src.is_file() {
            eprintln!("skip: no wolf-shaper on this host");
            return;
        }
        // Loose .so
        std::fs::copy(&src, dir.join("loose.so")).unwrap();
        // yabridge sidecar naming for a Windows plugin + the .dll it wraps
        std::fs::copy(&src, dir.join("synth.dll.so")).unwrap();
        std::fs::write(dir.join("synth.dll"), b"MZ not really").unwrap();
        // macOS-style bundle directory (what yabridge also emits)
        std::fs::create_dir_all(dir.join("Nested.vst/Contents")).unwrap();
        std::fs::copy(&src, dir.join("Nested.vst/Contents/wolf.so")).unwrap();

        let found = scan(std::slice::from_ref(&dir));
        let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
        // All three native forms enumerated, sidecar included; the raw .dll
        // is NOT probed (that would dlopen PE bytes and always fail).
        assert!(
            found
                .iter()
                .any(|e| e.path.extension().and_then(|x| x.to_str()) == Some("so")),
            "loose .so missing: {names:?}"
        );
        assert!(
            found
                .iter()
                .any(|e| e.path.to_string_lossy().contains("synth.dll.so")),
            "yabridge sidecar not scanned: {names:?}"
        );
        assert!(
            found
                .iter()
                .any(|e| e.path.to_string_lossy().contains("Nested.vst")),
            "bundle dir layout not scanned: {names:?}"
        );
        assert!(
            !found
                .iter()
                .any(|e| e.path.extension().and_then(|x| x.to_str()) == Some("dll")),
            "raw .dll must never be dlopen'd by scan"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===================== v0.19.1 Wine-bridge guard pins =====================

    /// Synthetic Windows DLL: MZ + PE header with the given `Machine` field.
    fn v191_fake_dll(path: &Path, machine: u16) {
        let mut b = vec![0u8; 512];
        b[0..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
        b[64..68].copy_from_slice(b"PE\0\0");
        b[68..70].copy_from_slice(&machine.to_le_bytes());
        std::fs::write(path, b).unwrap();
    }

    /// Fixture: a plugins folder holding a yabridge-magic wrapper with an
    /// i386 companion `.dll`, plus fake yabridge host binaries and a fake
    /// Wine loader whose BEHAVIOUR is `wine_body` (executed as a /bin/sh
    /// script by the probe). Sets the probe env seams; caller must tear down.
    fn v191_bridge_fixture(tag: &str, wine_body: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("sp-bridge-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("plugins")).unwrap();
        std::fs::create_dir_all(root.join("yabridge")).unwrap();
        for h in ["yabridge-host-32.exe.so", "yabridge-host.exe.so"] {
            std::fs::write(root.join("yabridge").join(h), "#!/bin/sh\n").unwrap();
        }
        let wine = root.join("wine");
        std::fs::write(&wine, format!("#!/bin/sh\n{wine_body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wine, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            root.join("plugins/Monk.dll.so"),
            b"MZ\x90\x00 libyabridge-chainloader-vst2.so (synthetic fixture)",
        )
        .unwrap();
        v191_fake_dll(&root.join("plugins/Monk.dll"), 0x014c);
        std::env::set_var("SLOOPER_WINE", &wine);
        std::env::set_var("SLOOPER_YABRIDGE_DIR", root.join("yabridge"));
        root
    }

    fn v191_bridge_teardown(root: &Path) {
        std::env::remove_var("SLOOPER_WINE");
        std::env::remove_var("SLOOPER_YABRIDGE_DIR");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn v191_pe_machine_reads_both_bitnesses() {
        let dir = std::env::temp_dir().join(format!("sp-pe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        v191_fake_dll(&dir.join("m32.dll"), 0x014c);
        v191_fake_dll(&dir.join("m64.dll"), 0x8664);
        std::fs::write(dir.join("junk.dll"), b"MZ junk").unwrap();
        assert_eq!(pe_machine(&dir.join("m32.dll")), Some(0x014c));
        assert_eq!(pe_machine(&dir.join("m64.dll")), Some(0x8664));
        assert_eq!(pe_machine(&dir.join("junk.dll")), None);
        // The bitness decision for a wrapper follows its companion dll.
        std::fs::copy(dir.join("junk.dll"), dir.join("mystery.dll")).unwrap();
        std::fs::write(dir.join("mystery.dll.so"), b"#!/bin/sh\n").unwrap();
        assert!(bridge_needs32(&dir.join("mystery.dll.so")), "unparseable PE must assume 32-bit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v191_scan_marks_dead_wine_bridge_without_crash() {
        let root = v191_bridge_fixture(
            "dead",
            "echo 'wine: failed to start L\"yabridge-host-32.exe.so\": c000007b' >&2\nexit 1",
        );
        let found = scan_with(std::slice::from_ref(&root.join("plugins")), false);
        let e = found
            .iter()
            .find(|e| e.path.to_string_lossy().contains("Monk.dll.so"))
            .expect("bridged wrapper must still be listed (greyed), never hidden");
        let err = e.error.as_deref().unwrap_or_default();
        assert!(err.contains("32-bit"), "error must name the bitness gap: {err}");
        assert!(err.contains("WINELOADER"), "error must name the remedy: {err}");
        // The raw `.dll` companion stays invisible — scan never probes PE.
        assert!(
            !found
                .iter()
                .any(|e| e.path.extension().and_then(|x| x.to_str()) == Some("dll")),
            "raw .dll must never become an entry"
        );
        v191_bridge_teardown(&root);
    }

    #[test]
    fn v191_scan_passes_bridge_when_wine_probe_passes() {
        // Probe success (the host's own banner) must take the NORMAL load
        // path: the synthetic wrapper then fails as a plain dlopen error —
        // proof the guard did not refuse it.
        let root = v191_bridge_fixture(
            "live",
            "echo 'yabridge host version 5.1.1 (32-bit compatibility mode)'\necho 'Usage: yabridge-host-32.exe a b c d'\nexit 1",
        );
        let found = scan_with(std::slice::from_ref(&root.join("plugins")), false);
        let e = found
            .iter()
            .find(|e| e.path.to_string_lossy().contains("Monk.dll.so"))
            .expect("wrapper entry present");
        let err = e.error.clone().unwrap_or_default();
        assert!(
            !err.contains("bridge unavailable"),
            "guard must not refuse when the probe passes: {err}"
        );
        assert!(!err.is_empty(), "junk wrapper must still fail the real load: {e:?}");
        v191_bridge_teardown(&root);
    }

    #[test]
    fn v191_open_inner_refuses_bridge_with_dead_wine() {
        let root = v191_bridge_fixture(
            "open",
            "echo 'wine: failed to start L\"x\": c000007b' >&2\nexit 1",
        );
        let p = root.join("plugins/Monk.dll.so");
        assert!(is_wine_bridged(&p));
        let err = open_inner(&p, VstKind::V2, 48_000, None)
            .err()
            .expect("dead Wine must be refused BEFORE any dlopen");
        assert!(err.contains("unavailable"), "{err}");
        v191_bridge_teardown(&root);
    }

    /// AC3.1: opening a local VST2 EFFECT goes through the full handshake
    /// (effOpen + resume) without error and yields a live, parameterised
    /// instance. The audible proof (track not silenced) is pinned by the
    /// e2e test `e2e_vst2_effect_not_silent` under real JACK.
    #[test]
    fn v10_local_vst2_effect_passes_audio() {
        let home = std::env::var("HOME").unwrap_or_default();
        let p = std::path::PathBuf::from(&home).join(".vst/wolf-shaper-vst.so");
        if !p.is_file() {
            eprintln!("skip: no wolf-shaper on this host");
            return;
        }
        let load = open(&p, VstKind::V2, 48_000, None).expect("open wolf-shaper effect");
        assert_eq!(load.entry.spec().1, VstKind::V2);
        assert!(
            !load.entry.descs().is_empty(),
            "effect exposes its parameters"
        );
        assert!(load.entry.vst2_handle().is_some(), "editor-capable handle");
    }

    // ================= v16 responsiveness pass pins ======================
    // (requirements list: scratch slooper-v16/REQUIREMENTS.md)

    /// v16 helper: a local plugin for the RT pins — VST3 wolf-shaper
    /// preferred (waveshaper, no internal latency), else VST2 Dragonfly.
    /// `None` when nothing is installed (CI skip).
    fn v16_test_plugin() -> Option<(PathBuf, VstKind)> {
        let found = scan(&default_vst_dirs());
        let pick = |needle: &str, kind: VstKind| -> Option<(PathBuf, VstKind)> {
            // Filter on kind too: `~/.vst` sorts before `~/.vst3`, and the
            // VST2 wolf-shaper .so opened as a VST3 dies with "no ModuleEntry".
            found
                .iter()
                .find(|e| {
                    e.error.is_none() && e.kind == kind && e.path.to_string_lossy().contains(needle)
                })
                .map(|e| (e.path.clone(), kind))
        };
        pick("wolf-shaper", VstKind::V3).or_else(|| pick("wolf-shaper", VstKind::V2))
    }

    /// v16 helper: GUI-pump iterations per second over a sampling window
    /// (the pump thread ticks once per loop; `gui_heartbeat` counts them).
    fn v16_pump_rate(sample_ms: u64) -> f64 {
        let t0 = std::time::Instant::now();
        let h0 = gui_heartbeat();
        std::thread::sleep(std::time::Duration::from_millis(sample_ms));
        let dt = t0.elapsed().as_secs_f64().max(0.001);
        (gui_heartbeat() - h0) as f64 / dt
    }

    /// AC1 pin (SMOKE, real editor on X11): the pump tick IS the plugin
    /// UI's input/framerate — idle stays on the cheap 30 ms tick, and a
    /// painted editor switches the loop to the fast EDITOR_TICK cadence
    /// (measured ≥100 Hz, target ~150). The old fixed 30 ms tick measured
    /// 27–33 Hz: exactly the "inputs are delayed, framerate is low" report.
    #[test]
    fn v16_vsthost_fast_pump_when_editors() {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1"
            || !crate::vstedit::x11_available()
        {
            eprintln!("skip AC1: SLOOPER_VST_SMOKE=1 + X11 required");
            return;
        }
        let idle = v16_pump_rate(700);
        assert!(
            idle <= 40.0,
            "idle pump must stay on the cheap 30 ms tick (≤40 Hz), got {idle:.1} Hz"
        );
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::path::Path::new(&home).join(".vst/DragonflyHallReverb-vst.so");
        if !path.is_file() {
            eprintln!("skip AC1: Dragonfly VST2 not installed");
            return;
        }
        let load = open(&path, VstKind::V2, 48_000, None).expect("open dragonfly");
        let h = load.entry.vst2_handle().expect("vst2 handle");
        let mut editor =
            crate::vstedit::open_editor(&h, &path.to_string_lossy(), "v16-ac1").expect("editor");
        let t0 = std::time::Instant::now();
        while !editor.painted() && !editor.open_failed() && !editor.is_closed() {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(12),
                "editor never painted"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            editor.painted(),
            "AC1 needs a PAINTED editor to measure the fast tick"
        );
        let fast = v16_pump_rate(1000);
        assert!(
            fast >= 100.0,
            "with a painted editor the pump must run ≥100 Hz (EDITOR_TICK_MS = {EDITOR_TICK_MS}), got {fast:.1} Hz"
        );
        editor.close();
        drop(editor);
        let after = v16_pump_rate(700);
        assert!(
            after <= 40.0,
            "pump must fall back to the idle tick once the editor closes, got {after:.1} Hz"
        );
        drop(load);
    }

    /// AC6 pin: the v16 bulk ring is a transparent, allocation-free
    /// sample pipe, and `flush()` truly empties the live pipeline. The
    /// ring itself is pinned directly (deterministic, plugin-free); the
    /// plugin-backed part pins finite passthrough and the post-flush
    /// echo-carry reset. (Deliberately no cross-instance / cross-schedule
    /// byte equality: plugin cores are stateful and carry process-global
    /// JUCE singletons — that is plugin behaviour, not ring behaviour.)
    #[test]
    fn v16_vst_rt_ring_determinism() {
        // -- FxRing: exact FIFO under arbitrary push/drain patterns.
        let mut r = FxRing::with_capacity(1000);
        let stream: Vec<f32> = (0..5000).map(|i| i as f32 * 0.001).collect();
        let mut out: Vec<f32> = Vec::new();
        let mut off = 0usize;
        let mut n = 0usize;
        let pattern = [137usize, 3, 999, 512, 1000, 1, 777, 250, 512, 64];
        while off < stream.len() {
            let take = pattern[n % pattern.len()].min(stream.len() - off);
            // never overfill: fully drain first when a push might overflow
            if r.len() + take > 1000 {
                let mut d = vec![0.0f32; 512];
                loop {
                    let got = r.drain_into(&mut d[..512.min(r.len())]);
                    if got == 0 {
                        break;
                    }
                    out.extend_from_slice(&d[..got]);
                }
            }
            r.push_slice(&stream[off..off + take]);
            off += take;
            n += 1;
            let mut d = vec![0.0f32; 300];
            loop {
                let got = r.drain_into(&mut d);
                if got == 0 {
                    break;
                }
                out.extend_from_slice(&d[..got]);
            }
        }
        assert_eq!(out.len(), stream.len(), "no samples lost or duplicated");
        assert!(
            out.iter().zip(stream.iter()).all(|(a, b)| a == b),
            "ring stream must be byte-exact"
        );

        // Overflow keeps the newest window, in order.
        let mut r = FxRing::with_capacity(16);
        let big: Vec<f32> = (0..40).map(|i| i as f32).collect();
        r.push_slice(&big);
        assert_eq!(r.len(), 16);
        let mut tail = vec![0.0f32; 16];
        r.drain_into(&mut tail);
        assert_eq!(&tail[..], &big[24..], "overflow keeps the newest window");
        r.push_slice(&[1.0, 2.0, 3.0]);
        r.clear();
        assert_eq!(r.len(), 0, "clear empties");

        // -- Plugin-backed: finite passthrough + post-flush echo reset.
        let Some((path, kind)) = v16_test_plugin() else {
            eprintln!("skip AC6 plugin half: no local test plugin");
            return;
        };
        const N: usize = 512 * 24;
        let sig: Vec<f32> = (0..N)
            .map(|i| (i as f32 * 0.037).sin() * 0.4 + (i as f32 * 0.011).cos() * 0.2)
            .collect();
        let load = open(&path, kind, 48_000, None).expect("open test plugin");
        let mut out = vec![0.0f32; N];
        {
            let mut o = vec![0.0f32; 512];
            for (k, w) in sig.chunks(512).enumerate() {
                load.entry.process(w, &mut o);
                out[k * 512..k * 512 + 512].copy_from_slice(&o);
            }
        }
        assert!(out.iter().all(|v| v.is_finite()), "output must stay finite");
        assert!(
            out.iter().any(|v| *v != 0.0),
            "signal must actually pass through"
        );

        // Ragged feed must stay finite too (ring must not glitch shapes).
        let cycle = [
            100usize, 700, 37, 1000, 512, 40, 903, 256, 640, 512, 96, 2000,
        ];
        let mut off = 0usize;
        let mut ci = 0usize;
        let mut left = N;
        while left > 0 {
            let n = cycle[ci % cycle.len()].min(left);
            let mut o = vec![0.0f32; n];
            load.entry.process(&sig[off..off + n], &mut o);
            assert!(
                o.iter().all(|v| v.is_finite()),
                "ragged block {ci} produced non-finite"
            );
            off += n;
            left -= n;
            ci += 1;
        }

        // flush() empties every ring: the very next (sub-block) call is
        // answered by the echo carry — the exact input samples.
        load.entry.flush();
        let probe = vec![0.125f32; 100];
        let mut po = vec![9.0f32; 100];
        load.entry.process(&probe, &mut po);
        assert!(
            po.iter().all(|v| (*v - 0.125).abs() < 1e-9),
            "after flush the pipeline is empty: the echo carry must return the input at once"
        );
        drop(load);
    }
}
