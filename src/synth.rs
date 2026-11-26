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
    SynthAssetV1, SynthHostV1, SynthInfoV1, SynthInfoV2, SynthInfoV3, SynthParamV1,
    SLOOPER_MAX_SYNTH_PARAMS, SLOOPER_SYNTH_ABI, SLOOPER_SYNTH_ABI_V2, SLOOPER_SYNTH_ABI_V3,
};

pub const MAX_PARAMS: usize = SLOOPER_MAX_SYNTH_PARAMS;

/// Bundled scripts. `synth_api.rs` is rewritten (and version-pinned) on
/// every start; the synths themselves are seeded when missing or stale.
/// The repo copy under synths/ is the single source (v149): embedded from
/// there, so this text, the crate module (lib.rs `#[path]`) and the
/// author-facing file can never drift
/// (pin `v149_synth_api_folder_copy_is_the_single_source`).
pub const API_SRC: &str = include_str!("../synths/synth_api.rs");
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
const BUNDLED_VERSION: &str = "9";

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
    // The instance the host is currently INSIDE (set by the entry guards on
    // Instance). host_load_map reads it: the map is per track, and this is
    // the only way a bare fn-pointer host call can find "its" instance.
    static CUR_INST: std::cell::Cell<*const Instance> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// RAII: marks `inst` as the host-caller for as long as the guard lives
/// (nesting-safe: the previous value is restored on drop).
struct InstGuard(Option<*const Instance>);
impl InstGuard {
    fn enter(inst: &Instance) -> Self {
        let prev = CUR_INST.with(|c| c.replace(inst));
        InstGuard(if prev.is_null() { None } else { Some(prev) })
    }
}
impl Drop for InstGuard {
    fn drop(&mut self) {
        CUR_INST.with(|c| c.set(self.0.unwrap_or(std::ptr::null())));
    }
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

/// v164 (card t_0c73850c): host trampoline for the live N-EDO tuning —
/// lock-free read of the process-wide atomic (see [`crate::tune`]).
extern "C" fn host_tune_key(note: u8) -> f64 {
    crate::tune::tune_key(note)
}

/// The host vtable handed to every synth `create`.
pub static HOST: SynthHostV1 = SynthHostV1 {
    abi: SLOOPER_SYNTH_ABI,
    load_asset: host_load_asset,
    load_map: host_load_map,
    tune_key: host_tune_key,
};

/// v51: hard cap of one track's multi-sample key map (the GUI enforces it
/// too; the bundled sampler keeps a stack array of this size).
pub const MAX_MAP_SLOTS: usize = 64;
/// `mode` bit 31 of the host `load_map` reply: this track HAS a key map
/// (empty map + this bit = silence, without it = legacy params).
pub const MAP_EXISTS: u32 = 0x8000_0000;

/// Copy the calling instance's key map out to a script (see the ABI doc).
/// Slots are packed `(asset_id << 16) | (velocity << 8) | root_note`,
/// slot 0..n dense (v52 R2: the velocity byte enables per-slot VELOCITY
/// LAYERS on a shared root; 127 answers every velocity).
extern "C" fn host_load_map(slots: *mut u64, max: u32, mode: *mut u32) -> u32 {
    let inst = CUR_INST.with(|c| c.get());
    if inst.is_null() {
        if !mode.is_null() {
            unsafe { *mode = 0 };
        }
        return 0;
    }
    unsafe { (*inst).load_map_into(slots, max, mode) }
}

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
    /// v100 (card t_52aa65b1): pitch-bend callback if the script declares
    /// a SynthInfoV2 table (`slooper_synth_info_v2`). None = ABI v1 script:
    /// `Instance::bend` is a no-op, exactly the pre-v100 drop.
    bend: Option<extern "C" fn(s: *mut core::ffi::c_void, ch: u8, st: f32)>,
    /// v106 (card t_a4df161f): per-note aftertouch callback if the script
    /// declares a SynthInfoV3 table (`slooper_synth_info_v3`). None = ABI
    /// v1/v2 script: `Instance::poly_at` is a no-op, exactly the pre-v106
    /// documented drop.
    poly_at: Option<extern "C" fn(s: *mut core::ffi::c_void, ch: u8, note: u8, val: u8)>,
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
    /// v51: multi-sample key map (the bundled sampler's piano-roll state).
    /// `map[0..map_n]` holds packed `(asset_id << 16) | (velocity << 8) |
    /// root_note` (v52 R2: the velocity byte carries per-slot velocity
    /// layers); the GUI
    /// writes slots BEFORE bumping the count so a reader that sees the new
    /// count also sees the data. `map_mode` carries the mode in the low
    /// bits plus bit-31 "a map EXISTS" (mode 0 = hard cutoff, 1 =
    /// crossfade; exists+n==0 = deliberately silent, vs "no map" = the
    /// legacy single-asset params).
    map: [AtomicU64; MAX_MAP_SLOTS],
    map_n: AtomicU32,
    map_mode: AtomicU32,
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
            map: core::array::from_fn(|_| AtomicU64::new(u64::MAX)),
            map_n: AtomicU32::new(0),
            map_mode: AtomicU32::new(0),
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
        let fresh = Self::new(self.fns, self._lib.clone(), &desc, self.sample_rate, &vals)?;
        // v51: audition/preview respawns ride this path — the key map must
        // ride with them or the sampled map preview would play only params.
        let n = (self.map_n.load(Ordering::Acquire) as usize).min(MAX_MAP_SLOTS);
        let slots: Vec<(u32, u8, u8)> = (0..n)
            .map(|i| {
                let p = self.map[i].load(Ordering::Acquire);
                ((p >> 16) as u32, (p & 0xFF) as u8, ((p >> 8) & 0xFF) as u8)
            })
            .collect();
        fresh.set_map_raw(&slots, self.map_mode.load(Ordering::Relaxed));
        Some(fresh)
    }

    pub fn set_param(&self, k: usize, v: f32) {
        if k < self.n_params {
            self.params[k].store(v.to_bits(), Ordering::Relaxed);
            self.param_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// v51: publish this instance's multi-sample key map. Slots beyond
    /// `slots.len()` are erased. Calling this marks the instance as
    /// HAVING a map (even with zero slots = deliberately silent);
    /// instances that never had one fall back to the legacy single-asset
    /// params. The GUI thread owns the map; the audio thread only ever
    /// reads it (through `host_load_map`).
    pub fn set_map(&self, slots: &[(u32, u8, u8)], mode: u8) {
        self.set_map_raw(slots, (mode as u32) | MAP_EXISTS)
    }

    /// "No key map" — legacy params rule again (rebinding a fresh synth
    /// without a saved map lands here).
    pub fn clear_map(&self) {
        self.set_map_raw(&[], 0);
    }

    fn set_map_raw(&self, slots: &[(u32, u8, u8)], mode_bits: u32) {
        let n = slots.len().min(MAX_MAP_SLOTS);
        for (i, s) in slots.iter().take(n).enumerate() {
            self.map[i].store(
                ((s.0 as u64) << 16) | ((s.2 as u64) << 8) | s.1 as u64,
                Ordering::Release,
            );
        }
        for slot in self.map.iter().skip(n) {
            slot.store(u64::MAX, Ordering::Relaxed);
        }
        // Count LAST: a script that sees n never reads stale slots.
        self.map_n.store(n as u32, Ordering::Release);
        self.map_mode.store(mode_bits, Ordering::Relaxed);
    }

    /// Readback of the published map (pins + the map editor mirror).
    /// `mode` is the low-bits mode; `exists` = a map was published.
    pub fn map_snapshot(&self) -> (Vec<(u32, u8, u8)>, u8, bool) {
        let n = (self.map_n.load(Ordering::Acquire) as usize).min(MAX_MAP_SLOTS);
        let bits = self.map_mode.load(Ordering::Relaxed);
        let slots = (0..n)
            .map(|i| {
                let p = self.map[i].load(Ordering::Acquire);
                ((p >> 16) as u32, (p & 0xFF) as u8, ((p >> 8) & 0xFF) as u8)
            })
            .collect();
        (slots, (bits & 0xFF) as u8, bits & MAP_EXISTS != 0)
    }

    /// The `host.load_map` worker: copy the packed slots out for the script.
    unsafe fn load_map_into(&self, slots: *mut u64, max: u32, mode: *mut u32) -> u32 {
        let n = (self.map_n.load(Ordering::Acquire) as usize).min(MAX_MAP_SLOTS);
        let out = n.min(max as usize);
        for i in 0..out {
            *slots.add(i) = self.map[i].load(Ordering::Acquire);
        }
        if !mode.is_null() {
            *mode = self.map_mode.load(Ordering::Relaxed);
        }
        out as u32
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
        let _g = InstGuard::enter(self);
        (self.fns.note_on)(self.state, ch, note, vel)
    }
    pub fn note_off(&self, ch: u8, note: u8) {
        let _g = InstGuard::enter(self);
        (self.fns.note_off)(self.state, ch, note)
    }
    pub fn all_off(&self) {
        let _g = InstGuard::enter(self);
        (self.fns.all_off)(self.state)
    }
    pub fn cc(&self, ch: u8, num: u8, val: u8) {
        let _g = InstGuard::enter(self);
        (self.fns.cc)(self.state, ch, num, val)
    }
    pub fn press(&self, ch: u8, val: u8) {
        let _g = InstGuard::enter(self);
        (self.fns.press)(self.state, ch, val)
    }
    /// v100 (card t_52aa65b1): channel pitch bend in SEMITONES (roll space:
    /// a full 14-bit deflection = ±12 st, `midipat::BEND_FULL_ST`). Routed
    /// live/RT, offline-bounce and both audition paths. ABI v1 scripts have
    /// no callback — the call is a no-op for them (the historic drop).
    pub fn bend(&self, ch: u8, st: f32) {
        if let Some(f) = self.fns.bend {
            let _g = InstGuard::enter(self);
            f(self.state, ch, st)
        }
    }
    /// v106 (card t_a4df161f): PER-NOTE aftertouch (MPE note pressure),
    /// RAW MIDI byte 0..=127 (d2 of the 0xA0 message; d1 = note). Routed
    /// live/RT, offline-bounce and both audition paths, like every other
    /// callback. ABI v1/v2 scripts have no callback — the call is a no-op
    /// for them (the documented pre-v106 drop, decision (a) of v104).
    pub fn poly_at(&self, ch: u8, note: u8, val: u8) {
        if let Some(f) = self.fns.poly_at {
            let _g = InstGuard::enter(self);
            f(self.state, ch, note, val)
        }
    }

    /// Render `out.len()` samples (OUT IS ACCUMULATED INTO — the plugin
    /// adds to whatever is there, matching the ABI). `params` is a snapshot.
    ///
    /// # Safety
    /// Only ever called from one thread at a time for this instance.
    pub unsafe fn process_into(&self, out: &mut [f32], params: &[f32; MAX_PARAMS]) {
        let _g = InstGuard::enter(self);
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
    Ok(build.join(format!("{stem}-{stamp:016x}.{}", crate::paths::dylib_ext())))
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
            // v114: a spawn failure contradicts any cached "available" —
            // drop the latch so the next frame shows the W3 banner.
            crate::fx::rustc_probe_stale();
            format!(
                "could not run rustc: {e} — {}",
                crate::fx::rustc_install_hint()
            )
        })?;
    if !output.status.success() {
        // v114: the toolchain ran but may itself be broken/gone — refresh
        // availability so a vanished rustc stops being claimed.
        crate::fx::rustc_probe_stale();
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
    // v100 (card t_52aa65b1): OPTIONAL ABI v2 entry point — the fx ABI
    // ladder is the precedent (v2 scripts export BOTH symbols; the host
    // probes the newer one). A missing, null or mis-stamped v2 export
    // degrades silently to "no bend", i.e. the v1 behavior.
    let mut bend: Option<extern "C" fn(*mut core::ffi::c_void, u8, f32)> = None;
    if let Ok(sym2) = lib.get::<extern "C" fn() -> *const SynthInfoV2>(b"slooper_synth_info_v2\0") {
        let i2 = sym2();
        if !i2.is_null() {
            let d2 = unsafe { &*i2 };
            if d2.abi == SLOOPER_SYNTH_ABI_V2 && d2.bend as usize != 0 {
                bend = Some(d2.bend);
            }
        }
    }
    // v106 (card t_a4df161f): OPTIONAL ABI v3 entry point, probed LAST and
    // WINNING when valid (SynthInfoV3 is cumulative, so its bend restates
    // the v2 one). A missing, null or mis-stamped v3 export degrades
    // silently to the v2 result — exactly the v100 ladder discipline.
    let mut poly_at: Option<extern "C" fn(*mut core::ffi::c_void, u8, u8, u8)> = None;
    if let Ok(sym3) = lib.get::<extern "C" fn() -> *const SynthInfoV3>(b"slooper_synth_info_v3\0") {
        let i3 = sym3();
        if !i3.is_null() {
            let d3 = unsafe { &*i3 };
            if d3.abi == SLOOPER_SYNTH_ABI_V3 && d3.poly_at as usize != 0 {
                poly_at = Some(d3.poly_at);
                if d3.bend as usize != 0 {
                    bend = Some(d3.bend);
                }
            }
        }
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
        bend,
        poly_at,
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

    /// v57: a layer's velocity is its RECORDED dynamic = unity gain.
    /// Peak amplitude of one slot (root 60, vel 64) must scale with
    /// note_vel/64 (capped x4), and a vel-127 layer must keep the old
    /// note/127 curve exactly.
    #[test]
    fn v57_sampler_layer_vel_is_unity_gain_point() {
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let reg = test_registry("v57gain");
        let sine: Arc<[f32]> = (0..4800).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        let inst = reg
            .create_instance("sampler", 48_000.0, &[])
            .expect("sampler instance");
        let desc = reg.desc("sampler").unwrap();
        let mut p = [0.0f32; MAX_PARAMS];
        for (k, pd) in desc.params.iter().enumerate() {
            p[k] = pd.default;
        }
        // Unity means unity: take the output-trim param out of the way.
        if let Some(gi) = desc.params.iter().position(|pd| pd.label == "gain") {
            p[gi] = 1.0;
        }
        // One sync block, then (re)set the map and sync again.
        let mut out = vec![0.0f32; 2048];
        unsafe { inst.process_into(&mut out, &p) };
        inst.set_map(&[(0, 60, 64)], 0);
        unsafe { inst.process_into(&mut out, &p) };
        let peak_at = |vel: u8| -> f32 {
            inst.all_off();
            let mut drain = vec![0.0f32; 4096];
            unsafe { inst.process_into(&mut drain, &p) };
            inst.note_on(0, 60, vel);
            let mut buf = vec![0.0f32; 2400];
            unsafe { inst.process_into(&mut buf, &p) };
            buf.iter().fold(0.0f32, |m, x| m.max(x.abs()))
        };
        let p64 = peak_at(64); // unity by definition
        let p127 = peak_at(127);
        let p32 = peak_at(32);
        assert!(p64 > 1e-4, "layer at its own dynamic must sound (peak {p64})");
        // ABSOLUTE check: the 0.5-amplitude asset at its own recorded
        // dynamic must play at 0.5 (unity) — the old note/127 curve
        // played it at 0.25 (the reported defect).
        assert!(
            (p64 - 0.5).abs() < 0.04,
            "layer velocity must be the UNITY gain point: 0.5-asset at its\n \
             own dynamic must peak ~0.5, got {p64}"
        );
        let up = p127 / p64;
        let down = p32 / p64;
        assert!(
            (up - 2.0).abs() < 0.06,
            "note 127 over a vel-64 layer = x2 gain, got {up}"
        );
        assert!(
            (down - 0.5).abs() < 0.02,
            "note 32 over a vel-64 layer = x0.5, got {down}"
        );
        // Default (127) layers keep the historic note/127 curve exactly.
        inst.set_map(&[(0, 60, 127)], 0);
        unsafe { inst.process_into(&mut out, &p) };
        let r = peak_at(100) / peak_at(50);
        assert!(
            (r - 2.0).abs() < 0.06,
            "vel-127 layer must follow note/127 (old behavior), got {r}"
        );
        // Boost cap: vel-16 layer at note 127 would be x7.9 uncapped.
        inst.set_map(&[(0, 60, 16)], 0);
        unsafe { inst.process_into(&mut out, &p) };
        let cap = peak_at(127) / peak_at(16);
        assert!(
            (cap - 4.0).abs() < 0.12,
            "boost must cap at x4 (+12 dB), got {cap}"
        );
        inst.all_off();
    }

    /// Native-suite tier 2: EVERY bundled synth must compile, load, render
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
            assert!(
                tail < 1e-3,
                "{stem} all_off must stop new sound (tail {tail})"
            );
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
        assert!(
            second.is_file(),
            "the fresh artifact must exist after rescan"
        );
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
            let j = code[i..]
                .find(b)
                .unwrap_or_else(|| panic!("end anchor {b} gone"));
            code[i..i + j].to_string()
        };
        let rescan = between("pub fn rescan", "fn stamped_artifact");
        let st_a = ["stamped_artifact(&build, &stem, &sr", "c"].concat();
        let nb = ["needs", "_build"].concat();
        assert!(rescan.contains(&st_a), "rescan must resolve via the stamp");
        assert!(!rescan.contains(&nb), "mtime-based rebuild is back");
        assert!(
            rescan.contains("if !lib.is_file()"),
            "artifact reuse is gone"
        );
        assert!(
            rescan.contains("prune_stale(&build, &stem, &lib)"),
            "stale artifact prune is gone"
        );
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
            assert!(
                s.contains("if let Err(e) = std::fs::rename(&tmp, out)"),
                "{start}: rename must be fallible-checked"
            );
            assert!(
                s.contains(&fallback),
                "{start}: exists-fallback gone (W14 concurrent-build)"
            );
        }
    }

    // ===================== v51 acceptance pins (engine side) =====================
    //
    // Multi-sample key map: the host publishes per-instance slots
    // (asset id + root note) + a mode through SynthHostV1::load_map, and
    // the bundled sampler picks/weights/mixes them live. These pins render
    // the REAL script (compile + audio) — behavior specs, not mocks.

    /// Shared registry: the v51 pins all exercise the same bundled
    /// sampler, so they share one seed dir (built once per binary run —
    /// the compile cache keys on mtime, see needs_build).
    fn v51_registry() -> Registry {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let dir = std::env::temp_dir().join("sp-synth-v51");
        ONCE.call_once(|| {
            let _ = std::fs::remove_dir_all(&dir);
        });
        std::env::set_var("SLOOPER_SYNTHS_DIR", &dir);
        Registry::open(dir.clone())
    }

    /// Asset 0 = a 200 Hz sine, asset 1 = an 800 Hz sine (100 ms), both at
    /// the instance rate so a note at its root plays the raw frequency.
    fn v51_tones() {
        let mk = |f: f64| -> Arc<[f32]> {
            (0..4800)
                .map(|i| (i as f64 * std::f64::consts::TAU * f / 48000.0).sin() as f32 * 0.5)
                .collect()
        };
        set_host_assets(Arc::new(vec![mk(200.0), mk(800.0)]), 48_000);
    }

    /// One note through the instance; returns the render for Goertzel.
    fn v51_render(inst: &Arc<Instance>, note: u8) -> Vec<f32> {
        let mut ev = vec![(0u64, 3u8, 0u8, note, 110u8), (40_000, 0, 0, note, 0)];
        let mut out = vec![0f32; 8192];
        crate::audio::render_midi_events(inst, &mut ev, 0, out.len(), &mut out);
        out
    }

    /// The sampler instance with unity gain and no map (legacy params).
    fn v51_sampler(reg: &Registry) -> Arc<Instance> {
        reg.create_instance("sampler", 48_000.0, &[0.0, 48.0, 1.0])
            .expect("sampler instance")
    }

    #[test]
    fn v51_sampler_map_cutoff_nearest_and_tie_down() {
        // AC3+AC4: roots 48 (200 Hz) and 54 (800 Hz). Closest wins; the
        // exact midpoint (51) plays the LOWER sample (round down); notes
        // outside the range play the extreme sample, never wrap.
        let reg = v51_registry();
        v51_tones();
        let inst = v51_sampler(&reg);
        inst.set_map(&[(0, 48, 127), (1, 54, 127)], 0);
        assert_eq!(inst.map_snapshot(), (vec![(0u32, 48u8, 127u8), (1, 54, 127)], 0, true));
        // A mapped sample plays at its ROOT pitch when the note equals the
        // root, so each case measures the TRANSPOSED frequency.
        let fsh =
            |base: f64, note: i32, root: i32| base * (2.0f64).powf((note - root) as f64 / 12.0);
        let dom = |note: u8, want: f64, off: f64| {
            let x = v51_render(&inst, note);
            let a = tone_power(&x, want, 48000.0, 512, 7680);
            let b = tone_power(&x, off, 48000.0, 512, 7680);
            assert!(
                a > 25.0 * b,
                "note {note}: wanted {want:.1} Hz (p {a}), off-freq {off:.1} leaked (p {b})"
            );
        };
        let n = |x: u8| x as i32;
        dom(48, fsh(200.0, n(48), 48), fsh(800.0, n(48), 54)); // at the low root
        dom(50, fsh(200.0, n(50), 48), fsh(800.0, n(50), 54)); // 2 vs 4 -> lower
        dom(52, fsh(800.0, n(52), 54), fsh(200.0, n(52), 48)); // 4 vs 2 -> upper
        dom(51, fsh(200.0, n(51), 48), fsh(800.0, n(51), 54)); // midpoint -> LOWER
        dom(40, fsh(200.0, n(40), 48), fsh(800.0, n(40), 54)); // below range
        dom(60, fsh(800.0, n(60), 54), fsh(200.0, n(60), 48)); // above range
    }

    #[test]
    fn v51_sampler_map_cutoff_equal_roots_first_wins() {
        // Two slots on the SAME root: the first-listed one wins
        // (deterministic, documented in the script).
        let reg = v51_registry();
        v51_tones();
        let inst = v51_sampler(&reg);
        inst.set_map(&[(0, 48, 127), (1, 48, 127)], 0);
        let x = v51_render(&inst, 48);
        let a = tone_power(&x, 200.0, 48000.0, 512, 7680);
        let b = tone_power(&x, 800.0, 48000.0, 512, 7680);
        assert!(a > 25.0 * b, "first slot must win a root tie: {a} vs {b}");
    }

    #[test]
    fn v51_sampler_map_crossfade_inverse_distance_mix() {
        // AC5 (the user's exact example): samples on A2 (45) and E3 (52),
        // note C3 (48) = 3 st above A2 and 4 below E3 -> A2 gets 4/7 of
        // the volume, E3 gets 3/7. Both are BENT to the note first:
        // A2's 200 Hz plays at 200*2^(3/12), E3's 800 Hz at 800*2^(-4/12).
        let reg = v51_registry();
        v51_tones();
        let inst = v51_sampler(&reg);
        inst.set_map(&[(0, 45, 127), (1, 52, 127)], 1);
        let x = v51_render(&inst, 48);
        let f_lo = 200.0 * (2.0f64).powf(3.0 / 12.0);
        let f_hi = 800.0 * (2.0f64).powf(-4.0 / 12.0);
        // Measure while BOTH shifted reads still sound (the +3 st sample
        // runs out first, at ~4030 frames) — equal window length for both,
        // or power would not be proportional to amplitude squared.
        let plo = line_power(&x, f_lo, 48000.0, 512, 3800);
        let phi = line_power(&x, f_hi, 48000.0, 512, 3800);
        assert!(
            plo > 5e4 && phi > 5e4,
            "both samples must sound: {plo} {phi}"
        );
        // Amplitude ratio 4/7 : 3/7 -> power ratio 16:9 = 1.78.
        let r = plo / phi;
        assert!(
            (r - 16.0 / 9.0).abs() < 0.5,
            "crossfade mix ratio off: power(lo/hi) {r}, wanted ~1.78 (4/7 vs 3/7)"
        );
    }

    #[test]
    fn v51_sampler_map_crossfade_exact_root_and_out_of_range() {
        // A note exactly on a root plays ONLY that sample; a note outside
        // the map range plays the single extreme sample (no phantom other
        // side to blend with).
        let reg = v51_registry();
        v51_tones();
        let inst = v51_sampler(&reg);
        inst.set_map(&[(0, 45, 127), (1, 52, 127)], 1);
        let x = v51_render(&inst, 45); // exact low root
        let a = tone_power(&x, 200.0, 48000.0, 512, 7680);
        let b = tone_power(&x, 800.0 * (2.0f64).powf(-7.0 / 12.0), 48000.0, 512, 7680);
        assert!(
            a > 25.0 * b,
            "exact root must silence the other side: {a} vs {b}"
        );
        let x = v51_render(&inst, 40); // below range -> lowest only
        let a = tone_power(&x, 200.0 * (2.0f64).powf(-5.0 / 12.0), 48000.0, 512, 7680);
        let b = tone_power(&x, 800.0 * (2.0f64).powf(-12.0 / 12.0), 48000.0, 512, 7680);
        assert!(
            a > 25.0 * b,
            "below range must play the lowest sample only: {a} vs {b}"
        );
    }

    #[test]
    fn v51_sampler_map_empty_falls_back_legacy_params_then_clear_is_silent() {
        // No map published -> legacy asset/root params rule (unchanged
        // v0.8/v0.10 behavior). After a map is published and then emptied,
        // the track is deliberately SILENT, not ghosted onto the params.
        let reg = v51_registry();
        v51_tones();
        let inst = v51_sampler(&reg); // params: asset 0, root 48
        let x = v51_render(&inst, 60); // +12 st -> 200 Hz at 400 Hz
        let a = tone_power(&x, 400.0, 48000.0, 512, 7680);
        let b = tone_power(&x, 800.0, 48000.0, 512, 7680);
        assert!(
            a > 25.0 * b,
            "legacy params must rule with no map: {a} vs {b}"
        );
        assert!(!inst.map_snapshot().2, "no map yet -> exists=false");

        inst.set_map(&[(1, 60, 127)], 0); // map up -> params ignored
        let x = v51_render(&inst, 60);
        let a = tone_power(&x, 800.0, 48000.0, 512, 7680);
        let b = tone_power(&x, 400.0, 48000.0, 512, 7680);
        assert!(a > 25.0 * b, "map must override the params: {a} vs {b}");

        inst.set_map(&[], 0); // map exists, zero slots -> silence
        let x = v51_render(&inst, 60);
        let peak = x.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak < 1e-5, "cleared map must be silent, peak {peak}");
    }

    #[test]
    fn v51_sampler_map_live_edit_and_spawn_fresh_carries() {
        // AC7: a map edit lands on the SAME running instance (no rebind),
        // and a spawn_fresh (the audition path) carries the map+mode.
        let reg = v51_registry();
        v51_tones();
        let inst = v51_sampler(&reg);
        inst.set_map(&[(0, 48, 127)], 0);
        let x = v51_render(&inst, 48);
        assert!(
            tone_power(&x, 200.0, 48000.0, 512, 7680)
                > 25.0 * tone_power(&x, 800.0, 48000.0, 512, 7680),
            "first map must sound"
        );
        inst.set_map(&[(1, 48, 127)], 0); // swap sample on the fly
        let x = v51_render(&inst, 48);
        assert!(
            tone_power(&x, 800.0, 48000.0, 512, 7680)
                > 25.0 * tone_power(&x, 200.0, 48000.0, 512, 7680),
            "live map edit must change the NEXT render with no rebind"
        );
        // Spawn a fresh copy (audition/offline path) — map + mode ride.
        inst.set_map(&[(0, 45, 127), (1, 52, 127)], 1);
        let fresh = inst.spawn_fresh().expect("spawn_fresh");
        assert_eq!(
            fresh.map_snapshot(),
            (vec![(0u32, 45u8, 127u8), (1, 52, 127)], 1, true),
            "spawn_fresh must carry the map + crossfade mode"
        );
        let x = v51_render(&fresh, 48);
        let plo = line_power(&x, 200.0 * (2.0f64).powf(3.0 / 12.0), 48000.0, 512, 3800);
        let phi = line_power(&x, 800.0 * (2.0f64).powf(-4.0 / 12.0), 48000.0, 512, 3800);
        assert!(
            plo > 5e4 && phi > 5e4,
            "the copy must crossfade like the original"
        );
    }

    // ---- v52 R2: velocity LAYERS (several slots on one root) ----

    /// One note at a GIVEN velocity (v51_render is fixed at 110).
    fn v52_render(inst: &Arc<Instance>, note: u8, vel: u8) -> Vec<f32> {
        let mut ev = vec![(0u64, 3u8, 0u8, note, vel), (40_000, 0, 0, note, 0)];
        let mut out = vec![0f32; 8192];
        crate::audio::render_midi_events(inst, &mut ev, 0, out.len(), &mut out);
        out
    }

    #[test]
    fn v52_sampler_velocity_layers_cutoff_switches() {
        // Two layers on ONE root: asset 0 (200 Hz) answers at v<=40,
        // asset 1 (800 Hz) at v<=100. Cutoff SWITCHES on the velocity
        // axis; a note under every layer falls back to the lowest.
        let reg = v51_registry();
        v51_tones();
        let inst = v51_sampler(&reg);
        inst.set_map(&[(1, 48, 100), (0, 48, 40)], 0); // unsorted on purpose
        let dom = |x: &[f32], want: f64, off: f64| {
            let a = tone_power(x, want, 48000.0, 512, 7680);
            let b = tone_power(x, off, 48000.0, 512, 7680);
            assert!(a > 25.0 * b, "wanted {want:.0} Hz: {a} vs {b}");
        };
        dom(&v52_render(&inst, 48, 30), 200.0, 800.0); // under both -> lowest
        dom(&v52_render(&inst, 48, 60), 200.0, 800.0); // highest floor 40
        dom(&v52_render(&inst, 48, 110), 800.0, 200.0); // highest floor 100
    }

    #[test]
    fn v52_sampler_velocity_layers_crossfade_blends() {
        // Layers 40 and 100 on one root: note v=70 sits EXACTLY halfway,
        // so both layers sound with equal BLEND weight (pitch axis
        // untouched — exact root hit). Note v=20 is under every layer ->
        // lowest only.
        // v57 retarget: equal blend weight no longer means equal POWER —
        // each layer plays at its recorded dynamic as unity, so the note
        // over the vel-40 layer (200 Hz) is boosted 70/40 = 1.75x and the
        // one over vel-100 (800 Hz) cut to 70/100 = 0.7x: amplitude ratio
        // 2.5, POWER ratio 6.25. (Was: r ~ 1.0 under note/127 gain.)
        let reg = v51_registry();
        v51_tones();
        let inst = v51_sampler(&reg);
        inst.set_map(&[(0, 48, 40), (1, 48, 100)], 1);
        let x = v52_render(&inst, 48, 70);
        let p0 = tone_power(&x, 200.0, 48000.0, 512, 3800);
        let p1 = tone_power(&x, 800.0, 48000.0, 512, 3800);
        assert!(p0 > 5e4 && p1 > 5e4, "both layers must sound: {p0} {p1}");
        let r = p0 / p1;
        assert!(
            (r - 6.25).abs() < 1.2,
            "v57 unity-reference blend: power ratio ~6.25, got {r}"
        );
        let x = v52_render(&inst, 48, 20);
        let p0 = tone_power(&x, 200.0, 48000.0, 512, 7680);
        let p1 = tone_power(&x, 800.0, 48000.0, 512, 7680);
        assert!(p0 > 25.0 * p1, "under all layers -> lowest only: {p0} {p1}");
    }

    // ---- v100 (card t_52aa65b1): scripted synths must HEAR glide bends ----
    // The glide lane bakes 14-bit pitch bends (kind 5, lsb/msb) that the
    // engine used to drop at both scripted dispatch arms. These pins drive
    // the REAL scripts through the event protocol only — a flat render vs a
    // 0 -> +12 st baked ramp — and measure a zero-crossing (pitch) proxy.

    /// Positive-going zero crossings = whole cycles (pitch proxy).
    fn v100_cycles(x: &[f32]) -> usize {
        x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count()
    }

    /// Render a held note under a bend ramp: `st_at(frac)` semitones at the
    /// ramp fraction, baked dense (every 10 ms) exactly like `bake_seg`
    /// (step 8 ticks at 120 BPM) and encoded via the editor's st_to_bend.
    fn v100_glide_render(inst: &std::sync::Arc<Instance>, note: u8, st_at: &dyn Fn(f32) -> f32) -> Vec<f32> {
        let mut ev: Vec<(u64, u8, u8, u8, u8)> = vec![(0u64, 3u8, 0u8, note, 110u8)];
        for i in 0..=100 {
            let frac = i as f32 / 100.0;
            let raw = crate::midipat::st_to_bend(st_at(frac));
            ev.push((
                (960 + i * 480) as u64,
                5u8,
                0u8,
                (raw & 0x7F) as u8,
                ((raw >> 7) & 0x7F) as u8,
            ));
        }
        ev.push((47_000, 0, 0, note, 0));
        let mut out = vec![0f32; 48_000];
        crate::audio::render_midi_events(inst, &mut ev, 0, out.len(), &mut out);
        out
    }

    #[test]
    fn v100_baked_glide_audible_on_scripted_sampler() {
        if !crate::fx::rustc_available() {
            return; // no compiler on this machine: nothing to exercise (W3 rule)
        }
        let reg = v51_registry();
        // Asset 0: 2 s of 200 Hz sine at half amplitude — long enough that
        // the glide window lands inside a SUSTAINED sample playback.
        let sine: Arc<[f32]> = (0..96_000)
            .map(|i| (i as f64 * std::f64::consts::TAU * 200.0 / 48_000.0).sin() as f32 * 0.5)
            .collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        let inst = reg
            .create_instance("sampler", 48_000.0, &[0.0, 48.0, 1.0])
            .expect("sampler instance (legacy params: asset 0, root 48, gain 1)");
        let flat = v100_glide_render(&inst, 48, &|_| 0.0);
        let up = v100_glide_render(&inst, 48, &|f| 12.0 * f);
        assert_ne!(flat, up, "baked bends never reached the scripted sampler (kind 5 dropped)");
        let late = |x: &[f32]| v100_cycles(&x[38_400..48_000]) as f64;
        let (cf, cu) = (late(&flat), late(&up));
        assert!(cf >= 30.0, "flat window must carry audible cycles: {cf}");
        let r = cu / cf;
        // Ramp ends at +12 st; the late window averages ~+10.8 st -> x1.87.
        assert!(
            r > 1.55 && r < 2.4,
            "baked 0->12 st glide must near-double the late-window pitch \
             (expect ~1.87): got {r:.3} (flat {cf} -> up {cu})"
        );
    }

    #[test]
    fn v100_baked_glide_audible_on_bundled_piano() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let inst = reg.create_instance("piano", 48_000.0, &[]).expect("bundled piano instance");
        let flat = v100_glide_render(&inst, 57, &|_| 0.0);
        let up = v100_glide_render(&inst, 57, &|f| 12.0 * f);
        assert_ne!(flat, up, "baked bends never reached the bundled piano (kind 5 dropped)");
        // Measure DURING the sample's audible life (zones do not loop: the
        // note is silent after ~1-2 s) and late enough into the ramp that a
        // flat render cannot pass: empirically ~1.6x here, vs ~1.25x in an
        // early window (mid-ramp physics, not a bug) and 1.0x in silence.
        let win = |x: &[f32]| v100_cycles(&x[19_200..28_800]) as f64;
        let (cf, cu) = (win(&flat), win(&up));
        assert!(cf >= 20.0, "piano must still sound in the measure window: {cf}");
        let r = cu / cf;
        assert!(
            r > 1.3 && r < 2.8,
            "glide must audibly raise the bundled piano's pitch: got {r:.3} \
             (flat {cf} -> up {cu})"
        );
    }

    // v101 (pb-2): THE render A/B pin the pb-1 spike promised — a
    // per-note PITCH CURVE (control points, not a hand-fed sweep) must
    // drive the scripted render through the REAL v167 carrier: the
    // sample-accurate `pattern_curve_events` generator → kind-7 events →
    // ABI bend(). Both polarities: curve ON vs no curve (flat control
    // with identical timing), plus shape proof: the curve bows UP to
    // +12 st mid-note and back DOWN by the end — a linear-sweep
    // regression would raise the LATE window, this must raise the MID
    // one and drop below flat at the end.

    fn v101_curve_render(reg: &crate::synth::Registry, synth: &str, note: u8, curved: bool) -> Vec<f32> {
        let mut pat = crate::midipat::Pattern::new(120);
        if curved {
            pat.note_bends.push(crate::midipat::NoteBend::new(vec![
                (0.0, -1200.0),
                (0.5, 1200.0),
                (1.0, -1200.0),
            ]));
        }
        pat.notes.push(crate::midipat::Note {
            ch: 0,
            note,
            vel: 110,
            press: 0,
            start: 0,
            len: 960,
            bend: if curved { Some(0) } else { None },
        });
        pat.bake_bends();
        // v167 (pbc-1): the audible lane NEVER carries owned curves —
        // the real carrier is the sample-accurate kind-7 generator used
        // below (the v167_curve_* pins assert the carrier law itself).
        assert!(pat.bends.is_empty(), "v167: lane excludes owned curves");
        assert_eq!(pat.curve_regions().is_empty(), !curved, "curve owns its note window");
        let inst = reg
            .create_instance(synth, 48_000.0, if synth == "sampler" { &[0.0, 48.0, 1.0] } else { &[] })
            .expect("scripted instance");
        let mut ev: Vec<(u64, u8, u8, u8, u8)> = vec![(0u64, 3u8, 0u8, note, 110u8)];
        if curved {
            ev.extend(crate::audio::pattern_curve_events(&pat, 48_000));
        }
        ev.push((47_000, 0, 0, note, 0));
        let mut out = vec![0f32; 48_000];
        crate::audio::render_midi_events(&inst, &mut ev, 0, out.len(), &mut out);
        out
    }

    #[test]
    fn v101_pitch_curve_drives_scripted_sampler_render() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let sine: Arc<[f32]> = (0..96_000)
            .map(|i| (i as f64 * std::f64::consts::TAU * 200.0 / 48_000.0).sin() as f32 * 0.5)
            .collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        let flat = v101_curve_render(&reg, "sampler", 48, false);
        let curve = v101_curve_render(&reg, "sampler", 48, true);
        assert_ne!(flat, curve, "per-note pitch curve never reached the scripted sampler render");
        let win = |x: &[f32], a: usize, b: usize| v100_cycles(&x[a..b]) as f64;
        // MID window (t .40-.60): curve peaks at +12 st there -> ~2x flat.
        let (mf, mc) = (win(&flat, 19_200, 28_800), win(&curve, 19_200, 28_800));
        // END window (t .80-1.0): curve is back at -12 st -> ~0.5x flat.
        let (ef, ec) = (win(&flat, 38_400, 48_000), win(&curve, 38_400, 48_000));
        eprintln!("v101 A/B sampler: mid {mf} -> {mc} ({:.3}x), end {ef} -> {ec} ({:.3}x)", mc / mf, ec / ef);
        assert!(mf >= 30.0, "flat control must carry cycles: {mf}");
        assert!(mc / mf > 1.5, "MID must climb ~2x: got {:.3}", mc / mf);
        // MEASURED ladder: end window averages 0.725x (window mean of a
        // down-ramp, not the endpoint math) — flat regression = 1.0.
        assert!(ec / ef < 0.80, "END must fall below flat (~0.72x measured): got {:.3}", ec / ef);
    }

    #[test]
    fn v101_pitch_curve_shape_not_a_linear_sweep() {
        // Same harness, the SHAPE polarity: a plain 0->+12 linear sweep
        // raises the late window; our up-and-down curve must raise MID
        // AND pull the end BELOW flat. A bake that ignored the middle
        // control point (e.g. first/last-only linear) fails this.
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let sine: Arc<[f32]> = (0..96_000)
            .map(|i| (i as f64 * std::f64::consts::TAU * 200.0 / 48_000.0).sin() as f32 * 0.5)
            .collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        let curve = v101_curve_render(&reg, "sampler", 48, true);
        let win = |x: &[f32], a: usize, b: usize| v100_cycles(&x[a..b]) as f64;
        let (mc, ec) = (win(&curve, 19_200, 28_800), win(&curve, 38_400, 48_000));
        eprintln!("v101 shape sampler: mid {mc} end {ec} ratio {:.3}", ec / mc);
        assert!(ec / mc < 0.45, "END must fall well below MID (dome shape): got {:.3}", ec / mc);
    }

    #[test]
    fn v101_pitch_curve_drives_bundled_piano_render() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let flat = v101_curve_render(&reg, "piano", 57, false);
        let curve = v101_curve_render(&reg, "piano", 57, true);
        assert_ne!(flat, curve, "pitch curve never reached the bundled piano render");
        let win = |x: &[f32], a: usize, b: usize| v100_cycles(&x[a..b]) as f64;
        let (mf, mc) = (win(&flat, 19_200, 28_800), win(&curve, 19_200, 28_800));
        eprintln!("v101 A/B piano: mid {mf} -> {mc} ({:.3}x)", mc / mf);
        assert!(mf >= 20.0, "piano must sound in the measure window: {mf}");
        assert!(mc / mf > 1.3, "curve must raise the piano's mid-window pitch: got {:.3}", mc / mf);
    }

    #[test]
    fn v100_all_bundled_synths_load() {
        // The ABI v2 bump must not reject ANY bundled script.
        let reg = v51_registry();
        for stem in ["additive", "drums", "fm", "granular", "pad", "piano", "pluck", "sampler", "tri_osc"] {
            assert!(reg.desc(stem).is_some(), "bundled synth {stem} failed to load: {:?}", reg.desc(stem));
        }
    }

    /// Third-party scripts compiled against synth_api.rs ABI 1 must keep
    /// loading (append-only doctrine, same rule as the fx ABI ladder), and
    /// an unknown ABI must still be REJECTED.
    #[test]
    #[ignore = "slow-tier: compiles two scripts with rustc (~20 s)"]
    fn v100_legacy_abi1_script_still_loads_and_bogus_rejected() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry(); // seeds bundled + writes synth_api.rs
        std::fs::write(
            reg.dir().join("v100_legacy_v1.rs"),
            r#"
include!("synth_api.rs");
extern "C" fn cr(_sr: f64, _h: *const SynthHostV1) -> *mut core::ffi::c_void {
    core::ptr::NonNull::dangling().as_ptr()
}
extern "C" fn de(_s: *mut core::ffi::c_void) {}
extern "C" fn no(_s: *mut core::ffi::c_void, _c: u8, _n: u8, _v: u8) {}
extern "C" fn nf(_s: *mut core::ffi::c_void, _c: u8, _n: u8) {}
extern "C" fn ao(_s: *mut core::ffi::c_void) {}
extern "C" fn cc(_s: *mut core::ffi::c_void, _c: u8, _n: u8, _v: u8) {}
extern "C" fn pr(_s: *mut core::ffi::c_void, _c: u8, _v: u8) {}
extern "C" fn po(_s: *mut core::ffi::c_void, _o: *mut f32, _n: usize, _p: *const f32) {}
pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: b"legacy-one\0".as_ptr() as *const core::ffi::c_char,
    author: core::ptr::null(),
    description: core::ptr::null(),
    n_params: 0,
    params: core::ptr::null(),
    create: cr, destroy: de, note_on: no, note_off: nf, all_off: ao,
    cc: cc, press: pr, process: po,
};
#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 {
    &DESC
}
"#,
        )
        .unwrap();
        std::fs::write(
            reg.dir().join("v100_bogus_abi.rs"),
            r#"
include!("synth_api.rs");
extern "C" fn cr(_sr: f64, _h: *const SynthHostV1) -> *mut core::ffi::c_void {
    core::ptr::NonNull::dangling().as_ptr()
}
extern "C" fn de(_s: *mut core::ffi::c_void) {}
extern "C" fn no(_s: *mut core::ffi::c_void, _c: u8, _n: u8, _v: u8) {}
extern "C" fn nf(_s: *mut core::ffi::c_void, _c: u8, _n: u8) {}
extern "C" fn ao(_s: *mut core::ffi::c_void) {}
extern "C" fn cc(_s: *mut core::ffi::c_void, _c: u8, _n: u8, _v: u8) {}
extern "C" fn pr(_s: *mut core::ffi::c_void, _c: u8, _v: u8) {}
extern "C" fn po(_s: *mut core::ffi::c_void, _o: *mut f32, _n: usize, _p: *const f32) {}
pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: 99,
    name: b"bogus-abi\0".as_ptr() as *const core::ffi::c_char,
    author: core::ptr::null(),
    description: core::ptr::null(),
    n_params: 0,
    params: core::ptr::null(),
    create: cr, destroy: de, note_on: no, note_off: nf, all_off: ao,
    cc: cc, press: pr, process: po,
};
#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 {
    &DESC
}
"#,
        )
        .unwrap();
        let reg = Registry::open(reg.dir().to_path_buf());
        assert!(reg.desc("v100_legacy_v1").is_some(), "ABI 1 script must still load after the v2 bump");
        assert!(reg.create_instance("v100_legacy_v1", 48_000.0, &[]).is_some());
        assert!(reg.desc("v100_bogus_abi").is_none(), "ABI 99 must be rejected");
        assert!(reg.create_instance("v100_bogus_abi", 48_000.0, &[]).is_none());
    }

    // ---- v106 (card t_a4df161f): poly aftertouch must REACH scripted
    // synths. The five kind-4 arms used to be a documented drop (decision
    // (a) of v104 shipped only the hosting half). ABI v3 (SynthInfoV3 +
    // slooper_synth_info_v3) adds poly_at(s, ch, note, val-raw-0..=127);
    // the ladder must degrade for v1/v2 and for a mis-stamped v3.

    /// One staged render of the probe scripts: optional poly-AT event at
    /// offset 960, window measured 4800..19200 (probe tone = 200 Hz).
    fn v106_probe_render(
        inst: &std::sync::Arc<Instance>,
        kind4: Option<(u8, u8, u8)>,
        kind5: Option<u16>,
    ) -> f64 {
        let mut ev: Vec<(u64, u8, u8, u8, u8)> = vec![(0u64, 3, 0, 48, 110)];
        if let Some((ch, note, val)) = kind4 {
            ev.push((960, 4, ch, note, val));
        }
        if let Some(raw) = kind5 {
            ev.push((960, 5, 0, (raw & 0x7F) as u8, ((raw >> 7) & 0x7F) as u8));
        }
        ev.push((47_000, 0, 0, 48, 0));
        let mut out = vec![0f32; 19_200];
        crate::audio::render_midi_events(inst, &mut ev, 0, out.len(), &mut out);
        tone_power(&out, 200.0, 48_000.0, 4_800, 19_200)
    }

    /// Shared boilerplate of the two probe scripts (v1 table + one-voice
    /// 200 Hz tone gated by `amp`; each script supplies `callbacks`).
    const V106_PROBE_HEAD: &str = r#"
include!("synth_api.rs");
use core::ffi::c_void;
struct P { sr: f64, ph: f64, on: bool, amp: f64 }
extern "C" fn cr(sr: f64, _h: *const SynthHostV1) -> *mut c_void {
    Box::into_raw(Box::new(P { sr, ph: 0.0, on: false, amp: 0.0 })) as *mut c_void
}
extern "C" fn de(s: *mut c_void) {
    if !s.is_null() { drop(unsafe { Box::from_raw(s as *mut P) }); }
}
extern "C" fn no(s: *mut c_void, _c: u8, _n: u8, v: u8) {
    let p = unsafe { &mut *(s as *mut P) };
    p.on = v > 0;
    p.amp = 0.0;
}
extern "C" fn nf(s: *mut c_void, _c: u8, _n: u8) { unsafe { &mut *(s as *mut P) }.on = false; }
extern "C" fn ao(s: *mut c_void) { unsafe { &mut *(s as *mut P) }.on = false; }
extern "C" fn cc(_s: *mut c_void, _c: u8, _n: u8, _v: u8) {}
extern "C" fn pr(_s: *mut c_void, _c: u8, _v: u8) {}
"#;

    /// Targeting proof script: valid cumulative v3. poly_at answers ONLY
    /// the exact (ch=1, note=61) live voice with amp = val/127 — so the
    /// render proves channel, note AND pressure byte independently.
    const V106_PROBE_V3: &str = r#"
extern "C" fn bd(_s: *mut c_void, _c: u8, _st: f32) {}
extern "C" fn pa(s: *mut c_void, ch: u8, note: u8, val: u8) {
    let p = unsafe { &mut *(s as *mut P) };
    p.amp = if p.on && ch == 1 && note == 61 { val as f64 / 127.0 } else { 0.0 };
}
extern "C" fn po(s: *mut c_void, out: *mut f32, n: usize, _p: *const f32) {
    let p = unsafe { &mut *(s as *mut P) };
    let out = unsafe { core::slice::from_raw_parts_mut(out, n) };
    let step = (std::f64::consts::TAU * 200.0 / p.sr) as f64;
    for o in out.iter_mut() {
        *o += (p.ph.sin() * 0.5 * p.amp) as f32;
        p.ph += step;
        if p.ph > std::f64::consts::TAU { p.ph -= std::f64::consts::TAU; }
    }
}
pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: b"v106-probe-v3\0".as_ptr() as *const core::ffi::c_char,
    author: core::ptr::null(),
    description: core::ptr::null(),
    n_params: 0, params: core::ptr::null(),
    create: cr, destroy: de, note_on: no, note_off: nf, all_off: ao,
    cc: cc, press: pr, process: po,
};
#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 { &DESC }
static DESC_V3: SynthInfoV3 = SynthInfoV3 { abi: SLOOPER_SYNTH_ABI_V3, bend: bd, poly_at: pa };
#[no_mangle]
pub extern "C" fn slooper_synth_info_v3() -> *const SynthInfoV3 { &DESC_V3 }
"#;

    /// Ladder-proof script: a VALID v2 table (bend lifts amp to 0.5) plus
    /// a MIS-STAMPED v3 table (abi: 77 — poly_at would lift amp to 1.0).
    /// The host must trust the stamp: poly_at stays unwired, bend works.
    const V106_PROBE_BOGUS3: &str = r#"
extern "C" fn bd(s: *mut c_void, _c: u8, st: f32) {
    let p = unsafe { &mut *(s as *mut P) };
    if st > 0.1 { p.amp = 0.5; }
}
extern "C" fn pa(s: *mut c_void, _c: u8, _n: u8, _v: u8) {
    unsafe { &mut *(s as *mut P) }.amp = 1.0;
}
extern "C" fn po(s: *mut c_void, out: *mut f32, n: usize, _p: *const f32) {
    let p = unsafe { &mut *(s as *mut P) };
    let out = unsafe { core::slice::from_raw_parts_mut(out, n) };
    let step = (std::f64::consts::TAU * 200.0 / p.sr) as f64;
    for o in out.iter_mut() {
        *o += (p.ph.sin() * 0.5 * p.amp) as f32;
        p.ph += step;
        if p.ph > std::f64::consts::TAU { p.ph -= std::f64::consts::TAU; }
    }
}
pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: b"v106-bogus3\0".as_ptr() as *const core::ffi::c_char,
    author: core::ptr::null(),
    description: core::ptr::null(),
    n_params: 0, params: core::ptr::null(),
    create: cr, destroy: de, note_on: no, note_off: nf, all_off: ao,
    cc: cc, press: pr, process: po,
};
#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 { &DESC }
static DESC_V2: SynthInfoV2 = SynthInfoV2 { abi: SLOOPER_SYNTH_ABI_V2, bend: bd };
#[no_mangle]
pub extern "C" fn slooper_synth_info_v2() -> *const SynthInfoV2 { &DESC_V2 }
static DESC_V3: SynthInfoV3 = SynthInfoV3 { abi: 77, bend: bd, poly_at: pa };
#[no_mangle]
pub extern "C" fn slooper_synth_info_v3() -> *const SynthInfoV3 { &DESC_V3 }
"#;

    /// Channel, note and the raw pressure byte must all arrive intact at
    /// a valid v3 script; a mis-stamped v3 must degrade to v2 (bend on,
    /// poly_at off) — the v100 ladder discipline one rung higher.
    #[test]
    #[ignore = "slow-tier: compiles two probe scripts with rustc (~20 s)"]
    fn v106_abi_v3_ladder_guards() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        std::fs::write(
            reg.dir().join("v106_probe_v3.rs"),
            format!("{V106_PROBE_HEAD}{V106_PROBE_V3}"),
        )
        .unwrap();
        std::fs::write(
            reg.dir().join("v106_bogus3.rs"),
            format!("{V106_PROBE_HEAD}{V106_PROBE_BOGUS3}"),
        )
        .unwrap();
        let reg = Registry::open(reg.dir().to_path_buf());

        // -- valid v3: the callback must observe ch/note/val.
        let a = reg
            .create_instance("v106_probe_v3", 48_000.0, &[])
            .expect("probe v3 instance");
        let full = v106_probe_render(&a, Some((1, 61, 127)), None);
        let half = v106_probe_render(&a, Some((1, 61, 64)), None);
        let wrong_note = v106_probe_render(&a, Some((1, 62, 127)), None);
        let wrong_ch = v106_probe_render(&a, Some((2, 61, 127)), None);
        let none = v106_probe_render(&a, None, None);
        eprintln!(
            "v106 targeting: full {full:.1} half {half:.1} wrong-note {wrong_note:.1} \
             wrong-ch {wrong_ch:.1} none {none:.1}"
        );
        assert!(full > 1e3, "correctly-targeted poly-AT must open the tone: {full}");
        assert!(none < 10.0, "no AT must stay silent: {none}");
        assert!(wrong_note < full / 100.0, "wrong note must NOT open the tone: {wrong_note}");
        assert!(wrong_ch < full / 100.0, "wrong channel must NOT open the tone: {wrong_ch}");
        let r = full / half.max(1e-9);
        assert!(
            r > 3.0 && r < 5.0,
            "pressure must arrive as the raw byte (127 vs 64 = ~4x power): got {r:.2}"
        );

        // -- mis-stamped v3: poly_at unwired, v2 bend still wired.
        let b = reg
            .create_instance("v106_bogus3", 48_000.0, &[])
            .expect("bogus-v3 instance");
        let at = v106_probe_render(&b, Some((1, 61, 127)), None);
        let bend = v106_probe_render(&b, None, Some(crate::midipat::st_to_bend(6.0)));
        eprintln!("v106 ladder bogus3: at {at:.1} bend {bend:.1}");
        assert!(at < 10.0, "a mis-stamped v3 table must NOT wire poly_at: {at}");
        assert!(bend > 1e2, "the v2 bend must survive a bogus v3 export: {bend}");
    }

    /// Contract change (same-commit retarget, the v103 bend-4 precedent):
    /// this v100 guard pinned the scripted poly-AT DROP on the bundled
    /// sampler; v106 moved the subject to bundled `drums` — the last v1
    /// script. v136 completes the bundled v3 migration, so the subject
    /// moves again: a dedicated v1-ONLY probe script (tone oscillator, no
    /// v2/v3 table) keeps the "clean, audio-invisible, panic-free drop"
    /// alive for every third-party script the ladder must degrade for.
    /// Fresh instances per render: the probe's phase is instance state, so
    /// two renders on ONE instance could never be byte-equal. The audible
    /// half lives in v106_pressure_audible_on_bundled_scripts + v136_*.
    #[test]
    #[ignore = "slow-tier: compiles a v1-only probe script with rustc (~10 s)"]
    fn v100_poly_aftertouch_still_dropped_cleanly() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry(); // seeds bundled + writes synth_api.rs
        std::fs::write(
            reg.dir().join("v136_v1_subject.rs"),
            format!(
                "{}{}",
                V106_PROBE_HEAD,
                r#"
extern "C" fn po(s: *mut c_void, out: *mut f32, n: usize, _p: *const f32) {
    let p = unsafe { &mut *(s as *mut P) };
    let out = unsafe { core::slice::from_raw_parts_mut(out, n) };
    let step = (std::f64::consts::TAU * 200.0 / p.sr) as f64;
    for o in out.iter_mut() {
        if p.on { *o += (p.ph.sin() * 0.25) as f32; }
        p.ph += step;
        if p.ph > std::f64::consts::TAU { p.ph -= std::f64::consts::TAU; }
    }
}
pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: b"v136-v1-subject\0".as_ptr() as *const core::ffi::c_char,
    author: core::ptr::null(),
    description: core::ptr::null(),
    n_params: 0, params: core::ptr::null(),
    create: cr, destroy: de, note_on: no, note_off: nf, all_off: ao,
    cc: cc, press: pr, process: po,
};
#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 { &DESC }
"#
            ),
        )
        .unwrap();
        let reg = Registry::open(reg.dir().to_path_buf());
        let render = |with_at: bool| -> Vec<f32> {
            let inst = reg
                .create_instance("v136_v1_subject", 48_000.0, &[])
                .expect("v1-only probe instance");
            let mut ev: Vec<(u64, u8, u8, u8, u8)> = vec![(0u64, 3u8, 0u8, 48u8, 110u8)];
            if with_at {
                for i in 0..=100 {
                    ev.push((960 + (i * 480) as u64, 4u8, 0u8, 48u8, (i % 128) as u8));
                }
            }
            ev.push((47_000, 0, 0, 48, 0));
            let mut out = vec![0f32; 48_000];
            crate::audio::render_midi_events(&inst, &mut ev, 0, out.len(), &mut out);
            out
        };
        let plain = render(false);
        let with_at = render(true);
        assert!(
            plain.iter().any(|x| x.abs() > 0.01),
            "v1 subject must actually sound for this guard"
        );
        assert_eq!(plain, with_at, "poly-AT must stay invisible to v1 scripts (no callback)");
    }

    /// The user-visible half: every bundled script that speaks v3 must
    /// HEAR pressure — held note, AT 127 vs no AT: window power rises by
    /// the designed ~2.25x (amp x1.5). Sabotage: drop poly_at from a
    /// script (or the probe from the host) and that stem goes red.
    ///
    /// Drums speaks v3 too (v136, card t_d28458f1) but stays OUT of this
    /// list ON PURPOSE: its AT laws are class-based expression — a CHOKE
    /// for open hats/crash/ride (window power FALLS), a filter-corner
    /// climb for hats/snare/clap, head tension for kick/tom — none is a
    /// power lift. Its audible-and-targeted proof is the v136_* family.
    #[test]
    fn v106_pressure_audible_on_bundled_scripts() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let sine: Arc<[f32]> = (0..96_000)
            .map(|i| (i as f64 * std::f64::consts::TAU * 200.0 / 48_000.0).sin() as f32 * 0.5)
            .collect();
        set_host_assets(Arc::new(vec![sine]), 48_000);
        // granular joins v134: its AT law is density (x2 spawn at 127 ->
        // ~1.87 power lift on overlapping grains, not the x1.5-amp stems'
        // 2.25) — still comfortably inside this window.
        for stem in ["additive", "fm", "granular", "pad", "piano", "pluck", "sampler", "tri_osc"] {
            let params: &[f32] = if stem == "sampler" { &[0.0, 48.0, 1.0] } else { &[] };
            let render = |at: bool| -> f64 {
                // FRESH instance per round: pluck seeds its noise per
                // voice slot, and the slot round-robin would make an
                // A/B on one instance unequal-conditions (measured 2.85).
                let inst = reg
                    .create_instance(stem, 48_000.0, params)
                    .unwrap_or_else(|| panic!("{stem} instance"));
                let mut ev: Vec<(u64, u8, u8, u8, u8)> =
                    vec![(0u64, 3u8, 0u8, 48u8, 110u8)];
                if at {
                    for i in 0..=40 {
                        ev.push((960 + (i * 480) as u64, 4u8, 0u8, 48u8, 127));
                    }
                }
                ev.push((47_000, 0, 0, 48, 0));
                let mut out = vec![0f32; 48_000];
                crate::audio::render_midi_events(&inst, &mut ev, 0, out.len(), &mut out);
                let w = 14_400..48_000;
                out[w.clone()].iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>()
                    / (48_000 - 14_400) as f64
            };
            let (plain, at) = (render(false), render(true));
            eprintln!("v106 {stem}: plain {plain:.6} at {at:.6} ratio {:.2}", at / plain.max(1e-12));
            assert!(plain > 1e-6, "{stem}: control render must actually sound: {plain}");
            let r = at / plain;
            assert!(
                r > 1.7 && r < 3.0,
                "{stem}: poly-AT at 127 must lift sustained power ~2.25x, got {r:.2} \
                 (plain {plain:.6} at {at:.6})"
            );
        }
    }

    // ---- v134 (card t_5a1658c9): the granular bundle dropped note
    // velocity on the floor and never joined the ABI v2/v3 ladder, so the
    // piano-roll velocity, GLIDE and poly-AT lanes were all silent on a
    // granular track. These pins drive the REAL bundled script through the
    // event protocol only: the velocity law must hold EXACTLY (grains keep
    // their untouched RNG stream, so A/B power ratios are law-precise), a
    // baked glide ramp must bend the live cloud, and pressure must
    // thicken + brighten ONLY the pressured voice.

    /// v134 assets: 0 = 2.5 s of 200 Hz sine (velocity/glide/density
    /// measures), 1 = 220 Hz tone + deterministic LCG noise — brightness
    /// needs broadband material; a pure tone carries no HF to pre-emphasise.
    fn v134_assets() {
        let sine: Arc<[f32]> = (0..120_000)
            .map(|i| (i as f64 * std::f64::consts::TAU * 200.0 / 48_000.0).sin() as f32 * 0.5)
            .collect();
        let tone_noise: Arc<[f32]> = (0..120_000)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                let h = (i as u32).wrapping_mul(2_654_435_761);
                let noise = (h as f64 / u32::MAX as f64) * 2.0 - 1.0;
                (((2.0 * std::f64::consts::TAU * 220.0 * t).sin() + noise) * 0.25) as f32
            })
            .collect();
        set_host_assets(Arc::new(vec![sine, tone_noise]), 48_000);
    }

    /// One granular render through the real event protocol. FRESH instance
    /// per call: the grain RNG re-seeds at create, so same-schedule renders
    /// are byte-comparable and the velocity A/B compares pure gain laws.
    /// `at_note` stages poly-AT (kind 4) as (note, val) across the same
    /// dense schedule every AT round uses (block-split parity: the engine
    /// batches sample-accurately, so the CONTROL must carry the same event
    /// ticks with val=0 = "pressure released"); `bend_up` bakes a dense
    /// 0→+12 st GLIDE ramp (kind 5) like `v100_glide_render`.
    fn v134_render(
        reg: &Registry,
        vel: u8,
        at_note: Option<(u8, u8)>,
        bend_up: bool,
        asset: u32,
    ) -> Vec<f32> {
        let inst = reg
            .create_instance("granular", 48_000.0, &[asset as f32])
            .expect("granular instance");
        let mut ev: Vec<(u64, u8, u8, u8, u8)> = vec![(0u64, 3u8, 0u8, 48u8, vel)];
        if let Some((n, val)) = at_note {
            for i in 0..=40 {
                ev.push((960 + (i * 480) as u64, 4u8, 0u8, n, val));
            }
        }
        if bend_up {
            for i in 0..=100 {
                let raw = crate::midipat::st_to_bend(12.0 * i as f32 / 100.0);
                ev.push((
                    (960 + (i * 480) as u64),
                    5u8,
                    0u8,
                    (raw & 0x7F) as u8,
                    ((raw >> 7) & 0x7F) as u8,
                ));
            }
        }
        ev.push((47_000, 0, 0, 48, 0));
        let mut out = vec![0f32; 48_000];
        crate::audio::render_midi_events(&inst, &mut ev, 0, out.len(), &mut out);
        out
    }

    fn v134_power(x: &[f32], a: usize, b: usize) -> f64 {
        x[a..b].iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / (b - a) as f64
    }

    /// R1: velocity drives grain gain by (vel/127)^1.3 — and because the
    /// law multiplies the untouched RNG grain draw, the sustained-power
    /// ratio vel64/vel127 must sit on (64/127)^2.6 EXACTLY (±3%). vel=1 is
    /// the card's floor case: >=30 dB below vel=127 (measured -55 dB).
    #[test]
    fn v134_velocity_lane_scales_grain_gain() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        v134_assets();
        let p = |vel: u8| v134_power(&v134_render(&reg, vel, None, false, 0), 14_400, 48_000);
        let (p127, p64, p1) = (p(127), p(64), p(1));
        let ratio = p64 / p127;
        let expect = (64.0f64 / 127.0).powf(2.6); // (v^1.3)^2 — power law
        eprintln!(
            "v134 vel: p127 {p127:.6} p64 {p64:.6} p1 {p1:.3e} ratio {ratio:.5} expect {expect:.5}"
        );
        assert!(p127 > 1e-5, "vel-127 control render must actually sound: {p127}");
        assert!(
            (ratio / expect - 1.0).abs() < 0.03,
            "velocity must scale grain gain by (vel/127)^1.3: power ratio \
             vel64/vel127 = {ratio:.5}, expected {expect:.5}"
        );
        let d1 = 10.0 * (p1 / p127).log10();
        assert!(d1 < -30.0, "vel=1 must sink >=30 dB under vel=127: got {d1:.1} dB");
    }

    /// R2: the GLIDE lane reaches the cloud — a baked 0→+12 st ramp raises
    /// the late-window pitch proxy of a held note (v100 harness discipline:
    /// zero-crossing cycles of the sine-asset grain stream).
    #[test]
    fn v134_glide_lane_bends_grain_pitch() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        v134_assets();
        let flat = v134_render(&reg, 110, None, false, 0);
        let up = v134_render(&reg, 110, None, true, 0);
        assert_ne!(flat, up, "baked bends never reached granular (v3 bend unwired)");
        let late = |x: &[f32]| v100_cycles(&x[38_400..48_000]) as f64;
        let (cf, cu) = (late(&flat), late(&up));
        eprintln!("v134 glide: late cycles {cf} -> {cu} ratio {:.3}", cu / cf);
        assert!(cf >= 10.0, "flat control must carry audible cycles in the late window: {cf}");
        let r = cu / cf;
        assert!(
            r > 1.5 && r < 2.4,
            "0->+12 st glide must near-double the cloud's late pitch: got {r:.3}"
        );
    }

    /// R3: poly-AT lives AND targets. Density: AT=127 doubles the held
    /// voice's spawn rate -> measurable sustained-power lift on the sine
    /// asset. Targeting: AT on a non-held note must be BYTE-INVISIBLE
    /// (same RNG stream, untouched voice law). Brightness: the per-grain
    /// pre-emphasis must raise the HF-vs-total tilt on broadband material.
    #[test]
    fn v134_poly_at_lane_density_brightness_and_targeting() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        v134_assets();
        let plain = v134_render(&reg, 110, None, false, 0);
        let at = v134_render(&reg, 110, Some((48, 127)), false, 0);
        let (pw, pa) = (v134_power(&plain, 14_400, 48_000), v134_power(&at, 14_400, 48_000));
        let r = pa / pw;
        eprintln!("v134 at density: {pw:.6} -> {pa:.6} ratio {r:.3}");
        assert!(pw > 1e-6, "control render must actually sound: {pw}");
        assert!(
            r > 1.5 && r < 2.6,
            "poly-AT at 127 must thicken the cloud (density x2 -> power lift): \
             got {r:.3} (plain {pw:.6} at {pa:.6})"
        );
        // Targeting, event-schedule-matched: AT 127 on a NON-held note must
        // behave EXACTLY like val=0 (pressure released, per the v3 contract
        // "a val of 0 is pressure released, NOT silence") — byte-identical
        // output proves the pressure byte was matched against voices, not
        // applied wholesale, and that 0 never mutes the cloud.
        let wrong = v134_render(&reg, 110, Some((49, 127)), false, 0);
        let rel = v134_render(&reg, 110, Some((49, 0)), false, 0);
        assert!(
            wrong == rel,
            "poly-AT on a NON-held note must not touch the held cloud \
             (must equal the val=0 release twin byte-for-byte)"
        );
        let tilt = |x: &[f32]| -> f64 {
            let hf: f64 = x[14_400..48_000]
                .windows(2)
                .map(|d| (d[1] as f64 - d[0] as f64).powi(2))
                .sum::<f64>()
                / (48_000 - 14_400 - 1) as f64;
            hf / v134_power(x, 14_400, 48_000)
        };
        let nplain = v134_render(&reg, 110, None, false, 1);
        let nat = v134_render(&reg, 110, Some((48, 127)), false, 1);
        let (tp, ta) = (tilt(&nplain), tilt(&nat));
        eprintln!("v134 at brightness tilt: {tp:.4} -> {ta:.4} ratio {:.3}", ta / tp);
        assert!(
            ta / tp > 1.5,
            "pressure must brighten the cloud (HF tilt must climb): {tp:.4} -> {ta:.4}"
        );
    }

    // ---- v136 (card t_d28458f1): the drums bundle stayed on ABI v1, so
    // the v100 GLIDE lane and the v106 poly-AT lane were silently dead on
    // drum tracks while every melodic sibling answered them. These pins
    // drive the REAL bundled script through the event protocol only.
    // Drums state is sample-exact (per-sample voice updates, integer event
    // ticks), so byte-identity assertions are valid for the targeting half.

    /// One drums render through the real event protocol. FRESH instance per
    /// call. `at_note` stages poly-AT (kind 4) at (note, val) on the same
    /// dense schedule the AT rounds use (960 + 480*i, like the v106 guard);
    /// pass val=0 for the schedule-matched "pressure released" control, so
    /// event-block-splitting parity is guaranteed between rounds.
    fn v136_drums_render(reg: &Registry, note: u8, at_note: Option<(u8, u8)>) -> Vec<f32> {
        let inst = reg
            .create_instance("drums", 48_000.0, &[])
            .expect("bundled drums instance");
        let mut ev: Vec<(u64, u8, u8, u8, u8)> = vec![(0u64, 3u8, 0u8, note, 110u8)];
        if let Some((n, val)) = at_note {
            for i in 0..=40 {
                ev.push((960 + (i * 480) as u64, 4u8, 0u8, n, val));
            }
        }
        ev.push((47_000, 0, 0, note, 0));
        let mut out = vec![0f32; 48_000];
        crate::audio::render_midi_events(&inst, &mut ev, 0, out.len(), &mut out);
        out
    }

    fn v136_power(x: &[f32], a: usize, b: usize) -> f64 {
        x[a..b].iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / (b - a) as f64
    }

    /// R1: the GLIDE lane reaches the kit — a baked 0->+12 st ramp near-
    /// doubles the late-window pitch of a held pitched TOM (note 60, the
    /// card's "pitched TOM row") and the kick body (note 36). v100 harness
    /// discipline: dense st_to_bend ramp + zero-crossing cycles late.
    #[test]
    fn v136_glide_lane_bends_tom_and_kick_pitch() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        for (note, name) in [(60u8, "tom"), (36u8, "kick")] {
            let inst = reg.create_instance("drums", 48_000.0, &[]).expect("drums");
            let flat = v100_glide_render(&inst, note, &|_| 0.0);
            let inst = reg.create_instance("drums", 48_000.0, &[]).expect("drums");
            let up = v100_glide_render(&inst, note, &|f| 12.0 * f);
            assert_ne!(flat, up, "baked bends never reached drums ({name})");
            let late = |x: &[f32]| v100_cycles(&x[38_400..48_000]) as f64;
            let (cf, cu) = (late(&flat), late(&up));
            eprintln!(
                "v136 glide {name} note {note}: late cycles {cf} -> {cu} ratio {:.3}",
                cu / cf
            );
            assert!(cf >= 6.0, "{name}: flat control must carry late cycles: {cf}");
            let r = cu / cf;
            assert!(
                r > 1.5 && r < 2.4,
                "0->+12 st glide must near-double the {name}'s late pitch: got {r:.3}"
            );
        }
    }

    /// R2a: pressure CHOKES the ringing cymbals — open hat (46), crash (55)
    /// and ride (51) held under AT 127 vs the schedule-matched val-0 twin:
    /// power over the AUDIBLE span (0.1..0.4 s — the open hat retires at
    /// ~0.81 s, so a late window would measure decay arithmetic, not the
    /// law) collapses with tau/(1+3*at) = 4x faster decay — yet the AT
    /// render stays non-silent (a pressure is not a mute).
    #[test]
    fn v136_poly_at_choke_open_hat_crash_ride() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        // Per-note (ratio window, never-mute floor window): the open hat
        // retires ~0.81 s and rings quietly, the crash rings 2.5 s and its
        // choke only separates LATE, the ride sits between them.
        for (note, ra, rb, fa, fb) in [
            (46u8, 4_800usize, 19_200usize, 960usize, 4_800usize),
            (55, 38_400, 48_000, 4_800, 19_200),
            (51, 19_200, 43_200, 4_800, 19_200),
        ] {
            let plain = v136_drums_render(&reg, note, Some((note, 0)));
            let at = v136_drums_render(&reg, note, Some((note, 127)));
            let (pw, pa) = (v136_power(&plain, ra, rb), v136_power(&at, ra, rb));
            let (pe, ae) = (v136_power(&plain, fa, fb), v136_power(&at, fa, fb));
            let r = pa / pw;
            eprintln!(
                "v136 at-choke note {note}: {pw:.3e} -> {pa:.3e} ratio {r:.4} | \
                 floor early {pe:.3e} -> {ae:.3e}"
            );
            assert!(pw > 1e-7, "note {note} control render must actually sound: {pw}");
            assert!(
                r < 0.3,
                "poly-AT at 127 must choke the ring of note {note} (tau shortens 4x): \
                 got ratio {r:.4} (plain {pw:.6} at {pa:.6})"
            );
            // Never-mute, RELATIVE floor: the first ~0.1 s is before the
            // choke law bites hard, so even at full pressure the attack
            // must keep >=1% of the control power (a mute goes to 0).
            assert!(
                ae > 0.01 * pe,
                "choke must damp, never mute note {note}: early {pe:.3e} -> {ae:.3e}"
            );
        }
    }

    /// R2b: head tension — pressed kick (36) and pitched tom (60) climb in
    /// pitch exactly x1.25 at full pressure, measured as late-window zc
    /// cycles against the schedule-matched val-0 twin.
    #[test]
    fn v136_poly_at_tension_pitches_tom_and_kick() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        for (note, name) in [(60u8, "tom"), (36u8, "kick")] {
            let rel = v136_drums_render(&reg, note, Some((note, 0)));
            let at = v136_drums_render(&reg, note, Some((note, 127)));
            // 0.3..1.0 s: wide enough that zc quantisation (±0.5 cycle)
            // cannot move the measured ratio off 1.25 by more than ~0.04.
            let late = |x: &[f32]| v100_cycles(&x[14_400..48_000]) as f64;
            let (c0, c1) = (late(&rel), late(&at));
            eprintln!(
                "v136 at-tension {name}: late cycles {c0} -> {c1} ratio {:.3}",
                c1 / c0
            );
            assert!(c0 >= 6.0, "{name} control must carry late cycles: {c0}");
            let r = c1 / c0;
            assert!(
                r > 1.12 && r < 1.4,
                "poly-AT 127 must raise the {name} pitch by the x1.25 head-tension law: \
                 got {r:.3} ({c0} -> {c1} cycles)"
            );
        }
    }

    /// R2c: brightness — a pressed closed hat (42: HF pre-emphasis at full
    /// pressure) and a pressed clap (37: noise bandpass 1250 -> 2250 Hz)
    /// raise the HF-vs-total tilt of the hit vs the val-0 twin. (A DC-
    /// block-corner law was tried first and is PROVABLY tilt-blind on this
    /// material — measured 1.004 — so brightness rides the proven v134
    /// pre-emphasis instead; threshold 1.2 sits between the law-off
    /// sabotage value 1.0 and the measured ~1.45.)
    #[test]
    fn v136_poly_at_brightens_hat_and_clap() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        // Window covers the full audible hit (hat tail is ~0.1 s); the
        // ratio floor 1.3 is set after measuring the law (hat x1.5 cutoff
        // -> ~1.45, clap BP x1.8 -> ~2.0) so sabotage (law off = 1.0)
        // fails with a wide margin and libm drift cannot reach it.
        let tilt = |x: &[f32]| -> f64 {
            let hf: f64 = x[480..12_000]
                .windows(2)
                .map(|d| (d[1] as f64 - d[0] as f64).powi(2))
                .sum::<f64>()
                / (12_000 - 480 - 1) as f64;
            hf / v136_power(x, 480, 12_000)
        };
        for (note, name) in [(42u8, "hat"), (37u8, "clap")] {
            let rel = v136_drums_render(&reg, note, Some((note, 0)));
            let at = v136_drums_render(&reg, note, Some((note, 127)));
            let (tp, ta) = (tilt(&rel), tilt(&at));
            eprintln!(
                "v136 at-bright {name}: tilt {tp:.4} -> {ta:.4} ratio {:.3}",
                ta / tp
            );
            assert!(tp > 1e-6, "{name} control must carry HF energy: {tp}");
            assert!(
                ta / tp > 1.2,
                "poly-AT 127 must brighten the {name} (HF tilt must climb): \
                 {tp:.4} -> {ta:.4}"
            );
        }
    }

    /// R3: pressure is per-note. AT 127 on a NON-held note must be byte-
    /// identical to NO events at all (drums is sample-exact, so block
    /// splits at the event ticks are audio-irrelevant), and the val-0 twin
    /// on the held note must also be byte-identical — a released pressure
    /// is the normal kit, never silence and never a retrigger. The held-
    /// note 127 render must differ (targeting is not a no-op).
    #[test]
    fn v136_poly_at_targets_notes_and_release_is_never_silence() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let plain = v136_drums_render(&reg, 46, None);
        let rel = v136_drums_render(&reg, 46, Some((46, 0)));
        let wrong = v136_drums_render(&reg, 46, Some((49, 127)));
        assert!(
            plain.iter().any(|x| x.abs() > 0.01),
            "open hat control render must actually sound"
        );
        assert_eq!(
            plain, rel,
            "poly-AT val 0 must not alter the sound (released pressure = normal kit)"
        );
        assert_eq!(
            plain, wrong,
            "poly-AT on a NON-held note must not touch the kit (byte-identity vs no-AT)"
        );
        let at = v136_drums_render(&reg, 46, Some((46, 127)));
        assert_ne!(plain, at, "the held voice's pressure must be heard (targeting is not a no-op)");
    }

    // ---- v137 (card t_3bd30160): the retire budgets hard-gated voices
    // mid-tail (CRASH|RIDE 2.0*dec+0.5 vs tau 1.5*dec = a crash silenced
    // at 1.47*tau ~ -13 dB, instant zero, no fade). The bundle now retires
    // at 7 time constants of each kind's slowest envelope component with
    // an 8 ms linear fade to exact zero. Pins below use the absolute-window
    // dBFS method (dsp-audio-probe); HEAD is the negative control for R1+R2.

    /// One drums render of a HELD note (no note_off — a cymbal release
    /// chokes in 20 ms and would mask the retire behavior), chosen decay,
    /// `secs` long. params positional: gain, tune, decay (rest defaults).
    fn v137_drums_held(reg: &Registry, note: u8, decay: f32, secs: f64) -> Vec<f32> {
        let inst = reg
            .create_instance("drums", 48_000.0, &[0.9, 0.0, decay])
            .expect("bundled drums instance");
        let mut ev: Vec<(u64, u8, u8, u8, u8)> = vec![(0u64, 3u8, 0u8, note, 110u8)];
        let mut out = vec![0f32; (48_000.0 * secs) as usize];
        crate::audio::render_midi_events(&inst, &mut ev, 0, out.len(), &mut out);
        out
    }

    /// R1: at the decay slider's TOP (2.5) the old budget (2.0*dec+0.5 =
    /// 5.5 s, while tau = 1.5*dec = 3.75 s) zeroed a held crash mid-ring.
    /// The tail must still AUDIBLY ring at the old gate's window, and be
    /// below an absolute floor only past the new 7*tau budget (26.258 s).
    /// HEAD control: silent at 5.5 s (gate already fired) -> this pin fails.
    #[test]
    fn v137_crash_tail_survives_old_retire_gate() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let x = v137_drums_held(&reg, 55, 2.5, 27.5);
        let sr = 48_000usize;
        let p_ctl = v136_power(&x, sr / 2, sr);
        // Window STRICTLY after the old hard gate (2.0*dec+0.5 = 5.508 s):
        // HEAD's voice is already zero there -> this assertion fails on HEAD,
        // the negative control. The new budget rings on through it.
        let p_old = v136_power(&x, (5.6 * sr as f64) as usize, (5.8 * sr as f64) as usize);
        eprintln!(
            "v137 crash dec=2.5: body power {p_ctl:.3e}, post-old-gate window (5.6..5.8 s) {p_old:.3e}"
        );
        assert!(p_ctl > 1e-4, "control body must actually sound: {p_ctl:.3e}");
        assert!(
            p_old > 1e-6,
            "crash at max decay must still ring AFTER the OLD retire gate t=5.508 s \
             (HEAD hard-silenced it there: budget 2.0*dec+0.5 = 1.47*tau, a -13 dB chop): \
             {p_old:.3e}"
        );
        // |x| < 1e-3 past budget+7 ms (budget = 7*3.75 s + 8 ms = 26.258 s;
        // index math EXPLICIT: 26.4 s * 48000, never a bare sample literal).
        let tail = &x[(26.4 * 48_000.0) as usize..];
        let pk = tail.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(
            pk < 1e-3,
            "past the new retire budget (7*3.75 s + 8 ms) the render must be \
             |x| < 1e-3, got tail peak {pk:.2e}"
        );
    }

    /// R2: retirement is a faded exit for EVERY class, never a gate: for
    /// each representative at default decay, at the retire boundary (last
    /// loud sample) the local sample-to-sample step must stay < 1e-3.
    /// HEAD fails this for crash (step ~2e-1), ohat and ride (~1e-2):
    /// their budgets were 1.5-2.4 tau — audible chops inside the ring.
    #[test]
    fn v137_retire_is_faded_not_stepped() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let sr = 48_000usize;
        // (note, name, render secs = kind budget at dec 1.0 + margin)
        for (note, name, secs) in [
            (36u8, "kick", 3.2f64),
            (38, "snare", 2.0),
            (37, "clap", 1.8),
            (41, "tom", 3.2),
            (46, "ohat", 4.4),
            (51, "ride", 5.8),
            (55, "crash", 11.4),
        ] {
            let x = v137_drums_held(&reg, note, 1.0, secs);
            let last = x.iter().rposition(|v| v.abs() > 1e-4).unwrap_or(0);
            assert!(
                last > sr / 10,
                "{name}: render went quiet suspiciously early (t={:.3} s) — body regression?",
                last as f64 / sr as f64
            );
            let (a, b) = (last.saturating_sub(sr / 10), (last + sr / 10).min(x.len() - 2));
            let step = x[a..b]
                .windows(2)
                .fold(0f64, |m, w| m.max((w[1] as f64 - w[0] as f64).abs()));
            eprintln!(
                "v137 {name} note {note}: retire boundary t={:.3} s, local max step {step:.2e}",
                last as f64 / sr as f64
            );
            assert!(
                step < 1e-3,
                "{name}: retire must be a faded exit (max local step {step:.2e} >= 1e-3 — \
                 a hard gate cuts the tail like HEAD did)"
            );
        }
    }

    /// R3 guard: 7*tau discipline must not make short drums eternal — the
    /// closed hat (tau 0.045*dec) budget at default decay is 7*0.045 + 8 ms
    /// ~ 0.32 s; be fully silent (|x| < 1e-4) by 0.50 s.
    #[test]
    fn v137_chat_retires_promptly() {
        if !crate::fx::rustc_available() {
            return;
        }
        let reg = v51_registry();
        let x = v137_drums_held(&reg, 42, 1.0, 1.0);
        assert!(
            x[..9_600].iter().any(|v| v.abs() > 0.01),
            "closed hat control body must sound (instantaneous peak in first 0.2 s)"
        );
        let pk = x[(48_000 / 2)..].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(pk < 1e-4, "closed hat must be silent (1e-4) by 0.50 s, got {pk:.2e}");
    }

    #[test]
    fn v149_synth_api_folder_copy_is_the_single_source() {
        // v149: the old src/synth_api.rs twin is gone — the repo folder copy
        // is embedded (API_SRC) AND compiled as the crate module (lib.rs
        // #[path]), so text, module and seeded user copy are one file. These
        // asserts hold by construction today; they FAIL loudly if the
        // include/module path is ever rewired to a divergent twin, or if the
        // deleted twin creeps back (the drift class this pin guards).
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(
            !root.join("src").join("synth_api.rs").exists(),
            "src/synth_api.rs twin is back — synths/synth_api.rs is the single source"
        );
        let lib = std::fs::read_to_string(root.join("src").join("lib.rs")).unwrap();
        assert!(
            lib.contains(r#"#[path = "../synths/synth_api.rs"]"#),
            "the synth_api crate module must stay rooted at the synths/ folder copy"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("synths").join("synth_api.rs")).unwrap(),
            API_SRC,
            "embedded ABI drifted from the repo copy"
        );
    }

    // ==== v148: the shipped load_map contract must teach the live packing ====
    //
    // The contract file (synths/synth_api.rs — the single source since
    // v149) is embedded in the binary and re-written into every user's
    // synths/ folder on boot (content-diff seeding) — it is the ONLY
    // documentation of the key-map ABI third-party samplers copy from.
    // Until v148 it still taught the pre-v52 `<< 8` slot packing while
    // the live ABI (see host_load_map above; v52 R2) packs asset |
    // velocity | root across three fields: scripts written to the shipped
    // text read the velocity byte as the asset id and silently play wrong
    // samples. Docs-only pins — the packing BEHAVIOR is covered by the
    // v51/v52 render pins; these keep the contract text and the internal
    // map doc from rotting again. (The original third pin here — a
    // sync check between the src/ twin and the synths/ copy — was retired
    // in the v149 rebase: with one file left it would compare the file
    // with itself, and v149_synth_api_folder_copy_is_the_single_source
    // guards the drift class structurally.)

    /// The exact packing expression every contract doc must now state.
    /// Split with concat! so the literal cannot satisfy the self-file
    /// `own.contains(...)` check below (a joined literal here would make
    /// that polarity permanently vacuous — sabotage-proven 2026-11-24).
    const V148_PACKING: &str = concat!("(asset_id << 16)", " | (velocity << 8) | root_note");

    #[test]
    fn v148_api_doc_teaches_the_v52_slot_packing() {
        // Positive polarity: the shipped contract text (embedded API_SRC —
        // the bytes that land in the user's synths/ folder) carries the
        // live packing, the unpack recipe, and the velocity-layer
        // semantics the bundled sampler actually implements.
        assert!(
            API_SRC.contains(V148_PACKING),
            "synth_api.rs load_map doc does not state the live packing {V148_PACKING:?} — \
             samplers written to the shipped text index wrong assets"
        );
        for needle in [
            "slot >> 16",
            "(slot >> 8) & 0xFF",
            "slot & 0xFF",
            "velocity layers",
            "at/below",
            "bracket",
            "127 answers every note",
        ] {
            assert!(
                API_SRC.contains(needle),
                "load_map doc lacks {needle:?} — the unpack/velocity-layer contract went undocumented again"
            );
        }
        // Negative polarity: the pre-v52 expression must be GONE from the
        // file users copy. Split literal (vacuity rule: a needle must not
        // be satisfyable by this test's own text, and the concat keeps
        // the stale form out of greppable source too).
        assert!(
            !API_SRC.contains(concat!("(asset_id << 8)", " |")),
            "the pre-v52 << 8 packing crept back into the shipped synth_api.rs contract"
        );
    }

    #[test]
    fn v148_instance_map_doc_packing_is_v52() {
        // Same doc-rot class, INTERNAL side: the Instance.map comment
        // above once taught the pre-v52 packing too. Reads OUR OWN file,
        // so BOTH polarity literals are split with concat! — neither can
        // be satisfied (nor tripped) by this test's own text.
        let own = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("synth.rs"),
        )
        .expect("own source readable")
        .replace("\r\n", "\n");
        assert!(
            own.contains(concat!("(asset_id << 16)", " | (velocity << 8) |")),
            "the Instance.map doc lost the live v52 packing"
        );
        assert!(
            !own.contains(concat!("(asset_id << 8)", " | root_note")),
            "the pre-v52 << 8 packing crept back into a src/synth.rs doc comment"
        );
    }

    // ==== v151 (card t_0c4df02a): script headers describe real DSP ======
    //
    // The bundled scripts are embedded and content-diff-seeded into every
    // user's synths/ folder, so a header claim IS user documentation.
    // Three copy-paste lies shipped: the drums soft() doc promised bus-sum
    // headroom the per-voice curve cannot give (the engine clipper is what
    // bounds the mix), the drums tmul comment claimed all "metallics"
    // ignore tune (hats follow tmul; only crash/ride ignore it), and
    // pluck's v100 bend header copied the oscillator-sibling line claiming
    // live re-pitch of HELD notes (the Karplus-Strong loop length is
    // frozen at strike, so the value only reaches new note_on's). The
    // other six copies of that boilerplate are TRUE (bend_ratio is
    // computed per block in their process()) and must survive: every pin
    // is scoped to its own file, and the pluck pin carries a positive
    // control on tri_osc. Docs-only tripwires — the DSP behaviors are
    // pinned by the v100/v136/v137 suites. Both polarities are concat!-
    // split so neither the stale wording nor a marker lives greppable in
    // this file.

    fn v151_script(name: &str) -> &'static str {
        BUNDLED
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, src)| *src)
            .expect("bundled script present")
    }

    #[test]
    fn v151_drums_soft_doc_states_per_voice_reality() {
        let d = v151_script("drums.rs");
        assert!(
            d.contains(concat!("Per-voice soft ", "saturation")),
            "soft()'s doc must state it shapes each VOICE's own peaks"
        );
        assert!(
            !d.contains(concat!("keeps summed hits from blowing ", "past unity")),
            "soft() cannot bound the bus sum — the engine output clipper does"
        );
    }

    #[test]
    fn v151_drums_tune_doc_matches_branches() {
        let d = v151_script("drums.rs");
        assert!(
            d.contains(concat!("melodic voices AND the ", "hats")),
            "the tmul comment must name the hat branch as tune-following"
        );
        assert!(
            !d.contains(concat!("metallics ", "ignore tune")),
            "false both ways: hats follow tmul, crash/ride do not"
        );
    }

    #[test]
    fn v151_pluck_bend_doc_is_trigger_time() {
        let p = v151_script("pluck.rs");
        assert!(
            p.contains(concat!("TRIGGER ", "frequency")),
            "pluck's bend header must say the value lands at note_on"
        );
        assert!(
            !p.contains(concat!("baked glide bends ", "HELD notes live")),
            "the KS loop is frozen at strike — held notes never re-pitch"
        );
        // Positive control: the wording is TRUE for the oscillator
        // siblings; the fix must not have glob-scrubbed it out of them.
        assert!(
            v151_script("tri_osc.rs")
                .contains(concat!("baked glide bends ", "HELD notes live")),
            "oscillator siblings legitimately keep the live-bend wording"
        );
    }

    #[test]
    fn v151_api_bend_doc_covers_loop_models() {
        // The shipped ABI contract doc must not demand per-block re-pitch
        // without acknowledging the delay-loop class (pluck ships both).
        assert!(
            API_SRC.contains(concat!("delay-loop ", "models")),
            "the bend contract lost its Karplus-Strong qualifier"
        );
    }

    // v164 (card t_0c73850c): the script-facing host callback answers at
    // the LIVE tuning, and the default tuning is bit-exact pre-v164 math.
    #[test]
    fn v164_host_tune_key_tracks_atomic() {
        let back = crate::tune::get();
        crate::tune::set(12, 60);
        for k in [0u8, 36, 60, 69, 90, 127] {
            assert_eq!(
                host_tune_key(k).to_bits(),
                (k as f64 - 69.0).to_bits(),
                "N=12 host_tune_key({k}) drifted from the old literal"
            );
        }
        crate::tune::set(24, 60);
        assert_eq!(host_tune_key(61), -8.5); // root C4 ladder
        assert_eq!(host_tune_key(84), 3.0); // one EDO octave = +12 st
        crate::tune::set(back.0, back.1);
    }
}
