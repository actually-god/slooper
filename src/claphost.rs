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
//! - v116: EVERY call into the plugin (open/probe handshake, RT
//!   process/start_processing, chunk save, preset-load, discovery) runs
//!   inside `crate::ffiguard::guarded` — CLAP ecosystems are C++, and a
//!   throw reaching a Rust frame would std::terminate the DAW. The catch
//!   turns the fault into a load error, a bypassed (parked) entry, or an
//!   empty verdict, with a status echo on stderr. (VST doctrine, v42.)
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::vsthost::{
    vst_loader_lock, FxRing, PresetLoadEvent, PresetLocation, StageEntry, VstKind, VstParamDesc,
    VstParams, MIDI_QUEUE_CAP, PRESET_KIND_FILE, PRESET_KIND_PLUGIN, VST_BLOCK,
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
    pub const CLAP_EVENT_PARAM_VALUE: u16 = 5;
    pub const CLAP_EVENT_MIDI: u16 = 10;
    pub const CLAP_PARAM_IS_READONLY: u32 = 1 << 3;
    pub const CLAP_PARAM_IS_HIDDEN: u32 = 1 << 2;
    /// v74: `CLAP_PARAM_IS_ENUM` — the plugin declares an integer choice
    /// parameter spanning min..=max; the row renders as a picker.
    pub const CLAP_PARAM_IS_ENUM: u32 = 1 << 4;
    pub const CLAP_PROCESS_ERROR: i32 = 0;

    pub const CLAP_NAME_SIZE: usize = 256;
    pub const CLAP_PATH_SIZE: usize = 1024;

    pub static CLAP_PLUGIN_FACTORY_ID_C: &CStr = c"clap.plugin-factory";
    pub static CLAP_EXT_PARAMS_C: &CStr = c"clap.params";
    pub static CLAP_EXT_AUDIO_PORTS_C: &CStr = c"clap.audio-ports";
    pub static CLAP_EXT_STATE_C: &CStr = c"clap.state";
    /// v67: plugin-side preset-load notification (CLAP v1 has no preset
    /// enumeration; this is the whole factory-preset story). The vendored
    /// 1.2.2 header ships primary + compat ids - answer both.
    pub static CLAP_EXT_PRESET_LOAD_C: &CStr = c"clap.preset-load/2";
    pub static CLAP_EXT_PRESET_LOAD_COMPAT_C: &CStr = c"clap.preset-load.draft/2";
    /// v79: entry-level preset-discovery factory ids (primary + compat).
    pub static CLAP_PRESET_DISCOVERY_FACTORY_ID_C: &CStr =
        c"clap.preset-discovery-factory/2";
    pub static CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT_C: &CStr =
        c"clap.preset-discovery-factory/draft-2";
    /// v79: `clap_preset_discovery_flags` (only what the crawl reads).
    pub const CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT: u32 = 1 << 0;

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

    // v117 (card t_a4175cb0): EVERY field below whose fn is implemented by
    // THIS file is `extern "C-unwind"`, and so are all Rust-implemented
    // callback fields in the other host-side vtables here (log, preset-load,
    // event lists, streams, discovery indexer/receiver). These fns run when
    // PLUGIN code calls back into the host; a plain "C" ABI frame carries a
    // `panic_cannot_unwind` guard, so a Rust panic inside one aborts (no
    // ffiguard catch can ever see it) and a C++ throw from the plugin cannot
    // unwind through it either. C-unwind keeps the vtable layout identical —
    // the callee just promises an unwindable frame. rustc E0308s across the
    // unwind ABI (no fn-pointer coercion between "C" and "C-unwind"), so
    // decl and impl can never drift apart silently. Plugin-IMPLEMENTED
    // vtables (entry/factory/plugin/provider) stay plain "C": we only call
    // those, and `crate::ffiguard::guarded` at the call site is their
    // firewall (v116). Do not add a new host-side callback on plain "C".
    #[repr(C)]
    pub struct ClapHost {
        pub clap_version: ClapVersion,
        pub host_data: *mut c_void,
        pub name: *const c_char,
        pub vendor: *const c_char,
        pub url: *const c_char,
        pub version: *const c_char,
        pub get_extension:
            unsafe extern "C-unwind" fn(host: *const ClapHost, id: *const c_char) -> *const c_void,
        pub request_restart: unsafe extern "C-unwind" fn(host: *const ClapHost),
        pub request_process: unsafe extern "C-unwind" fn(host: *const ClapHost),
        pub request_callback: unsafe extern "C-unwind" fn(host: *const ClapHost),
    }

    #[repr(C)]
    pub struct ClapHostLog {
        /// severity: 0 fatal .. 5 trace (clap.log)
        pub log: unsafe extern "C-unwind" fn(host: *const ClapHost, severity: u32, message: *const c_char),
    }

    #[repr(C)]
    pub struct ClapPluginFactory {
        pub get_plugin_count: unsafe extern "C" fn(f: *const ClapPluginFactory) -> u32,
        pub get_plugin_descriptor: unsafe extern "C" fn(
            f: *const ClapPluginFactory,
            index: u32,
        ) -> *const ClapPluginDescriptor,
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
        pub activate: unsafe extern "C" fn(
            p: *const ClapPlugin,
            sr: f64,
            min_frames: u32,
            max_frames: u32,
        ) -> bool,
        pub deactivate: unsafe extern "C" fn(p: *const ClapPlugin),
        pub start_processing: unsafe extern "C" fn(p: *const ClapPlugin) -> bool,
        pub stop_processing: unsafe extern "C" fn(p: *const ClapPlugin),
        pub reset: unsafe extern "C" fn(p: *const ClapPlugin),
        pub process: unsafe extern "C" fn(p: *const ClapPlugin, proc: *const ClapProcess) -> i32,
        pub get_extension:
            unsafe extern "C" fn(p: *const ClapPlugin, id: *const c_char) -> *const c_void,
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
        pub size: unsafe extern "C-unwind" fn(list: *const ClapInputEvents) -> u32,
        pub get: unsafe extern "C-unwind" fn(
            list: *const ClapInputEvents,
            index: u32,
        ) -> *const ClapEventHeader,
    }

    /// Plugin -> host event list (the host implements try_push).
    #[repr(C)]
    pub struct ClapOutputEvents {
        pub ctx: *mut c_void,
        pub try_push: unsafe extern "C-unwind" fn(
            list: *const ClapOutputEvents,
            event: *const ClapEventHeader,
        ) -> bool,
    }

    /// v67: host sink for `clap.preset-load/2` (v67). The plugin reports a
    /// preset it loaded through its own machinery (`loaded`), or a failed
    /// load location (`on_error`).
    #[repr(C)]
    pub struct ClapHostPresetLoad {
        pub on_error: unsafe extern "C-unwind" fn(
            host: *const ClapHost,
            location_kind: u32,
            location: *const c_char,
            load_key: *const c_char,
            os_error: i32,
            msg: *const c_char,
        ),
        pub loaded: unsafe extern "C-unwind" fn(
            host: *const ClapHost,
            location_kind: u32,
            location: *const c_char,
            load_key: *const c_char,
        ),
    }

    /// v79: plugin side of `clap.preset-load/2` — host-commanded load
    /// ([`crate::vsthost::StageEntry::load_preset_location`]).
    #[repr(C)]
    pub struct ClapPluginPresetLoad {
        pub from_location: unsafe extern "C" fn(
            plugin: *const ClapPlugin,
            location_kind: u32,
            location: *const c_char,
            load_key: *const c_char,
        ) -> bool,
    }

    // ---- v79: preset-discovery family (factory/preset-discovery.h) ----
    // Entry-level factory ("clap.preset-discovery-factory/2"): the host
    // creates a provider, becomes its indexer (declare_* callbacks land
    // here), crawls the declared FILE locations and reads each file's
    // presets through a metadata receiver — then feeds the collected
    // (kind, location, load_key) triples into ClapPluginPresetLoad.

    #[repr(C)]
    pub struct ClapUniversalPluginId {
        pub abi: *const c_char,
        pub id: *const c_char,
    }

    #[repr(C)]
    pub struct ClapPresetDiscoveryProviderDescriptor {
        pub clap_version: ClapVersion,
        pub id: *const c_char,
        pub name: *const c_char,
        pub vendor: *const c_char,
    }

    #[repr(C)]
    pub struct ClapPresetDiscoveryFiletype {
        pub name: *const c_char,
        pub description: *const c_char,
        pub file_extension: *const c_char,
    }

    #[repr(C)]
    pub struct ClapPresetDiscoveryLocation {
        pub flags: u32,
        pub name: *const c_char,
        pub kind: u32,
        pub location: *const c_char,
    }

    #[repr(C)]
    pub struct ClapPresetDiscoverySoundpack {
        pub flags: u32,
        pub id: *const c_char,
        pub name: *const c_char,
        pub description: *const c_char,
        pub homepage_url: *const c_char,
        pub vendor: *const c_char,
        pub image_path: *const c_char,
        pub release_timestamp: u64,
    }

    #[repr(C)]
    pub struct ClapPresetDiscoveryMetadataReceiver {
        pub receiver_data: *mut c_void,
        pub on_error: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            os_error: i32,
            error_message: *const c_char,
        ),
        pub begin_preset: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            name: *const c_char,
            load_key: *const c_char,
        ) -> bool,
        pub add_plugin_id: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            plugin_id: *const ClapUniversalPluginId,
        ),
        pub set_soundpack_id: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            soundpack_id: *const c_char,
        ),
        pub add_creator: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            creator: *const c_char,
        ),
        pub set_description: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            description: *const c_char,
        ),
        pub set_timestamps: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            creation_time: u64,
            modification_time: u64,
        ),
        pub add_feature: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            feature: *const c_char,
        ),
        pub add_extra_info: unsafe extern "C-unwind" fn(
            receiver: *const ClapPresetDiscoveryMetadataReceiver,
            key: *const c_char,
            value: *const c_char,
        ),
    }

    #[repr(C)]
    pub struct ClapPresetDiscoveryProvider {
        pub desc: *const ClapPresetDiscoveryProviderDescriptor,
        pub provider_data: *mut c_void,
        pub init: unsafe extern "C" fn(provider: *const ClapPresetDiscoveryProvider) -> bool,
        pub destroy: unsafe extern "C" fn(provider: *const ClapPresetDiscoveryProvider),
        pub get_metadata: unsafe extern "C" fn(
            provider: *const ClapPresetDiscoveryProvider,
            location_kind: u32,
            location: *const c_char,
            metadata_receiver: *const ClapPresetDiscoveryMetadataReceiver,
        ) -> bool,
        pub get_extension: unsafe extern "C" fn(
            provider: *const ClapPresetDiscoveryProvider,
            extension_id: *const c_char,
        ) -> *const c_void,
    }

    #[repr(C)]
    pub struct ClapPresetDiscoveryIndexer {
        pub clap_version: ClapVersion,
        pub name: *const c_char,
        pub vendor: *const c_char,
        pub url: *const c_char,
        pub version: *const c_char,
        pub indexer_data: *mut c_void,
        pub declare_filetype: unsafe extern "C-unwind" fn(
            indexer: *const ClapPresetDiscoveryIndexer,
            filetype: *const ClapPresetDiscoveryFiletype,
        ) -> bool,
        pub declare_location: unsafe extern "C-unwind" fn(
            indexer: *const ClapPresetDiscoveryIndexer,
            location: *const ClapPresetDiscoveryLocation,
        ) -> bool,
        pub declare_soundpack: unsafe extern "C-unwind" fn(
            indexer: *const ClapPresetDiscoveryIndexer,
            soundpack: *const ClapPresetDiscoverySoundpack,
        ) -> bool,
        pub get_extension: unsafe extern "C-unwind" fn(
            indexer: *const ClapPresetDiscoveryIndexer,
            extension_id: *const c_char,
        ) -> *const c_void,
    }

    #[repr(C)]
    pub struct ClapPresetDiscoveryFactory {
        pub count: unsafe extern "C" fn(factory: *const ClapPresetDiscoveryFactory) -> u32,
        pub get_descriptor: unsafe extern "C" fn(
            factory: *const ClapPresetDiscoveryFactory,
            index: u32,
        ) -> *const ClapPresetDiscoveryProviderDescriptor,
        pub create: unsafe extern "C" fn(
            factory: *const ClapPresetDiscoveryFactory,
            indexer: *const ClapPresetDiscoveryIndexer,
            provider_id: *const c_char,
        ) -> *const ClapPresetDiscoveryProvider,
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
        pub read:
            unsafe extern "C-unwind" fn(s: *const ClapIStream, buffer: *mut c_void, size: u64) -> i64,
    }

    #[repr(C)]
    pub struct ClapOStream {
        pub ctx: *mut c_void,
        pub write:
            unsafe extern "C-unwind" fn(s: *const ClapOStream, buffer: *const c_void, size: u64) -> i64,
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
/// v117 (card t_a4175cb0): the plugin->host direction firewall. Every
/// host-implemented CLAP callback body runs inside this. A Rust panic MUST
/// NOT unwind out of a callback frame: the catchers above are the plugin's
/// C++ frames — libstdc++ `catch(...)` treats the Rust panic as a foreign
/// object and `_Unwind_DeleteException` lands in std's `__rust_drop_panic`
/// = "Rust panics must be rethrown" ABORT (measured: ffiguard's own catch
/// cannot be the recoverable landing site for a Rust panic; and the
/// personality installs Rust `catch_unwind` landing pads for ANY class, so
/// a shim-level catch_unwind would break the C++ throw path instead).
/// Catching HERE, at the boundary, converts the panic into the callback's
/// safe default (null/false/0 — the same values a declining host returns)
/// and leaves the plugin's call intact. The C-unwind ABI on the callbacks
/// stays as the doctrine defense (a future callback that legitimately
/// propagates a foreign unwind cannot re-introduce the abort at its own
/// frame).
#[inline]
fn host_cb_guard<T: Default, F: FnOnce() -> T>(f: F) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(payload) => {
            let s = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic payload".to_string());
            eprintln!("[slooper] CLAP host callback panicked (degraded to default): {s}");
            T::default()
        }
    }
}

/// v117: counts how often the panic trap below `host_log_cb` fires, so the
/// pin can prove the plugin's re-entry COMPLETED (trap hit exactly N times)
/// rather than merely that the process is still up.
#[cfg(test)]
pub(crate) static V117_TRAP_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

// A process-wide `clap_host_t` handed to every plugin. `get_extension`
// answers only `clap.log` (everything else is optional per spec; plugins
// must tolerate NULL). The request_* hooks are documented no-ops.

struct HostGlobals {
    log: ffi::ClapHostLog,
    // The `clap_host_t` itself is PER ENTRY since v67 (host_data routes
    // plugin-side preset-load reports back to their entry); these
    // strings are shared by all entries.
    #[allow(dead_code)]
    name: CString,
    #[allow(dead_code)]
    vendor: CString,
    #[allow(dead_code)]
    url: CString,
    #[allow(dead_code)]
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
    HostGlobals {
        log,
        name,
        vendor,
        url,
        version,
    }
});

/// v67: per-entry sink for plugin-side preset-load reports. Each entry
/// hands its plugins a dedicated `clap_host_t` whose `host_data` points
/// here (both the Box and the Arc clone it holds are LEAKED - plugins are
/// never unloaded and entries are never destroyed), so the one global
/// `PRESET_LOAD` vtable can route every callback to the reporting entry.
#[derive(Default)]
struct PresetSlot {
    report: std::sync::Mutex<Option<crate::vsthost::PresetLoadEvent>>,
}

static PRESET_LOAD: ffi::ClapHostPresetLoad = ffi::ClapHostPresetLoad {
    on_error: host_preset_error,
    loaded: host_preset_loaded,
};

unsafe fn preset_slot_of(host: *const ffi::ClapHost) -> Option<&'static PresetSlot> {
    if host.is_null() {
        return None;
    }
    let data = unsafe { (*host).host_data };
    if data.is_null() {
        return None;
    }
    Some(unsafe { &*(data as *const PresetSlot) })
}

unsafe extern "C-unwind" fn host_preset_loaded(
    host: *const ffi::ClapHost,
    _location_kind: u32,
    location: *const c_char,
    load_key: *const c_char,
) {
    host_cb_guard(|| {
        if let Some(slot) = unsafe { preset_slot_of(host) } {
            let mut report = slot.report.lock().unwrap_or_else(|p| p.into_inner());
            *report = Some(crate::vsthost::PresetLoadEvent::Loaded {
                location: unsafe { cstr_to_string(location) },
                load_key: (!load_key.is_null()).then(|| unsafe { cstr_to_string(load_key) }),
            });
        }
    })
}

unsafe extern "C-unwind" fn host_preset_error(
    host: *const ffi::ClapHost,
    _location_kind: u32,
    location: *const c_char,
    load_key: *const c_char,
    _os_error: i32,
    msg: *const c_char,
) {
    host_cb_guard(|| {
        if let Some(slot) = unsafe { preset_slot_of(host) } {
            let mut report = slot.report.lock().unwrap_or_else(|p| p.into_inner());
            *report = Some(crate::vsthost::PresetLoadEvent::Failed {
                location: unsafe { cstr_to_string(location) },
                load_key: (!load_key.is_null()).then(|| unsafe { cstr_to_string(load_key) }),
                message: if msg.is_null() {
                    "plugin reported no message".to_string()
                } else {
                    unsafe { cstr_to_string(msg) }
                },
            });
        }
    })
}

// ======================================== v79 preset discovery ======
//
// The host's INDEXER role for `clap.preset-discovery-factory/2`: record
// what each provider declares (filetypes + locations), crawl the FILE
// locations, and read each candidate file's presets through a per-file
// metadata receiver. Everything here runs on the control thread under
// `vst_loader_lock` (entered once by `preset_locations`), never against
// a processing plugin. Providers and indexers are NEVER destroyed — the
// indexer must outlive its provider, and the standing leak policy makes
// that cheap (entries are never destroyed either).

/// Per-provider recording context (lives as `indexer_data`).
#[derive(Default)]
struct IndexCtx {
    /// Declared filetype extensions, lowercased; `""` = match every file.
    filetypes: Mutex<Vec<String>>,
    /// `(kind, flags, location)` per declared clap_preset_discovery_location_t.
    locations: Mutex<Vec<(u32, u32, Option<String>)>>,
}

/// Per-file collection context (lives as `receiver_data`, stack-bound to
/// one `get_metadata` call).
struct RunCtx {
    kind: u32,
    location: Option<String>,
    factory: bool,
    out: Vec<PresetLocation>,
    errors: Vec<String>,
}

const DISCOVERY_MAX_PROVIDERS: u32 = 8;
const DISCOVERY_MAX_FILES: usize = 512;
const DISCOVERY_MAX_PRESETS: usize = 1024;

unsafe fn index_ctx_of(ix: *const ffi::ClapPresetDiscoveryIndexer) -> Option<&'static IndexCtx> {
    if ix.is_null() {
        return None;
    }
    let d = unsafe { (*ix).indexer_data };
    if d.is_null() {
        return None;
    }
    Some(unsafe { &*(d as *const IndexCtx) })
}

unsafe fn run_ctx_of<'a>(
    recv: *const ffi::ClapPresetDiscoveryMetadataReceiver,
) -> Option<&'a mut RunCtx> {
    if recv.is_null() {
        return None;
    }
    let d = unsafe { (*recv).receiver_data };
    if d.is_null() {
        return None;
    }
    Some(unsafe { &mut *(d as *mut RunCtx) })
}

unsafe extern "C-unwind" fn idx_declare_filetype(
    ix: *const ffi::ClapPresetDiscoveryIndexer,
    ft: *const ffi::ClapPresetDiscoveryFiletype,
) -> bool {
    host_cb_guard(|| {
        let Some(ctx) = (unsafe { index_ctx_of(ix) }) else {
            return false;
        };
        if ft.is_null() {
            return false;
        }
        let ext = unsafe { cstr_to_string((*ft).file_extension) }.to_lowercase();
        if let Ok(mut v) = ctx.filetypes.lock() {
            if v.len() < 64 {
                v.push(ext);
            }
        }
        true
    })
}

unsafe extern "C-unwind" fn idx_declare_location(
    ix: *const ffi::ClapPresetDiscoveryIndexer,
    loc: *const ffi::ClapPresetDiscoveryLocation,
) -> bool {
    host_cb_guard(|| {
        let Some(ctx) = (unsafe { index_ctx_of(ix) }) else {
            return false;
        };
        if loc.is_null() {
            return false;
        }
        let (kind, flags) = unsafe { ((*loc).kind, (*loc).flags) };
        let location = unsafe {
            if (*loc).location.is_null() {
                None
            } else {
                Some(cstr_to_string((*loc).location))
            }
        };
        if let Ok(mut v) = ctx.locations.lock() {
            if v.len() < 32 {
                v.push((kind, flags, location));
            }
        }
        true
    })
}

unsafe extern "C-unwind" fn idx_declare_soundpack(
    _ix: *const ffi::ClapPresetDiscoveryIndexer,
    _sp: *const ffi::ClapPresetDiscoverySoundpack,
) -> bool {
    host_cb_guard(|| {
        true // sound packs are display metadata; the crawl does not need them
    })
}

unsafe extern "C-unwind" fn idx_get_extension(
    _ix: *const ffi::ClapPresetDiscoveryIndexer,
    _id: *const c_char,
) -> *const c_void {
    host_cb_guard(|| {
        std::ptr::null() // no indexer extensions offered (invalidation etc. unhosted)
    })
}

unsafe extern "C-unwind" fn recv_on_error(
    recv: *const ffi::ClapPresetDiscoveryMetadataReceiver,
    _os_error: i32,
    msg: *const c_char,
) {
    host_cb_guard(|| {
        if let Some(ctx) = unsafe { run_ctx_of(recv) } {
            if ctx.errors.len() < 16 {
                ctx.errors.push(unsafe { cstr_to_string(msg) });
            }
        }
    })
}

unsafe extern "C-unwind" fn recv_begin_preset(
    recv: *const ffi::ClapPresetDiscoveryMetadataReceiver,
    name: *const c_char,
    load_key: *const c_char,
) -> bool {
    host_cb_guard(|| {
        let Some(ctx) = (unsafe { run_ctx_of(recv) }) else {
            return false;
        };
        if name.is_null() {
            return true; // name is mandatory; ignore incomplete entries
        }
        if ctx.out.len() >= DISCOVERY_MAX_PRESETS {
            return false; // tells the provider to stop calling back
        }
        ctx.out.push(PresetLocation {
            name: unsafe { cstr_to_string(name) },
            load_key: unsafe {
                if load_key.is_null() {
                    None
                } else {
                    Some(cstr_to_string(load_key))
                }
            },
            kind: ctx.kind,
            location: ctx.location.clone(),
            factory: ctx.factory,
        });
        true
    })
}

unsafe extern "C-unwind" fn recv_pid(
    _r: *const ffi::ClapPresetDiscoveryMetadataReceiver,
    _pid: *const ffi::ClapUniversalPluginId,
) {
    host_cb_guard(|| {
    })
}
unsafe extern "C-unwind" fn recv_str1(
    _r: *const ffi::ClapPresetDiscoveryMetadataReceiver,
    _s: *const c_char,
) {
    host_cb_guard(|| {
    })
}
unsafe extern "C-unwind" fn recv_ts(_r: *const ffi::ClapPresetDiscoveryMetadataReceiver, _a: u64, _b: u64) {
    host_cb_guard(|| {
    })
}
unsafe extern "C-unwind" fn recv_str2(
    _r: *const ffi::ClapPresetDiscoveryMetadataReceiver,
    _a: *const c_char,
    _b: *const c_char,
) {
    host_cb_guard(|| {
    })
}

/// One `get_metadata` call against one file (or the plugin container).
fn discovery_run(
    prov: *const ffi::ClapPresetDiscoveryProvider,
    kind: u32,
    location: Option<&str>,
    factory: bool,
    out: &mut Vec<PresetLocation>,
) {
    let mut run = RunCtx {
        kind,
        location: location.map(|s| s.to_string()),
        factory,
        out: Vec::new(),
        errors: Vec::new(),
    };
    let recv = ffi::ClapPresetDiscoveryMetadataReceiver {
        receiver_data: std::ptr::from_mut(&mut run).cast::<c_void>(),
        on_error: recv_on_error,
        begin_preset: recv_begin_preset,
        add_plugin_id: recv_pid,
        set_soundpack_id: recv_str1,
        add_creator: recv_str1,
        set_description: recv_str1,
        set_timestamps: recv_ts,
        add_feature: recv_str1,
        add_extra_info: recv_str2,
    };
    let loc_c = location.and_then(|s| CString::new(s).ok());
    let ok = unsafe {
        ((*prov).get_metadata)(
            prov,
            kind,
            loc_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &recv,
        )
    };
    // A false verdict means "this file yielded nothing" — its on_error
    // (if any) is collected in run.errors; valid files still contribute.
    if run.out.is_empty() && (!ok || !run.errors.is_empty()) {
        eprintln!(
            "[clap:discovery] {} (\"{}\")",
            run.errors
                .first()
                .map(|s| s.as_str())
                .unwrap_or("provider declined the location"),
            location.unwrap_or("(plugin)")
        );
    }
    out.append(&mut run.out);
}

/// Declared filetype filter: no (or empty) declarations match anything.
fn filetype_match(exts: &[String], path: &Path) -> bool {
    if exts.is_empty() || exts.iter().any(|e| e.is_empty()) {
        return true;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => exts.contains(&ext.to_lowercase()),
        None => false,
    }
}

unsafe extern "C-unwind" fn host_get_extension(
    _host: *const ffi::ClapHost,
    id: *const c_char,
) -> *const c_void {
    host_cb_guard(|| {
        if id.is_null() {
            return std::ptr::null();
        }
        let id = CStr::from_ptr(id);
        if id.to_bytes() == b"clap.log" {
            return &HOST.log as *const _ as *const c_void;
        }
        // v67: clap.preset-load - the plugin's notification channel for
        // presets IT loaded (the CLAP v1 ABI has no host-side enumeration).
        // Primary + compat ids both answered.
        if id == ffi::CLAP_EXT_PRESET_LOAD_C || id == ffi::CLAP_EXT_PRESET_LOAD_COMPAT_C {
            return &PRESET_LOAD as *const _ as *const c_void;
        }
        std::ptr::null()
    })
}

unsafe extern "C-unwind" fn host_noop(_host: *const ffi::ClapHost) { host_cb_guard(|| {}) }

unsafe extern "C-unwind" fn host_log_cb(_host: *const ffi::ClapHost, severity: u32, msg: *const c_char) {
    host_cb_guard(|| {
        if msg.is_null() {
            return;
        }
        let m = CStr::from_ptr(msg).to_string_lossy();
        // v117 test trap (pin v117_clap_host_log_panic_survives): panic the
        // host callback while plugin frames sit below it on the stack. The
        // host_cb_guard boundary catch must convert this into the default
        // return; remove the boundary catch and the SAME trap unwinds into
        // the plugin's C++ layer and ABORTS the process ("Rust panics must
        // be rethrown") — that is the bug class this pin holds shut.
        #[cfg(test)]
        if std::env::var_os("SLOOPER_TEST_PANIC_HOST_LOG").is_some() {
            V117_TRAP_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            panic!("v117 test trap: panic inside the CLAP host log callback");
        }
        // Fatal/error always print; lower severities behind an env flag so a
        // chatty plugin cannot spam a release session.
        if severity <= 2 || std::env::var_os("SLOOPER_CLAP_LOG").is_some() {
            eprintln!("[clap:{severity}] {m}");
        }
    })
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

static LIBS: LazyLock<Mutex<HashMap<PathBuf, CachedLib>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
            .map_err(|_| {
                format!(
                    "'{}' exports no clap_entry (not a CLAP plugin?)",
                    path.display()
                )
            })?;
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
    const INSTRUMENT_FEATURES: [&str; 6] = [
        "instrument",
        "note-effect",
        "synthesizer",
        "sampler",
        "drum",
        "drum-machine",
    ];
    feats
        .iter()
        .any(|f| INSTRUMENT_FEATURES.contains(&f.as_str()))
}

// ================================================================= scan ===

/// Probe one `.clap` file; one [`ScanProbe`] per plugin inside it. NEVER
/// instantiates: descriptors answer metadata without a plugin object
/// (the v17 "never touch a live library twice" rule at its safest).
#[derive(Debug)]
pub struct ScanProbe {
    pub plugin_id: String,
    pub name: String,
    pub vendor: String,
    pub instrument: bool,
}

pub fn probe(path: &Path) -> Result<Vec<ScanProbe>, String> {
    let _loader = vst_loader_lock();
    probe_unlocked(path)
}

/// `probe` WITHOUT the loader lock — v37: this is the body the forked
/// scan child runs (it inherits the lock as held-by-nobody and MUST NOT
/// re-take it; the parent holds it across the fork).
/// v116: guarded — a C++ throw from the factory calls must reach the scan
/// as a probe ERROR (file listed as unloadable), never as std::terminate.
pub(crate) fn probe_unlocked(path: &Path) -> Result<Vec<ScanProbe>, String> {
    match crate::ffiguard::guarded(|| probe_inner(path)) {
        Ok(r) => r,
        Err(e) => Err(format!("CLAP plugin threw during probe (bridge lost?): {e}")),
    }
}

fn probe_inner(path: &Path) -> Result<Vec<ScanProbe>, String> {
    let entry = entry_for(path)?;
    let factory =
        unsafe { factory_of(entry) }.ok_or_else(|| "no clap.plugin-factory".to_string())?;
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

unsafe extern "C-unwind" fn cb_in_size(list: *const ffi::ClapInputEvents) -> u32 {
    host_cb_guard(|| {
        let b = unsafe { &*((*list).ctx as *const InBuf) };
        b.ptrs.len() as u32
    })
}

unsafe extern "C-unwind" fn cb_in_get(
    list: *const ffi::ClapInputEvents,
    index: u32,
) -> *const ffi::ClapEventHeader {
    host_cb_guard(|| {
        let b = unsafe { &*((*list).ctx as *const InBuf) };
        match b.ptrs.get(index as usize) {
            Some(p) => *p,
            None => std::ptr::null(),
        }
    })
}

unsafe extern "C-unwind" fn cb_out_try_push(
    list: *const ffi::ClapOutputEvents,
    event: *const ffi::ClapEventHeader,
) -> bool {
    host_cb_guard(|| {
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
    })
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
    /// v116: parked (dead) — set on the audio thread by the RT guard or
    /// the error streak; AtomicBool + Relaxed so `StageEntry::is_dead`
    /// (UI/engine bypass display) reads it across threads, matching the
    /// `RtStageEntry::dead` contract.
    dead: AtomicBool,
    note_id: i32,
    /// v103 (card t_62c5a14e): bend-range trio — see vsthost::RtState;
    /// same currency rule (queue() rescales roll-space 0xE0, RPN0 rides
    /// the raw-MIDI arm of fill_events as plain CC events).
    bend_range: std::sync::Arc<AtomicU8>,
    bend_seen: u8,
    bend_announced: u16,
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
    state_ext: *const ffi::ClapPluginState,
    /// v79: plugin side of `clap.preset-load/2` (null = unsupported —
    /// `supports_preset_load` answers false). Queried once at open.
    preset_load: *const ffi::ClapPluginPresetLoad,
    /// v79: entry-level preset-discovery factory (null = plugin ships
    /// none; the UI then offers only the file entry point).
    discovery_factory: *const ffi::ClapPresetDiscoveryFactory,
    /// v79: `preset_locations()` result, computed once per entry under
    /// `vst_loader_lock` (never plugin code per call afterwards).
    preset_cache: std::sync::Mutex<Option<Vec<PresetLocation>>>,
    /// 0 = plugin declares no audio input ports (an instrument).
    in_ports: u32,
    midi_drops: std::sync::Arc<AtomicU64>,
    /// v67: plugin-side preset-load reports (CLAP `clap.preset-load`).
    preset_slot: std::sync::Arc<PresetSlot>,
    /// v103: per-track pitch-bend range (see vsthost::StageEntry).
    bend_range: std::sync::Arc<AtomicU8>,
    /// Test-only read-back path (fixture probe params): the params
    /// extension captured at open. NEVER called while processing.
    #[cfg(test)]
    params_ext: *const ffi::ClapPluginParams,
}

unsafe impl Send for ClapStageEntry {}
unsafe impl Sync for ClapStageEntry {}

impl ClapStageEntry {
    /// Test-only (v103, card t_62c5a14e): read a PLAIN parameter value
    /// straight from the plugin (the fixture's probe params 4/5 record
    /// received pitch bend / RPN-0 data). Same caller-owns-the-plugin
    /// contract as a `process` driver — fixture pins are solo.
    #[cfg(test)]
    pub(crate) fn plug_value(&self, id: u32) -> Option<f64> {
        if self.params_ext.is_null() {
            return None;
        }
        let mut out = 0.0f64;
        let ok =
            unsafe { ((*self.params_ext).get_value)(self.plugin, id, &mut out) };
        if ok {
            Some(out)
        } else {
            None
        }
    }
}

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
    /// Normalized cell value -> the plugin's PLAIN range (converted at the
    /// event boundary; CLAP params speak plain, slooper cells speak 0..1).
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
        // v103: same currency seam as vsthost::RtStageEntry::queue —
        // roll-space bends rescale to the track range, RPN0 announces
        // per channel before the first bend since the last change.
        if m[0] & 0xF0 == 0xE0 {
            let r = s.bend_range.load(Ordering::Relaxed);
            let bit = 1u16 << (m[0] & 0x0F);
            if s.bend_seen == u8::MAX || s.bend_announced & bit == 0 {
                for b in crate::vsthost::rpn0_burst(m[0] & 0x0F, r) {
                    self.push_q(s, b, pos);
                }
                s.bend_announced |= bit;
            }
            let v = crate::vsthost::bend_rescale(((m[2] as u16) << 7) | (m[1] as u16), r);
            return self.push_q(s, [m[0], (v & 0x7F) as u8, ((v >> 7) & 0x7F) as u8], pos);
        }
        self.push_q(s, m, pos)
    }

    fn push_q(&self, s: &mut RtState, m: [u8; 3], pos: i64) {
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
        // v141 (card t_342d8563): stage from the PERSISTENT field. The old
        // code did `let deliver = std::mem::take(&mut s.deliver)` to release
        // the `&mut s` borrow before the push_* calls — but the take left the
        // field default-empty, so EVERY eventful block malloc'd a fresh Vec
        // in the partition loop above and freed it again when the local
        // dropped: RT heap churn exactly the class the module's "allocates
        // nothing" rule forbids. No snapshot is needed for the plugin
        // log-reentry aliasing that comment feared: push_* only write the
        // event pool, they never call plugin code — the plugin runs later,
        // in process_block, once this fn's borrow of `s` is dead. Iterate by
        // index and copy each (m, pos) pair out so no borrow of `s.deliver`
        // is live across a push (mirrors vsthost::deliver_midi, which has
        // always reused its field). `s.deliver` then keeps its capacity
        // across blocks: clear+push reallocates only while still growing.
        for k in 0..s.deliver.len() {
            let (m, pos) = (s.deliver[k].m, s.deliver[k].pos);
            let t = (pos - start).clamp(0, VST_BLOCK as i64 - 1) as u32;
            let status = m[0] & 0xF0;
            let ch = (m[0] & 0x0F) as i16;
            match status {
                0x90 if m[2] > 0 => self.push_note(
                    s,
                    t,
                    ffi::CLAP_EVENT_NOTE_ON,
                    ch,
                    m[1],
                    m[2] as f64 / 127.0,
                ),
                // 0x90 with velocity 0 is note off by the classic convention.
                0x80 | 0x90 => self.push_note(s, t, ffi::CLAP_EVENT_NOTE_OFF, ch, m[1], 0.0),
                // CC / program / pressure / pitch bend / poly AT travel as
                // raw bytes: spec says plugins should take both forms, and
                // every CLAP plugin on this host does. v103: pitch bend
                // (0xE0) was MISSING here — bends were silently dropped for
                // CLAP entirely. v104 (card t_0ab25d9a): poly aftertouch
                // (0xA0) was the same hole one status nibble over — the MPE
                // voice message vanished for CLAP while VST2/VST3 carried
                // it (VST2 raw batch; VST3 0xA0 => PolyAftertouch since
                // v42/MPE). Engine-side split (documented, decision (a)):
                // SCRIPTED synths still have no poly-AT callback (kind-4
                // dispatch arms drop) — follow-up feature card, not here.
                0xA0 | 0xB0 | 0xC0 | 0xD0 | 0xE0 => self.push_midi(s, t, m),
                _ => {}
            }
        }
    }

    fn ev_place(s: &mut RtState, len: usize) -> Option<&mut EvSlot> {
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
            // YANES (org.yanes.native) silently drops note events whose
            // port_index is -1 (spec-wildcard); we host exactly one note
            // port, and the raw-MIDI path uses port 0 too — send 0.
            port_index: 0,
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
    /// v116: the plugin vtable trips here run inside the [`guarded`] block
    /// loop in [`StageEntry::process`] — a C++ throw unwinds to the host
    /// catch, which PARKS the entry before the next call reaches this fn.
    unsafe fn process_block(&self, s: &mut RtState) {
        if s.dead.load(Ordering::Relaxed) {
            s.stage_ol.fill(0.0);
            s.stage_or.fill(0.0);
            return;
        }
        if !s.started {
            // First block on the REAL audio thread: activate processing.
            if !((*self.plugin).start_processing)(self.plugin) {
                s.dead.store(true, Ordering::Relaxed);
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
            audio_inputs: if self.in_ports > 0 {
                &s.in_buf
            } else {
                std::ptr::null()
            },
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
                s.dead.store(true, Ordering::Relaxed);
            }
            s.stage_ol.fill(0.0);
            s.stage_or.fill(0.0);
        } else {
            s.err_streak = 0;
        }
    }
}

// ====================================================== StageEntry impl ===

impl ClapStageEntry {
    /// v79: full discovery-provider handshake — provider create + init,
    /// declared-location crawl, per-file metadata reads. Caller MUST
    /// hold `vst_loader_lock` (via [`StageEntry::preset_locations`]).
    /// File locations are crawled ONE level (files only, sorted, capped)
    /// and filtered by the provider's declared filetypes.
    unsafe fn discover_unlocked(&self) -> Vec<PresetLocation> {
        let f = self.discovery_factory;
        if f.is_null() {
            return Vec::new();
        }
        let mut out: Vec<PresetLocation> = Vec::new();
        let n = std::cmp::min(unsafe { ((*f).count)(f) }, DISCOVERY_MAX_PROVIDERS);
        for i in 0..n {
            let d = unsafe { ((*f).get_descriptor)(f, i) };
            if d.is_null() || unsafe { (*d).id.is_null() } {
                continue;
            }
            let idc = match unsafe { CStr::from_ptr((*d).id).to_str() } {
                Ok(s) => match CString::new(s) {
                    Ok(c) => c,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            let ctx = Box::leak(Box::new(IndexCtx::default()));
            let indexer = Box::leak(Box::new(ffi::ClapPresetDiscoveryIndexer {
                clap_version: ffi::ClapVersion {
                    major: 1,
                    minor: 2,
                    revision: 0,
                },
                name: HOST.name.as_ptr(),
                vendor: HOST.vendor.as_ptr(),
                url: HOST.url.as_ptr(),
                version: HOST.version.as_ptr(),
                indexer_data: ctx as *const IndexCtx as *mut c_void,
                declare_filetype: idx_declare_filetype,
                declare_location: idx_declare_location,
                declare_soundpack: idx_declare_soundpack,
                get_extension: idx_get_extension,
            }));
            let prov = unsafe { ((*f).create)(f, indexer, idc.as_ptr()) };
            if prov.is_null() {
                continue;
            }
            if !unsafe { ((*prov).init)(prov) } {
                continue;
            }
            let exts: Vec<String> = ctx.filetypes.lock().map(|v| v.clone()).unwrap_or_default();
            let locs: Vec<(u32, u32, Option<String>)> =
                ctx.locations.lock().map(|v| v.clone()).unwrap_or_default();
            for (kind, flags, loc) in locs {
                if out.len() >= DISCOVERY_MAX_PRESETS {
                    break;
                }
                let factory = flags & ffi::CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT != 0;
                match kind {
                    PRESET_KIND_PLUGIN => {
                        discovery_run(prov, kind, None, factory, &mut out);
                    }
                    PRESET_KIND_FILE => {
                        let Some(l) = loc else { continue };
                        let p = PathBuf::from(&l);
                        if p.is_file() {
                            discovery_run(prov, kind, Some(&l), factory, &mut out);
                        } else if let Ok(rd) = std::fs::read_dir(&p) {
                            let mut files: Vec<PathBuf> = rd
                                .filter_map(|e| e.ok())
                                .map(|e| e.path())
                                .filter(|ep| ep.is_file())
                                .collect();
                            files.sort();
                            files.truncate(DISCOVERY_MAX_FILES);
                            for fp in files {
                                if !filetype_match(&exts, &fp) {
                                    continue;
                                }
                                let Some(s) = fp.to_str() else { continue };
                                discovery_run(prov, kind, Some(s), factory, &mut out);
                                if out.len() >= DISCOVERY_MAX_PRESETS {
                                    break;
                                }
                            }
                        }
                    }
                    _ => {} // unknown location kind: nothing safe to crawl
                }
            }
        }
        out
    }
}

impl StageEntry for ClapStageEntry {
    /// v116: an RT guard catch PARKS the entry — surface that through the
    /// v42 seam so engine/UI bypass display treats a dead CLAP exactly
    /// like a dead VST (Relaxed mirror, same discipline as RtStageEntry).
    fn is_dead(&self) -> bool {
        unsafe { (*self.state.get()).dead.load(Ordering::Relaxed) }
    }
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
    fn set_bend_range(&self, st: u8) {
        self.bend_range.store(st.min(24), Ordering::Relaxed);
    }
    fn bend_range(&self) -> u8 {
        self.bend_range.load(Ordering::Relaxed)
    }

    fn take_preset_report(&self) -> Option<crate::vsthost::PresetLoadEvent> {
        self.preset_slot
            .report
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// v79: true for CLAP entries whose plugin hosts `clap_plugin_preset_load`.
    fn supports_preset_load(&self) -> bool {
        !self.preset_load.is_null()
    }

    /// v79: run the preset-discovery handshake once (provider create +
    /// init, declared-location crawl, per-file metadata), cache forever.
    /// The provider/indexer stay alive for the process lifetime — spec
    /// requires the indexer to outlive the provider and entries are
    /// never destroyed (standing leak policy). Takes `vst_loader_lock`
    /// itself, so callers (UI thread) hold nothing.
    fn preset_locations(&self) -> Vec<PresetLocation> {
        if self.preset_load.is_null() {
            return Vec::new();
        }
        let mut cache = self.preset_cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(v) = cache.as_ref() {
            return v.clone();
        }
        let _loader = vst_loader_lock();
        // v116: the discovery handshake (provider count/init/get_metadata)
        // is plugin code — guarded. A caught throw caches the EMPTY list:
        // retrying per UI refresh would only re-throw, and the parameters
        // window just shows no presets (echo on stderr).
        let v = match crate::ffiguard::guarded(|| unsafe { self.discover_unlocked() }) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[slooper] CLAP plugin threw during preset discovery (bridge lost?): {e} \u{201c}{}\u{201d} — no presets listed",
                    self.name
                );
                Vec::new()
            }
        };
        *cache = Some(v.clone());
        v
    }

    /// v79: command the LIVE instance — `from_location(kind, location,
    /// load_key)` under `vst_loader_lock` (control thread = the plugin's
    /// spec-defined main thread; same discipline class as the CLAP open
    /// and chunk paths). The report this command provokes is DRAINED
    /// here: our verdict is authoritative, and the PARAMETERS pump must
    /// not echo our own command back as a plugin-side change later.
    fn load_preset_location(
        &self,
        kind: u32,
        location: Option<&str>,
        load_key: Option<&str>,
    ) -> Result<(), String> {
        if self.preset_load.is_null() {
            return Err("plugin does not host clap_plugin_preset_load".into());
        }
        let loc_c = match location {
            Some(s) => Some(
                CString::new(s).map_err(|_| "preset location contains a NUL byte".to_string())?,
            ),
            None => None,
        };
        let key_c = match load_key {
            Some(s) => Some(
                CString::new(s).map_err(|_| "preset load_key contains a NUL byte".to_string())?,
            ),
            None => None,
        };
        let _loader = vst_loader_lock();
        // v116: from_location is plugin code — guarded. A caught throw
        // surfaces as a plain load ERROR (the PARAMETERS window echoes it);
        // the report slot is drained either way so a swallowed throw can
        // not leave a ghost report for the pump to echo later.
        let trip = crate::ffiguard::guarded(|| unsafe {
            ((*self.preset_load).from_location)(
                self.plugin,
                kind,
                loc_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                key_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        });
        let report = self
            .preset_slot
            .report
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let ok = match trip {
            Ok(ok) => ok,
            Err(e) => {
                return Err(format!(
                    "CLAP plugin \u{201c}{}\u{201d} threw during preset load (bridge lost?): {e}",
                    self.name
                ))
            }
        };
        if ok {
            if let Some(PresetLoadEvent::Failed { message, .. }) = report {
                return Err(message); // accepted, then contradicted: error wins
            }
            Ok(())
        } else {
            Err(match report {
                Some(PresetLoadEvent::Failed { message, .. }) => message,
                _ => "the plugin refused the preset load".to_string(),
            })
        }
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
        unsafe extern "C-unwind" fn wr(
            stream: *const ffi::ClapOStream,
            data: *const c_void,
            n: u64,
        ) -> i64 {
            host_cb_guard(|| {
                let buf = unsafe { &mut *((*stream).ctx as *mut Vec<u8>) };
                let src = unsafe { std::slice::from_raw_parts(data as *const u8, n as usize) };
                buf.extend_from_slice(src);
                n as i64
                    })
        }
        let os = ffi::ClapOStream {
            ctx: &mut buf as *mut Vec<u8> as *mut c_void,
            write: wr,
        };
        let ok = match crate::ffiguard::guarded(|| unsafe {
            ((*self.state_ext).save)(self.plugin, &os)
        }) {
            Ok(ok) => ok,
            // v116: a save-time throw (dead bridge) degrades to "no state"
            // — the entry is NOT parked (GUI-side fault, RT may be fine).
            Err(e) => {
                eprintln!(
                    "[slooper] CLAP plugin threw during state save (bridge lost?): {e} \u{201c}{}\u{201d} — state not captured",
                    self.name
                );
                return None;
            }
        };
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

        // v116: FIREWALL BOUNDARY for the CLAP RT path — the twin of the
        // v42 guard at `RtStageEntry::process`. A plugin that throws
        // (dead wine bridge, hostile C++ internals) is caught here, PARKED,
        // and bypassed from the next block on; the DAW survives. The loop
        // region is deliberately destructor-free on the unwind path (all
        // buffers live in `RtState`), so the foreign exception only runs
        // drop glue on plugin frames on its way to the C++ catch.
        if let Err(e) = crate::ffiguard::guarded(|| {
            while s.in_ring.len() >= VST_BLOCK {
                s.in_ring.drain_into(&mut s.stage_l);
                s.stage_r.copy_from_slice(&s.stage_l);
                s.stage_ol.fill(0.0);
                s.stage_or.fill(0.0);
                // v103: adopt range change, announce eagerly on ch0 (see the
                // vsthost process_inner twin for the full contract).
                let br = s.bend_range.load(Ordering::Relaxed);
                if br != s.bend_seen {
                    if s.bend_seen != u8::MAX {
                        s.bend_announced = 0;
                    }
                    for m in crate::vsthost::rpn0_burst(0, br) {
                        self.push_q(s, m, s.abs_processed);
                    }
                    s.bend_announced |= 1;
                    s.bend_seen = br;
                }
                // SAFETY: audio thread only; `s` is the single live RtState.
                unsafe { self.process_block(s) };
                s.abs_processed += VST_BLOCK as i64;
                let got = s.stage_ol.len().min(VST_BLOCK);
                s.out_ring.push_slice(&s.stage_ol[..got]);
            }
        }) {
            s.dead.store(true, Ordering::Relaxed);
            // A throw can interrupt `fill_events` mid-staging: drop the
            // staged list so the plugin is never handed stale event slots.
            s.ev_count = 0;
            s.inbuf_data.ptrs.clear();
            eprintln!(
                "[slooper] CLAP plugin threw during process (bridge lost?): {e} \u{201c}{}\u{201d} bypassed; remove and re-add it to restore sound",
                self.name
            );
            output[..n].fill(0.0);
            return;
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
        // v141: `deliver` persists across blocks (no per-block take), so
        // this block's staged CONTENT is dropped here; capacity stays warm
        // for the next partition. Nothing reads it between process() calls.
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
/// v116: guarded end-to-end — the WHOLE handshake is plugin code (factory
/// walk, create_plugin, init, get_extension, params, ports, activate,
/// state.load). Any throw becomes a load ERROR (chain row shows the
/// failure), never std::terminate. A half-built instance stays leaked
/// (standing policy — destroy semantics of a thrown init are plugin-defined).
pub(crate) fn open_unlocked(
    path: &Path,
    plugin_id: Option<&str>,
    sample_rate: u32,
    saved: Option<&[f32]>,
    state: Option<&[u8]>,
) -> Result<std::sync::Arc<ClapStageEntry>, String> {
    match crate::ffiguard::guarded(|| open_inner(path, plugin_id, sample_rate, saved, state)) {
        Ok(r) => r,
        Err(e) => Err(format!("CLAP plugin threw during open (bridge lost?): {e}")),
    }
}

fn open_inner(
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

    // v67: per-entry `clap_host_t` - `host_data` routes this plugin's
    // preset-load reports back to the owning entry (the vtable itself
    // stays one global). The Box and the Arc clone inside host_data are
    // leaked on purpose: plugins are never unloaded, entries never
    // destroyed (standing leak policy).
    let preset_slot = std::sync::Arc::new(PresetSlot::default());
    let host = Box::leak(Box::new(ffi::ClapHost {
        clap_version: ffi::ClapVersion {
            major: 1,
            minor: 2,
            revision: 0,
        },
        host_data: std::sync::Arc::into_raw(preset_slot.clone()) as *mut c_void,
        name: HOST.name.as_ptr(),
        vendor: HOST.vendor.as_ptr(),
        url: HOST.url.as_ptr(),
        version: HOST.version.as_ptr(),
        get_extension: host_get_extension,
        request_restart: host_noop,
        request_process: host_noop,
        request_callback: host_noop,
    }));
    let plugin = unsafe {
        let id =
            CString::new(CStr::from_ptr((*desc).id).to_str().unwrap_or("")).unwrap_or_default();
        (factory.create_plugin)(factory, host, id.as_ptr())
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
    // v79: plugin side of host-commanded preset load (primary id, compat
    // fallback for draft-era plugins). Null = plugin declines loads.
    let mut preset_load = get_ext(ffi::CLAP_EXT_PRESET_LOAD_C) as *const ffi::ClapPluginPresetLoad;
    if preset_load.is_null() {
        preset_load = get_ext(ffi::CLAP_EXT_PRESET_LOAD_COMPAT_C) as *const ffi::ClapPluginPresetLoad;
    }
    // v79: entry-level preset-discovery factory pointer probed under the
    // open lock; the handshake itself is LAZY (first `preset_locations()`).
    let get_factory = entry.get_factory;
    let mut discovery_factory =
        unsafe { get_factory(ffi::CLAP_PRESET_DISCOVERY_FACTORY_ID_C.as_ptr()) }
            as *const ffi::ClapPresetDiscoveryFactory;
    if discovery_factory.is_null() {
        discovery_factory =
            unsafe { get_factory(ffi::CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT_C.as_ptr()) }
                as *const ffi::ClapPresetDiscoveryFactory;
    }

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
            let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let writable =
                info.flags & (ffi::CLAP_PARAM_IS_READONLY | ffi::CLAP_PARAM_IS_HIDDEN) == 0;
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
            // v74: bridge-kit metadata. The ABI hands us the PLAIN range
            // (with the documented linear default normalization), and
            // IS_ENUM declares an integer picker; choice labels are
            // SAMPLED from the plugin's own value_to_text — at most 64
            // read-only calls at load, never during a drag.
            let steps = if info.flags & ffi::CLAP_PARAM_IS_ENUM != 0 {
                clap_enum_steps(info.min_value, info.max_value)
            } else {
                0
            };
            let enum_labels = if steps > 0 {
                crate::vsthost::sample_step_labels(steps, false, |i| {
                    let mut buf = [0 as c_char; 256];
                    let ok = unsafe {
                        ((*params_ext).value_to_text)(
                            plugin,
                            info.id,
                            info.min_value + i as f64,
                            buf.as_mut_ptr(),
                            buf.len() as u32,
                        )
                    };
                    if !ok {
                        return None;
                    }
                    let s = unsafe { CStr::from_ptr(buf.as_ptr()) }
                        .to_string_lossy()
                        .trim()
                        .to_string();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                })
            } else {
                Vec::new()
            };
            descs.push(VstParamDesc {
                id: info.id,
                name,
                default,
                writable,
                program_change: false,
                unit: 0,
                unit_text: String::new(),
                plain_min: info.min_value as f32,
                plain_max: info.max_value as f32,
                steps,
                enum_labels,
            });
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
                out_channels = info.channel_count.clamp(1, 2);
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
            struct Cur {
                bytes: Vec<u8>,
                pos: usize,
            }
            unsafe extern "C-unwind" fn rd(
                stream: *const ffi::ClapIStream,
                buffer: *mut c_void,
                n: u64,
            ) -> i64 {
                host_cb_guard(|| {
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
                            })
            }
            let mut cur = Cur {
                bytes: bytes.to_vec(),
                pos: 0,
            };
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

    let bend_range = std::sync::Arc::new(AtomicU8::new(crate::vsthost::DEFAULT_BEND_RANGE));
    let st = RtState {
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
        dead: AtomicBool::new(false),
        note_id: 0,
        bend_range: bend_range.clone(),
        bend_seen: u8::MAX,
        bend_announced: 0,
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
        state_ext,
        preset_load,
        discovery_factory,
        preset_cache: std::sync::Mutex::new(None),
        in_ports,
        midi_drops: std::sync::Arc::new(AtomicU64::new(0)),
        preset_slot,
        bend_range,
        #[cfg(test)]
        params_ext,
    });
    unsafe {
        let s = &mut *std::sync::Arc::get_mut(&mut e).unwrap().state.get();
        // Mono pipeline: feed the plugin a stereo buffer (L=R) when it
        // declares an input port; take L back out.
        s.in_chans = vec![s.stage_l.as_mut_ptr(), s.stage_r.as_mut_ptr()];
        s.out_chans = vec![s.stage_ol.as_mut_ptr(), s.stage_or.as_mut_ptr()];
        s.in_buf.data32 = s.in_chans.as_mut_ptr();
        s.in_buf.channel_count = 2;
        s.out_buf.data32 = s.out_chans.as_mut_ptr();
        s.out_buf.channel_count = s.out_buf.channel_count.clamp(1, 2);
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

/// v74: CLAP declared-enum → discrete value count. An enum is an integer
/// parameter spanning `min..=max`; the picker cap is 64 CHOICES (must
/// match `sample_step_labels`' accepted range) and both bounds must be
/// whole numbers, otherwise treat it as continuous (0).
pub(crate) fn clap_enum_steps(min: f64, max: f64) -> u32 {
    let span = max - min;
    if !(1.0..=63.0).contains(&span) {
        return 0;
    }
    if span.fract().abs() > 1e-6 || min.fract().abs() > 1e-6 || max.fract().abs() > 1e-6 {
        return 0;
    }
    span as u32 + 1
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
pub(crate) mod tests {
    use super::*;

    /// v74 (TS-002 R4): the pure CLAP enum math. Only integer min/max
    /// within the shared 64-choice cap become stepped descriptors; the
    /// IS_ENUM bit rides the real ABI value.
    #[test]
    fn v74_clap_enum_steps_pure() {
        assert_eq!(ffi::CLAP_PARAM_IS_ENUM, 1 << 4, "CLAP ABI bit position");
        assert_eq!(clap_enum_steps(0.0, 3.0), 4, "0..=3 = four choices");
        assert_eq!(clap_enum_steps(10.0, 13.0), 4, "non-zero whole min");
        assert_eq!(clap_enum_steps(0.0, 0.5), 0, "non-integer span = continuous");
        assert_eq!(clap_enum_steps(0.1, 3.0), 0, "non-integer min rejected");
        assert_eq!(clap_enum_steps(0.0, 100.0), 0, "above the cap = continuous");
        assert_eq!(clap_enum_steps(0.0, 63.0), 64, "cap edge matches the sampler");
        assert_eq!(clap_enum_steps(0.0, 64.0), 0, "one past the cap declines");
    }

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
        let id = if which == "gain" {
            "com.slooper.fixture-gain"
        } else {
            "com.slooper.fixture-sine"
        };
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
        let gain = probes
            .iter()
            .find(|x| x.plugin_id == "com.slooper.fixture-gain")
            .unwrap();
        let sine = probes
            .iter()
            .find(|x| x.plugin_id == "com.slooper.fixture-sine")
            .unwrap();
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
        let entries = crate::vsthost::scan(std::slice::from_ref(&dir));
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
        assert!(
            !entries[0].path.to_string_lossy().ends_with('#'),
            "path stays clean of the #id tail"
        );
    }

    #[test]
    fn v34_clap_open_reads_normalized_params() {
        let e = fixture_entry("gain");
        // v67: fixture-gain gained `fixload` (the preset-load trigger) - 3.
        // v103: +2 HIDDEN probe params (bend currency) - 5.
        // v104: +3 HIDDEN probe params (poly-AT observer, card t_0ab25d9a) - 8.
        // Hidden params stay in descs (v66 doctrine: the mirror must keep
        // them); they are NOT writable, so no new knobs appear in any window.
        assert_eq!(e.descs().len(), 8);
        assert_eq!(e.descs()[0].name, "fixgain");
        // plain default 1.0 on [0,1] normalizes to 1.0
        assert!((e.descs()[0].default - 1.0).abs() < 1e-6);
        assert!(
            (e.params().get(0) - 1.0).abs() < 1e-6,
            "cells seed from plugin, not host"
        );
    }

    /// v67 (goals 3/5, host half): a CLAP plugin that loads a preset
    /// through its OWN machinery must reach the host - the fixture calls
    /// `clap_host_preset_load.loaded` when `fixload` arms. The entry
    /// surfaces the report (drained once, exact location, no container
    /// key), and the plugin-side state change lands in the next chunk so
    /// project save persists it.
    #[test]
    fn v67_clap_preset_load_report_surfaces() {
        let e = fixture_entry("gain");
        assert!(
            e.take_preset_report().is_none(),
            "nothing to report until the plugin loads"
        );
        e.params().set(2, 1.0); // fixload cell (plain == normalized on [0,1])
        let input: Vec<f32> = (0..1536).map(|_| 0.5).collect();
        let mut out = vec![0.0f32; 1536];
        e.process(&input, &mut out); // delivers the PARAM_VALUE event
        let r = e
            .take_preset_report()
            .expect("the fixture reported its preset load");
        match &r {
            crate::vsthost::PresetLoadEvent::Loaded { location, load_key } => {
                assert_eq!(location, "slooper-fixture-factory-preset");
                assert!(load_key.is_none(), "factory preset carries no container key");
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
        assert!(e.take_preset_report().is_none(), "reports drain once");
        let chunk = e.chunk().expect("state saved");
        assert!(
            chunk.starts_with(b"F1 0.75"),
            "the plugin's own preset state reaches the chunk: {:?}",
            String::from_utf8_lossy(&chunk)
        );
    }

    /// v67 negative polarity: no `fixload` arm means no report at all,
    /// and the plugin's untouched state keeps the chunk at the default.
    #[test]
    fn v67_clap_preset_load_silent_until_triggered() {
        let e = fixture_entry("gain");
        let input: Vec<f32> = (0..1024).map(|_| 0.5).collect();
        let mut out = vec![0.0f32; 1024];
        e.process(&input, &mut out);
        assert!(e.take_preset_report().is_none());
        let chunk = e.chunk().expect("state saved");
        assert!(
            chunk.starts_with(b"F1 1.00"),
            "no plugin-side load expected: {:?}",
            String::from_utf8_lossy(&chunk)
        );
    }

    // ============================ v79: host-commanded preset load =======

    /// v79: temp preset dir with a deterministic file set:
    /// multi.fxp (container: 2 keyed presets), single.fxp (whole-file
    /// preset, no key), bad.fxp (yields nothing — the negative control
    /// that must NOT poison the rest of the crawl), decoy.txt (wrong
    /// extension — must be filtered by the declared filetype).
    fn v79_preset_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("v79-presets-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("multi.fxp"),
            "FIXPRESET FA Contained One\nFIXPRESET FB Contained Two\n",
        )
        .unwrap();
        std::fs::write(dir.join("single.fxp"), "FIXPRESET DEFAULT\n").unwrap();
        std::fs::write(dir.join("bad.fxp"), "not a preset file\n").unwrap();
        std::fs::write(dir.join("decoy.txt"), "FIXPRESET FA Should Not Appear\n").unwrap();
        dir
    }

    /// v79 (R1): the extension surface is per-plugin — the gain fixture
    /// hosts `clap_plugin_preset_load`, the sine fixture is the negative
    /// control and must answer "unsupported" everywhere.
    #[test]
    fn v79_clap_preset_load_supported_by_gain_only() {
        let g = fixture_entry("gain");
        assert!(g.supports_preset_load(), "fixture-gain hosts the extension");
        let s = fixture_entry("sine");
        assert!(
            !s.supports_preset_load(),
            "fixture-sine is the negative-polarity control"
        );
        assert!(s.preset_locations().is_empty(), "unsupported entries discover nothing");
        assert!(
            s.load_preset_file(Path::new("/nonexistent.fxp")).is_err(),
            "unsupported entries refuse loads"
        );
    }

    /// v79 (R2): the discovery handshake — provider created + init'd with
    /// OUR indexer vtable, declared locations recorded, the PLUGIN-kind
    /// container read through the metadata receiver. Env-free run: only
    /// the plugin-internal factory presets exist.
    #[test]
    fn v79_clap_discover_plugin_container() {
        std::env::remove_var("SLOOPER_CLAP_FIXTURE_PRESET_DIR");
        let e = fixture_entry("gain");
        let locs = e.preset_locations();
        let names: Vec<&str> = locs.iter().map(|l| l.name.as_str()).collect();
        assert!(
            names.contains(&"Fix Factory A") && names.contains(&"Fix Factory B"),
            "factory presets discovered: {names:?}"
        );
        for l in &locs {
            assert_eq!(l.kind, crate::vsthost::PRESET_KIND_PLUGIN, "kind rides the ABI enum");
            assert!(l.location.is_none(), "plugin container carries no path");
            assert!(l.factory, "location declared FACTORY_CONTENT");
        }
        let a = locs.iter().find(|l| l.name == "Fix Factory A").unwrap();
        assert_eq!(a.load_key.as_deref(), Some("FA"), "container entries carry load keys");
        // Cached: the second call must not re-enter plugin code (same vec,
        // and a plugin that would add on re-init cannot — the cache wins).
        assert_eq!(e.preset_locations().len(), locs.len());
    }

    /// v79 (R2): FILE locations from the provider are crawled by the
    /// host — declared-filetype filter, container + whole-file entries,
    /// and a yielding-failure file that must NOT break the crawl.
    #[test]
    fn v79_clap_discover_file_dir_skips_bad() {
        let dir = v79_preset_dir();
        std::env::set_var("SLOOPER_CLAP_FIXTURE_PRESET_DIR", &dir);
        let locs = fixture_entry("gain").preset_locations();
        std::env::remove_var("SLOOPER_CLAP_FIXTURE_PRESET_DIR");
        let names: Vec<String> = locs.iter().map(|l| l.name.clone()).collect();
        assert!(
            names.iter().any(|n| n == "Contained One") && names.iter().any(|n| n == "Contained Two"),
            "container entries discovered: {names:?}"
        );
        assert!(names.iter().any(|n| n == "Preset DEFAULT"), "whole-file preset discovered: {names:?}");
        assert!(names.iter().any(|n| n == "Fix Factory A"), "plugin container still included");
        assert!(
            !names.iter().any(|n| n.contains("Should Not Appear")),
            "decoy.txt must be filtered by the declared filetype: {names:?}"
        );
        let files: Vec<&PresetLocation> =
            locs.iter().filter(|l| l.kind == crate::vsthost::PRESET_KIND_FILE).collect();
        assert_eq!(files.len(), 3, "multi.fxp yields 2, single.fxp 1, bad.fxp 0");
        let dir_s = dir.display().to_string();
        for l in &files {
            assert!(
                l.location.as_deref().map(|p| p.starts_with(&dir_s)).unwrap_or(false),
                "file preset carries its crawled path: {:?}",
                l.location
            );
            assert!(!l.factory, "FILE location was declared USER_CONTENT");
        }
        let two = locs.iter().find(|l| l.name == "Contained Two").unwrap();
        assert_eq!(two.load_key.as_deref(), Some("FB"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// v79 (R3): picking a discovered PLUGIN-container preset switches
    /// the LIVE instance (proven through the state chunk) and the
    /// plugin's own `loaded` report is DRAINED by the loader — the
    /// PARAMETERS pump must not echo our own command back later.
    #[test]
    fn v79_clap_host_load_factory_preset_changes_state() {
        let e = fixture_entry("gain");
        let locs = e.preset_locations();
        let b = locs.iter().find(|l| l.name == "Fix Factory B").expect("B discovered");
        assert_eq!(e.load_preset_location(b.kind, b.location.as_deref(), b.load_key.as_deref()), Ok(()));
        let chunk = e.chunk().expect("state saved");
        assert!(
            chunk.starts_with(b"F1 0.500"),
            "live instance switched: {:?}",
            String::from_utf8_lossy(&chunk)
        );
        assert!(e.take_preset_report().is_none(), "loader drained the report the command provoked");
    }

    /// v79 (R4): the file entry point — whole-file preset, no load key.
    /// The plugin parsed the file itself AND reported; both land.
    #[test]
    fn v79_clap_host_load_file_plain() {
        let dir = v79_preset_dir();
        let e = fixture_entry("gain");
        assert_eq!(e.load_preset_file(&dir.join("single.fxp")), Ok(()));
        let chunk = e.chunk().expect("state saved");
        assert!(
            chunk.starts_with(b"F1 0.375"),
            "parsed file preset applied: {:?}",
            String::from_utf8_lossy(&chunk)
        );
        assert!(e.take_preset_report().is_none(), "success report drained");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// v79 (R5): refusals surface the plugin's own `on_error` message
    /// through Err, drain the slot, and leave the instance untouched.
    #[test]
    fn v79_clap_host_load_refused_reports_message() {
        let dir = v79_preset_dir();
        let e = fixture_entry("gain");
        let err = e.load_preset_file(&dir.join("bad.fxp")).expect_err("bad file must refuse");
        assert!(err.contains("no preset"), "plugin's own message rides Err: {err}");
        assert!(e.take_preset_report().is_none(), "refusal report drained by the loader");
        let err2 = e
            .load_preset_location(crate::vsthost::PRESET_KIND_PLUGIN, None, Some("ZZ"))
            .expect_err("unknown key must refuse");
        assert!(err2.contains("unknown preset key"), "key refusal: {err2}");
        let chunk = e.chunk().expect("state saved");
        assert!(
            chunk.starts_with(b"F1 1.000"),
            "refusals changed nothing: {:?}",
            String::from_utf8_lossy(&chunk)
        );
        std::fs::remove_dir_all(&dir).ok();
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
        let tail: f32 = out[1200..1536].iter().fold(0.0f32, |m, x| m.max(x.abs()));
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
        assert!(
            peak > 0.4 && peak <= 0.55,
            "A4 full velocity peaks ~0.5, got {peak}"
        );
        let zc = out[1025..1536]
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let expect = (440.0 * 2.0 * 511.0 / 48_000.0) as isize;
        assert!(
            (zc as isize - expect).abs() <= 2,
            "440 Hz expected: zc {zc} vs {expect}"
        );
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
        let e2 = open(
            &p,
            Some("com.slooper.fixture-gain"),
            48_000,
            Some(&[0.25, 1.0]),
            Some(&chunk),
        )
        .unwrap();
        assert!((e2.params().get(0) - 0.25).abs() < 1e-6);
        let mut out = vec![0.0f32; 1536];
        e2.process(&[1.0f32; 1536], &mut out);
        let tail = out[1200..1536].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(
            (tail - 0.25).abs() < 0.05,
            "recovered gain 0.25 expected, got {tail}"
        );
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

    /// v103 (card t_62c5a14e): pitch bend must REACH a CLAP plugin at the
    /// track's announced range. Pre-v103 the 0xE0 status had NO arm in
    /// fill_events — bends were silently dropped for CLAP entirely, so
    /// BOTH asserts are red pre-fix (bend param stays 0, RPN param 0).
    /// The fixture records raw-MIDI pitch bend into hidden param 4 and
    /// the RPN-0 data MSB (while slot 0/0) into hidden param 5.
    #[test]
    fn v103_clap_pitch_bend_reaches_plugin() {
        let e = fixture_entry("gain");
        e.set_bend_range(4);
        assert_eq!(e.bend_range(), 4, "the entry adopts the track range");
        let mut out = vec![0.0f32; 1536]; // 3 CLAP blocks
        // +3 st on ch0 (midipat law: raw 10240).
        let raw = 10240u16;
        let ev = vec![(0usize, [0xE0u8, (raw & 0x7F) as u8, (raw >> 7) as u8])];
        e.process_with_midi(&[0.0f32; 1536], &mut out, &ev);
        let got = e.plug_value(4).expect("probe-bend param reads back");
        let want = crate::vsthost::bend_rescale(raw, 4) as f64;
        assert!(
            (got - want).abs() < 0.5,
            "rescaled bend reaches the plugin: got {got}, want {want}"
        );
        let rpn = e.plug_value(5).expect("probe-rpn param reads back");
        assert_eq!(rpn, 4.0, "RPN0 data-MSB = the range the plugin was told");
        assert_eq!(e.midi_drops(), 0, "the burst never overflows the queue");
    }

    /// v104 (card t_0ab25d9a): poly aftertouch must REACH a CLAP plugin.
    /// Pre-fix the fill_events match armed 0x90/0x80/0xB0/0xC0/0xD0/0xE0
    /// and 0xA0 fell through `_ => {}` — the exact v103 0xE0 failure class
    /// one status nibble over: poly-AT (the core MPE voice message) was
    /// silently dropped for CLAP entirely. ALL asserts are red pre-fix
    /// (count stays 0). The fixture records the raw 0xA0 into hidden
    /// probe params 6 (count), 7 ((ch<<8)|note), 8 (pressure).
    #[test]
    fn v104_clap_poly_at_reaches_plugin() {
        let e = fixture_entry("gain");
        assert_eq!(e.plug_value(6), Some(0.0), "fresh instance saw no poly-AT");
        let mut out = vec![0.0f32; 1536]; // 3 CLAP blocks
        // ch2, note 64, pressure 100.
        let ev = vec![(0usize, [0xA2u8, 64, 100])];
        e.process_with_midi(&[0.0f32; 1536], &mut out, &ev);
        assert_eq!(
            e.plug_value(6),
            Some(1.0),
            "the 0xA0 reached the plugin's event list (pre-v104: dropped)"
        );
        assert_eq!(
            e.plug_value(7),
            Some(((2 << 8) | 64) as f64),
            "channel + note preserved"
        );
        assert_eq!(e.plug_value(8), Some(100.0), "pressure preserved");
        assert_eq!(e.midi_drops(), 0, "the queue never overflowed");
    }

    // ================================================================ v116
    // ffiguard coverage over the CLAP backend (card t_cb95ff24): a C++
    // throw from ANY plugin call site must be caught, surfaced, and
    // survived — never std::terminate. The fixtures are real C++ throws
    // (clap_fixture_throw.cpp), so pre-guard these pins don't fail soft:
    // they ABORT the test process (that IS the bug class). unix-only:
    // Windows builds compile the no-shim `guarded` fallback by design.

    /// Compile the throw-class fixture (C++: `$CXX` || `c++`) into
    /// `target/clap-fixture/fixture-throw.clap`, mtime-stale like
    /// [`fixture_clap`]; None + silent skip when no C++ compiler exists.
    #[cfg(not(windows))]
    pub fn fixture_clap_throw() -> Option<PathBuf> {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let src = manifest.join("vendor/clap-headers/fixture/clap_fixture_throw.cpp");
        let out_dir = manifest.join("target/clap-fixture");
        let out = out_dir.join("fixture-throw.clap");
        let _ = std::fs::create_dir_all(&out_dir);
        let stale = match (std::fs::metadata(&out), std::fs::metadata(&src)) {
            (Ok(o), Ok(s)) => o.modified().ok()? < s.modified().ok()?,
            _ => true,
        };
        if stale {
            let cxx = std::env::var("CXX").unwrap_or_else(|_| "c++".into());
            let status = std::process::Command::new(cxx)
                .arg("-shared")
                .arg("-fPIC")
                .arg("-O1")
                .arg("-I")
                .arg(manifest.join("vendor/clap-headers/include"))
                .arg(&src)
                .arg("-o")
                .arg(&out)
                .stdout(std::process::Stdio::null())
                .status()
                .ok()?;
            if !status.success() {
                return None;
            }
        }
        Some(out)
    }

    #[cfg(not(windows))]
    fn throw_entry(which: &str) -> std::sync::Arc<ClapStageEntry> {
        let p = fixture_clap_throw().expect("throw fixture compiled");
        let id = format!("com.slooper.fixture-throw-{which}");
        open(&p, Some(&id), 48_000, None, None).unwrap_or_else(|e| panic!("open {id}: {e}"))
    }

    /// R1: a throw from `process()` on the audio thread must land in the
    /// guard, PARK the entry (visible through the v42 `is_dead` seam),
    /// emit silence, and keep the process ALIVE — the exact scenario that
    /// pre-v116 took the whole DAW down. Negative control: the park is
    /// per-entry; a healthy plugin still renders.
    #[cfg(not(windows))]
    #[test]
    fn v116_clap_process_throw_parks_and_survives() {
        let e = throw_entry("proc");
        assert!(!e.is_dead(), "fresh entry is not parked");
        let input = [1.0f32; 1024];
        let mut out = [0.0f32; 1024];
        e.process(&input, &mut out);
        assert!(e.is_dead(), "a process() throw must PARK the entry");
        assert!(
            out.iter().all(|s| *s == 0.0),
            "the throwing call must emit silence"
        );
        // Parked stays parked: no further plugin trips, permanent silence.
        let mut out2 = [0.0f32; 512];
        e.process(&input[..512], &mut out2);
        assert!(out2.iter().all(|s| *s == 0.0), "parked entry stays bypassed");
        assert!(e.is_dead());
        // Negative control: a healthy entry (different DSO) still produces.
        let g = fixture_entry("gain");
        let mut gout = [0.0f32; 512];
        g.process(&input[..512], &mut gout);
        assert!(
            gout[500].abs() > 0.5,
            "the gain fixture must still pass audio after the caught throw"
        );
        assert!(!g.is_dead());
    }

    /// R2: a throw from `init()` inside the open handshake must surface as
    /// a load ERROR — the caller (chain row / synth pick) reports failure,
    /// and the backend is usable afterwards.
    #[cfg(not(windows))]
    #[test]
    fn v116_clap_open_throw_init_returns_err() {
        let p = fixture_clap_throw().expect("throw fixture compiled");
        let r = open(&p, Some("com.slooper.fixture-throw-init"), 48_000, None, None);
        let e = r.expect_err("a throwing init() must surface as a load ERROR");
        assert!(e.contains("threw"), "error must name the fault class: {e}");
        // Negative polarity: the backend lives — other plugins still open.
        let g = fixture_entry("gain");
        assert!(!g.descs().is_empty(), "healthy plugin opened normally");
        assert!(!g.is_dead());
    }

    /// R3: a throw from a plugin-factory call during SCAN must reach the
    /// scan as a probe ERROR (the file is reported unloadable, the scan
    /// continues). Env-gated trap scopes the throw to the probe pin only.
    #[cfg(not(windows))]
    #[test]
    fn v116_clap_probe_throw_returns_err() {
        let p = fixture_clap_throw().expect("throw fixture compiled");
        std::env::set_var("SLOOPER_CLAP_FIXTURE_THROW_PROBE", "1");
        let r = probe(&p);
        std::env::remove_var("SLOOPER_CLAP_FIXTURE_THROW_PROBE");
        let e = r.expect_err("a throwing factory call must surface as a probe ERROR");
        assert!(e.contains("threw"), "error must name the fault class: {e}");
        // With the trap off the SAME file probes clean — 5 descriptors
        // (v117 added fixture-throw-hostcb: four -> five).
        let ok = probe(&p).expect("the DSO must probe fine after the caught throw");
        assert_eq!(ok.len(), 5, "all five throw descriptors listed");
    }

    /// R4: a throw during state SAVE (chunk harvest, GUI/save thread)
    /// degrades to "no state captured" — and does NOT park the RT entry
    /// (the audio thread may be perfectly healthy).
    #[cfg(not(windows))]
    #[test]
    fn v116_clap_chunk_throw_returns_none() {
        let e = throw_entry("state");
        assert!(e.chunk().is_none(), "a throwing save() must degrade to no-state");
        assert!(!e.is_dead(), "a GUI-side save fault must not park the RT entry");
        assert!(e.chunk().is_none(), "second save also survives as None");
        assert!(!e.is_dead(), "the entry is still alive for RT");
    }

    /// R5: a throw from `clap.preset-load/2` `from_location()` (host-
    /// commanded load, control thread) surfaces as a plain Err for the
    /// PARAMETERS window echo; the report slot is drained so no ghost
    /// plugin-side load echoes later.
    #[cfg(not(windows))]
    #[test]
    fn v116_clap_preset_load_throw_returns_err() {
        let e = throw_entry("load");
        assert!(e.supports_preset_load(), "the fixture vends the extension");
        let msg = e
            .load_preset_location(PRESET_KIND_PLUGIN, None, None)
            .expect_err("a throwing from_location must surface as Err");
        assert!(msg.contains("threw"), "error must name the fault class: {msg}");
        assert!(
            e.take_preset_report().is_none(),
            "the Err path must drain the report slot (no ghost echo)"
        );
        assert!(!e.is_dead(), "a control-thread load fault must not park RT");
    }

    /// R6: a throw from the LAZY preset-discovery handshake yields an
    /// EMPTY list, cached — a UI refresh must not re-enter the throwing
    /// provider every paint.
    #[cfg(not(windows))]
    #[test]
    fn v116_clap_discovery_throw_caches_empty() {
        let e = throw_entry("load");
        let v = e.preset_locations();
        assert!(v.is_empty(), "a throwing discovery provider must yield no locations");
        // Second call is served from the cache — no re-throw, still empty.
        let v2 = e.preset_locations();
        assert!(v2.is_empty(), "the empty verdict is CACHED (no throw storm)");
        assert!(!e.is_dead(), "discovery faults must not park the RT entry");
    }

    // ================================================================ v117
    // C-unwind ABI over the ENTIRE CLAP host-callback surface (card
    // t_a4175cb0): every fn WE implement that PLUGIN code calls must be
    // `extern "C-unwind"`, else a Rust panic inside it hits the
    // `panic_cannot_unwind` guard = std::abort (no ffiguard catch is ever
    // reached), and a C++ throw cannot unwind through it either. The ffiguard
    // rule from 2026-11-08, applied retroactively to the v34 backend.

    /// Compile-time ABI pin. Measured rustc behaviour: NO coercion exists
    /// between `extern "C"` and `extern "C-unwind"` fn pointers (E0308 in
    /// BOTH directions), so every binding and vtable literal below compiles
    /// ONLY while the callback item AND its vtable field carry the exact
    /// C-unwind type. Reverting either side — or forgetting one straggler —
    /// fails the whole test binary at compile time: that IS the pin firing
    /// (verify by hand-reverting one definition, `cargo test --lib
    /// v117_clap_host_cbs_are_cunwind` must then fail to COMPILE, restore,
    /// green again).
    #[test]
    fn v117_clap_host_cbs_are_cunwind() {
        use std::os::raw::{c_char, c_void};
        // --- item side: every nameable host-impl callback, exact signature
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost, u32, *const c_char, *const c_char) = host_preset_loaded;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost, u32, *const c_char, *const c_char, i32, *const c_char) = host_preset_error;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost, *const c_char) -> *const c_void = host_get_extension;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost) = host_noop;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost, u32, *const c_char) = host_log_cb;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapInputEvents) -> u32 = cb_in_size;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapInputEvents, u32) -> *const ffi::ClapEventHeader = cb_in_get;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapOutputEvents, *const ffi::ClapEventHeader) -> bool = cb_out_try_push;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryIndexer, *const ffi::ClapPresetDiscoveryFiletype) -> bool = idx_declare_filetype;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryIndexer, *const ffi::ClapPresetDiscoveryLocation) -> bool = idx_declare_location;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryIndexer, *const ffi::ClapPresetDiscoverySoundpack) -> bool = idx_declare_soundpack;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryIndexer, *const c_char) -> *const c_void = idx_get_extension;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, i32, *const c_char) = recv_on_error;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char, *const c_char) -> bool = recv_begin_preset;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const ffi::ClapUniversalPluginId) = recv_pid;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char) = recv_str1;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, u64, u64) = recv_ts;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char, *const c_char) = recv_str2;
        // --- vtable side: construct every HOST-IMPLEMENTED vtable exactly
        // as prod does (field decl types must match the items byte-for-byte)
        // and read each fn-pointer field back at the C-unwind type.
        let vt_log = ffi::ClapHostLog { log: host_log_cb };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost, u32, *const c_char) = vt_log.log;
        let vt_pl = ffi::ClapHostPresetLoad { on_error: host_preset_error, loaded: host_preset_loaded };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost, u32, *const c_char, *const c_char, i32, *const c_char) = vt_pl.on_error;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost, u32, *const c_char, *const c_char) = vt_pl.loaded;
        let vt_host = ffi::ClapHost {
            clap_version: ffi::ClapVersion { major: 1, minor: 2, revision: 0 },
            host_data: std::ptr::null_mut(),
            name: std::ptr::null(),
            vendor: std::ptr::null(),
            url: std::ptr::null(),
            version: std::ptr::null(),
            get_extension: host_get_extension,
            request_restart: host_noop,
            request_process: host_noop,
            request_callback: host_noop,
        };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost, *const c_char) -> *const c_void = vt_host.get_extension;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost) = vt_host.request_restart;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost) = vt_host.request_process;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapHost) = vt_host.request_callback;
        let vt_in = ffi::ClapInputEvents { ctx: std::ptr::null_mut(), size: cb_in_size, get: cb_in_get };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapInputEvents) -> u32 = vt_in.size;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapInputEvents, u32) -> *const ffi::ClapEventHeader = vt_in.get;
        let vt_out = ffi::ClapOutputEvents { ctx: std::ptr::null_mut(), try_push: cb_out_try_push };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapOutputEvents, *const ffi::ClapEventHeader) -> bool = vt_out.try_push;
        let vt_recv = ffi::ClapPresetDiscoveryMetadataReceiver {
            receiver_data: std::ptr::null_mut(),
            on_error: recv_on_error,
            begin_preset: recv_begin_preset,
            add_plugin_id: recv_pid,
            set_soundpack_id: recv_str1,
            add_creator: recv_str1,
            set_description: recv_str1,
            set_timestamps: recv_ts,
            add_feature: recv_str1,
            add_extra_info: recv_str2,
        };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, i32, *const c_char) = vt_recv.on_error;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char, *const c_char) -> bool = vt_recv.begin_preset;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const ffi::ClapUniversalPluginId) = vt_recv.add_plugin_id;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char) = vt_recv.set_soundpack_id;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char) = vt_recv.add_creator;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char) = vt_recv.set_description;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, u64, u64) = vt_recv.set_timestamps;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char) = vt_recv.add_feature;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryMetadataReceiver, *const c_char, *const c_char) = vt_recv.add_extra_info;
        let vt_ix = ffi::ClapPresetDiscoveryIndexer {
            clap_version: ffi::ClapVersion { major: 1, minor: 2, revision: 0 },
            name: std::ptr::null(),
            vendor: std::ptr::null(),
            url: std::ptr::null(),
            version: std::ptr::null(),
            indexer_data: std::ptr::null_mut(),
            declare_filetype: idx_declare_filetype,
            declare_location: idx_declare_location,
            declare_soundpack: idx_declare_soundpack,
            get_extension: idx_get_extension,
        };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryIndexer, *const ffi::ClapPresetDiscoveryFiletype) -> bool = vt_ix.declare_filetype;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryIndexer, *const ffi::ClapPresetDiscoveryLocation) -> bool = vt_ix.declare_location;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryIndexer, *const ffi::ClapPresetDiscoverySoundpack) -> bool = vt_ix.declare_soundpack;
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapPresetDiscoveryIndexer, *const c_char) -> *const c_void = vt_ix.get_extension;
        // Stream callbacks: the prod `wr`/`rd` items are fn-LOCAL (not
        // nameable here); these dummies prove the FIELD types are C-unwind
        // (a plain-"C" field would E0308 on this very assignment).
        unsafe extern "C-unwind" fn v117_dummy_rd(_s: *const ffi::ClapIStream, _b: *mut c_void, _n: u64) -> i64 { 0 }
        unsafe extern "C-unwind" fn v117_dummy_wr(_s: *const ffi::ClapOStream, _b: *const c_void, _n: u64) -> i64 { 0 }
        let vt_is = ffi::ClapIStream { ctx: std::ptr::null_mut(), read: v117_dummy_rd };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapIStream, *mut c_void, u64) -> i64 = vt_is.read;
        let vt_os = ffi::ClapOStream { ctx: std::ptr::null_mut(), write: v117_dummy_wr };
        let _: unsafe extern "C-unwind" fn(*const ffi::ClapOStream, *const c_void, u64) -> i64 = vt_os.write;
    }

    /// Source-scan polarity (belt to the type pin's braces): every host-impl
    /// callback DEFINITION exists in C-unwind form, and NO plain-"C"
    /// definition of the class (host_/cb_/idx_/recv_ prefixes + the two
    /// fn-local stream callbacks) survives anywhere in the file. Needles are
    /// assembled at RUN time via format! — the pin's own text never contains
    /// a matchable needle (vacuity guard per the source-pin doctrine).
    #[test]
    fn v117_clap_host_cbs_plain_c_absent() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/claphost.rs"));
        for n in [
            "host_preset_loaded",
            "host_preset_error",
            "host_get_extension",
            "host_noop",
            "host_log_cb",
            "cb_in_size",
            "cb_in_get",
            "cb_out_try_push",
            "idx_declare_filetype",
            "idx_declare_location",
            "idx_declare_soundpack",
            "idx_get_extension",
            "recv_on_error",
            "recv_begin_preset",
            "recv_pid",
            "recv_str1",
            "recv_ts",
            "recv_str2",
        ] {
            let want = format!("extern \"C-unwind\" fn {n}");
            assert!(src.contains(&want), "host callback {n} must be defined extern C-unwind");
            let bad = format!("extern \"C\" fn {n}");
            assert!(!src.contains(&bad), "host callback {n} must NOT have a plain-\"C\" definition");
        }
        // fn-local stream callbacks (chunk save / state load):
        for n in ["wr(", "rd("] {
            let want = format!("unsafe extern \"C-unwind\" fn {n}");
            assert!(src.contains(&want), "stream callback {n} must be defined extern C-unwind");
            let bad = format!("unsafe extern \"C\" fn {n}");
            assert!(!src.contains(&bad), "stream callback {n} must NOT have a plain-\"C\" definition");
        }
        // Class straggler net: no plain-"C" definition of ANY host prefix.
        for pfx in ["host_", "cb_", "idx_", "recv_"] {
            let bad = format!("extern \"C\" fn {pfx}");
            assert!(!src.contains(&bad), "a plain-\"C\" callback survived with prefix {pfx}");
        }
        // Boundary firewall (the layer that makes a host-side panic
        // RECOVERABLE): every host-impl callback body runs inside
        // host_cb_guard — checked per name (window: fn head to the next
        // top-level item) plus a global count that no wrap can quietly
        // vanish or double up.
        let mut guarded_n = 0;
        for n in [
            "host_preset_loaded",
            "host_preset_error",
            "host_get_extension",
            "host_noop",
            "host_log_cb",
            "cb_in_size",
            "cb_in_get",
            "cb_out_try_push",
            "idx_declare_filetype",
            "idx_declare_location",
            "idx_declare_soundpack",
            "idx_get_extension",
            "recv_on_error",
            "recv_begin_preset",
            "recv_pid",
            "recv_str1",
            "recv_ts",
            "recv_str2",
            "wr",
            "rd",
        ] {
            let key = format!("fn {n}(");
            let i = src.find(&key).unwrap_or_else(|| panic!("host callback {n} vanished"));
            let rest = &src[i..];
            let mut end = ["\nunsafe extern", "\nfn ", "\nstatic ", "\n/// ", "\n// "]
                .iter()
                .filter_map(|m| rest.find(m))
                .min()
                .unwrap_or(rest.len());
            if n == "wr" || n == "rd" {
                // fn-LOCAL callbacks: the top-level markers sit far outside
                // their bodies — cap the window so a sibling's guard can
                // never stand in for their own.
                end = end.min(1200);
            }
            let body = &rest[..end];
            assert!(
                body.contains("host_cb_guard("),
                "host callback {n} body must run inside host_cb_guard"
            );
            guarded_n += 1;
        }
        assert_eq!(guarded_n, 20, "full class inventoried");
        // concat!-split needle: the raw source bytes of this very assert
        // must not match the pattern it counts (vacuity guard).
        let wraps = src.matches(concat!("host_cb_guard", "(|| ")).count();
        assert_eq!(wraps, 20, "every host-impl callback body must be wrapped (found {wraps})");
    }

    /// End-to-end proof of what the boundary catch buys: with the panic
    /// trap armed, the fixture plugin's re-entry into `host->log` panics
    /// INSIDE the host frame while plugin frames sit below it on the stack.
    /// The `host_cb_guard` boundary catch converts the panic into the
    /// callback's default return: the plugin's `process()` completes, the
    /// block renders (echo passthrough — the fixture never writes output),
    /// the entry is UNTOUCHED. Pre-v117 (plain-"C" frame) the panic aborted
    /// at the callback frame; with C-unwind but NO boundary catch it
    /// aborts in std's `__rust_drop_panic` after the plugin's catch-all
    /// swallows it ("Rust panics must be rethrown") — both sabotages make
    /// this pin SIGABRT (it cannot fail soft; that IS the bug class).
    #[cfg(not(windows))]
    #[test]
    fn v117_clap_host_log_panic_survives() {
        use std::sync::atomic::Ordering::Relaxed;
        let e = throw_entry("hostcb");
        let hits0 = V117_TRAP_HITS.load(Relaxed);
        assert!(!e.is_dead(), "fresh entry is not parked");
        std::env::set_var("SLOOPER_TEST_PANIC_HOST_LOG", "1");
        let input = [1.0f32; 1024];
        let mut out = [0.0f32; 1024];
        e.process(&input, &mut out);
        std::env::remove_var("SLOOPER_TEST_PANIC_HOST_LOG");
        // (a) The plugin re-entered host->log once per internal 512-frame
        // block (1024 fed = 2), every panic was caught, and process()
        // RETURNED — pre-fix the first trip never returned (SIGABRT).
        assert_eq!(
            V117_TRAP_HITS.load(Relaxed),
            hits0 + 2,
            "the fixture must re-enter the host log once per armed 512-block"
        );
        assert!(!e.is_dead(), "a caught host-side panic must not park the plugin entry");
        // (b) Entry fully healthy after the catches: trap-off trips run the
        // same callback without re-arming.
        let mut out2 = [0.0f32; 512];
        e.process(&input[..512], &mut out2);
        assert_eq!(V117_TRAP_HITS.load(Relaxed), hits0 + 2, "trap-off trips must not hit the trap");
        assert!(!e.is_dead());
        // (c) Negative control: a healthy entry (another DSO) renders.
        let g = fixture_entry("gain");
        let mut gout = [0.0f32; 512];
        g.process(&input[..512], &mut gout);
        assert!(gout[500].abs() > 0.5, "the gain fixture must still pass audio");
        assert!(!g.is_dead());
    }
}
