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
//!   no stream (silence), a lost endpoint stops the stream with a Notice and
//!   the next Apply rebuilds.
//! * MIDI does not come from this backend (W9 wires WinMM devices into the
//!   existing MidiMsg pipeline).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use windows::core::{BSTR, GUID, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE};

use super::*;

/// KSDATAFORMAT subtypes (mmreg; pinned literally — same values as
/// `KSDATAFORMAT_SUBTYPE_{IEEE_FLOAT,PCM}`, which not every windows-rs
/// feature set exports).
const KSSUBTYPE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00AA00389B71);
const KSSUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00AA00389B71);
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
        for o in 0..take {
            dst[o] = self.buf[self.r];
            self.r = (self.r + 1) % self.cap;
        }
        for o in take..n.min(dst.len()) {
            dst[o] = 0.0;
        }
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
fn drain_capture(cap: &mut CapStream) -> Result<(), String> {
    loop {
        let need = unsafe { cap.svc.0.GetNextPacketSize() }.map_err(|e| e.to_string())?;
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
                .map_err(|e| e.to_string())?
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
        unsafe {
            cap.svc
                .0
                .ReleaseBuffer(frames as u32)
                .map_err(|e| e.to_string())?
        };
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
}

impl RtCfg {
    fn run(mut self) {
        let mut evs: Vec<(u32, Vec<u8>)> = Vec::new(); // W9 will feed WinMM MIDI here
        loop {
            let mut handles: Vec<HANDLE> = vec![self.stop.0, self.render_ev.0];
            handles.extend(self.capture.iter().map(|c| c.ev.0));
            let got = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            let Some(k) = got.0.checked_sub(WAIT_OBJECT_0.0) else {
                break; // WAIT_FAILED: device vanished
            };
            match k {
                0 => break, // stop event
                1 => {
                    if let Err(e) = self.render_cycle(&mut evs) {
                        let _ = self
                            .tx
                            .try_send(Evt::Error(format!("WASAPI stream stopped: {e}")));
                        break;
                    }
                }
                k => {
                    let ci = (k - 2) as usize;
                    if ci < self.capture.len() {
                        if let Err(e) = drain_capture(&mut self.capture[ci]) {
                            let _ = self
                                .tx
                                .try_send(Evt::Error(format!("WASAPI capture stopped: {e}")));
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

    fn render_cycle(&mut self, evs: &mut Vec<(u32, Vec<u8>)>) -> Result<(), String> {
        let pad = unsafe { self.render.0.GetCurrentPadding() }.map_err(|e| e.to_string())?;
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
            let buf = self
                .render_svc
                .0
                .GetBuffer(n as u32)
                .map_err(|e| e.to_string())?;
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
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// manager thread
// ---------------------------------------------------------------------------

pub fn run(
    cmd_rx: Receiver<Cmd>,
    evt_tx: SyncSender<Evt>,
    gain: Arc<AtomicU32>,
    peak: Arc<AtomicU32>,
    meter: Arc<crate::meter::MeterRing>,
    xruns: Arc<AtomicU64>,
    midi_drops: Arc<AtomicU64>,
    engine: Arc<Engine>,
) {
    if std::env::var("SLOOPER_NULL_AUDIO").is_ok() {
        let _ = evt_tx.send(Evt::Unavailable("null audio (SLOOPER_NULL_AUDIO)".into()));
        return;
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
        return;
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
    };
    session.publish_ports();
    let engine = session.engine.clone();
    manager_loop(&cmd_rx, &evt_tx, &engine, sr, &mut session);
    session.teardown();
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
}

impl ManagerBackend for WasapiSession {
    fn refresh(&mut self, _tx: &SyncSender<Evt>) {
        self.publish_ports();
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
        };
        self.rt = Some(
            std::thread::Builder::new()
                .name("slooper-wasapi-rt".into())
                .spawn(move || cfg.run())
                .expect("spawn wasapi rt thread"),
        );
        self.engine.set_sample_rate(rstream.mix.sr);
        if !cap_err.is_empty() {
            let _ = tx.try_send(Evt::Error(cap_err));
        }
        let _ = tx.try_send(Evt::Applied {
            ins: ok_in,
            outs: ok_out,
        });
    }

    fn idle(&mut self, _tx: &SyncSender<Evt>) {
        // hot-plug: republish on endpoint-set change; reap an RT thread that
        // died on its own (device lost) so the next Apply can rebuild.
        self.publish_ports();
        if self.rt.as_ref().map(|t| t.is_finished()).unwrap_or(false) {
            self.stop_rt();
            let _ = self
                .evt_tx
                .try_send(Evt::Notice("WASAPI stream stopped; re-Apply ports".into()));
        }
    }

    fn teardown(&mut self) {
        self.stop_rt();
        self.en = None; // drops the COM object before CoUninitialize
        if self.com_ok {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
