//! Synthesizer plugin framework — the MIDI counterpart of [`crate::fx`].
//!
//! Every script `*.rs` in the synths folder (`~/.local/share/slooper/synths`)
//! is compiled with rustc into a cdylib and loaded behind the small C ABI in
//! [`crate::synth_api`]. A *track* owns exactly one synth instance (its
//! `Instance` is created on the GUI thread and pushed into the engine, which
//! calls it on the audio thread); a track with no binding falls back to the
//! bundled `piano`. The bundled `piano` and `sampler` scripts live in the
//! repo and are seeded into the folder on first start (updated in place when
//! slooper ships a newer bundled version — same rule as effect scripts).
//!
//! Sliders ride atomics exactly like effect params, and samplers fetch rack
//! audio through the host table ([`set_host_assets`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub use crate::synth_api::{
    SynthAssetV1, SynthHostV1, SynthInfoV1, SynthParamV1, SLOOPER_MAX_SYNTH_PARAMS,
    SLOOPER_SYNTH_ABI,
};

pub const MAX_PARAMS: usize = SLOOPER_MAX_SYNTH_PARAMS;

/// Bundled scripts. `synth_api.rs` is rewritten (and version-pinned) on
/// every start; the synths themselves are seeded when missing or stale.
pub const API_SRC: &str = include_str!("synth_api.rs");
pub const BUNDLED: &[(&str, &str)] = &[
    ("piano.rs", include_str!("../synths/piano.rs")),
    ("drums.rs", include_str!("../synths/drums.rs")),
    ("pad.rs", include_str!("../synths/pad.rs")),
    ("tri_osc.rs", include_str!("../synths/tri_osc.rs")),
    ("fm.rs", include_str!("../synths/fm.rs")),
    ("pluck.rs", include_str!("../synths/pluck.rs")),
    ("granular.rs", include_str!("../synths/granular.rs")),
    ("additive.rs", include_str!("../synths/additive.rs")),
    ("sampler.rs", include_str!("../synths/sampler.rs")),
    // Attribution for the piano samples (TimGM6mb, GPL-2); the registry
    // only compiles *.rs, so this rides along as documentation.
    (
        "THIRD-PARTY-NOTICES.md",
        include_str!("../synths/THIRD-PARTY-NOTICES.md"),
    ),
];
/// The stamp `// slooper-synth-version: N` on line 2 of every bundled
/// script marks it as OURS (an unstamped same-named file is a hand-edited
/// user file and is never overwritten). Since v0.12 the seed update itself
/// is CONTENT-compared — this constant no longer gates anything at runtime
/// (the exact-match gate froze the sampler fix out of every install; see
/// `v12_seed_stale_stamp_updates_despite_version_skew`), so it lives only
/// for the tests that assert the shipped scripts stay stamped.
#[cfg(test)]
const BUNDLED_VERSION: &str = "6";

/// Default synths folder (overridable with `SLOOPER_SYNTHS_DIR` for tests).
pub fn default_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SLOOPER_SYNTHS_DIR") {
        return PathBuf::from(d);
    }
    crate::paths::data_dir().join("synths")
}

fn cache_dir() -> PathBuf {
    // Folder name predates paths.rs: `<cache home>/slooper-synth`
    // (`~/.cache/slooper-synth` on Linux, `%LOCALAPPDATA%\slooper-synth`
    // on Windows — WINDOWS-PORT W2).
    crate::paths::cache_home().join("slooper-synth")
}

/// Write the ABI contract + bundled scripts into the folder. Bundled scripts
/// present in the folder but stamped with an older `// slooper-synth-version`
/// are replaced (backup `<name>.rs.bak`) — the same anti-freeze rule that
/// keeps effect-script fixes visible.
pub fn seed_folder(dir: &Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return; // read-only folder: the registry just lists fewer synths
    }
    let api = dir.join("synth_api.rs");
    // The shipped API file already carries the current version marker.
    let want_api = API_SRC.to_string();
    let stale_api = std::fs::read_to_string(&api)
        .map(|s| s != want_api)
        .unwrap_or(true);
    if stale_api && std::fs::write(&api, &want_api).is_ok() {
        // Version note only; scripts recompile on next rescan by mtime.
    }
    for (name, src) in BUNDLED {
        let p = dir.join(name);
        match std::fs::read_to_string(&p) {
            Ok(cur) => {
                if cur == *src {
                    continue;
                }
                // Only auto-update copies still carrying OUR version stamp —
                // a hand-edited piano.rs is the user's file. The stamp lives
                // on line 2 (line 1 is the `==== slooper ==` banner), so
                // scan the header block, not just the first line.
                // The v0.12 gate dropped its second condition — "the bundled
                // copy carries the exact current BUNDLED_VERSION string":
                // per-file stamps drift from that folder-wide constant, and
                // every drift silently froze the file on every existing
                // install (the sampler live-resync fix shipped at stamp 4 vs
                // global 6 and never reached a single machine — the v0.12
                // "sampler requires a restart" report). Bundled text is ours
                // by construction: stamped file + content difference updates.
                let stamped = cur
                    .lines()
                    .take(4)
                    .any(|l| l.contains("slooper-synth-version:"))
                    && src.contains("slooper-synth-version:");
                if stamped && cur != *src {
                    let bak = dir.join(format!("{name}.bak"));
                    let _ = std::fs::write(&bak, cur);
                    let _ = std::fs::write(&p, src);
                }
            }
            Err(_) => {
                let _ = std::fs::write(&p, src);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host services (rack audio for samplers)
// ---------------------------------------------------------------------------

/// Rack table exposed to samplers; refreshed by the engine on set_project.
static HOST_TABLE: RwLock<Option<(Arc<Vec<crate::assets::AssetData>>, u32)>> = RwLock::new(None);

// Keeps the Arc alive so the pointer returned to a plugin stays valid at
// least until the next load_asset on the same (audio) thread.
thread_local! {
    static HOST_KEEP: std::cell::RefCell<Option<Arc<Vec<crate::assets::AssetData>>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn set_host_assets(assets: Arc<Vec<crate::assets::AssetData>>, rate: u32) {
    if let Ok(mut t) = HOST_TABLE.write() {
        *t = Some((assets, rate));
    }
}

extern "C" fn host_load_asset(id: u32, out: *mut SynthAssetV1) -> u8 {
    let Ok(t) = HOST_TABLE.read() else {
        return 0;
    };
    let Some((table, rate)) = t.as_ref() else {
        return 0;
    };
    let Some(data) = table.get(id as usize) else {
        return 0;
    };
    if data.is_empty() {
        return 0;
    }
    HOST_KEEP.with(|k| *k.borrow_mut() = Some(table.clone()));
    unsafe {
        (*out).data = data.as_ptr();
        (*out).len = data.len() as u32;
        (*out).sample_rate = *rate;
    }
    1
}

/// The host vtable handed to every synth `create`.
pub static HOST: SynthHostV1 = SynthHostV1 {
    abi: SLOOPER_SYNTH_ABI,
    load_asset: host_load_asset,
};

// ---------------------------------------------------------------------------
// Descriptors & instances
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct SynthParamDesc {
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

#[derive(Clone, Debug)]
pub struct SynthDesc {
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub params: Vec<SynthParamDesc>,
}

#[derive(Clone, Copy)]
struct SynthFns {
    create: extern "C" fn(sample_rate: f64, host: *const SynthHostV1) -> *mut core::ffi::c_void,
    destroy: extern "C" fn(s: *mut core::ffi::c_void),
    note_on: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, note: u8, vel: u8),
    note_off: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, note: u8),
    all_off: extern "C" fn(s: *mut core::ffi::c_void),
    cc: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, num: u8, val: u8),
    press: extern "C" fn(s: *mut core::ffi::c_void, ch: u8, val: u8),
    process: extern "C" fn(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32),
}

/// Library handle that is NEVER dlclosed — see the identical comment in
/// [`crate::fx`]: closing a Rust-runtime plugin while host teardown
/// destructors still run aborts the process.
struct LibHandle(std::mem::ManuallyDrop<libloading::Library>);
unsafe impl Send for LibHandle {}
unsafe impl Sync for LibHandle {}
impl std::ops::Deref for LibHandle {
    type Target = libloading::Library;
    fn deref(&self) -> &libloading::Library {
        &self.0
    }
}

/// One live synth instance. Param sliders are atomics (the GUI rides them);
/// note traffic is only ever driven by the audio thread.
pub struct Instance {
    fns: SynthFns,
    state: *mut core::ffi::c_void,
    sample_rate: f64,
    n_params: usize,
    params: Box<[AtomicU32; MAX_PARAMS]>,
    param_gen: AtomicU64,
    _lib: Arc<LibHandle>,
}
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Instance {
    fn new(
        fns: SynthFns,
        lib: Arc<LibHandle>,
        desc: &SynthDesc,
        sample_rate: f64,
        values: &[f32],
    ) -> Option<Arc<Self>> {
        let state = (fns.create)(sample_rate, &HOST);
        if state.is_null() {
            return None;
        }
        let inst = Arc::new(Instance {
            fns,
            state,
            sample_rate,
            n_params: desc.params.len().min(MAX_PARAMS),
            params: Box::new(core::array::from_fn(|k| {
                let d = desc.params.get(k).map(|p| p.default).unwrap_or(0.0);
                AtomicU32::new(values.get(k).copied().unwrap_or(d).to_bits())
            })),
            param_gen: AtomicU64::new(1),
            _lib: lib,
        });
        Some(inst)
    }

    /// A brand-new instance of the SAME script with the SAME slider values
    /// — offline bounces use one per clip so live state never leaks into a
    /// render (mirrors `fx::Instance::spawn_fresh`).
    pub fn spawn_fresh(&self) -> Option<Arc<Self>> {
        let vals = self.values();
        let desc = SynthDesc {
            name: String::new(),
            author: None,
            description: None,
            params: vals
                .iter()
                .map(|v| SynthParamDesc {
                    label: String::new(),
                    min: *v,
                    max: *v,
                    default: *v,
                })
                .collect(),
        };
        Self::new(self.fns, self._lib.clone(), &desc, self.sample_rate, &vals)
    }

    pub fn set_param(&self, k: usize, v: f32) {
        if k < self.n_params {
            self.params[k].store(v.to_bits(), Ordering::Relaxed);
            self.param_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn param_gen(&self) -> u64 {
        self.param_gen.load(Ordering::Relaxed)
    }

    pub fn read_params(&self, out: &mut [f32; MAX_PARAMS]) {
        for (o, p) in out.iter_mut().zip(&self.params[..self.n_params]) {
            *o = f32::from_bits(p.load(Ordering::Relaxed));
        }
    }

    pub fn values(&self) -> Vec<f32> {
        let mut p = [0.0f32; MAX_PARAMS];
        self.read_params(&mut p);
        p[..self.n_params].to_vec()
    }

    pub fn note_on(&self, ch: u8, note: u8, vel: u8) {
        (self.fns.note_on)(self.state, ch, note, vel)
    }
    pub fn note_off(&self, ch: u8, note: u8) {
        (self.fns.note_off)(self.state, ch, note)
    }
    pub fn all_off(&self) {
        (self.fns.all_off)(self.state)
    }
    pub fn cc(&self, ch: u8, num: u8, val: u8) {
        (self.fns.cc)(self.state, ch, num, val)
    }
    pub fn press(&self, ch: u8, val: u8) {
        (self.fns.press)(self.state, ch, val)
    }

    /// Render `out.len()` samples (OUT IS ACCUMULATED INTO — the plugin
    /// adds to whatever is there, matching the ABI). `params` is a snapshot.
    ///
    /// # Safety
    /// Only ever called from one thread at a time for this instance.
    pub unsafe fn process_into(&self, out: &mut [f32], params: &[f32; MAX_PARAMS]) {
        (self.fns.process)(self.state, out.as_mut_ptr(), out.len(), params.as_ptr())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        (self.fns.destroy)(self.state);
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct SynthEntry {
    pub stem: String,
    pub desc: Option<SynthDesc>,
    fns: Option<SynthFns>,
    pub error: Option<String>,
    _lib: Option<Arc<LibHandle>>,
}

/// The synths folder plus everything loaded from it.
pub struct Registry {
    dir: PathBuf,
    items: Vec<SynthEntry>,
}
unsafe impl Send for Registry {}
unsafe impl Sync for Registry {}

impl Registry {
    pub fn open(dir: PathBuf) -> Self {
        seed_folder(&dir);
        let mut r = Registry {
            dir,
            items: Vec::new(),
        };
        r.rescan();
        r
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn stems(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|e| e.fns.is_some())
            .map(|e| e.stem.clone())
            .collect()
    }

    pub fn desc(&self, stem: &str) -> Option<&SynthDesc> {
        self.items
            .iter()
            .find(|e| e.stem == stem && e.fns.is_some())
            .and_then(|e| e.desc.as_ref())
    }

    pub fn errors(&self) -> Vec<(String, String)> {
        self.items
            .iter()
            .filter_map(|e| e.error.as_ref().map(|r| (e.stem.clone(), r.clone())))
            .collect()
    }

    fn find(&self, stem: &str) -> Option<&SynthEntry> {
        self.items
            .iter()
            .find(|e| e.stem == stem && e.fns.is_some())
    }

    /// Create a live instance of `stem` (returns None on unknown/stem/reject).
    pub fn create_instance(
        &self,
        stem: &str,
        sample_rate: f64,
        values: &[f32],
    ) -> Option<Arc<Instance>> {
        let e = self.find(stem)?;
        let fns = e.fns?;
        Instance::new(fns, e._lib.clone()?, e.desc.as_ref()?, sample_rate, values)
    }

    /// Recompile what changed and reload. Never panics on bad scripts —
    /// rejections land in `error` for the GUI.
    pub fn rescan(&mut self) {
        seed_folder(&self.dir);
        let build = cache_dir();
        let mut out = Vec::new();
        let mut ok: HashMap<String, SynthEntry> = HashMap::new();
        let mut files: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for f in rd.flatten().map(|e| e.path()) {
                let is_script = f.extension().and_then(|e| e.to_str()) == Some("rs")
                    && f.file_name().map(|n| n != "synth_api.rs").unwrap_or(false);
                if is_script {
                    files.push(f);
                }
            }
        }
        files.sort();
        for src in files {
            let stem = src
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let reject = |e: String| SynthEntry {
                stem: stem.clone(),
                desc: None,
                fns: None,
                error: Some(e),
                _lib: None,
            };
            // W14 (WINDOWS-PORT): the artifact is CONTENT-stamped, so an
            // unchanged script always reuses the existing file (no rustc,
            // no rename at all) and an edited script builds to a FRESH
            // name that can never collide with the old mapped dll.
            let lib = match stamped_artifact(&build, &stem, &src) {
                Ok(p) => p,
                Err(e) => {
                    out.push(reject(e));
                    continue;
                }
            };
            let entry = if !lib.is_file() {
                match compile_synth(&src, &lib) {
                    Ok(()) => {
                        prune_stale(&build, &stem, &lib);
                        unsafe { load_one(&stem, &lib) }
                    }
                    Err(e) => reject(e),
                }
            } else {
                unsafe { load_one(&stem, &lib) }
            };
            if entry.fns.is_some() {
                ok.insert(stem, entry);
            } else {
                out.push(entry);
            }
        }
        // Valid scripts first, alphabetical, then rejections.
        let mut valid: Vec<SynthEntry> = ok.into_values().collect();
        valid.sort_by_key(|e| e.stem.clone());
        out.sort_by_key(|e| e.stem.clone());
        out.splice(0..0, valid);
        self.items = out;
    }
}

/// W14: `<cache>/<stem>-<content-hash>.<dylib-ext>`. The old scheme was a
/// fixed `<stem>.<ext>` rebuilt whenever the source mtime moved — on
/// Windows that is a DEATH CLASS, not a nuisance: the repo never dlcloses,
/// so the moment a script dll is loaded, the next rebuild's staging
/// `rename` targets a MAPPED file and Windows refuses it
/// (ERROR_USER_MAPPED_FILE; Linux silently swaps the inode, so the bug was
/// structurally invisible there). Folding CONTENT instead of mtime+size
/// also stops every re-seed from recompiling all bundled scripts: same
/// bytes → same artifact → reused as-is. Same philosophy as the fx
/// registry's stamped `.build/` artifacts (fx's fold stays mtime+size —
/// unifying the two is a later, optional convergence).
fn stamped_artifact(build: &Path, stem: &str, src: &Path) -> Result<PathBuf, String> {
    let bytes = std::fs::read(src).map_err(|e| format!("stat failed: {e}"))?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    stem.hash(&mut h);
    bytes.hash(&mut h);
    let stamp = h.finish();
    Ok(build.join(format!(
        "{stem}-{stamp:016x}.{}",
        crate::paths::dylib_ext()
    )))
}

/// Delete older artifacts of the same stem — extra stamps from edits and
/// the pre-W14 fixed-name file (which nothing looks up anymore).
/// Best-effort by design: deleting a still-mapped dll fails on Windows and
/// the file lingers until it unmaps — harmless.
fn prune_stale(build: &Path, stem: &str, keep: &Path) {
    if let Ok(rd) = std::fs::read_dir(build) {
        let prefix = format!("{stem}-");
        let legacy = format!("{stem}.{}", crate::paths::dylib_ext());
        for ent in rd.flatten() {
            let p = ent.path();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if (name.starts_with(&prefix) || name == legacy) && p != keep {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Serializes rustc invocations: test threads (and a future rescan during
/// playback) must not write the same staging file (`.tmp` sidecar)
/// concurrently.
fn compile_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn compile_synth(src: &Path, out: &Path) -> Result<(), String> {
    let _serial = compile_lock();
    if let Some(p) = out.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("could not create build dir: {e}"))?;
    }
    let stem = out
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("slooper_synth");
    let crate_name = {
        let mut s = String::from("syn_");
        for c in stem.chars() {
            s.push(if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            });
        }
        s
    };
    let tmp = crate::paths::build_sidecar(out, "tmp");
    let output = Command::new("rustc")
        .args([
            "--edition",
            "2021",
            "--crate-type",
            "cdylib",
            "-C",
            "opt-level=2",
        ])
        .arg("--crate-name")
        .arg(&crate_name)
        .arg("-o")
        .arg(&tmp)
        .arg(src)
        .output()
        .map_err(|e| {
            format!(
                "could not run rustc: {e} — {}",
                crate::fx::rustc_install_hint()
            )
        })?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        std::fs::write(crate::paths::build_sidecar(out, "failed"), err.as_bytes()).ok();
        let mut err: String = err.trim().chars().take(1800).collect();
        err.push('…');
        return Err(format!("compile failed:\n{err}"));
    }
    // W14 (WINDOWS-PORT): a second slooper instance may have built the same
    // content stamp concurrently — on Windows renaming onto a dll THAT
    // PROCESS has mapped fails (the reverse also holds for our own earlier
    // registries, hence content-stamped names). Either way, if the artifact
    // exists now, we have exactly the file we were trying to produce.
    if let Err(e) = std::fs::rename(&tmp, out) {
        if !out.is_file() {
            return Err(format!("could not store {}: {e}", out.display()));
        }
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

unsafe fn load_one(stem: &str, lib_path: &Path) -> SynthEntry {
    let reject = |e: String| SynthEntry {
        stem: stem.to_string(),
        desc: None,
        fns: None,
        error: Some(e),
        _lib: None,
    };
    let lib = match libloading::Library::new(lib_path) {
        Ok(l) => Arc::new(LibHandle(std::mem::ManuallyDrop::new(l))),
        Err(e) => return reject(format!("load failed: {e}")),
    };
    let sym: libloading::Symbol<extern "C" fn() -> *const SynthInfoV1> = match lib
        .get(b"slooper_synth_info\0")
    {
        Ok(s) => s,
        Err(_) => {
            return reject("missing entry point slooper_synth_info — see the bundled synths".into())
        }
    };
    let info = sym();
    if info.is_null() {
        return reject("slooper_synth_info returned null".into());
    }
    let d = &*info;
    if d.abi != SLOOPER_SYNTH_ABI {
        return reject(format!(
            "synth ABI {} is not supported (app speaks ABI {SLOOPER_SYNTH_ABI})",
            d.abi
        ));
    }
    if d.n_params as usize > MAX_PARAMS {
        return reject(format!("declares {} params (max {MAX_PARAMS})", d.n_params));
    }
    let cstr = |p: *const core::ffi::c_char| -> Option<String> {
        if p.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(p)
            .to_str()
            .ok()
            .map(|s| s.to_string())
    };
    let name = match cstr(d.name) {
        Some(n) if !n.is_empty() => n,
        _ => return reject("name missing or not UTF-8".into()),
    };
    let nulls = [
        d.create as usize,
        d.destroy as usize,
        d.note_on as usize,
        d.note_off as usize,
        d.all_off as usize,
        d.cc as usize,
        d.press as usize,
        d.process as usize,
    ]
    .iter()
    .filter(|p| **p == 0)
    .count();
    if nulls > 0 {
        return reject(format!("{nulls} callback pointer(s) are null"));
    }
    let params = (0..d.n_params as usize)
        .filter(|_| !d.params.is_null())
        .map(|k| unsafe { &*d.params.add(k) })
        .map(|p| SynthParamDesc {
            label: cstr(p.label).unwrap_or_else(|| "p?".to_string()),
            min: p.min,
            max: p.max,
            default: p.default,
        })
        .collect();
    let desc = SynthDesc {
        name,
        author: cstr(d.author),
        description: cstr(d.description),
        params,
    };
    let fns = SynthFns {
        create: d.create,
        destroy: d.destroy,
        note_on: d.note_on,
        note_off: d.note_off,
        all_off: d.all_off,
        cc: d.cc,
        press: d.press,
        process: d.process,
    };
    SynthEntry {
        stem: stem.to_string(),
        desc: Some(desc),
        fns: Some(fns),
        error: None,
        _lib: Some(lib),
    }
}

/// Seed-value helper shared with the GUI: clamp saved slider values into
/// the freshly-declared ranges, padding with defaults.
pub fn seed_values(saved: &[f32], desc: &SynthDesc) -> Vec<f32> {
    desc.params
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let v = saved.get(k).copied().unwrap_or(p.default);
            if p.max > p.min {
                v.clamp(p.min, p.max)
            } else {
                p.default
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry(tag: &str) -> Registry {
        let dir = std::env::temp_dir().join(format!("sp-synth-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("SLOOPER_SYNTHS_DIR", &dir);
        Registry::open(dir.clone())
    }

    #[test]
    fn seed_folder_updates_stamped_stale_scripts() {
        // The v0.7 modal-piano swap is invisible to existing installs
        // unless an old STAMPED copy updates in place — and the version
        // stamp sits on line 2 of every bundled script (line 1 is the
        // `==== slooper ==` banner), so the header scan must see it.
        let dir = std::env::temp_dir().join("sp-synth-seed");
        let _ = std::fs::remove_dir_all(&dir);
        seed_folder(&dir);
        let piano = dir.join("piano.rs");
        let cur = std::fs::read_to_string(&piano).unwrap();
        assert!(cur.contains(&format!("slooper-synth-version: {BUNDLED_VERSION}")));
        // An install from a previous app version: old content, still stamped.
        let old = "// ============ slooper ==\n// slooper-synth-version: 0\n// old FM voice\n"
            .to_string();
        std::fs::write(&piano, &old).unwrap();
        // A hand-edited script (no stamp in the header) is the user's file.
        let mine = dir.join("sampler.rs");
        std::fs::write(&mine, "// my custom synth\n").unwrap();
        seed_folder(&dir);
        let upd = std::fs::read_to_string(&piano).unwrap();
        assert!(
            upd.contains("TimGM6mb"),
            "stale stamped piano.rs was not updated in place"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("piano.rs.bak")).unwrap(),
            old,
            "old copy not backed up"
        );
        assert_eq!(
            std::fs::read_to_string(&mine).unwrap(),
            "// my custom synth\n"
        );
        assert!(!dir.join("sampler.rs.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v12_seed_stale_stamp_updates_despite_version_skew() {
        // v0.12 "sampler still requires a restart for every new sample":
        // the sampler's live-resync fix shipped in v0.10 with SCRIPT stamp 4,
        // but the folder-wide BUNDLED_VERSION was bumped later, and the old
        // update gate also required the bundled copy to carry the EXACT
        // global version string — so the update was rejected forever and
        // every install holding a stamp-3 sampler kept the pre-fix script.
        // The on-disk stamp marks the file as OURS; a content difference is
        // the only staleness signal that actually works.
        let dir = std::env::temp_dir().join("sp-synth-seed-skew");
        let _ = std::fs::remove_dir_all(&dir);
        seed_folder(&dir);
        let sampler = dir.join("sampler.rs");
        let bundled = std::fs::read_to_string(&sampler).unwrap();
        // Simulate an older install: still stamped, body predates the fix.
        let stale: String = bundled
            .lines()
            .enumerate()
            .map(|(i, l)| {
                if i == 1 {
                    "// slooper-synth-version: 0".to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let stale = stale.replace("data_ptr", "resync_deferred");
        assert_ne!(
            stale, bundled,
            "fixture must differ from the bundled script"
        );
        std::fs::write(&sampler, &stale).unwrap();
        seed_folder(&dir);
        let upd = std::fs::read_to_string(&sampler).unwrap();
        assert_eq!(
            upd, bundled,
            "stale stamped sampler.rs was never updated (version-skew gate)"
        );
        assert!(
            dir.join("sampler.rs.bak").exists(),
            "old copy not backed up"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_piano_and_sampler_load_and_render() {
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let reg = test_registry("bundled");
        let stems = reg.stems();
        // W14: the rejection reasons MUST ride the failure line — on the
        // CI windows lane this test failed as bare `stems: []` with the
        // actual load/compile errors trapped unreachable inside the
        // registry, forcing a whole CI cycle per triage guess.
        let errs = reg.errors();
        assert!(
            stems.contains(&"piano".to_string()),
            "stems: {stems:?}, errors: {errs:?}"
        );
        assert!(
            stems.contains(&"sampler".to_string()),
            "stems: {stems:?}, errors: {errs:?}"
        );
        // Give the host a rack asset (asset id 0) so the sampler has something
        // to play; with an empty table silence is correct behavior.
        let sine: Arc<[f32]> = (0..4800).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        for stem in ["piano", "sampler"] {
            let inst = reg
                .create_instance(stem, 48_000.0, &[])
                .unwrap_or_else(|| panic!("{stem} instance — errors: {:?}", reg.errors()));
            let mut p = [0.0f32; MAX_PARAMS];
            inst.read_params(&mut p);
            // defaults for unknown params are 0 — pass desc defaults instead
            let desc = reg.desc(stem).unwrap();
            for (k, pd) in desc.params.iter().enumerate() {
                p[k] = pd.default;
            }
            let mut out = vec![0.0f32; 2048];
            // One block first: a sampler syncs its asset buffer here, and
            // notes started before that legitimately do nothing.
            unsafe { inst.process_into(&mut out, &p) };
            inst.note_on(0, 60, 100);
            let mut out = vec![0.0f32; 2048];
            unsafe { inst.process_into(&mut out, &p) };
            let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak > 1e-4, "{stem} must make sound (peak {peak})");
            inst.all_off();
            let mut out2 = vec![0.0f32; 4096];
            unsafe { inst.process_into(&mut out2, &p) };
            let tail = out2.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(tail < peak.max(1e-9), "{stem} all_off must stop new sound");
        }
    }

    /// FL Suite tier 2: EVERY bundled synth must compile, load, render
    /// audible sound at its own default params, and fall silent on
    /// all_off. The list DERIVES from `BUNDLED` (same rule as the effect
    /// bundle): a new script joins the gate by being bundled.
    #[test]
    fn all_bundled_synths_render_sound() {
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let reg = test_registry("allbundled");
        let sine: Arc<[f32]> = (0..4800).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        let mut tested = 0;
        for (name, _src) in BUNDLED {
            if !name.ends_with(".rs") {
                continue;
            }
            let stem = name.trim_end_matches(".rs");
            let inst = reg
                .create_instance(stem, 48_000.0, &[])
                .unwrap_or_else(|| panic!("{stem} instance — errors: {:?}", reg.errors()));
            let desc = reg.desc(stem).unwrap();
            let mut p = [0.0f32; MAX_PARAMS];
            for (k, pd) in desc.params.iter().enumerate() {
                p[k] = pd.default;
            }
            // One sync block (samplers pick up their asset), then notes.
            let mut out = vec![0.0f32; 2048];
            unsafe { inst.process_into(&mut out, &p) };
            inst.note_on(0, 60, 110);
            inst.note_on(0, 64, 100);
            inst.note_on(0, 67, 92);
            // Half a second covers the slowest default attack.
            let mut out = vec![0.0f32; 24000];
            unsafe { inst.process_into(&mut out, &p) };
            let peak = out.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(peak > 1e-4, "{stem} must make sound (peak {peak})");
            assert!(
                out.iter().all(|x| x.is_finite()),
                "{stem} produced non-finite output"
            );
            inst.all_off();
            let mut out2 = vec![0.0f32; 4096];
            unsafe { inst.process_into(&mut out2, &p) };
            let tail = out2.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(tail < 1e-3, "{stem} all_off must stop new sound (tail {tail})");
            tested += 1;
        }
        assert!(tested >= 8, "expected 8 bundled synths, tested {tested}");
    }

    /// v0.8 item 2: "the sampler doesn't currently work" — beyond making
    /// sound, the sampler must PITCH the rack asset by the semitone
    /// distance from its root note: +12 st ⇒ exactly twice the rate.
    #[test]
    fn v08_sampler_plays_asset_pitched() {
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let reg = test_registry("pitched");
        let sine: Arc<[f32]> = (0..48000)
            .map(|i| (i as f64 * 2.0 * std::f64::consts::TAU * 200.0 / 48000.0).sin() as f32 * 0.5)
            .collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        let inst = reg
            .create_instance("sampler", 48_000.0, &[])
            .unwrap_or_else(|| panic!("sampler instance — errors: {:?}", reg.errors()));
        let desc = reg.desc("sampler").unwrap();
        let mut p = [0.0f32; MAX_PARAMS];
        for (k, pd) in desc.params.iter().enumerate() {
            p[k] = pd.default;
        }
        p[1] = 60.0; // root note = C4
        let render = |note: u8| -> Vec<f32> {
            let mut out = vec![0.0f32; 8192];
            unsafe { inst.process_into(&mut out, &p) }; // asset sync block
            inst.note_on(0, note, 110);
            let mut out = vec![0.0f32; 8192];
            unsafe { inst.process_into(&mut out, &p) };
            inst.all_off();
            out
        };
        let zc = |x: &[f32]| -> f64 {
            x.iter()
                .skip(256)
                .zip(x.iter().skip(257))
                .filter(|(a, b)| **a * **b < 0.0)
                .count() as f64
        };
        let r60 = render(60); // at root: the sample's own 200 Hz
        let r72 = render(72); // +12 st: 400 Hz
        let (z0, z1) = (zc(&r60), zc(&r72));
        assert!(z0 > 100.0, "root render must be real signal (zc {z0})");
        let ratio = z1 / z0;
        assert!(
            (ratio - 2.0).abs() < 0.25,
            "+12 st must double the pitch: zc {z0} vs {z1} (ratio {ratio})"
        );
    }

    /// Goertzel-style squared magnitude of `x` at frequency `f` over
    /// `x[a..b]` — enough to tell a harmonic line from the noise floor.
    fn tone_power(x: &[f32], f: f64, sr: f64, a: usize, b: usize) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        let w = std::f64::consts::TAU * f / sr;
        for (n, s) in x.iter().enumerate().take(b).skip(a) {
            let ph = w * n as f64;
            re += *s as f64 * ph.cos();
            im -= *s as f64 * ph.sin();
        }
        re * re + im * im
    }

    /// Peak power within ±0.6 % of `f` (covers the synth's stiff-string
    /// partial stretch) — so the test measures "a line lives near n·f0",
    /// not the exact coefficient choice.
    fn line_power(x: &[f32], f: f64, sr: f64, a: usize, b: usize) -> f64 {
        let mut m = 0.0f64;
        let mut k = -3;
        while k <= 3 {
            let ff = f * (1.0 + 0.002 * k as f64);
            m = m.max(tone_power(x, ff, sr, a, b));
            k += 1;
        }
        m
    }

    #[test]
    fn v08_piano_is_a_recorded_grand() {
        // AC2.2 (v0.8): the bundled piano is now real recorded grand-
        // piano multisamples (TimGM6mb 'Piano 1'). Pin the properties
        // that make it a piano and not a toy: correct pitch, harmonic
        // spectrum that falls with frequency, sustain with a live decay,
        // real velocity dynamics, a damper that stops sound, and no NaN.
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let reg = test_registry("grand");
        let inst = reg
            .create_instance("piano", 48_000.0, &[])
            .unwrap_or_else(|| panic!("piano instance — errors: {:?}", reg.errors()));
        let desc = reg.desc("piano").unwrap().clone();
        let mut p = [0.0f32; MAX_PARAMS];
        for (k, pd) in desc.params.iter().enumerate() {
            p[k] = pd.default;
        }
        let sr = 48_000.0f64;
        let render = |note: u8, vel: u8, secs: f64| -> Vec<f32> {
            inst.all_off();
            inst.note_on(0, note, vel);
            let n = (secs * sr) as usize;
            let mut out = vec![0.0f32; n];
            let mut at = 0usize;
            while at < n {
                let end = (at + 1024).min(n);
                let mut blk = vec![0.0f32; end - at];
                unsafe { inst.process_into(&mut blk, &p) };
                out[at..end].copy_from_slice(&blk);
                at = end;
            }
            out
        };
        let rms = |a: &[f32]| -> f64 {
            a.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / a.len() as f64
        };
        let out = render(57, 110, 3.0); // A3
        assert!(out.iter().all(|s| s.is_finite()), "NaN/inf in piano");
        let f0 = 220.0f64;
        let head = &out[(0.08 * sr) as usize..(0.8 * sr) as usize];
        let tail = &out[(2.3 * sr) as usize..(2.9 * sr) as usize];
        assert!(rms(head) > 1e-4, "piano must make sound");
        assert!(rms(tail) > 1e-9, "a real piano still rings at 2.3 s");
        assert!(rms(head) > 1.3 * rms(tail), "the note must decay");
        // Pitch: strong line at A3 (all zone/octave math baked in).
        let on = line_power(&out, f0, sr, (0.08 * sr) as usize, (0.8 * sr) as usize);
        let off = tone_power(&out, 253.0, sr, (0.08 * sr) as usize, (0.8 * sr) as usize);
        assert!(
            on > 25.0 * off,
            "fundamental must sit at 220 Hz: {on} vs {off}"
        );
        // Harmonic spectrum falling with frequency (no static, no buzz).
        let low = line_power(&out, f0, sr, (0.08 * sr) as usize, (0.8 * sr) as usize);
        let high = line_power(&out, 7000.0, sr, (0.08 * sr) as usize, (0.8 * sr) as usize);
        assert!(low > 10.0 * high, "spectrum must fall with frequency");
        // Velocity dynamics: 4x softer hit is dramatically quieter. The
        // recorded source enforces a power curve too ((vel/127)^1.55), but
        // sampled instruments breathe ~half as loud per velocity step as
        // a raw oscillator did — 6x is what a real grand gives here.
        let loud = render(60, 127, 0.5);
        let soft = render(60, 40, 0.5);
        let (ml, ms) = (
            loud.iter().fold(0f32, |m, s| m.max(s.abs())),
            soft.iter().fold(0f32, |m, s| m.max(s.abs())),
        );
        assert!(ml > 0.05 && ml < 1.2, "sane peak level, got {ml}");
        assert!(
            ml > 4.0 * ms,
            "velocity must move the dynamic (loud {ml} soft {ms})"
        );
        // Damper: sound gone a short while after note-off.
        inst.all_off();
        inst.note_on(0, 60, 110);
        for _ in 0..10 {
            let mut blk = vec![0.0f32; 512];
            unsafe { inst.process_into(&mut blk, &p) };
        }
        inst.note_off(0, 60);
        // The damper is a ~120 ms mute ramp, so measure the TAIL after it,
        // not the release transient itself.
        let mut blk = vec![0.0f32; 512];
        for _ in 0..18 {
            unsafe { inst.process_into(&mut blk, &p) };
        }
        let mut quiet = 0.0f32;
        for _ in 0..40 {
            let mut blk = vec![0.0f32; 512];
            unsafe { inst.process_into(&mut blk, &p) };
            quiet = quiet.max(blk.iter().fold(0f32, |m, s| m.max(s.abs())));
        }
        assert!(
            quiet < 0.02 * ml,
            "damper must stop the note, tail peak {quiet}"
        );
        inst.all_off();
    }

    #[test]
    fn invalid_scripts_are_rejected_with_reasons() {
        let reg = test_registry("reject");
        std::fs::write(
            reg.dir().join("broken.rs"),
            "pub static DESC: u8 = 1; // no ABI at all",
        )
        .unwrap();
        let reg = Registry::open(reg.dir().to_path_buf());
        let errs = reg.errors();
        assert!(errs.iter().any(|(s, _)| s == "broken"), "errors: {errs:?}");
        assert!(!reg.stems().contains(&"broken".to_string()));
    }

    // ===================== v0.10 acceptance pins =====================

    fn v10_host_table(data: Vec<f32>, extra: usize) {
        let mut v: Vec<crate::assets::AssetData> = (0..extra + 1)
            .map(|_| std::sync::Arc::from(Vec::<f32>::new()))
            .collect();
        v[2] = std::sync::Arc::from(data);
        set_host_assets(std::sync::Arc::new(v), 48_000);
    }

    /// AC1.1: a note staged at sample 0 of the very first block must sound
    /// — the sampler syncs its buffer inside process(), and without priming
    /// the first note_on landed on an empty buffer (the "must re-select the
    /// synth for it to work" bug).
    #[test]
    fn v10_sampler_note_at_block_zero_is_not_dropped() {
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let reg = test_registry("v10-first-note");
        v10_host_table(vec![0.8f32; 900], 2);
        let inst = reg
            .create_instance("sampler", 48_000.0, &[2.0, 48.0, 1.0])
            .unwrap_or_else(|| panic!("sampler instance — errors: {:?}", reg.errors()));
        let mut ev = vec![(0u64, 3u8, 0u8, 60u8, 110u8), (40_000, 0, 0, 60, 0)];
        let mut out = vec![0f32; 48_000 / 4];
        crate::audio::render_midi_events(&inst, &mut ev, 0, out.len(), &mut out);
        let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.05, "first note dropped: peak {peak}");
    }

    /// AC1.2: re-recording / re-deriving a sample keeps its asset id — the
    /// sampler must notice the content moved and replay the NEW data
    /// (previously it kept the stale buffer until the synth was re-bound).
    #[test]
    fn v10_sampler_rereads_asset_when_content_moves() {
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let reg = test_registry("v10-stale");
        v10_host_table(vec![0.9f32; 900], 2);
        let inst = reg
            .create_instance("sampler", 48_000.0, &[2.0, 48.0, 1.0])
            .unwrap_or_else(|| panic!("sampler instance — errors: {:?}", reg.errors()));
        let render = |inst: &Instance| {
            let mut ev = vec![(0u64, 3u8, 0u8, 60u8, 110u8), (40_000, 0, 0, 60, 0)];
            let mut out = vec![0f32; 48_000 / 8];
            crate::audio::render_midi_events(inst, &mut ev, 0, out.len(), &mut out);
            out.iter().fold(0f32, |m, x| m.max(x.abs()))
        };
        let peak1 = render(&inst);
        assert!(peak1 > 0.5, "loud sample must render loud (peak {peak1})");

        // Same asset id, new contents (a quieter take).
        v10_host_table(vec![0.02f32; 700], 2);
        let peak2 = render(&inst);
        assert!(
            peak2 < peak1 / 10.0,
            "sampler replayed STALE data: old {peak1} vs new {peak2}"
        );

        // Nothing moved since: identical render (no re-copy churn).
        let peak3 = render(&inst);
        assert!(
            (peak3 - peak2).abs() < 1e-6,
            "stable table must not alter output"
        );
    }

    #[test]
    fn w3_synth_compile_sidecar_ext() {
        // W3 (WINDOWS-PORT): staging/poison names follow the target suffix.
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise
        }
        let dir = std::env::temp_dir().join("sp-synth-w3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join(format!("pin.{}", crate::paths::dylib_ext()));
        let bad = dir.join("bad.rs");
        std::fs::write(&bad, "fn broken( {").unwrap();
        let e = compile_synth(&bad, &out).unwrap_err();
        assert!(e.contains("compile failed"), "{e}");
        assert!(crate::paths::build_sidecar(&out, "failed").is_file());
        assert!(!out.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===================== W14 (WINDOWS-PORT) script-build pins =====================

    #[test]
    fn w14_synth_artifact_is_content_stamped() {
        // The artifact name folds CONTENT, not mtime: an identical re-seed
        // (fresh mtime) must reuse the file, and any content edit must
        // produce a FRESH name (so a rename can never target the mapped
        // old dll — the Windows death class this whole task removes).
        let dir = std::env::temp_dir().join("sp-synth-w14stamp");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("kick.rs");
        std::fs::write(&src, "// v1 identical\n").unwrap();
        let a = stamped_artifact(&dir, "kick", &src).unwrap();
        std::fs::write(&src, "// v1 identical\n").unwrap(); // same bytes, new mtime
        assert_eq!(
            stamped_artifact(&dir, "kick", &src).unwrap(),
            a,
            "same content must reuse the artifact"
        );
        std::fs::write(&src, "// v2 edited\n").unwrap();
        let b = stamped_artifact(&dir, "kick", &src).unwrap();
        assert_ne!(a, b, "edited content must build a FRESH name");
        // shape: flat in the build dir, `{stem}-` prefix, target suffix;
        // and the stem folds into the hash (same bytes, other stem ≠ collide).
        assert_eq!(a.parent().unwrap(), dir);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("kick-") && name.ends_with(&format!(".{}", crate::paths::dylib_ext())),
            "{name}"
        );
        assert_ne!(stamped_artifact(&dir, "snare", &src).unwrap(), b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn w14_synth_edit_reloads_while_old_artifact_mapped() {
        // THE functional pin for the mapped-dll class: the loaded artifact
        // is LEAKED by design (repo never dlcloses), then the script is
        // edited. Under the pre-W14 fixed-name scheme every Windows rescan
        // after this died on "could not store …: Access is denied"; Linux
        // silently swapped the inode and never saw it.
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let dir = std::env::temp_dir().join("sp-synth-w14edit");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tri = BUNDLED
            .iter()
            .find(|(n, _)| *n == "tri_osc.rs")
            .map(|(_, s)| (*s).to_string())
            .expect("tri_osc bundled");
        std::fs::write(dir.join("w14live.rs"), &tri).unwrap();
        let art = |stem_src: &Path| stamped_artifact(&cache_dir(), "w14live", stem_src);
        let first = art(&dir.join("w14live.rs")).unwrap();
        let reg = Registry::open(dir.clone());
        assert!(
            reg.create_instance("w14live", 48_000.0, &[]).is_some(),
            "custom script must load: errors {:?}",
            reg.errors()
        );
        // Edit the script (content change; the old artifact stays MAPPED).
        std::fs::write(dir.join("w14live.rs"), format!("{tri}\n// w14 edit\n")).unwrap();
        let second = art(&dir.join("w14live.rs")).unwrap();
        assert_ne!(first, second, "an edit must build a FRESH artifact name");
        let reg = Registry::open(dir.clone());
        assert!(
            reg.create_instance("w14live", 48_000.0, &[]).is_some(),
            "edited script must rebuild and load: errors {:?}",
            reg.errors()
        );
        assert!(second.is_file(), "the fresh artifact must exist after rescan");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn w14_script_compile_shape_pinned() {
        // Source pins over the seams the W14 design depends on. Span-scoped
        // (comment-stripped, needles built by concatenation) so pin text
        // can never self-match (W12e/W6 vacuity traps).
        let src = include_str!("synth.rs");
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let between = |a: &str, b: &str| -> String {
            let i = code.find(a).unwrap_or_else(|| panic!("anchor {a} gone"));
            let j = code[i..].find(b).unwrap_or_else(|| panic!("end anchor {b} gone"));
            code[i..i + j].to_string()
        };
        let rescan = between("pub fn rescan", "fn stamped_artifact");
        let st_a = ["stamped_artifact(&build, &stem, &sr", "c"].concat();
        let nb = ["needs", "_build"].concat();
        assert!(rescan.contains(&st_a), "rescan must resolve via the stamp");
        assert!(!rescan.contains(&nb), "mtime-based rebuild is back");
        assert!(rescan.contains("if !lib.is_file()"), "artifact reuse is gone");
        assert!(rescan.contains("prune_stale(&build, &stem, &lib)"), "stale artifact prune is gone");
        let fallback = ["if !out.is", "_file()"].concat();
        for (start, end) in [
            ("fn compile_synth", "unsafe fn load_one"),
            ("fn compile_effect", "fn sanitize_crate_name"),
        ] {
            let s = if start == "fn compile_synth" {
                between(start, end)
            } else {
                let fxsrc = include_str!("fx.rs");
                let fxc: String = fxsrc
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let i = fxc.find(start).expect("fx compile gone");
                let j = fxc[i..].find(end).expect("fx end gone");
                fxc[i..i + j].to_string()
            };
            assert!(s.contains("if let Err(e) = std::fs::rename(&tmp, out)"), "{start}: rename must be fallible-checked");
            assert!(s.contains(&fallback), "{start}: exists-fallback gone (W14 concurrent-build)");
        }
    }
}
