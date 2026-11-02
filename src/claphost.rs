//! CLAP 1.x plugin hosting (v34 R3).
//!
//! A third plugin backend alongside VST2 (vendor/vst2) and VST3 (vst3-host):
//! a small hand-written binding to the C ABI described by the vendored
//! headers in `vendor/clap-headers/` (CLAP 1.2.2, MIT). The ABI is a single
//! exported `clap_entry` struct: init → factory → create_plugin → per-plugin
//! extension interfaces (`clap.params`, `clap.audio-ports`, `clap.state`).
//!
//! Hard rules inherited from the VST hosting experience (see the slooper-dev
//! skill + plugin-hosting.md):
//! - Plugin libraries are NEVER dlclosed and their instances are never
//!   destroyed (no Drop) — a JUCE/Steinberg toolkit teardown while another
//!   instance is alive is unrecoverable. Entries leak, on purpose.
//! - All instantiation happens on the shared plugin GUI thread under
//!   `vst_loader_lock()` (plugin factories touch process-global state).
//! - The audio thread allocates nothing and takes no locks: one
//!   `UnsafeCell<RtState>` per entry, same shape as the VST2 stage entry.
//! - Parameter cells ([`VstParams`], normalized 0..1) are the ONLY param
//!   surface the UI touches; the audio thread converts them to CLAP's PLAIN
//!   values and delivers them as sample-accurate `PARAM_VALUE` events.
//!
//! Deliberate scope cuts for v1:
//! - No CLAP GUI hosting (the `clap.gui` extension wants a native window per
//!   plugin, unlike VST's reparenting): `has_editor()` is false and the
//!   slider window is the editor, like GUI-less VST3 instruments.
//! - Output events (plugin→host param echo, NOTE_END voice tracking) are
//!   received and counted but not consumed; nothing in slooper needs them
//!   without an editor.
//! - Latency reports are ignored (same as the VST paths: the echo ring
//!   carries dry audio until the plugin produces output).
//! - A `.clap` file may contain several plugins; every descriptor is
//!   scanned and its id travels through `VstEntry`/`ChainSave`/`ChainRow`.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::vsthost::{
    vst_loader_lock, StageEntry, VstKind, VstParamDesc, VstParams, FxRing, MIDI_QUEUE_CAP,
    VST_BLOCK,
};

// ================================================================ FFI ====
//
// Layout-equivalent structs for the CLAP 1.2.2 C ABI (field order IS the
// contract; cross-checked against vendor/clap-headers/include/clap/*.h).

#[allow(non_camel_case_types)]
mod ffi {
    use super::*;

    pub const CLAP_EVENT_NOTE_ON: u16 = 0;
    pub const CLAP_EVENT_NOTE_OFF: u16 = 1;
    pub const CLAP_EVENT_NOTE_CHOKE: u16 = 2;
    pub const CLAP_EVENT_NOTE_END: u16 = 3;
    pub const CLAP_EVENT_PARAM_VALUE: u16 = 5;
    pub const CLAP_EVENT_MIDI: u16 = 10;
    pub const CLAP_PARAM_IS_READONLY: u32 = 1 << 3;
    pub const CLAP_PARAM_IS_HIDDEN: u32 = 1 << 2;
    pub const CLAP_PROCESS_ERROR: i32 = 0;

    pub const CLAP_NAME_SIZE: usize = 256;
    pub const CLAP_PATH_SIZE: usize = 1024;

    pub static CLAP_PLUGIN_FACTORY_ID_C: &CStr = c"clap.plugin-factory";
    pub static CLAP_EXT_PARAMS_C: &CStr = c"clap.params";
    pub static CLAP_EXT_AUDIO_PORTS_C: &CStr = c"clap.audio-ports";
    pub static CLAP_EXT_STATE_C: &CStr = c"clap.state";
    pub static CLAP_EXT_LOG_C: &CStr = c"clap.log";

    #[repr(C)]
    pub struct ClapVersion {
        pub major: u32,
        pub minor: u32,
        pub revision: u32,
    }

    #[repr(C)]
    pub struct ClapEntry {
        pub clap_version: ClapVersion,
        pub init: unsafe extern "C" fn(plugin_path: *const c_char) -> bool,
        pub deinit: unsafe extern "C" fn(),
        pub get_factory: unsafe extern "C" fn(factory_id: *const c_char) -> *const c_void,
    }

    #[repr(C)]
    pub struct ClapPluginDescriptor {
        pub clap_version: ClapVersion,
        pub id: *const c_char,
        pub name: *const c_char,
        pub vendor: *const c_char,
        pub url: *const c_char,
        pub manual_url: *const c_char,
        pub support_url: *const c_char,
        pub version: *const c_char,
        pub description: *const c_char,
        pub features: *const *const c_char,
    }

    #[repr(C)]
    pub struct ClapHost {
        pub clap_version: ClapVersion,
        pub host_data: *mut c_void,
        pub name: *const c_char,
        pub vendor: *const c_char,
        pub url: *const c_char,
        pub version: *const c_char,
        pub get_extension: unsafe extern "C" fn(host: *const ClapHost, id: *const c_char) -> *const c_void,
        pub request_restart: unsafe extern "C" fn(host: *const ClapHost),
        pub request_process: unsafe extern "C" fn(host: *const ClapHost),
        pub request_callback: unsafe extern "C" fn(host: *const ClapHost),
    }

    #[repr(C)]
    pub struct ClapHostLog {
        /// severity: 0 fatal .. 5 trace (clap.log)
        pub log: unsafe extern "C" fn(host: *const ClapHost, severity: u32, message: *const c_char),
    }

    #[repr(C)]
    pub struct ClapPluginFactory {
        pub get_plugin_count: unsafe extern "C" fn(f: *const ClapPluginFactory) -> u32,
        pub get_plugin_descriptor:
            unsafe extern "C" fn(f: *const ClapPluginFactory, index: u32) -> *const ClapPluginDescriptor,
        pub create_plugin: unsafe extern "C" fn(
            f: *const ClapPluginFactory,
            host: *const ClapHost,
            plugin_id: *const c_char,
        ) -> *const ClapPlugin,
    }

    #[repr(C)]
    pub struct ClapPlugin {
        pub desc: *const ClapPluginDescriptor,
        pub plugin_data: *mut c_void,
        pub init: unsafe extern "C" fn(p: *const ClapPlugin) -> bool,
        pub destroy: unsafe extern "C" fn(p: *const ClapPlugin),
        pub activate:
            unsafe extern "C" fn(p: *const ClapPlugin, sr: f64, min_frames: u32, max_frames: u32) -> bool,
        pub deactivate: unsafe extern "C" fn(p: *const ClapPlugin),
        pub start_processing: unsafe extern "C" fn(p: *const ClapPlugin) -> bool,
        pub stop_processing: unsafe extern "C" fn(p: *const ClapPlugin),
        pub reset: unsafe extern "C" fn(p: *const ClapPlugin),
        pub process: unsafe extern "C" fn(p: *const ClapPlugin, proc: *const ClapProcess) -> i32,
        pub get_extension: unsafe extern "C" fn(p: *const ClapPlugin, id: *const c_char) -> *const c_void,
        pub on_main_thread: unsafe extern "C" fn(p: *const ClapPlugin),
    }

    #[repr(C)]
    pub struct ClapAudioBuffer {
        pub data32: *mut *mut f32,
        pub data64: *mut *mut f64,
        pub channel_count: u32,
        pub latency: u32,
        pub constant_mask: u64,
    }

    #[repr(C)]
    pub struct ClapEventHeader {
        pub size: u32,
        pub time: u32,
        pub space_id: u16,
        pub type_: u16,
        pub flags: u32,
    }

    #[repr(C)]
    pub struct ClapEventNote {
        pub header: ClapEventHeader,
        pub note_id: i32,
        pub port_index: i16,
        pub channel: i16,
        pub key: i16,
        pub velocity: f64,
    }

    #[repr(C)]
    pub struct ClapEventParamValue {
        pub header: ClapEventHeader,
        pub param_id: u32,
        pub cookie: *mut c_void,
        pub note_id: i32,
        pub port_index: i16,
        pub channel: i16,
        pub key: i16,
        pub value: f64,
    }

    #[repr(C)]
    pub struct ClapEventMidi {
        pub header: ClapEventHeader,
        pub port_index: u16,
        pub data: [u8; 3],
    }

    /// Host -> plugin event list (the host implements size/get).
    #[repr(C)]
    pub struct ClapInputEvents {
        pub ctx: *mut c_void,
        pub size: unsafe extern "C" fn(list: *const ClapInputEvents) -> u32,
        pub get: unsafe extern "C" fn(list: *const ClapInputEvents, index: u32) -> *const ClapEventHeader,
    }

    /// Plugin -> host event list (the host implements try_push).
    #[repr(C)]
    pub struct ClapOutputEvents {
        pub ctx: *mut c_void,
        pub try_push:
            unsafe extern "C" fn(list: *const ClapOutputEvents, event: *const ClapEventHeader) -> bool,
    }

    #[repr(C)]
    pub struct ClapProcess {
        pub steady_time: i64,
        pub frames_count: u32,
        pub transport: *const c_void,
        pub audio_inputs: *const ClapAudioBuffer,
        pub audio_outputs: *mut ClapAudioBuffer,
        pub audio_inputs_count: u32,
        pub audio_outputs_count: u32,
        pub in_events: *const ClapInputEvents,
        pub out_events: *const ClapOutputEvents,
    }

    #[repr(C)]
    pub struct ClapParamInfo {
        pub id: u32,
        pub flags: u32,
        pub cookie: *mut c_void,
        pub name: [c_char; CLAP_NAME_SIZE],
        pub module: [c_char; CLAP_PATH_SIZE],
        pub min_value: f64,
        pub max_value: f64,
        pub default_value: f64,
    }

    #[repr(C)]
    pub struct ClapPluginParams {
        pub count: unsafe extern "C" fn(p: *const ClapPlugin) -> u32,
        pub get_info:
            unsafe extern "C" fn(p: *const ClapPlugin, index: u32, out: *mut ClapParamInfo) -> bool,
        pub get_value: unsafe extern "C" fn(p: *const ClapPlugin, id: u32, out: *mut f64) -> bool,
        pub value_to_text: unsafe extern "C" fn(
            p: *const ClapPlugin,
            id: u32,
            value: f64,
            out_buffer: *mut c_char,
            capacity: u32,
        ) -> bool,
        pub text_to_value: unsafe extern "C" fn(
            p: *const ClapPlugin,
            id: u32,
            text: *const c_char,
            out: *mut f64,
        ) -> bool,
        pub flush: unsafe extern "C" fn(
            p: *const ClapPlugin,
            in_events: *const ClapInputEvents,
            out_events: *const ClapOutputEvents,
        ),
    }

    #[repr(C)]
    pub struct ClapAudioPortInfo {
        pub id: u32,
        pub name: [c_char; CLAP_NAME_SIZE],
        pub flags: u32,
        pub channel_count: u32,
        pub port_type: *const c_char,
        pub in_place_pair: u32,
    }

    #[repr(C)]
    pub struct ClapPluginAudioPorts {
        pub count: unsafe extern "C" fn(p: *const ClapPlugin, is_input: bool) -> u32,
        pub get: unsafe extern "C" fn(
            p: *const ClapPlugin,
            index: u32,
            is_input: bool,
            out: *mut ClapAudioPortInfo,
        ) -> bool,
    }

    #[repr(C)]
    pub struct ClapIStream {
        pub ctx: *mut c_void,
        pub read: unsafe extern "C" fn(s: *const ClapIStream, buffer: *mut c_void, size: u64) -> i64,
    }

    #[repr(C)]
    pub struct ClapOStream {
        pub ctx: *mut c_void,
        pub write: unsafe extern "C" fn(s: *const ClapOStream, buffer: *const c_void, size: u64) -> i64,
    }

    #[repr(C)]
    pub struct ClapPluginState {
        pub save: unsafe extern "C" fn(p: *const ClapPlugin, out: *const ClapOStream) -> bool,
        pub load: unsafe extern "C" fn(p: *const ClapPlugin, input: *const ClapIStream) -> bool,
    }

    /// Largest CLAP event we produce (param value with all fields): 56 B.
    pub const EVENT_SLOT: usize = 64;
}

/// An aligned, address-stable event storage slot.
#[repr(C, align(8))]
struct EvSlot([u8; ffi::EVENT_SLOT]);

impl Default for EvSlot {
    fn default() -> Self {
        EvSlot([0u8; ffi::EVENT_SLOT])
    }
}

// ============================================================== host =====
//
// A process-wide `clap_host_t` handed to every plugin. `get_extension`
// answers only `clap.log` (everything else is optional per spec; plugins
// must tolerate NULL). The request_* hooks are documented no-ops.

struct HostGlobals {
    log: ffi::ClapHostLog,
    host: ffi::ClapHost,
    name: CString,
    vendor: CString,
    url: CString,
    version: CString,
}

unsafe impl Sync for HostGlobals {}
unsafe impl Send for HostGlobals {}

static HOST: LazyLock<HostGlobals> = LazyLock::new(|| {
    let name = CString::new("slooper").unwrap();
    let vendor = CString::new("actually-god").unwrap();
    let url = CString::new("https://github.com/actually-god/slooper").unwrap();
    let version = CString::new(env!("CARGO_PKG_VERSION")).unwrap();
    let log = ffi::ClapHostLog { log: host_log_cb };
    let host = ffi::ClapHost {
        clap_version: ffi::ClapVersion { major: 1, minor: 2, revision: 0 },
        host_data: std::ptr::null_mut(),
        name: name.as_ptr(),
        vendor: vendor.as_ptr(),
        url: url.as_ptr(),
        version: version.as_ptr(),
        get_extension: host_get_extension,
        request_restart: host_noop,
        request_process: host_noop,
        request_callback: host_noop,
    };
    HostGlobals { log, host, name, vendor, url, version }
});

unsafe extern "C" fn host_get_extension(
    _host: *const ffi::ClapHost,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    let id = CStr::from_ptr(id);
    if id.to_bytes() == b"clap.log" {
        return &HOST.log as *const _ as *const c_void;
    }
    std::ptr::null()
}

unsafe extern "C" fn host_noop(_host: *const ffi::ClapHost) {}

unsafe extern "C" fn host_log_cb(_host: *const ffi::ClapHost, severity: u32, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let m = CStr::from_ptr(msg).to_string_lossy();
    // Fatal/error always print; lower severities behind an env flag so a
    // chatty plugin cannot spam a release session.
    if severity <= 2 || std::env::var_os("SLOOPER_CLAP_LOG").is_some() {
        eprintln!("[clap:{severity}] {m}");
    }
}

// ====================================================== lib/entry cache ===
//
// One dlopen per file, ONE init per file, never dlclosed (see module docs).

struct CachedLib {
    // Kept alive forever (leak policy); the entry pointer points into it.
    _lib: std::mem::ManuallyDrop<libloading::Library>,
    entry: *const ffi::ClapEntry,
    inited: bool,
}

unsafe impl Send for CachedLib {}

static LIBS: LazyLock<Mutex<HashMap<PathBuf, CachedLib>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Fetch (and lazily dlopen+init) the CLAP entry of `path`. Caller holds
/// `vst_loader_lock()` — dlopen + entry init touch process-global state.
fn entry_for(path: &Path) -> Result<&'static ffi::ClapEntry, String> {
    let mut libs = LIBS.lock().unwrap_or_else(|e| e.into_inner());
    let key = path.to_path_buf();
    if !libs.contains_key(&key) {
        // SAFETY: dlopen of a plugin file; the Library is never dropped.
        let lib = unsafe { libloading::Library::new(path) }
            .map_err(|e| format!("cannot open '{}' as CLAP: {e}", path.display()))?;
        let entry: libloading::Symbol<*const ffi::ClapEntry> = unsafe { lib.get(b"clap_entry\0") }
            .map_err(|_| format!("'{}' exports no clap_entry (not a CLAP plugin?)", path.display()))?;
        let entry = *entry;
        libs.insert(
            key.clone(),
            CachedLib {
                _lib: std::mem::ManuallyDrop::new(lib),
                entry,
                inited: false,
            },
        );
    }
    let cl = libs.get_mut(&key).unwrap();
    // One init per file, per spec 1.2 ("best effort" single call, matched
    // pair). We never call deinit, matching the never-dlclose policy.
    if !cl.inited {
        let cpath = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| "plugin path contains a NUL".to_string())?;
        // SAFETY: first and only call; spec requires plugins to tolerate
        // repeats anyway.
        let ok = unsafe { ((*cl.entry).init)(cpath.as_ptr()) };
        if !ok {
            return Err(format!("CLAP init() refused '{}'", path.display()));
        }
        cl.inited = true;
    }
    let e = unsafe { &*cl.entry };
    if e.clap_version.major != 1 {
        return Err(format!(
            "CLAP ABI v{}.{} unsupported (this host implements v1)",
            e.clap_version.major, e.clap_version.minor
        ));
    }
    Ok(e)
}

/// The plugin factory of a file (empty if it offers none).
unsafe fn factory_of(entry: &'static ffi::ClapEntry) -> Option<&'static ffi::ClapPluginFactory> {
    let p = (entry.get_factory)(ffi::CLAP_PLUGIN_FACTORY_ID_C.as_ptr());
    (!p.is_null()).then(|| &*(p as *const ffi::ClapPluginFactory))
}

unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

unsafe fn features_of(d: &ffi::ClapPluginDescriptor) -> Vec<String> {
    let mut out = Vec::new();
    if d.features.is_null() {
        return out;
    }
    let mut i = 0usize;
    loop {
        let f = *d.features.add(i);
        if f.is_null() {
            break;
        }
        out.push(cstr_to_string(f));
        i += 1;
    }
    out
}

/// True when the descriptor's feature list marks a note-consuming plugin —
/// the CLAP replacement for VST3's "Instrument" category (plugin-features.h).
fn features_say_instrument(feats: &[String]) -> bool {
    const INSTRUMENT_FEATURES: [&str; 6] =
        ["instrument", "note-effect", "synthesizer", "sampler", "drum", "drum-machine"];
    feats.iter().any(|f| INSTRUMENT_FEATURES.contains(&f.as_str()))
}

// ================================================================= scan ===

/// Probe one `.clap` file; one [`ScanProbe`] per plugin inside it. NEVER
/// instantiates: descriptors answer metadata without a plugin object
/// (the v17 "never touch a live library twice" rule at its safest).
pub struct ScanProbe {
    pub plugin_id: String,
    pub name: String,
    pub vendor: String,
    pub instrument: bool,
}

pub fn probe(path: &Path) -> Result<Vec<ScanProbe>, String> {
    let _loader = vst_loader_lock();
    let entry = entry_for(path)?;
    let factory = unsafe { factory_of(entry) }.ok_or_else(|| "no clap.plugin-factory".to_string())?;
    let count = unsafe { (factory.get_plugin_count)(factory) };
    let mut out = Vec::new();
    for i in 0..count.min(64) {
        let dptr = unsafe { (factory.get_plugin_descriptor)(factory, i) };
        if dptr.is_null() {
            continue;
        }
        let d = unsafe { &*dptr };
        let feats = unsafe { features_of(d) };
        out.push(ScanProbe {
            plugin_id: unsafe { cstr_to_string(d.id) },
            name: unsafe { cstr_to_string(d.name) },
            vendor: unsafe { cstr_to_string(d.vendor) },
            instrument: features_say_instrument(&feats),
        });
    }
    if out.is_empty() {
        return Err("CLAP factory lists no plugins".into());
    }
    Ok(out)
}

/// True for files the CLAP backend should probe.
pub fn is_clap_file(name: &str) -> bool {
    name.ends_with(".clap")
}

// ======================================================== RT event lists ==
//
// The host-side input list stores fixed 64-byte slots; the Vec is reserved
// to capacity at open and NEVER grows, so slot addresses stay stable while
// the plugin walks them through size()/get().

/// Data behind `ClapInputEvents::ctx`: stable pointers into the pool.
#[repr(C)]
struct InBuf {
    ptrs: Vec<*const ffi::ClapEventHeader>,
}

/// Data behind `ClapOutputEvents::ctx`: a small lossy ring of received
/// events (consumed by nobody yet; NOTE_END/param echo are counted).
struct OutBuf {
    slots: Vec<EvSlot>,
    head: usize,
    count: usize,
    received: u64,
}

const OUT_RING: usize = 256;

unsafe extern "C" fn cb_in_size(list: *const ffi::ClapInputEvents) -> u32 {
    let b = unsafe { &*((*list).ctx as *const InBuf) };
    b.ptrs.len() as u32
}

unsafe extern "C" fn cb_in_get(
    list: *const ffi::ClapInputEvents,
    index: u32,
) -> *const ffi::ClapEventHeader {
    let b = unsafe { &*((*list).ctx as *const InBuf) };
    match b.ptrs.get(index as usize) {
        Some(p) => *p,
        None => std::ptr::null(),
    }
}

unsafe extern "C" fn cb_out_try_push(
    list: *const ffi::ClapOutputEvents,
    event: *const ffi::ClapEventHeader,
) -> bool {
    let b = unsafe { &mut *((*list).ctx as *mut OutBuf) };
    b.received += 1;
    let n = unsafe { (*event).size as usize };
    if n == 0 || n > ffi::EVENT_SLOT {
        return false;
    }
    let dst = &mut b.slots[b.head];
    // Overwrite the oldest slot when full: output events are advisory in v1.
    unsafe {
        std::ptr::copy_nonoverlapping(event as *const u8, dst.0.as_mut_ptr(), n);
    }
    b.head = (b.head + 1) % OUT_RING;
    b.count = (b.count + 1).min(OUT_RING);
    true
}

// ============================================================ RT state ====

#[derive(Clone)]
struct QueuedMsg {
    m: [u8; 3],
    pos: i64,
}

struct RtState {
    /// Mono input ring + dry-pass echo + output ring (same pipeline as the
    /// VST2 stage entry: whole-block FIFO moves, echo carries dry until the
    /// plugin produces).
    in_ring: FxRing,
    out_ring: FxRing,
    echo: FxRing,
    stage_l: Vec<f32>,
    stage_r: Vec<f32>,
    stage_ol: Vec<f32>,
    stage_or: Vec<f32>,
    /// Stable channel-pointer arrays for the audio buffers.
    in_chans: Vec<*mut f32>,
    out_chans: Vec<*mut f32>,
    in_buf: ffi::ClapAudioBuffer,
    out_buf: ffi::ClapAudioBuffer,
    /// Event pool: reserved to capacity at open, never reallocated.
    ev_pool: Vec<EvSlot>,
    /// Events staged for the current block (index into `ev_pool`).
    ev_count: usize,
    inbuf_data: InBuf,
    in_list: ffi::ClapInputEvents,
    outbuf_data: OutBuf,
    out_list: ffi::ClapOutputEvents,
    /// Pending MIDI (raw 3-byte messages + absolute fed-sample position).
    pending: Vec<QueuedMsg>,
    deliver: Vec<QueuedMsg>,
    abs_fed: i64,
    abs_processed: i64,
    /// Param mirror: last delivered normalized values + param generation.
    last_vals: Vec<f32>,
    last_pg: u64,
    /// `start_processing` is deferred to the FIRST process() call (that is
    /// the real audio thread; activation happens on the plugin GUI thread).
    started: bool,
    /// Consecutive process() errors; a plugin that keeps failing is parked
    /// in passthrough rather than called forever.
    err_streak: u32,
    dead: bool,
    note_id: i32,
}

const RING_CAP: usize = 64 * 1024;
const EV_POOL_CAP: usize = 2 * 1024;

// =========================================================== the entry ====

/// One live CLAP plugin shared between the UI (params, state) and the audio
/// thread (process). Mirrors `vsthost::RtStageEntry`; see its docs.
pub struct ClapStageEntry {
    state: std::cell::UnsafeCell<RtState>,
    params: std::sync::Arc<VstParams>,
    descs: Vec<VstParamDesc>,
    /// (min, max) PLAIN range per parameter, for cell-normalized <-> plain
    /// conversion at the RT boundary.
    ranges: Vec<(f64, f64)>,
    /// Parameter ids in descriptor order (events address params by id).
    ids: Vec<u32>,
    name: String,
    spec: (PathBuf, VstKind),
    plugin_id: Option<String>,
    /// Raw plugin instance: audio thread uses it in process(); the control
    /// plane (state save/load) uses it under `vst_loader_lock()`. NEVER
    /// destroyed (leak policy).
    plugin: *const ffi::ClapPlugin,
    params_ext: *const ffi::ClapPluginParams,
    state_ext: *const ffi::ClapPluginState,
    /// 0 = plugin declares no audio input ports (an instrument).
    in_ports: u32,
    out_channels: u32,
    midi_drops: std::sync::Arc<AtomicU64>,
}

unsafe impl Send for ClapStageEntry {}
unsafe impl Sync for ClapStageEntry {}

impl std::fmt::Debug for ClapStageEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapStageEntry")
            .field("name", &self.name)
            .field("spec", &self.spec)
            .field("plugin_id", &self.plugin_id)
            .finish()
    }
}

impl ClapStageEntry {
    #[inline]
    fn raw(&self) -> *const ffi::ClapPlugin {
        self.plugin
    }

    /// The plugin's current PLAIN value -> normalized 0..1.
    fn plain_to_norm(&self, index: usize, plain: f64) -> f32 {
        let (mn, mx) = self.ranges[index];
        if (mx - mn).abs() < f64::EPSILON {
            return 0.0;
        }
        (((plain - mn) / (mx - mn)) as f32).clamp(0.0, 1.0)
    }

    fn norm_to_plain(&self, index: usize, norm: f32) -> f64 {
        let (mn, mx) = self.ranges[index];
        mn + (mx - mn) * norm as f64
    }

    /// UI side: record a normalized value. The audio thread converts and
    /// delivers it as a PARAM_VALUE event in its next block (never call
    /// params.flush() concurrently with process() — the spec forbids it).
    fn set_param_cell(&self, index: usize, value: f32) {
        self.params.set(index, value);
    }

    fn queue_msg(&self, s: &mut RtState, m: [u8; 3], pos: i64) {
        if s.pending.len() < MIDI_QUEUE_CAP {
            s.pending.push(QueuedMsg { m, pos });
        } else {
            self.midi_drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn all_off_queue(&self, s: &mut RtState) {
        let at = s.abs_fed;
        for c in 0..16u8 {
            self.queue_msg(s, [0xB0 | c, 123, 0], at);
            self.queue_msg(s, [0xB0 | c, 120, 0], at);
        }
    }

    /// Convert due queued messages + dirty params into this block's event
    /// pool. Param events land at time 0; note/midi events at their exact
    /// in-block offset (v33 sample-exact discipline).
    fn fill_events(&self, s: &mut RtState, start: i64) {
        s.inbuf_data.ptrs.clear();
        s.ev_count = 0;

        // 1. Parameter mirror: emit anything that moved since last block.
        let gen = self.params.generation();
        if gen != s.last_pg {
            s.last_pg = gen;
            for i in 0..self.descs.len() {
                if !self.descs[i].writable {
                    continue;
                }
                let v = self.params.get(i);
                if (v - s.last_vals[i]).abs() <= f32::EPSILON {
                    continue;
                }
                s.last_vals[i] = v;
                let plain = self.norm_to_plain(i, v);
                let id = self.ids[i];
                self.push_param_value(s, id, plain);
            }
        }

        // 2. Due MIDI: one-pass partition, stable order, then stable sort.
        let end = start + VST_BLOCK as i64;
        s.deliver.clear();
        let mut keep = 0usize;
        for k in 0..s.pending.len() {
            let q = &s.pending[k];
            if q.pos < end {
                let pos = q.pos;
                let m = q.m;
                s.deliver.push(QueuedMsg { m, pos });
            } else {
                s.pending[keep] = s.pending[k].clone();
                keep += 1;
            }
        }
        s.pending.truncate(keep);
        if s.deliver.is_empty() {
            return;
        }
        s.deliver.sort_by_key(|q| q.pos);
        // Snapshot: a plugin may re-enter the host (log callback) while we
        // stage its events, and the host's log path takes rt.lock() — a
        // borrowed iterator over `s.deliver` would alias that `&mut *s`
        // further down. Copy the due messages out and release the borrow.
        let deliver = std::mem::take(&mut s.deliver);
        for q in deliver.iter() {
            let t = (q.pos - start).clamp(0, VST_BLOCK as i64 - 1) as u32;
            let status = q.m[0] & 0xF0;
            let ch = (q.m[0] & 0x0F) as i16;
            match status {
                0x90 if q.m[2] > 0 => self.push_note(s, t, ffi::CLAP_EVENT_NOTE_ON, ch, q.m[1], q.m[2] as f64 / 127.0),
                // 0x90 with velocity 0 is note off by the classic convention.
                0x80 | 0x90 => self.push_note(s, t, ffi::CLAP_EVENT_NOTE_OFF, ch, q.m[1], 0.0),
                // CC / program / pressure travel as raw bytes: spec says
                // plugins should take both forms, and every CLAP plugin on
                // this host does.
                0xB0 | 0xC0 | 0xD0 => self.push_midi(s, t, q.m),
                _ => {}
            }
        }
    }

    fn ev_place<'a>(s: &'a mut RtState, len: usize) -> Option<&'a mut EvSlot> {
        debug_assert!(len <= ffi::EVENT_SLOT);
        if s.ev_count >= EV_POOL_CAP || s.ev_count >= s.ev_pool.len() {
            return None;
        }
        let i = s.ev_count;
        s.ev_count += 1;
        Some(&mut s.ev_pool[i])
    }

    fn push_note(&self, s: &mut RtState, time: u32, type_: u16, channel: i16, key: u8, vel: f64) {
        // Bump the id BEFORE taking the slot borrow (ev_place holds &mut s).
        if type_ == ffi::CLAP_EVENT_NOTE_ON {
            s.note_id = s.note_id.wrapping_add(1);
        }
        let id = if type_ == ffi::CLAP_EVENT_NOTE_ON {
            s.note_id
        } else {
            -1
        };
        let slot = match Self::ev_place(s, std::mem::size_of::<ffi::ClapEventNote>()) {
            Some(x) => x,
            None => {
                self.midi_drops.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let ev = ffi::ClapEventNote {
            header: ffi::ClapEventHeader {
                size: std::mem::size_of::<ffi::ClapEventNote>() as u32,
                time,
                space_id: 0,
                type_,
                flags: 0,
            },
            note_id: id,
            port_index: -1,
            channel,
            key: key as i16,
            velocity: vel,
        };
        unsafe {
            std::ptr::write(slot.0.as_mut_ptr() as *mut ffi::ClapEventNote, ev);
            let p = slot.0.as_ptr() as *const ffi::ClapEventHeader;
            s.inbuf_data.ptrs.push(p);
        }
    }

    fn push_midi(&self, s: &mut RtState, time: u32, m: [u8; 3]) {
        let slot = match Self::ev_place(s, std::mem::size_of::<ffi::ClapEventMidi>()) {
            Some(x) => x,
            None => {
                self.midi_drops.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let ev = ffi::ClapEventMidi {
            header: ffi::ClapEventHeader {
                size: std::mem::size_of::<ffi::ClapEventMidi>() as u32,
                time,
                space_id: 0,
                type_: ffi::CLAP_EVENT_MIDI,
                flags: 0,
            },
            port_index: 0,
            data: m,
        };
        unsafe {
            std::ptr::write(slot.0.as_mut_ptr() as *mut ffi::ClapEventMidi, ev);
            let p = slot.0.as_ptr() as *const ffi::ClapEventHeader;
            s.inbuf_data.ptrs.push(p);
        }
    }

    fn push_param_value(&self, s: &mut RtState, param_id: u32, value: f64) {
        let slot = match Self::ev_place(s, std::mem::size_of::<ffi::ClapEventParamValue>()) {
            Some(x) => x,
            None => return,
        };
        let ev = ffi::ClapEventParamValue {
            header: ffi::ClapEventHeader {
                size: std::mem::size_of::<ffi::ClapEventParamValue>() as u32,
                time: 0,
                space_id: 0,
                type_: ffi::CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id,
            cookie: std::ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        };
        unsafe {
            std::ptr::write(slot.0.as_mut_ptr() as *mut ffi::ClapEventParamValue, ev);
            let p = slot.0.as_ptr() as *const ffi::ClapEventHeader;
            s.inbuf_data.ptrs.push(p);
        }
    }

    /// Process ONE internal 512-frame block on the audio thread.
    unsafe fn process_block(&self, s: &mut RtState) {
        if s.dead {
            s.stage_ol.fill(0.0);
            s.stage_or.fill(0.0);
            return;
        }
        if !s.started {
            // First block on the REAL audio thread: activate processing.
            if !((*self.plugin).start_processing)(self.plugin) {
                s.dead = true;
                s.stage_ol.fill(0.0);
                return;
            }
            s.started = true;
        }
        self.fill_events(s, s.abs_processed);

        let frames = VST_BLOCK as u32;
        let mut proc = ffi::ClapProcess {
            steady_time: s.abs_processed,
            frames_count: frames,
            transport: std::ptr::null(),
            audio_inputs: if self.in_ports > 0 { &s.in_buf } else { std::ptr::null() },
            audio_outputs: &mut s.out_buf,
            audio_inputs_count: if self.in_ports > 0 { 1 } else { 0 },
            audio_outputs_count: 1,
            in_events: &s.in_list,
            out_events: &s.out_list,
        };
        let status = ((*self.plugin).process)(self.plugin, &mut proc);
        if status == ffi::CLAP_PROCESS_ERROR {
            s.err_streak += 1;
            if s.err_streak > 8 {
                // A plugin that fails block after block is broken or was
                // refused: park it in passthrough instead of spinning.
                s.dead = true;
            }
            s.stage_ol.fill(0.0);
            s.stage_or.fill(0.0);
        } else {
            s.err_streak = 0;
        }
    }
}

// ====================================================== StageEntry impl ===

impl StageEntry for ClapStageEntry {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn path(&self) -> &Path {
        &self.spec.0
    }
    fn spec(&self) -> &(PathBuf, VstKind) {
        &self.spec
    }
    fn kind(&self) -> VstKind {
        VstKind::Clap
    }
    fn plugin_id(&self) -> Option<&str> {
        self.plugin_id.as_deref()
    }
    fn descs(&self) -> &[VstParamDesc] {
        &self.descs
    }
    fn params(&self) -> &std::sync::Arc<VstParams> {
        &self.params
    }
    fn has_editor(&self) -> bool {
        // CLAP GUI hosting (the `clap.gui` extension + native windows) is
        // not implemented: the slider window IS the editor for CLAP.
        false
    }
    fn set_param(&self, index: usize, value: f32) {
        self.set_param_cell(index, value);
    }
    fn set_param_rt(&self, index: usize, value: f32) {
        self.set_param_cell(index, value);
    }
    fn midi_drops(&self) -> u64 {
        self.midi_drops.load(Ordering::Relaxed)
    }

    fn all_notes_off(&self) {
        // SAFETY: audio-thread-only (engine contract identical to VST).
        let s = unsafe { &mut *self.state.get() };
        self.all_off_queue(s);
    }

    fn flush(&self) {
        let s = unsafe { &mut *self.state.get() };
        s.in_ring.clear();
        s.out_ring.clear();
        s.echo.clear();
        s.pending.clear();
        s.deliver.clear();
        s.abs_fed = 0;
        s.abs_processed = 0;
    }

    fn chunk(&self) -> Option<Vec<u8>> {
        if self.state_ext.is_null() {
            return None;
        }
        let _loader = vst_loader_lock();
        let mut buf: Vec<u8> = Vec::new();
        unsafe extern "C" fn wr(
            stream: *const ffi::ClapOStream,
            data: *const c_void,
            n: u64,
        ) -> i64 {
            let buf = unsafe { &mut *((*stream).ctx as *mut Vec<u8>) };
            let src = unsafe { std::slice::from_raw_parts(data as *const u8, n as usize) };
            buf.extend_from_slice(src);
            n as i64
        }
        let os = ffi::ClapOStream {
            ctx: &mut buf as *mut Vec<u8> as *mut c_void,
            write: wr,
        };
        let ok = unsafe { ((*self.state_ext).save)(self.plugin, &os) };
        ok.then_some(buf)
    }

    fn process(&self, input: &[f32], output: &mut [f32]) {
        // Mirrors RtStageEntry::process: mono-in/out, internal 512 blocks,
        // echo passthrough until the plugin produces.
        let s = unsafe { &mut *self.state.get() };
        let n = input.len().min(output.len());

        let dropped = s.in_ring.push_slice(&input[..n]);
        s.echo.push_slice(&input[..n]);
        s.abs_fed += n as i64;
        if dropped > 0 {
            s.echo.drop_front(dropped);
        }

        while s.in_ring.len() >= VST_BLOCK {
            s.in_ring.drain_into(&mut s.stage_l);
            s.stage_r.copy_from_slice(&s.stage_l);
            s.stage_ol.fill(0.0);
            s.stage_or.fill(0.0);
            // SAFETY: audio thread only; `s` is the single live RtState.
            unsafe { self.process_block(s) };
            s.abs_processed += VST_BLOCK as i64;
            let got = s.stage_ol.len().min(VST_BLOCK);
            s.out_ring.push_slice(&s.stage_ol[..got]);
        }

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
        // The `deliver` snapshot was consumed by fill_events this block.
        unsafe { (*self.state.get()).deliver.clear() };
    }

    fn process_with_midi(&self, input: &[f32], output: &mut [f32], events: &[(usize, [u8; 3])]) {
        {
            let s = unsafe { &mut *self.state.get() };
            let base = s.abs_fed;
            for (off, m) in events {
                self.queue_msg(s, *m, base + *off as i64);
            }
        }
        self.process(input, output);
    }
}

// ============================================================ open() ======

/// Instantiate and activate a CLAP plugin. MUST run on the plugin GUI
/// thread (same rule as VST2/VST3: toolkit thread affinity binds there).
/// `saved` = normalized cell values from a chain row (never written as a
/// blanket param dump — v17 fresh-open zero-write rule; the values seed the
/// CELLS, and the audio thread's first block delivers them as events).
pub fn open(
    path: &Path,
    plugin_id: Option<&str>,
    sample_rate: u32,
    saved: Option<&[f32]>,
    state: Option<&[u8]>,
) -> Result<std::sync::Arc<ClapStageEntry>, String> {
    let _loader = vst_loader_lock();
    open_unlocked(path, plugin_id, sample_rate, saved, state)
}

/// [`open`]'s body WITHOUT the loader lock: `vsthost::open_inner` calls
/// this while already holding it (the lock is not reentrant).
pub(crate) fn open_unlocked(
    path: &Path,
    plugin_id: Option<&str>,
    sample_rate: u32,
    saved: Option<&[f32]>,
    state: Option<&[u8]>,
) -> Result<std::sync::Arc<ClapStageEntry>, String> {
    let entry = entry_for(path)?;
    let factory = unsafe { factory_of(entry) }.ok_or("no clap.plugin-factory")?;

    // Pick the descriptor: requested id, else the first plugin in the file.
    let count = unsafe { (factory.get_plugin_count)(factory) };
    let mut desc: *const ffi::ClapPluginDescriptor = std::ptr::null();
    for i in 0..count.min(64) {
        let d = unsafe { (factory.get_plugin_descriptor)(factory, i) };
        if d.is_null() {
            continue;
        }
        match plugin_id {
            Some(want) => {
                let id = unsafe { cstr_to_string((*d).id) };
                if id == want {
                    desc = d;
                    break;
                }
            }
            None => {
                desc = d;
                break;
            }
        }
    }
    if desc.is_null() {
        return Err(match plugin_id {
            Some(id) => format!("CLAP file does not contain plugin '{id}'"),
            None => "CLAP file has no plugin descriptors".into(),
        });
    }

    let plugin = unsafe {
        let id = CString::new(CStr::from_ptr((*desc).id).to_str().unwrap_or("")).unwrap_or_default();
        (factory.create_plugin)(factory, &HOST.host, id.as_ptr())
    };
    if plugin.is_null() {
        return Err("CLAP create_plugin returned null".into());
    }
    let ok = unsafe { ((*plugin).init)(plugin) };
    if !ok {
        // Leak the half-built instance (destroy semantics of a failed init
        // are plugin-defined; the library never unloads anyway).
        return Err("CLAP plugin init() refused".into());
    }

    let get_ext = |id: &CStr| unsafe { ((*plugin).get_extension)(plugin, id.as_ptr()) };
    let params_ext = get_ext(ffi::CLAP_EXT_PARAMS_C) as *const ffi::ClapPluginParams;
    let ports_ext = get_ext(ffi::CLAP_EXT_AUDIO_PORTS_C) as *const ffi::ClapPluginAudioPorts;
    let state_ext = get_ext(ffi::CLAP_EXT_STATE_C) as *const ffi::ClapPluginState;

    // ---- parameter descriptors (PLAIN min/max; cells stay normalized) ----
    let mut descs = Vec::new();
    let mut ranges = Vec::new();
    let mut ids = Vec::new();
    let mut cur_vals = Vec::new();
    if !params_ext.is_null() {
        let n = unsafe { ((*params_ext).count)(plugin) };
        for i in 0..n.min(4096) {
            let mut info: ffi::ClapParamInfo = unsafe { std::mem::zeroed() };
            if !unsafe { ((*params_ext).get_info)(plugin, i, &mut info) } {
                continue;
            }
            let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }.to_string_lossy().into_owned();
            let writable = info.flags & (ffi::CLAP_PARAM_IS_READONLY | ffi::CLAP_PARAM_IS_HIDDEN) == 0;
            let span = info.max_value - info.min_value;
            let default = if span.abs() > f64::EPSILON {
                ((info.default_value - info.min_value) / span) as f32
            } else {
                0.0
            };
            // Honest current value (fresh instance == its own defaults;
            // NEVER write values into a fresh-opened plugin — v17).
            let mut cur = info.default_value;
            unsafe {
                ((*params_ext).get_value)(plugin, info.id, &mut cur);
            }
            ids.push(info.id);
            descs.push(VstParamDesc { id: info.id, name, default, writable });
            ranges.push((info.min_value, info.max_value));
            cur_vals.push(default_from_plain(cur, &info));
        }
    }

    // ---- audio ports: first input/output port shapes the buffers ----
    let mut in_ports = 0u32;
    let mut out_channels = 2u32;
    if !ports_ext.is_null() {
        in_ports = unsafe { ((*ports_ext).count)(plugin, true) };
        let nout = unsafe { ((*ports_ext).count)(plugin, false) };
        if nout > 0 {
            let mut info: ffi::ClapAudioPortInfo = unsafe { std::mem::zeroed() };
            if unsafe { ((*ports_ext).get)(plugin, 0, false, &mut info) } {
                out_channels = info.channel_count.max(1).min(2);
            }
        }
    }

    let sr = sample_rate.max(1) as f64;
    let active = unsafe { ((*plugin).activate)(plugin, sr, 1, VST_BLOCK as u32) };
    if !active {
        return Err("CLAP activate() refused".into());
    }

    // State load AFTER activate (plugins commonly defer real state until
    // active; the chunk already carries parameter values).
    if let Some(bytes) = state {
        if !state_ext.is_null() {
            struct Cur { bytes: Vec<u8>, pos: usize }
            unsafe extern "C" fn rd(
                stream: *const ffi::ClapIStream,
                buffer: *mut c_void,
                n: u64,
            ) -> i64 {
                let c = unsafe { &mut *((*stream).ctx as *mut Cur) };
                let avail = (c.bytes.len() - c.pos).min(n as usize);
                if avail == 0 {
                    return 0;
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        c.bytes.as_ptr().add(c.pos),
                        buffer as *mut u8,
                        avail,
                    );
                }
                c.pos += avail;
                avail as i64
            }
            let mut cur = Cur { bytes: bytes.to_vec(), pos: 0 };
            let is = ffi::ClapIStream {
                ctx: &mut cur as *mut Cur as *mut c_void,
                read: rd,
            };
            unsafe {
                ((*state_ext).load)(plugin, &is);
            }
        }
    }

    // ---- seed cells: plugin-current values, chain-row values override ----
    let params = match saved {
        Some(v) => VstParams::with_values(&descs, v),
        None => VstParams::with_values(&descs, &cur_vals),
    };

    let name = unsafe { cstr_to_string((*desc).name) };
    let plugin_id = Some(unsafe { cstr_to_string((*desc).id) });

    let mut ev_pool = Vec::new();
    ev_pool.resize_with(EV_POOL_CAP, EvSlot::default);
    let mut outbuf_data = OutBuf {
        slots: Vec::new(),
        head: 0,
        count: 0,
        received: 0,
    };
    outbuf_data.slots.resize_with(OUT_RING, EvSlot::default);

    let mut st = RtState {
        in_ring: FxRing::with_capacity(RING_CAP),
        out_ring: FxRing::with_capacity(RING_CAP),
        echo: FxRing::with_capacity(RING_CAP),
        stage_l: vec![0.0; VST_BLOCK],
        stage_r: vec![0.0; VST_BLOCK],
        stage_ol: vec![0.0; VST_BLOCK],
        stage_or: vec![0.0; VST_BLOCK],
        in_chans: Vec::new(),
        out_chans: Vec::new(),
        in_buf: ffi::ClapAudioBuffer {
            data32: std::ptr::null_mut(),
            data64: std::ptr::null_mut(),
            channel_count: 0,
            latency: 0,
            constant_mask: 0,
        },
        out_buf: ffi::ClapAudioBuffer {
            data32: std::ptr::null_mut(),
            data64: std::ptr::null_mut(),
            channel_count: out_channels,
            latency: 0,
            constant_mask: 0,
        },
        ev_pool,
        ev_count: 0,
        inbuf_data: InBuf { ptrs: Vec::new() },
        in_list: ffi::ClapInputEvents {
            ctx: std::ptr::null_mut(),
            size: cb_in_size,
            get: cb_in_get,
        },
        outbuf_data,
        out_list: ffi::ClapOutputEvents {
            ctx: std::ptr::null_mut(),
            try_push: cb_out_try_push,
        },
        pending: Vec::new(),
        deliver: Vec::new(),
        abs_fed: 0,
        abs_processed: 0,
        last_vals: vec![0.0; descs.len()],
        last_pg: u64::MAX, // force first-block sync of every cell
        started: false,
        err_streak: 0,
        dead: false,
        note_id: 0,
    };
    // Wire the self-referential pointers now (RtState lives in an
    // UnsafeCell and is NEVER moved afterwards).
    let mut e = std::sync::Arc::new(ClapStageEntry {
        state: std::cell::UnsafeCell::new(st),
        params,
        descs,
        ranges,
        ids,
        name,
        spec: (path.to_path_buf(), VstKind::Clap),
        plugin_id,
        plugin,
        params_ext,
        state_ext,
        in_ports,
        out_channels,
        midi_drops: std::sync::Arc::new(AtomicU64::new(0)),
    });
    unsafe {
        let s = &mut *std::sync::Arc::get_mut(&mut e).unwrap().state.get();
        // Mono pipeline: feed the plugin a stereo buffer (L=R) when it
        // declares an input port; take L back out.
        s.in_chans = vec![
            s.stage_l.as_mut_ptr(),
            s.stage_r.as_mut_ptr(),
        ];
        s.out_chans = vec![
            s.stage_ol.as_mut_ptr(),
            s.stage_or.as_mut_ptr(),
        ];
        s.in_buf.data32 = s.in_chans.as_mut_ptr();
        s.in_buf.channel_count = 2;
        s.out_buf.data32 = s.out_chans.as_mut_ptr();
        s.out_buf.channel_count = s.out_buf.channel_count.min(2).max(1);
        // Single-channel plugins: only L is fed/drained.
        if s.out_buf.channel_count == 1 {
            s.out_chans.truncate(1);
            s.out_buf.data32 = s.out_chans.as_mut_ptr();
        }
        s.in_list.ctx = &mut s.inbuf_data as *mut InBuf as *mut c_void;
        s.out_list.ctx = &mut s.outbuf_data as *mut OutBuf as *mut c_void;
    }
    // NOTE: all self-referential pointers were written AFTER the RtState
    // reached its final home inside the Arc; the UnsafeCell contents never
    // move again for the entry's lifetime.
    Ok(e)
}

fn default_from_plain(cur: f64, info: &ffi::ClapParamInfo) -> f32 {
    let span = info.max_value - info.min_value;
    if span.abs() > f64::EPSILON {
        ((cur - info.min_value) / span) as f32
    } else {
        0.0
    }
}

// ================================================================= tests =

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile the repo's fixture plugin into `target/clap-fixture/`
    /// (skipping gracefully when no C compiler is available). Returns the
    /// .clap path. Rebuilt when older than the source.
    pub fn fixture_clap() -> Option<PathBuf> {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let src = manifest.join("vendor/clap-headers/fixture/clap_fixture.c");
        let out_dir = manifest.join("target/clap-fixture");
        let out = out_dir.join("fixture.clap");
        let _ = std::fs::create_dir_all(&out_dir);
        let stale = match (std::fs::metadata(&out), std::fs::metadata(&src)) {
            (Ok(o), Ok(s)) => o.modified().ok()? < s.modified().ok()?,
            _ => true,
        };
        if stale {
            let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
            let status = std::process::Command::new(cc)
                .arg("-shared")
                .arg("-fPIC")
                .arg("-O1")
                .arg("-I")
                .arg(manifest.join("vendor/clap-headers/include"))
                .arg(&src)
                .arg("-o")
                .arg(&out)
                .arg("-lm")
                .stdout(std::process::Stdio::null())
                .status()
                .ok()?;
            if !status.success() {
                return None;
            }
        }
        Some(out)
    }

    fn fixture_entry(which: &str) -> std::sync::Arc<ClapStageEntry> {
        let p = fixture_clap().expect("fixture compiled");
        let id = if which == "gain" { "com.slooper.fixture-gain" } else { "com.slooper.fixture-sine" };
        // (open/probe/chunk lock the loader internally — this mutex is
        //  NOT reentrant; taking it here too would self-deadlock.)
        open(&p, Some(id), 48_000, None, None).unwrap_or_else(|e| panic!("open {id}: {e}"))
    }

    #[test]
    fn v34_clap_probe_lists_plugins_and_roles() {
        let p = match fixture_clap() {
            Some(p) => p,
            None => return,
        };
        // (open/probe/chunk lock the loader internally — this mutex is
        //  NOT reentrant; taking it here too would self-deadlock.)
        let probes = probe(&p).unwrap();
        assert_eq!(probes.len(), 2, "fixture exports two plugins");
        let gain = probes.iter().find(|x| x.plugin_id == "com.slooper.fixture-gain").unwrap();
        let sine = probes.iter().find(|x| x.plugin_id == "com.slooper.fixture-sine").unwrap();
        assert!(!gain.instrument, "audio-effect is an effect");
        assert!(sine.instrument, "note-effect is an instrument");
        assert_eq!(gain.name, "Fixture Gain");
    }

    /// v34 pin (R3): the generic folder scan discovers `.clap` files and
    /// emits ONE row per plugin descriptor (multi-plugin files like
    /// Camshaft must be addressable individually).
    #[test]
    fn v34_clap_scan_lists_file_plugins_per_descriptor() {
        let src = match fixture_clap() {
            Some(p) => p,
            None => return,
        };
        let dir = std::env::temp_dir().join(format!("sp-clapscan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&src, dir.join("fixtures.clap")).unwrap();
        let entries = crate::vsthost::scan(&[dir.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
        let mut ids: Vec<&str> = entries
            .iter()
            .filter(|e| e.kind == crate::vsthost::VstKind::Clap && e.error.is_none())
            .filter_map(|e| e.plugin_id.as_deref())
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["com.slooper.fixture-gain", "com.slooper.fixture-sine"],
            "one scan row per CLAP descriptor"
        );
        let sine = entries
            .iter()
            .find(|e| e.plugin_id.as_deref() == Some("com.slooper.fixture-sine"))
            .unwrap();
        assert!(sine.instrument, "note-effect scans as an instrument");
        assert!(!entries[0].path.to_string_lossy().ends_with('#'), "path stays clean of the #id tail");
    }

    #[test]
    fn v34_clap_open_reads_normalized_params() {
        let e = fixture_entry("gain");
        assert_eq!(e.descs().len(), 2);
        assert_eq!(e.descs()[0].name, "fixgain");
        // plain default 1.0 on [0,1] normalizes to 1.0
        assert!((e.descs()[0].default - 1.0).abs() < 1e-6);
        assert!((e.params().get(0) - 1.0).abs() < 1e-6, "cells seed from plugin, not host");
    }

    #[test]
    fn v34_clap_gain_effect_scales_audio() {
        let e = fixture_entry("gain");
        let input: Vec<f32> = (0..1024).map(|i| ((i % 2) as f32) * 2.0 - 1.0).collect();
        let mut out = vec![0.0f32; 1024];
        e.process(&input, &mut out);
        // First 512-block: default gain 1.0 -> same square through.
        let peak = out[600..1024].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.9, "gain=1 passes audio through (peak {peak})");
    }

    #[test]
    fn v34_clap_param_cell_reaches_sound() {
        let e = fixture_entry("gain");
        e.params().set(0, 0.5); // normalized == plain on [0,1]
        let input: Vec<f32> = (0..1536).map(|_| 1.0).collect();
        let mut out = vec![0.0f32; 1536];
        e.process(&input, &mut out);
        // After the param block lands (echo carries dry only until the
        // plugin produces), the tail sits at half amplitude.
        let tail: f32 = out[1200..1536]
            .iter()
            .fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((tail - 0.5).abs() < 0.05, "half gain expected, got {tail}");
    }

    #[test]
    fn v34_clap_synth_note_makes_sound() {
        let e = fixture_entry("sine");
        let mut out = vec![0.0f32; 1536];
        let ev = vec![(0usize, [0x90u8, 69, 127])];
        // Instruments ride a silent input of output length — the exact
        // contract `render_midi_vst` uses (process() pumps blocks per fed
        // sample; empty input would mean no blocks at all, VST included).
        e.process_with_midi(&[0.0f32; 1536], &mut out, &ev);
        let peak = out[1024..1536].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.4 && peak <= 0.55, "A4 full velocity peaks ~0.5, got {peak}");
        let zc = out[1025..1536]
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let expect = (440.0 * 2.0 * 511.0 / 48_000.0) as isize;
        assert!((zc as isize - expect).abs() <= 2, "440 Hz expected: zc {zc} vs {expect}");
    }

    #[test]
    fn v34_clap_state_roundtrip_survives_reopen() {
        let p = match fixture_clap() {
            Some(p) => p,
            None => return,
        };
        // (open/probe/chunk lock the loader internally — this mutex is
        //  NOT reentrant; taking it here too would self-deadlock.)
        let e = open(&p, Some("com.slooper.fixture-gain"), 48_000, None, None).unwrap();
        e.params().set(0, 0.25);
        // Deliver the param, then harvest state (the RT thread owns the
        // process side; run a block so the fixture applies the value).
        let mut buf = vec![0.0f32; 512];
        e.process(&[1.0f32; 512], &mut buf);
        let chunk = e.chunk().expect("state saved");
        assert!(chunk.starts_with(b"F1 0.25"));
        // Reopen from state: fresh entry must come up at gain 0.25.
        let e2 =
            open(&p, Some("com.slooper.fixture-gain"), 48_000, Some(&[0.25, 1.0]), Some(&chunk))
                .unwrap();
        assert!((e2.params().get(0) - 0.25).abs() < 1e-6);
        let mut out = vec![0.0f32; 1536];
        e2.process(&[1.0f32; 1536], &mut out);
        let tail = out[1200..1536].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((tail - 0.25).abs() < 0.05, "recovered gain 0.25 expected, got {tail}");
    }

    #[test]
    fn v34_clap_flush_drops_stale_rings() {
        let e = fixture_entry("gain");
        e.process(&[1.0f32; 100], &mut vec![0.0f32; 100]);
        {
            let s = unsafe { &mut *e.state.get() };
            assert!(s.abs_fed > 0);
        }
        e.flush();
        let s = unsafe { &*e.state.get() };
        assert_eq!(s.abs_fed, 0);
        assert_eq!(s.in_ring.len(), 0);
    }

    #[test]
    fn v34_clap_unknown_plugin_id_is_clean_error() {
        let p = match fixture_clap() {
            Some(p) => p,
            None => return,
        };
        // (open/probe/chunk lock the loader internally — this mutex is
        //  NOT reentrant; taking it here too would self-deadlock.)
        let err = open(&p, Some("com.slooper.nope"), 48_000, None, None).unwrap_err();
        assert!(err.contains("com.slooper.nope"), "{err}");
    }
}
