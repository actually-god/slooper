//! WASAPI audio backend for Windows (WINDOWS-PORT W5).
//!
//! Same contract as `jack.rs`, mirrored structurally on purpose:
//! * `run` is the manager-thread entry (`Backend::start` spawns it);
//! * it builds the shared engine handler with `super::new_proc` and runs the
//!   shared `super::manager_loop` behind `super::ManagerBackend`;
//! * the RT thread is a pure shim: wait on device events, hand one block to
//!   `Proc::process_block` EXACTLY ONCE per render cycle (block-level rules:
//!   `src/backend.rs`).
//!
//! Design decisions (hardware proof is the user checklist in
//! docs/INSTALL.md "Windows"):
//! * Shared mode, device MIX format. Float mix runs per-channel zero-copy;
//!   PCM 16/24-in-32/32 mixes convert through PRE-ALLOCATED f32 planes —
//!   the RT thread never allocates (rings/planes are sized at Apply time).
//! * A selectable "port" IS one (endpoint, channel) pair: stereo endpoints
//!   appear as `Name (L)` / `Name (R)`. The Cmd/Evt/UI port model stays
//!   identical to JACK's mono-port pool; `Cmd::Apply`'s L/R bits are then
//!   redundant (the channel lives in the name) and are ignored here.
//! * ONE render device drives the clock (the first selected output entry);
//!   extra selected render endpoints get a Notice — the block contract
//!   allows exactly one `process_block` per cycle. Any number of capture
//!   endpoints may be bound; packets drain into per-slot rings.
//! * NO_START ethos: no endpoints is `Evt::Unavailable`, an empty patch runs
//!   no stream (silence). A stream that DIES is classified by the RT thread
//!   and run through `super::rt_recovery` by the idle hook (W12a): a
//!   still-present device re-Applies automatically (bounded budget —
//!   replugging a USB DAC resumes audio with no click), a vanished one gets
//!   an honest Notice.
//! * RT scheduling (W12a): MMCSS "Pro Audio" when the system offers it,
//!   plain TIME_CRITICAL otherwise; reverted when the thread exits.
//! * MIDI: WinMM inputs live behind this backend too (W9): midir opens the
//!   user-selected devices on the manager thread and their per-device
//!   callback threads push raw events (with their WinMM µs timestamps) into
//!   a `midiin::MidiInQueue`; the RT loop drains it into `process_block`
//!   each cycle and `midiin::spread` turns the stamps into sub-block frame
//!   offsets (W12b; anchor = block start, unstamped drivers keep W9's
//!   block-start behavior).
//!   Linux keeps 100% JACK MIDI — none of this compiles there.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use midir::{MidiInput, MidiInputConnection};

use windows::core::{w, BSTR, GUID, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, GetCurrentThread,
    SetEvent, SetThreadPriority, WaitForMultipleObjects, INFINITE, THREAD_PRIORITY_TIME_CRITICAL,
};

use super::rt_recovery::{self, Patch};
use crate::midiin::{self, MidiInProducer, MidiInQueue};

use super::*;

/// KSDATAFORMAT subtypes (mmreg; pinned literally — same values as
/// `KSDATAFORMAT_SUBTYPE_{IEEE_FLOAT,PCM}`, which not every windows-rs
/// feature set exports).
const KSSUBTYPE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00AA_0038_9B71);
const KSSUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00AA_0038_9B71);
/// `CLSID_MMDeviceEnumerator` from mmdeviceapi.h — windows-rs generates no
/// CLSIDs for plain COM classes, so it is pinned here (verified against
/// the SDK header).
const CLSID_MMDEVICE_ENUMERATOR: GUID = GUID::from_u128(0xbc14880d_95a3_4c1c_a2f6_6cf308c8d50d);
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// Move COM interface wrappers + HANDLEs between threads. SAFETY: MMDevice
/// and WASAPI interfaces are documented free-threaded (both apartments);
/// streams are stopped and their thread joined before any of these drop, so
/// no in-flight callback can race teardown.
struct SendIf<T>(T);
unsafe impl<T> Send for SendIf<T> {}
unsafe impl<T> Sync for SendIf<T> {}

/// NUL-terminated UTF-16 buffer for a PCWSTR argument.
fn utf16z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// PWSTR -> String (manual walk; the string is CoTaskMem-owned and freed by
/// the caller).
unsafe fn pwstr_to_string(pw: windows::core::PWSTR) -> String {
    if pw.0.is_null() {
        return String::new();
    }
    let mut units = Vec::new();
    let mut p = pw.0;
    unsafe {
        while *p != 0 {
            units.push(*p);
            p = p.add(1);
        }
        String::from_utf16_lossy(&units)
    }
}

// ---------------------------------------------------------------------------
// mix formats
// ---------------------------------------------------------------------------

/// Sample layout of a device mix format (the stream format we activate in).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sample {
    F32,
    I16,
    I32,
}

impl Sample {
    fn bytes(self) -> usize {
        match self {
            Sample::F32 | Sample::I32 => 4,
            Sample::I16 => 2,
        }
    }
}

/// Owned copy of the device mix format. `Initialize` gets a pointer to the
/// inner `WAVEFORMATEX` copy (never to a freed CoTaskMem block).
#[derive(Clone, Copy)]
struct Mix {
    fmt: WAVEFORMATEX,
    sub: GUID,
    sample: Sample,
    ch: u16,
    sr: u32,
}

fn sample_of_bits(bits: u16) -> Option<Sample> {
    // 24-bit device formats (packed RGB3) are intentionally unsupported:
    // frame-addressable planes keep the RT shim simple.
    match bits {
        16 => Some(Sample::I16),
        32 => Some(Sample::I32),
        _ => None,
    }
}

/// Parse + copy the mix format of an opened client. 24-bit packed formats
/// (3-byte frames) are rejected: slooper wants frame-addressable planes.
fn mix_of(client: &IAudioClient) -> Result<Mix, String> {
    unsafe {
        let raw = client.GetMixFormat().map_err(|e| e.to_string())?;
        if raw.is_null() {
            return Err("GetMixFormat returned null".into());
        }
        let plain = *raw;
        // WAVEFORMATEX is repr(packed) in windows-rs: copy fields out first.
        let tag = plain.wFormatTag;
        let chn = plain.nChannels;
        let rate = plain.nSamplesPerSec;
        let bits = plain.wBitsPerSample;
        let bal = plain.nBlockAlign;
        let cb = plain.cbSize;
        let is_ext = tag == WAVE_FORMAT_EXTENSIBLE && cb >= 22;
        let mut m = Mix {
            fmt: plain,
            sub: KSSUBTYPE_FLOAT,
            sample: Sample::F32,
            ch: chn.max(1),
            sr: rate,
        };
        if is_ext {
            let ext = *(raw as *const WAVEFORMATEXTENSIBLE);
            m.sub = ext.SubFormat;
        }
        CoTaskMemFree(Some(raw as *const core::ffi::c_void));
        if bal != 0
            && (bal as usize) != m.ch as usize * sample_of_bits(bits).map(sample_bytes).unwrap_or(0)
        {
            // Packed 24-bit (block = ch*3) or exotic layout: unsupported.
            return Err(format!(
                "unsupported frame layout ({} ch x {} bit, block {})",
                m.ch, bits, bal
            ));
        }
        m.sample = if is_ext {
            if m.sub == KSSUBTYPE_FLOAT {
                float_sample(bits)?
            } else if m.sub == KSSUBTYPE_PCM {
                sample_of_bits(bits).ok_or("unsupported PCM bit depth")?
            } else {
                return Err("device mix format is neither float nor PCM".into());
            }
        } else {
            match tag {
                WAVE_FORMAT_IEEE_FLOAT => float_sample(bits)?,
                1 => sample_of_bits(bits).ok_or("unsupported PCM bit depth")?,
                t => return Err(format!("unsupported wave format tag {t}")),
            }
        };
        Ok(m)
    }
}

fn sample_bytes(s: Sample) -> usize {
    s.bytes()
}
fn float_sample(bits: u16) -> Result<Sample, String> {
    if bits == 32 {
        Ok(Sample::F32)
    } else {
        Err(format!("unsupported float bit depth {bits}"))
    }
}

// ---------------------------------------------------------------------------
// endpoints -> ports
// ---------------------------------------------------------------------------

/// One ACTIVE endpoint of one flow, with its parsed mix format.
struct DevInfo {
    id: String,
    /// Display name made unique across the flow (duplicate friendlies get
    /// ` [2]` etc. — `Cmd::Apply` round-trips this name).
    label: String,
    mix: Option<Mix>,
}

fn friendly_name(dev: &IMMDevice, id: &str) -> String {
    unsafe {
        if let Ok(store) = dev.OpenPropertyStore(STGM_READ) {
            if let Ok(pv) = store.GetValue(&PKEY_Device_FriendlyName) {
                if let Ok(b) = BSTR::try_from(&pv) {
                    let s = b.to_string();
                    if !s.is_empty() {
                        return s;
                    }
                }
            }
        }
    }
    id.to_string()
}

/// Enumerate ACTIVE endpoints of one flow with parsed mix formats.
fn enumerate(en: &IMMDeviceEnumerator, render: bool) -> Vec<DevInfo> {
    let mut out = Vec::new();
    let flow = if render { eRender } else { eCapture };
    let Ok(coll) = (unsafe { en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }) else {
        return out;
    };
    let Ok(n) = (unsafe { coll.GetCount() }) else {
        return out;
    };
    for i in 0..n {
        let Ok(dev) = (unsafe { coll.Item(i) }) else {
            continue;
        };
        let id = unsafe {
            match dev.GetId() {
                Ok(pw) => {
                    let s = pwstr_to_string(pw);
                    CoTaskMemFree(Some(pw.0 as *const core::ffi::c_void));
                    s
                }
                Err(_) => continue,
            }
        };
        let friendly = friendly_name(&dev, &id);
        // unique label
        let base = friendly.clone();
        let mut label = base.clone();
        let mut k = 2;
        while out.iter().any(|d: &DevInfo| d.label == label) {
            label = format!("{base} [{k}]");
            k += 1;
        }
        let mix = unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) }
            .ok()
            .and_then(|c| mix_of(&c).ok());
        out.push(DevInfo { id, label, mix });
    }
    out
}

/// The port list shown in the Ports window: one PortInfo per CHANNEL
/// (mono endpoint: one entry; stereo: `label (L)` + `label (R)`).
fn port_entries(devs: &[DevInfo]) -> Vec<PortInfo> {
    let mut v = Vec::new();
    for d in devs {
        let ch = d.mix.map(|m| m.ch).unwrap_or(1);
        if ch <= 1 {
            v.push(PortInfo {
                name: d.label.clone(),
                client: d.id.clone(),
            });
        } else {
            for sfx in ['L', 'R'] {
                v.push(PortInfo {
                    name: format!("{} ({sfx})", d.label),
                    client: d.id.clone(),
                });
            }
        }
    }
    v
}

/// Resolve a `Cmd::Apply` port name back to (endpoint id, channel index).
fn resolve(name: &str, devs: &[DevInfo]) -> Option<(String, usize)> {
    for d in devs {
        let ch = d.mix.map(|m| m.ch).unwrap_or(1);
        if name == d.label {
            return Some((d.id.clone(), 0));
        }
        if ch > 1 {
            if name == format!("{} (L)", d.label) {
                return Some((d.id.clone(), 0));
            }
            if name == format!("{} (R)", d.label) {
                return Some((d.id.clone(), 1));
            }
        }
    }
    None
}

/// Open + configure a shared event-callback stream on an endpoint id.
struct Stream {
    client: IAudioClient,
    mix: Mix,
    ring: usize,
    ev: HANDLE,
}

fn open_stream(en: &IMMDeviceEnumerator, id: &str) -> Result<Stream, String> {
    let w = utf16z(id);
    let dev: IMMDevice = unsafe { en.GetDevice(PCWSTR(w.as_ptr())) }.map_err(|e| e.to_string())?;
    let client = unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) }
        .map_err(|e| format!("activate: {e}"))?;
    let mix = mix_of(&client)?;
    let ev = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(|e| format!("CreateEventW: {e}"))?;
    // Shared mode, event callback, driver-chosen buffer/period (0,0): the
    // canonical cpal/MSDN activation for a latency-insensitive DAW clock.
    let fmt = mix.fmt; // local copy: never borrow out of the packed struct
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                &fmt as *const WAVEFORMATEX,
                None,
            )
            .map_err(|e| format!("Initialize: {e}"))?;
        client
            .SetEventHandle(ev)
            .map_err(|e| format!("SetEventHandle: {e}"))?;
    }
    let ring = unsafe { client.GetBufferSize() }.unwrap_or(1024) as usize;
    Ok(Stream {
        client,
        mix,
        ring,
        ev,
    })
}

// ---------------------------------------------------------------------------
// realtime thread
// ---------------------------------------------------------------------------

/// Bounded f32 ring (RT-safe: fixed capacity, drop-oldest on overflow).
/// One per bound (capture endpoint, channel).
struct Ring {
    buf: Vec<f32>,
    cap: usize,
    w: usize,
    r: usize,
    len: usize,
}

impl Ring {
    fn new(cap: usize) -> Ring {
        let cap = cap.max(1);
        Ring {
            buf: vec![0.0; cap],
            cap,
            w: 0,
            r: 0,
            len: 0,
        }
    }
    fn push(&mut self, s: &[f32]) {
        for &v in s {
            self.buf[self.w] = v;
            self.w = (self.w + 1) % self.cap;
            if self.len == self.cap {
                self.r = (self.r + 1) % self.cap; // drop oldest
            } else {
                self.len += 1;
            }
        }
    }
    /// Consume `n` frames into `dst` (zero-padded when short).
    fn pop_into(&mut self, n: usize, dst: &mut [f32]) {
        let take = n.min(self.len).min(dst.len());
        for d in &mut dst[..take] {
            *d = self.buf[self.r];
            self.r = (self.r + 1) % self.cap;
        }
        let end = n.min(dst.len());
        dst[take..end].fill(0.0);
        self.len -= take;
    }
}

/// Per-block buffer view over the slot planes; null for unbound slots,
/// exactly like a disconnected JACK port (see `src/backend.rs`).
struct WasapiIo {
    ins: [*const f32; MAX_SLOTS],
    outs: [*mut f32; MAX_SLOTS],
}
impl crate::backend::BlockIo for WasapiIo {
    fn in_buf(&self, slot: usize) -> *const f32 {
        self.ins.get(slot).copied().unwrap_or(std::ptr::null())
    }
    fn out_buf(&self, slot: usize) -> *mut f32 {
        self.outs.get(slot).copied().unwrap_or(std::ptr::null_mut())
    }
}

#[inline]
fn to_f32(src: &[u8], s: Sample) -> f32 {
    match s {
        Sample::F32 => f32::from_ne_bytes(src[0..4].try_into().unwrap()),
        Sample::I16 => i16::from_ne_bytes(src[0..2].try_into().unwrap()) as f32 / 32768.0,
        Sample::I32 => i32::from_ne_bytes(src[0..4].try_into().unwrap()) as f32 / 2147483648.0,
    }
}

#[inline]
fn from_f32(v: f32, s: Sample, dst: &mut [u8]) {
    match s {
        Sample::F32 => dst[0..4].copy_from_slice(&v.to_ne_bytes()),
        Sample::I16 => {
            let s = (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            dst[0..2].copy_from_slice(&s.to_ne_bytes());
        }
        Sample::I32 => {
            let s = (v.clamp(-1.0, 1.0) * 2147483647.0).round() as i32;
            dst[0..4].copy_from_slice(&s.to_ne_bytes());
        }
    }
}

/// One capture endpoint: its service, bound (slot, channel) list, and the
/// rings feeding each slot's input plane.
struct CapStream {
    id: String,
    /// Liveness anchor: the capture service is only valid while its
    /// audio client lives; never called through.
    #[allow(dead_code)]
    client: SendIf<IAudioClient>,
    svc: SendIf<IAudioCaptureClient>,
    ev: SendIf<HANDLE>,
    mix: Mix,
    slots: Vec<(usize, usize)>,
    /// Scratch: one frame-major deinterleave block + one channel column.
    stage: Vec<f32>,
    col: Vec<f32>,
    rings: Vec<Ring>,
}

/// Drain every pending packet of one capture endpoint into its rings.
fn drain_capture(cap: &mut CapStream) -> Result<(), RtFault> {
    loop {
        let need = unsafe { cap.svc.0.GetNextPacketSize() }.map_err(rt_fault)?;
        if need == 0 {
            return Ok(());
        }
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut frames: u32 = 0;
        let mut flags: u32 = 0;
        unsafe {
            cap.svc
                .0
                .GetBuffer(&mut ptr, &mut frames, &mut flags, None, None)
                .map_err(rt_fault)?
        };
        let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
        let ch = cap.mix.ch as usize;
        let bpf = cap.mix.sample.bytes() * ch;
        let n = frames as usize;
        if !silent && !ptr.is_null() {
            cap.stage.resize(n * ch, 0.0);
            let bps = bpf / ch;
            let src = unsafe { std::slice::from_raw_parts(ptr, n * bpf) };
            for (i, v) in cap.stage.iter_mut().enumerate() {
                let off = i * bps;
                *v = to_f32(&src[off..off + bps], cap.mix.sample);
            }
        }
        for (si, &(_slot, chidx)) in cap.slots.iter().enumerate() {
            cap.col.clear();
            if silent {
                cap.col.resize(n, 0.0);
            } else {
                cap.col.extend((0..n).map(|f| cap.stage[f * ch + chidx]));
            }
            if si < cap.rings.len() {
                cap.rings[si].push(&cap.col);
            }
        }
        unsafe { cap.svc.0.ReleaseBuffer(frames).map_err(rt_fault)? };
    }
}

/// W12a: RT thread scheduling for the lifetime of the stream thread. The
/// MMCSS "Pro Audio" scheduler class is the DAW-correct choice (system-wide
/// protection against priority inversion and starvation); plain
/// TIME_CRITICAL covers hosts without the MMCSS service (e.g. CI runners).
/// The guard reverts on drop, so no exit path can skip cleanup; `Nothing`
/// is the honest no-API-succeeded arm (audio runs, merely unscheduled).
enum RtBoost {
    Mmcss(HANDLE),
    TimeCritical,
    Nothing,
}

fn boost_rt_thread() -> RtBoost {
    unsafe {
        let mut idx = 0u32;
        if let Ok(h) = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut idx) {
            return RtBoost::Mmcss(h);
        }
        if SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL).is_ok() {
            RtBoost::TimeCritical
        } else {
            RtBoost::Nothing
        }
    }
}

impl Drop for RtBoost {
    fn drop(&mut self) {
        if let RtBoost::Mmcss(h) = self {
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(*h);
            }
        }
    }
}

/// Classified RT-thread death (W12a): `code` is an `rt_recovery::FAULT_*`
/// value the manager reads once the thread is reaped; `msg` rides the
/// `Evt::Error`.
struct RtFault {
    code: u32,
    msg: String,
}

/// Map a WASAPI failure to an `RtFault`: endpoint invalidation (device
/// unplugged / stream taken down by the OS) is the recoverable case;
/// everything else is a generic stream error.
fn rt_fault(e: windows::core::Error) -> RtFault {
    let code = if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        rt_recovery::FAULT_DEVICE
    } else {
        rt_recovery::FAULT_STREAM
    };
    RtFault {
        code,
        msg: e.to_string(),
    }
}

/// Everything the RT thread owns; moved in, dropped at thread exit after the
/// manager stopped + joined it.
struct RtCfg {
    proc: Proc,
    tx: SyncSender<Evt>,
    stop: SendIf<HANDLE>,
    render: SendIf<IAudioClient>,
    render_svc: SendIf<IAudioRenderClient>,
    render_ev: SendIf<HANDLE>,
    render_mix: Mix,
    /// (slot, channel) on the render endpoint.
    render_slots: Vec<(usize, usize)>,
    render_ring: usize,
    capture: Vec<CapStream>,
    /// Per-slot engine planes (index = slot; empty = unbound).
    in_planes: Vec<Vec<f32>>,
    out_planes: Vec<Vec<f32>>,
    xruns: Arc<AtomicU64>,
    /// W9: WinMM callbacks push here; drained into `evs` every render cycle.
    midi: Arc<MidiInQueue>,
    /// W12b: the stream's sample rate — the clock `midiin::spread` converts
    /// WinMM µs stamps into.
    sr: u32,
    /// W12a: classified cause of death, read by the manager after reap.
    fault: Arc<AtomicU32>,
}

impl RtCfg {
    fn run(mut self) {
        // W12a: RT scheduling for this thread's whole life (reverted on
        // drop — no exit path, including the `break`s below, skips it).
        let _boost = boost_rt_thread();
        let mut evs: Vec<(u32, Vec<u8>)> = Vec::new();
        // W12b: per-cycle scratch — the drained events' WinMM µs stamps,
        // positionally paired with `evs` (both reused, no RT allocation).
        let mut midi_ts: Vec<u64> = Vec::new();
        loop {
            let mut handles: Vec<HANDLE> = vec![self.stop.0, self.render_ev.0];
            handles.extend(self.capture.iter().map(|c| c.ev.0));
            let got = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            if got == WAIT_FAILED {
                // A handle we own died — count it as device loss. MUST be
                // checked BEFORE the index arithmetic (W12a): WAIT_FAILED -
                // WAIT_OBJECT_0 is not None, so the old `checked_sub` guard
                // was dead code that would busy-spin this loop.
                self.fault
                    .store(rt_recovery::FAULT_DEVICE, Ordering::SeqCst);
                break;
            }
            let k = got.0 - WAIT_OBJECT_0.0;
            match k {
                0 => break, // stop event: no fault stored, no recovery
                1 => {
                    if let Err(f) = self.render_cycle(&mut evs, &mut midi_ts) {
                        self.fault.store(f.code, Ordering::SeqCst);
                        let _ = self
                            .tx
                            .try_send(Evt::Error(format!("WASAPI stream stopped: {}", f.msg)));
                        break;
                    }
                }
                k => {
                    let ci = (k - 2) as usize;
                    if ci < self.capture.len() {
                        if let Err(f) = drain_capture(&mut self.capture[ci]) {
                            self.fault.store(f.code, Ordering::SeqCst);
                            let _ = self
                                .tx
                                .try_send(Evt::Error(format!("WASAPI capture stopped: {}", f.msg)));
                            break;
                        }
                    }
                }
            }
        }
        unsafe {
            let _ = self.render.0.Stop();
        }
    }

    fn render_cycle(
        &mut self,
        evs: &mut Vec<(u32, Vec<u8>)>,
        midi_ts: &mut Vec<u64>,
    ) -> Result<(), RtFault> {
        let pad = unsafe { self.render.0.GetCurrentPadding() }.map_err(rt_fault)?;
        let ring = self.render_ring as u32;
        if pad >= ring {
            // Full buffer at wake time = we were too late; count and skip.
            self.xruns.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let n = (ring - pad) as usize;
        for cap in self.capture.iter_mut() {
            drain_capture(cap)?;
        }
        // pop rings -> input planes. Field-disjoint mutable borrows: the
        // capture list and the plane table are separate `self` fields.
        for cap in self.capture.iter_mut() {
            for si in 0..cap.rings.len() {
                let slot = cap.slots[si].0;
                if let Some(plane) = self.in_planes.get_mut(slot) {
                    cap.rings[si].pop_into(n, plane);
                }
            }
        }
        evs.clear();
        midi_ts.clear();
        // W9/W12b: raw MIDI that landed since the last cycle, with its WinMM
        // µs stamps; `spread` converts the stamps into sub-block frame
        // offsets (anchor = block start), reusing `midi_ts` — no RT alloc.
        self.midi.drain_into(evs, midi_ts);
        midiin::spread(evs, midi_ts, self.sr, n as u32);
        let mut ins: [*const f32; MAX_SLOTS] = [std::ptr::null(); MAX_SLOTS];
        let mut outs: [*mut f32; MAX_SLOTS] = [std::ptr::null_mut(); MAX_SLOTS];
        for s in 0..MAX_SLOTS {
            if s < self.in_planes.len() && !self.in_planes[s].is_empty() {
                ins[s] = self.in_planes[s].as_ptr();
            }
            if s < self.out_planes.len() && !self.out_planes[s].is_empty() {
                outs[s] = self.out_planes[s].as_mut_ptr();
            }
        }
        let io = WasapiIo { ins, outs };
        self.proc.process_block(n, evs, &io);
        // interleave bound output planes -> the render buffer
        let sample = self.render_mix.sample;
        let bps = sample.bytes();
        let ch = self.render_mix.ch as usize;
        unsafe {
            let buf = self.render_svc.0.GetBuffer(n as u32).map_err(rt_fault)?;
            if !buf.is_null() {
                let dst = std::slice::from_raw_parts_mut(buf, n * ch * bps);
                for (slot, chidx) in &self.render_slots {
                    if *slot >= self.out_planes.len() || *chidx >= ch {
                        continue;
                    }
                    let plane = &self.out_planes[*slot];
                    let lim = n.min(plane.len());
                    for (f, &v) in plane[..lim].iter().enumerate() {
                        let off = (f * ch + chidx) * bps;
                        from_f32(v, sample, &mut dst[off..off + bps]);
                    }
                }
            }
            self.render_svc
                .0
                .ReleaseBuffer(n as u32, 0)
                .map_err(rt_fault)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// manager thread
// ---------------------------------------------------------------------------

/// W12g: same reference-channel + verdict contract as `jack::run` — the
/// Windows switch loop may re-dispatch with the same handles; `Quit` on
/// the null-audio path, `Unavailable` when COM cannot even enumerate.
#[allow(clippy::too_many_arguments)]
pub fn run(
    cmd_rx: &Receiver<Cmd>,
    evt_tx: &SyncSender<Evt>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    meter: Arc<crate::meter::MeterRing>,
    xruns: Arc<AtomicU64>,
    midi_drops: Arc<AtomicU64>,
    engine: Arc<Engine>,
) -> ManagerExit {
    if std::env::var("SLOOPER_NULL_AUDIO").is_ok() {
        let _ = evt_tx.send(Evt::Unavailable("null audio (SLOOPER_NULL_AUDIO)".into()));
        return ManagerExit::Quit;
    }
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let en = unsafe {
        CoCreateInstance::<_, IMMDeviceEnumerator>(&CLSID_MMDEVICE_ENUMERATOR, None, CLSCTX_ALL)
    };
    let Ok(en) = en else {
        let _ = evt_tx.send(Evt::Unavailable(
            "No WASAPI device enumerator (COM failure)".into(),
        ));
        if com_ok {
            unsafe { CoUninitialize() }
        }
        return ManagerExit::Unavailable;
    };
    // The default render endpoint advertises the sample rate before any
    // patch is applied (the GUI enables fx chains on Evt::Ready).
    let (sr, dev_name) = match unsafe { en.GetDefaultAudioEndpoint(eRender, eConsole) } {
        Ok(d) => {
            let id = unsafe { d.GetId() }
                .map(|pw| {
                    let s = unsafe { pwstr_to_string(pw) };
                    unsafe { CoTaskMemFree(Some(pw.0 as *const core::ffi::c_void)) };
                    s
                })
                .unwrap_or_default();
            let name = friendly_name(&d, &id);
            let sr = unsafe { d.Activate::<IAudioClient>(CLSCTX_ALL, None) }
                .ok()
                .and_then(|c| mix_of(&c).ok())
                .map(|m| m.sr)
                .unwrap_or(48000);
            (sr, format!("WASAPI: {name}"))
        }
        Err(_) => (48000, "WASAPI (no default output)".into()),
    };
    engine.set_sample_rate(sr);
    eprintln!("[slooper] {dev_name} @ {sr} Hz");
    let _ = evt_tx.send(Evt::Ready {
        client_name: dev_name,
        sample_rate: sr,
    });

    let mut session = WasapiSession {
        en: Some(en),
        xruns,
        com_ok,
        engine,
        gain,
        peak,
        meter,
        midi_drops,
        evt_tx: evt_tx.clone(),
        rt: None,
        rt_stop: None,
        ports_sig: String::new(),
        midi_q: MidiInQueue::new(),
        midi_conns: Vec::new(),
        midi_sig: String::new(),
        last_patch: None,
        retry_budget: rt_recovery::RETRY_BUDGET,
        rt_spawn: None,
        fault: Arc::new(AtomicU32::new(rt_recovery::FAULT_NONE)),
    };
    session.publish_ports();
    session.publish_midi();
    let engine = session.engine.clone();
    let exit = manager_loop(cmd_rx, evt_tx, &engine, sr, &mut session);
    session.teardown();
    exit
}

struct WasapiSession {
    /// `None` after `teardown` (COM objects must die before CoUninitialize).
    en: Option<IMMDeviceEnumerator>,
    xruns: Arc<AtomicU64>,
    com_ok: bool,
    engine: Arc<Engine>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    meter: Arc<crate::meter::MeterRing>,
    midi_drops: Arc<AtomicU64>,
    evt_tx: SyncSender<Evt>,
    rt: Option<std::thread::JoinHandle<()>>,
    rt_stop: Option<SendIf<HANDLE>>,
    /// Endpoint-set fingerprint; republish ports only on change so idle
    /// ticks do not churn the UI list.
    ports_sig: String,
    /// W9: shared MIDI bus (RT drains, WinMM callbacks push).
    midi_q: Arc<MidiInQueue>,
    /// Open WinMM devices; their Drop closes the device. Manager thread only.
    midi_conns: Vec<MidiInputConnection<MidiInProducer>>,
    /// Label-set fingerprint of the last `Evt::MidiInputs`.
    midi_sig: String,
    /// W12a: the patch the CURRENT (or last) RT thread ran; recovery
    /// re-Applies it verbatim. Cleared by an explicit silence-Apply.
    last_patch: Option<Patch>,
    /// W12a: auto-reapplies remaining (reset when a stream survived).
    retry_budget: u32,
    /// W12a: when the current RT thread was spawned (survival math).
    rt_spawn: Option<std::time::Instant>,
    /// W12a: RT thread writes its cause of death here before breaking.
    fault: Arc<AtomicU32>,
}

impl WasapiSession {
    fn publish_ports(&mut self) {
        let Some(en) = self.en.as_ref().cloned() else {
            return;
        };
        let ins = enumerate(&en, false);
        let outs = enumerate(&en, true);
        let sig = format!(
            "{:?}|{:?}",
            ins.iter()
                .map(|d| (d.id.clone(), d.mix.map(|m| (m.sr, m.ch))))
                .collect::<Vec<_>>(),
            outs.iter()
                .map(|d| (d.id.clone(), d.mix.map(|m| (m.sr, m.ch))))
                .collect::<Vec<_>>(),
        );
        if sig == self.ports_sig {
            return;
        }
        self.ports_sig = sig;
        let _ = self.evt_tx.try_send(Evt::Ports {
            ins: port_entries(&ins),
            outs: port_entries(&outs),
        });
    }

    // ---- W9: WinMM MIDI inputs -------------------------------------------

    /// Push the device list to the GUI when it changed (startup, Refresh,
    /// and idle hot-plug). Opening never happens here — the picker's
    /// Apply Selection decides that, mirroring the audio port flow.
    fn publish_midi(&mut self) {
        if let Ok(labels) = midi_labels() {
            let sig = labels.join("\u{1}");
            if sig != self.midi_sig {
                self.midi_sig = sig;
                let _ = self.evt_tx.try_send(Evt::MidiInputs(labels));
            }
        }
    }

    /// Open exactly `sel` (closing everything slooper had). The picker's
    /// own label list rides back so it can stay truthful after Apply.
    fn apply_midi(&mut self, sel: &[String], tx: &SyncSender<Evt>) {
        self.midi_conns.clear();
        let (conns, labels, errs, opened) = open_midi_devices(sel, &self.midi_q, &self.midi_drops);
        self.midi_conns = conns;
        for e in errs {
            let _ = tx.try_send(Evt::Error(e));
        }
        // The MONITOR panel counts Evt::MidiSources ("sources feeding my
        // input") — on Windows the opened WinMM devices ARE that set, so
        // the panel never lies "nothing is being received" while keys
        // arrive. On Linux the jack path sends its own auto-link list.
        let _ = tx.try_send(Evt::MidiSources(opened));
        match labels {
            Ok(labels) => {
                let _ = tx.try_send(Evt::MidiInputs(labels));
            }
            Err(e) => {
                let _ = tx.try_send(Evt::Error(format!("MIDI: {e}")));
            }
        }
    }

    fn stop_rt(&mut self) {
        if let Some(h) = self.rt_stop.take() {
            unsafe {
                let _ = SetEvent(h.0);
            }
            if let Some(t) = self.rt.take() {
                let _ = t.join();
            }
            unsafe {
                let _ = CloseHandle(h.0);
            }
        } else {
            if let Some(t) = self.rt.take() {
                let _ = t.join();
            }
        }
    }

    /// W12a: do ALL endpoint labels of a patch resolve in the CURRENT
    /// enumeration? (Same `resolve` the Apply path uses, so recovery can
    /// only fire on a patch the Apply path could actually bring up.)
    fn patch_present(&self, p: &Patch) -> bool {
        let Some(en) = self.en.as_ref() else {
            return false;
        };
        let ins = enumerate(en, false);
        let outs = enumerate(en, true);
        p.0.iter().all(|n| resolve(n, &ins).is_some())
            && p.1.iter().all(|(n, _)| resolve(n, &outs).is_some())
    }
}

/// Deduplicated WinMM device labels in enumeration order (midir's winmm
/// backend lists devices by index, so a label's POSITION is stable across
/// enumerations — that is what makes suffixed duplicates like
/// `"KeyLab [2]"` round-trip through `Cmd::SetMidiInputs` back to one
/// device).
fn midi_labels() -> Result<Vec<String>, String> {
    let mi = MidiInput::new("slooper-midi-enum").map_err(|e| e.to_string())?;
    let raw: Vec<String> = mi
        .ports()
        .iter()
        .filter_map(|p| mi.port_name(p).ok())
        .collect();
    Ok(midiin::dedupe_labels(raw))
}

/// Open every selected WinMM device (free function so Windows CI pins run
/// the machinery on the real WinMM stack, which a headless runner simply
/// reports as zero devices — everything must degrade honestly there).
/// Returns (connections, labels, per-device errors). One `MidiInput`
/// instance per opening: midir's `connect` consumes it. Callback threads
/// push into `q`; ring overflows count into `drops`.
/// What `open_midi_devices` yields: connections, enumerate result,
/// selected labels, opened labels (alias for the clippy "complex type").
type MidiOpen = (
    Vec<MidiInputConnection<MidiInProducer>>,
    Result<Vec<String>, String>,
    Vec<String>,
    Vec<String>,
);

fn open_midi_devices(sel: &[String], q: &Arc<MidiInQueue>, drops: &Arc<AtomicU64>) -> MidiOpen {
    let labels = match midi_labels() {
        Ok(l) => l,
        Err(e) => return (Vec::new(), Err(e), Vec::new(), Vec::new()),
    };
    let mut conns = Vec::new();
    let mut errs = Vec::new();
    let mut opened = Vec::new();
    for label in midiin::open_set(&labels, sel) {
        let Some(pos) = labels.iter().position(|l| *l == label) else {
            continue;
        };
        let mi = match MidiInput::new("slooper-midi-in") {
            Ok(mi) => mi,
            Err(e) => {
                errs.push(format!("MIDI init failed: {e}"));
                break;
            }
        };
        let ports = mi.ports();
        let Some(port) = ports.get(pos) else {
            errs.push(format!("MIDI device \"{label}\" vanished during open"));
            continue;
        };
        let prod = MidiInProducer::new(q.clone(), drops.clone());
        match mi.connect(port, "slooper", |ts, bytes, p| p.on_bytes(bytes, ts), prod) {
            Ok(conn) => {
                opened.push(label.clone());
                conns.push(conn);
            }
            Err(_) => errs.push(format!(
                "could not open MIDI input \"{label}\" (another application may hold it)"
            )),
        }
    }
    (conns, Ok(labels), errs, opened)
}

impl ManagerBackend for WasapiSession {
    fn refresh(&mut self, _tx: &SyncSender<Evt>) {
        self.publish_ports();
        self.publish_midi();
    }

    fn apply(&mut self, ins: &[String], outs: &[(String, u8)], tx: &SyncSender<Evt>) {
        self.stop_rt();
        let Some(en) = self.en.as_ref().cloned() else {
            let _ = tx.try_send(Evt::Error("WASAPI backend already closed".into()));
            return;
        };
        let in_devs = enumerate(&en, false);
        let out_devs = enumerate(&en, true);
        // resolve selections -> per-slot (endpoint id, channel); the L/R bits
        // of `outs` are redundant on Windows (channel lives in the name).
        let in_map: Vec<Option<(String, usize)>> = ins
            .iter()
            .take(MAX_SLOTS)
            .map(|n| resolve(n, &in_devs))
            .collect();
        let out_map: Vec<Option<(String, usize)>> = outs
            .iter()
            .take(MAX_SLOTS)
            .map(|(n, _)| resolve(n, &out_devs))
            .collect();
        let ok_in = in_map.iter().filter(|m| m.is_some()).count();
        let ok_out = out_map.iter().filter(|m| m.is_some()).count();

        if ok_out == 0 {
            // Empty patch (or nothing resolved): silence, per NO_START ethos.
            // W12a: an explicit silence forgets the recovery patch — there
            // is nothing the user wants automatically restored.
            self.last_patch = None;
            if ok_in > 0 {
                let _ = tx.try_send(Evt::Notice(
                    "WASAPI: no output device applied; inputs idle".into(),
                ));
            }
            let _ = tx.try_send(Evt::Applied { ins: 0, outs: 0 });
            return;
        }

        // One render endpoint drives the clock: the first resolved out slot.
        let rid = out_map.iter().flatten().next().unwrap().0.clone();
        let render_slots: Vec<(usize, usize)> = out_map
            .iter()
            .enumerate()
            .filter_map(|(s, m)| {
                m.as_ref()
                    .filter(|(id, _)| *id == rid)
                    .map(|(_, c)| (s, *c))
            })
            .collect();
        let extra = out_map
            .iter()
            .flatten()
            .filter(|(id, _)| *id != rid)
            .count();
        if extra > 0 {
            let _ = tx.try_send(Evt::Notice(format!(
                "WASAPI mixes to ONE output device; {extra} extra selection(s) ignored"
            )));
        }
        let rstream = match open_stream(&en, &rid) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.try_send(Evt::Error(format!("output device: {e}")));
                return;
            }
        };
        let rsvc = unsafe { rstream.client.GetService::<IAudioRenderClient>() };
        let Ok(rsvc) = rsvc else {
            let _ = tx.try_send(Evt::Error("render GetService failed".into()));
            unsafe {
                let _ = CloseHandle(rstream.ev);
            }
            return;
        };

        // capture endpoints: one stream per DISTINCT resolved endpoint id
        let mut cap_streams: Vec<CapStream> = Vec::new();
        let mut cap_err = String::new();
        for (slot, m) in in_map.iter().enumerate() {
            let Some((id, ch)) = m.clone() else { continue };
            let mut cidx = cap_streams.iter().position(|c| c.id == id);
            if cidx.is_none() {
                match open_stream(&en, &id) {
                    Ok(st) => match unsafe { st.client.GetService::<IAudioCaptureClient>() } {
                        Ok(svc) => {
                            cap_streams.push(CapStream {
                                id: id.clone(),
                                client: SendIf(st.client),
                                svc: SendIf(svc),
                                ev: SendIf(st.ev),
                                mix: st.mix,
                                slots: Vec::new(),
                                stage: Vec::new(),
                                col: Vec::new(),
                                rings: Vec::new(),
                            });
                            cidx = Some(cap_streams.len() - 1);
                        }
                        Err(e) => cap_err = format!("capture GetService: {e}"),
                    },
                    Err(e) => cap_err = format!("capture device: {e}"),
                }
            }
            let Some(ci) = cidx else { continue };
            let cs = &mut cap_streams[ci];
            if ch < cs.mix.ch as usize {
                cs.slots.push((slot, ch));
                cs.rings.push(Ring::new(4 * rstream.ring + 16));
            }
        }

        // Slot planes sized to the render ring (+slack); unbound slots stay
        // empty vectors -> WasapiIo returns null -> the core excludes them.
        let in_planes: Vec<Vec<f32>> = (0..MAX_SLOTS)
            .map(|s| {
                if in_map.get(s).map(|m| m.is_some()).unwrap_or(false) {
                    vec![0.0; rstream.ring + 16]
                } else {
                    Vec::new()
                }
            })
            .collect();
        let out_planes: Vec<Vec<f32>> = (0..MAX_SLOTS)
            .map(|s| {
                if render_slots.iter().any(|(slot, _)| *slot == s) {
                    vec![0.0; rstream.ring + 16]
                } else {
                    Vec::new()
                }
            })
            .collect();
        // Masks: one channel per slot -> exactly one L/R bit set each. (The
        // WASAPI io excludes by null anyway; keep the masks truthful too.)
        let mask_in = Arc::new(AtomicU64::new(0));
        let mask_out_l = Arc::new(AtomicU64::new(0));
        let mask_out_r = Arc::new(AtomicU64::new(0));
        for (s, m) in in_map.iter().enumerate() {
            if m.is_some() {
                mask_in.fetch_or(1u64 << s, Ordering::Relaxed);
            }
        }
        for (slot, ch) in &render_slots {
            let bit = 1u64 << *slot;
            if *ch == 0 {
                mask_out_l.fetch_or(bit, Ordering::Relaxed);
            } else {
                mask_out_r.fetch_or(bit, Ordering::Relaxed);
            }
        }

        let stop_ev = unsafe { CreateEventW(None, true, false, PCWSTR::null()) };
        let Ok(stop_ev) = stop_ev else {
            let _ = tx.try_send(Evt::Error("CreateEventW(stop) failed".into()));
            for c in cap_streams {
                unsafe {
                    let _ = CloseHandle(c.ev.0);
                }
            }
            unsafe {
                let _ = CloseHandle(rstream.ev);
            }
            return;
        };
        self.rt_stop = Some(SendIf(stop_ev));

        let cfg = RtCfg {
            proc: new_proc(
                self.evt_tx.clone(),
                self.gain.clone(),
                self.peak.clone(),
                self.meter.clone(),
                self.midi_drops.clone(),
                mask_in,
                mask_out_l,
                mask_out_r,
                self.engine.clone(),
                rstream.mix.sr,
            ),
            tx: self.evt_tx.clone(),
            stop: SendIf(stop_ev),
            render: SendIf(rstream.client),
            render_svc: SendIf(rsvc),
            render_ev: SendIf(rstream.ev),
            render_mix: rstream.mix,
            render_slots,
            render_ring: rstream.ring,
            capture: cap_streams,
            in_planes,
            out_planes,
            xruns: self.xruns.clone(),
            midi: self.midi_q.clone(),
            sr: rstream.mix.sr,
            fault: self.fault.clone(),
        };
        self.rt = Some(
            std::thread::Builder::new()
                .name("slooper-wasapi-rt".into())
                .spawn(move || cfg.run())
                .expect("spawn wasapi rt thread"),
        );
        // W12a: arm recovery for the stream that just came up. The budget
        // is NOT touched here — recovery-initiated Applies must not refill
        // it (idle resets it only for a stream that SURVIVED).
        self.fault.store(rt_recovery::FAULT_NONE, Ordering::SeqCst);
        self.last_patch = Some((ins.to_vec(), outs.to_vec()));
        self.rt_spawn = Some(std::time::Instant::now());
        self.engine.set_sample_rate(rstream.mix.sr);
        if !cap_err.is_empty() {
            let _ = tx.try_send(Evt::Error(cap_err));
        }
        let _ = tx.try_send(Evt::Applied {
            ins: ok_in,
            outs: ok_out,
        });
    }

    fn idle(&mut self, tx: &SyncSender<Evt>) {
        // hot-plug: republish on endpoint-set change; reap an RT thread that
        // died on its own (device lost) and let `rt_recovery` decide whether
        // the stream comes back by itself (W12a).
        self.publish_ports();
        // W9: keep the MIDI picker truthful when a device appears/disappears.
        // Newly-appearing SELECTED devices are not opened automatically —
        // same re-Apply discipline as the audio ports.
        self.publish_midi();
        if self.rt.as_ref().map(|t| t.is_finished()).unwrap_or(false) {
            self.stop_rt();
            let fault = self.fault.swap(rt_recovery::FAULT_NONE, Ordering::SeqCst);
            // A stream that held up for a while resets the budget: a death
            // after survival is a NEW story, not a continuation of one.
            let survived = self
                .rt_spawn
                .map(|t| t.elapsed() >= std::time::Duration::from_secs(rt_recovery::SURVIVED_SECS))
                .unwrap_or(false);
            if survived {
                self.retry_budget = rt_recovery::RETRY_BUDGET;
            }
            let present = self
                .last_patch
                .as_ref()
                .map(|p| self.patch_present(p))
                .unwrap_or(false);
            match rt_recovery::plan(fault, self.last_patch.as_ref(), present, self.retry_budget) {
                rt_recovery::IdleRecovery::Nothing => {
                    let _ =
                        tx.try_send(Evt::Notice("WASAPI stream stopped; re-Apply ports".into()));
                }
                rt_recovery::IdleRecovery::Reapply(patch) => {
                    self.retry_budget = self.retry_budget.saturating_sub(1);
                    let _ = tx.try_send(Evt::Notice(
                        "WASAPI: recovering stream after device change".into(),
                    ));
                    self.apply(&patch.0, &patch.1, tx);
                }
                rt_recovery::IdleRecovery::DeviceGone => {
                    let _ = tx.try_send(Evt::Notice(
                        "WASAPI output device vanished; pick a device and re-Apply ports".into(),
                    ));
                }
                rt_recovery::IdleRecovery::GaveUp => {
                    let _ = tx.try_send(Evt::Notice(
                        "WASAPI stream keeps failing; check the device, then re-Apply ports".into(),
                    ));
                }
            }
        }
    }

    fn set_midi_inputs(&mut self, sel: &[String], tx: &SyncSender<Evt>) {
        self.apply_midi(sel, tx);
    }

    fn teardown(&mut self) {
        self.stop_rt();
        // W9: close WinMM devices before the session's Arcs die (Drop does
        // midiInReset/Stop/Close on this thread — never the RT thread).
        self.midi_conns.clear();
        self.en = None; // drops the COM object before CoUninitialize
        if self.com_ok {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// W9 pins (WINDOWS-PORT): the real WinMM machinery. These run in the
// windows-native CI job and compile under the gnullvm cross-check. A
// headless runner has midiInGetNumDevs() == 0 — that is exactly the truth
// being pinned: every WinMM entry point must degrade honestly.
#[cfg(all(test, windows))]
mod w9_midi_tests {
    use super::*;

    #[test]
    fn w9_winmm_api_lists_devices_cleanly() {
        let mi = MidiInput::new("slooper-w9-probe").expect("WinMM must initialize");
        assert_eq!(mi.port_count(), mi.ports().len());
        for p in mi.ports() {
            assert!(
                !mi.port_name(&p).map(|n| n.is_empty()).unwrap_or(true),
                "every WinMM port must yield a usable name"
            );
        }
        // The label set the picker round-trips is unique no matter how raw
        // device names repeat.
        let labels = midi_labels().expect("enumeration must work with zero devices");
        for (i, a) in labels.iter().enumerate() {
            assert!(!labels[i + 1..].contains(a), "labels must be unique: {a}");
        }
    }

    #[test]
    fn w9_winmm_unknown_and_empty_selections_open_nothing() {
        let q = MidiInQueue::new();
        let drops = Arc::new(AtomicU64::new(0));
        // Stale / non-existent selections: no connections, no error rows,
        // and the fresh label list still rides back.
        let (conns, labels, errs, opened) =
            open_midi_devices(&["Device That Does Not Exist".to_string()], &q, &drops);
        assert!(conns.is_empty() && opened.is_empty());
        assert!(labels.is_ok(), "the picker list must come back: {labels:?}");
        assert!(
            errs.is_empty(),
            "unknown device must be a no-op, not an error"
        );
        // Empty selection = close everything; idempotent; nothing counted.
        let (c2, _, e2, o2) = open_midi_devices(&[], &q, &drops);
        assert!(c2.is_empty() && e2.is_empty() && o2.is_empty());
        let (c3, _, _, _) = open_midi_devices(&[], &q, &drops);
        assert!(c3.is_empty());
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(q.drops(), 0);
    }
}

// ---------------------------------------------------------------------------
// W12a pins (WINDOWS-PORT): RT scheduling + fault classification, on the
// real Win32 API surface. A headless runner may lack the MMCSS service —
// honest degradation (fall through to TIME_CRITICAL, never a panic) IS the
// contract, mirroring the W9 zero-device pins.
#[cfg(all(test, windows))]
mod w12a_boost_tests {
    use super::*;

    #[test]
    fn w12a_boost_rt_thread_degrades_honestly() {
        let b1 = boost_rt_thread();
        // At least ONE scheduling class must engage on any Windows host.
        assert!(
            matches!(b1, RtBoost::Mmcss(_) | RtBoost::TimeCritical),
            "neither MMCSS nor TIME_CRITICAL accepted the RT boost"
        );
        drop(b1); // Drop must revert cleanly (Mmcss arm dispatches AvRevert)
        let b2 = boost_rt_thread(); // second call on the same thread: legal
        drop(b2);
    }

    #[test]
    fn w12a_rt_fault_classifies_device_invalidation() {
        let e = windows::core::Error::from_hresult(AUDCLNT_E_DEVICE_INVALIDATED);
        assert_eq!(rt_fault(e).code, rt_recovery::FAULT_DEVICE);
        let generic = windows::core::HRESULT(0x8000_FFFF_u32 as i32);
        assert_eq!(
            rt_fault(windows::core::Error::from_hresult(generic)).code,
            rt_recovery::FAULT_STREAM
        );
        assert!(
            !rt_fault(windows::core::Error::from_hresult(generic))
                .msg
                .is_empty(),
            "message rides the Evt"
        );
    }
}
