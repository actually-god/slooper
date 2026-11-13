//! JACK / PipeWire-JACK audio backend shim (Linux, PRIMARY OS).
//!
//! WINDOWS-PORT W5: ALL jack-crate vocabulary of the audio module lives in
//! this file (cfg'd out on Windows, where `wasapi.rs` provides the same `run`
//! entry point). Everything shared — `Cmd`/`Evt`, `Backend`, `Engine`,
//! `Proc::process_block`, `manager_loop`, `new_proc` — is in `mod.rs`; the
//! device-side contract is documented in `src/backend.rs` and on
//! `super::ManagerBackend`.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use ::jack::jack_sys as j;
use ::jack::{
    AsyncClient, AudioIn, AudioOut, Client, ClientOptions, ClientStatus, Control, MidiIn,
    NotificationHandler, Port, PortFlags, PortId, ProcessHandler, ProcessScope,
};

use super::*;

pub const CLIENT_NAME: &str = "slooper";
const MIDI_PORT_SHORT: &str = "midi_in";

// one bundle per RT thread: command/event channels, master gain, transport
// mirrors and the v39 meter taps — every handle `run` owns for its lifetime.
// W12g: the channel ends come by reference (the Windows switch loop may call
// run again with the same ones); the Arcs are re-cloned per dispatch. The
// verdict tells that loop to park (`Unavailable` after an honest report),
// re-dispatch (`Switch`), or leave (`Quit` — also the null-audio path, so
// headless tests see the identical thread-death they always did).
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
    // Test-only kill switch: headless GUI tests must not open PipeWire-JACK
    // clients at all. Spawning and closing dozens of clients per suite races
    // libpipewire's own callback threads during teardown (UAF inside the
    // client library, reported as glibc heap corruption), and none of that
    // surface is what a headless UI test exercises. Behaves exactly like the
    // "no JACK server" path below.
    if std::env::var("SLOOPER_NULL_AUDIO").is_ok() {
        let _ = evt_tx.send(Evt::Unavailable("null audio (SLOOPER_NULL_AUDIO)".into()));
        return ManagerExit::Quit;
    }
    // W12f: on Windows EVERY entry into jack-sys triggers its dlopen, and
    // the lazy loader PANICS when the bare name (JACK_LIB) fails to
    // resolve — so an explicit-JACK choice without a discovered DLL must
    // report, never call. (Auto-selection only routes here after
    // discovery succeeded, so this is exactly the "user chose JACK and
    // JACK went away" path.)
    #[cfg(windows)]
    if dll::found().is_none() {
        let msg = format!(
            "native JACK selected but {} was not found — install jack for Windows \
             (jackbuilds), set SLOOPER_JACK_DLL, or choose WASAPI in \
             Settings > AUDIO BACKEND",
            dll::dll_name()
        );
        eprintln!("[slooper] {msg}");
        let _ = evt_tx.send(Evt::Unavailable(msg));
        return ManagerExit::Unavailable;
    }
    let (client, status) = match Client::new(CLIENT_NAME, ClientOptions::NO_START_SERVER) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!(
                "No JACK/PipeWire-JACK server available: {}",
                describe_err(e)
            );
            eprintln!("[slooper] {}", msg);
            let _ = evt_tx.send(Evt::Unavailable(msg));
            return ManagerExit::Unavailable;
        }
    };
    if status.contains(ClientStatus::NAME_NOT_UNIQUE) {
        eprintln!("[slooper] client renamed to '{}'", client.name());
    }

    // Register the fixed slot pool + MIDI port before activation. Pool ports
    // live for the whole client lifetime, so their raw pointers are stable and
    // can be cached in the realtime handler (no lookups, no allocation in RT).
    let mut ins_raw = [std::ptr::null_mut(); MAX_SLOTS];
    let mut outs_raw = [std::ptr::null_mut(); MAX_SLOTS];
    for i in 0..MAX_SLOTS {
        match client.register_port(&format!("in{i}"), AudioIn::default()) {
            Ok(p) => ins_raw[i] = p.raw(),
            Err(e) => {
                let _ = evt_tx.send(Evt::Error(format!(
                    "failed to register input slot in{i}: {}",
                    describe_err(e)
                )));
                return ManagerExit::Unavailable;
            }
        }
        match client.register_port(&format!("out{i}"), AudioOut::default()) {
            Ok(p) => outs_raw[i] = p.raw(),
            Err(e) => {
                let _ = evt_tx.send(Evt::Error(format!(
                    "failed to register output slot out{i}: {}",
                    describe_err(e)
                )));
                return ManagerExit::Unavailable;
            }
        }
    }

    let midi_port = match client.register_port(MIDI_PORT_SHORT, MidiIn::default()) {
        Ok(p) => p,
        Err(e) => {
            let _ = evt_tx.send(Evt::Error(format!(
                "failed to create MIDI input port: {}",
                describe_err(e)
            )));
            return ManagerExit::Unavailable;
        }
    };

    let mask_in = Arc::new(AtomicU64::new(0));
    // Output slots carry independent L / R bits (see `Cmd::Apply`).
    let mask_out_l = Arc::new(AtomicU64::new(0));
    let mask_out_r = Arc::new(AtomicU64::new(0));
    let notif = Notif {
        tx: evt_tx.clone(),
        xruns: xruns.clone(),
    };
    // Sample rate is queryable before activation; the metronome grid is
    // sized from it.
    let sr = client.sample_rate();
    engine.set_sample_rate(sr);
    let proc = JackProc {
        proc: new_proc(
            evt_tx.clone(),
            gain,
            peak,
            meter,
            midi_drops,
            mask_in.clone(),
            mask_out_l.clone(),
            mask_out_r.clone(),
            engine.clone(),
            sr,
        ),
        midi_in: midi_port,
        ports: RawPorts {
            ins: ins_raw,
            outs: outs_raw,
        },
        midi_ev_scratch: Vec::new(),
    };

    let active = match client.activate_async(notif, proc) {
        Ok(a) => a,
        Err(e) => {
            let _ = evt_tx.send(Evt::Error(format!(
                "failed to activate JACK client: {}",
                describe_err(e)
            )));
            return ManagerExit::Unavailable;
        }
    };

    let client_name = active.as_client().name().to_string();
    let sample_rate = active.as_client().sample_rate();
    eprintln!(
        "[slooper] JACK client '{}' active @ {} Hz",
        client_name, sample_rate
    );
    let _ = evt_tx.send(Evt::Ready {
        client_name: client_name.clone(),
        sample_rate,
    });
    send_ports(active.as_client(), evt_tx);

    // Remote MIDI sources currently wired into our midi_in port. Devices often
    // reappear on a fresh port name every connect, so this set is re-derived
    // periodically and on registration events (see rescan_midi_sources).
    let midi_sources: Vec<String> = Vec::new();

    // What each slot is currently connected to (manager-thread state only).
    let cur_in: Vec<Option<String>> = vec![None; MAX_SLOTS];
    let cur_out: Vec<Option<(String, u8)>> = vec![None; MAX_SLOTS];

    let mut session = JackSession {
        client: Some(active),
        cur_in,
        cur_out,
        mask_in,
        mask_out_l,
        mask_out_r,
        midi_sources,
    };
    let exit = super::manager_loop(cmd_rx, evt_tx, &engine, sample_rate, &mut session);
    session.teardown();
    exit
}

/// Connect with retries: PipeWire-JACK does not accept connections on a
/// freshly-activated client's ports until its activation has settled into the
/// graph, so a just-started remote client needs a few cycles before its ports
/// can be wired up. Runs on the manager thread (sleeping is fine).
fn connect_retry(client: &Client, src: &str, dst: &str) -> Result<(), ::jack::Error> {
    let mut last_err = None;
    for _ in 0..10 {
        match client.connect_ports_by_name(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

/// Connect / disconnect external ports against the fixed slot pool, updating
/// the RT-visible masks. Runs on the manager thread (may block/allocate).
#[allow(clippy::too_many_arguments)] // one arg per slot-pool state piece; a wrapper struct is pure ceremony
fn apply_layout(
    client: &Client,
    ins: &[String],
    outs: &[(String, u8)],
    cur_in: &mut [Option<String>],
    cur_out: &mut [Option<(String, u8)>],
    mask_in: &AtomicU64,
    mask_out_l: &AtomicU64,
    mask_out_r: &AtomicU64,
    tx: &SyncSender<Evt>,
) {
    if ins.len() > MAX_SLOTS || outs.len() > MAX_SLOTS {
        let _ = tx.try_send(Evt::Error(format!(
            "too many ports selected: max {MAX_SLOTS} inputs and {MAX_SLOTS} outputs"
        )));
    }

    let mut applied_in = 0usize;
    for (i, slot) in cur_in.iter_mut().enumerate() {
        let new_in = ins.get(i).cloned();
        if *slot == new_in {
            continue; // unchanged: keep the existing connection
        }
        let pool_name = format!("{}:in{i}", client.name());
        if slot.take().is_some() {
            // Stop reading before disconnecting, so RT never reads a slot that
            // is mid-teardown (unconnected input buffers hold stale data).
            mask_in.fetch_and(!(1u64 << i), Ordering::Relaxed);
            if let Some(pool) = client.port_by_name(&pool_name) {
                if let Err(e) = client.disconnect(&pool) {
                    let _ = tx.try_send(Evt::Error(format!(
                        "could not disconnect input slot in{i}: {}",
                        describe_err(e)
                    )));
                }
            }
        }
        if let Some(src) = new_in {
            match connect_retry(client, &src, &pool_name) {
                Ok(()) => {
                    // Only start reading once the connection exists.
                    mask_in.fetch_or(1u64 << i, Ordering::Relaxed);
                    *slot = Some(src);
                    applied_in += 1;
                }
                Err(e) => {
                    let msg = format!(
                        "could not connect input {src} -> {pool_name}: {}",
                        describe_err(e)
                    );
                    eprintln!("[slooper] {msg}");
                    let _ = tx.try_send(Evt::Error(msg));
                }
            }
        }
    }

    let mut applied_out = 0usize;
    for (i, slot) in cur_out.iter_mut().enumerate() {
        // Route bits: 1 = L bus, 2 = R bus; 0 counts as both (mono downmix
        // — the historical behaviour, and the default for fresh selections).
        let new_out = outs
            .get(i)
            .map(|(n, lr)| (n.clone(), if lr & 3 == 0 { 3 } else { lr & 3 }));
        let same_name = slot.as_ref().map(|s| &s.0) == new_out.as_ref().map(|s| &s.0);
        if same_name && slot.is_some() {
            // Still connected to the same port: only the L/R routing can
            // have moved — retarget the masks without touching the link.
            if let Some((_, lr)) = &new_out {
                if slot.as_ref().map(|s| s.1) != Some(*lr) {
                    mask_out_l.fetch_and(!(1u64 << i), Ordering::Relaxed);
                    mask_out_r.fetch_and(!(1u64 << i), Ordering::Relaxed);
                    if lr & 1 != 0 {
                        mask_out_l.fetch_or(1u64 << i, Ordering::Relaxed);
                    }
                    if lr & 2 != 0 {
                        mask_out_r.fetch_or(1u64 << i, Ordering::Relaxed);
                    }
                    *slot = new_out;
                }
            }
            continue;
        }
        if *slot == new_out {
            continue;
        }
        let pool_name = format!("{}:out{i}", client.name());
        if slot.take().is_some() {
            // Keep writing until the disconnect completes, then stop.
            if let Some(pool) = client.port_by_name(&pool_name) {
                if let Err(e) = client.disconnect(&pool) {
                    let _ = tx.try_send(Evt::Error(format!(
                        "could not disconnect output slot out{i}: {}",
                        describe_err(e)
                    )));
                }
            }
            mask_out_l.fetch_and(!(1u64 << i), Ordering::Relaxed);
            mask_out_r.fetch_and(!(1u64 << i), Ordering::Relaxed);
        }
        if let Some((dst, lr)) = new_out {
            match connect_retry(client, &pool_name, &dst) {
                Ok(()) => {
                    if lr & 1 != 0 {
                        mask_out_l.fetch_or(1u64 << i, Ordering::Relaxed);
                    }
                    if lr & 2 != 0 {
                        mask_out_r.fetch_or(1u64 << i, Ordering::Relaxed);
                    }
                    *slot = Some((dst, lr));
                    applied_out += 1;
                }
                Err(e) => {
                    let msg = format!(
                        "could not connect output {pool_name} -> {dst}: {}",
                        describe_err(e)
                    );
                    // v21: input failures already printed; output failures
                    // only reached the status line, which made "no audio"
                    // debugging blind in headless/launch-log runs.
                    eprintln!("[slooper] {msg}");
                    let _ = tx.try_send(Evt::Error(msg));
                }
            }
        }
    }

    eprintln!("[slooper] applied {applied_in} input(s), {applied_out} output(s)");
    let _ = tx.send(Evt::Applied {
        ins: applied_in,
        outs: applied_out,
    });
}

/// Enumerate remote audio ports, hiding those whose client belongs to a
/// disabled device (see [`crate::ports`]), and push them to the GUI.
fn send_ports(client: &Client, tx: &SyncSender<Evt>) {
    let (ins, outs) = enumerate_visible_ports(client);
    let _ = tx.send(Evt::Ports { ins, outs });
}

/// Like `send_ports`, but non-blocking (for JACK's notification thread).
fn send_ports_try(client: &Client, tx: &SyncSender<Evt>) {
    let (ins, outs) = enumerate_visible_ports(client);
    let _ = tx.try_send(Evt::Ports { ins, outs });
}

/// Re-derive which remote MIDI source ports feed `slooper:midi_in` and
/// make the actual connections. Devices often get a brand-new port name on
/// every connect (and may use any channel), so a single manually-made link
/// is not enough: we wire *every* remote MIDI source port into our input,
/// drop links whose source vanished, and report the live set to the GUI.
/// Runs on the manager thread only (connects may block). Returns true if the
/// connected set changed.
fn rescan_midi_sources(client: &Client, tx: &SyncSender<Evt>, known: &mut Vec<String>) -> bool {
    let mine = client.name().to_string();
    let prefix = format!("{mine}:");
    let dst = format!("{prefix}midi_in");

    // Remote MIDI source ports (any client, any channel — channels are
    // irrelevant for transport-level binding; the device picks its own).
    let found: Vec<String> = client
        .ports(None, Some("midi"), PortFlags::IS_OUTPUT)
        .into_iter()
        .filter(|n| !n.starts_with(&prefix))
        .collect();

    // Nothing to do if the set is unchanged (connect_retry would still work,
    // but skipping avoids pointless connect calls every 250 ms).
    if found == *known {
        return false;
    }

    // Drop links whose source port no longer exists.
    for name in known.iter() {
        if !found.contains(name) {
            let _ = client.disconnect_ports_by_name(name, &dst);
        }
    }
    // (Re)connect everything currently present. connect_retry absorbs the
    // PipeWire-JACK "freshly activated" delay for ports that just appeared.
    for name in found.iter() {
        if let Err(e) = connect_retry(client, name, &dst) {
            eprintln!("[slooper] MIDI link {name} → {dst}: {e}");
        }
    }
    *known = found.clone();
    eprintln!(
        "[slooper] MIDI sources: {}",
        if known.is_empty() {
            "none".into()
        } else {
            known.join(", ")
        }
    );
    let _ = tx.try_send(Evt::MidiSources(known.clone()));
    true
}

/// Shared port enumeration: candidates = other clients' audio ports, minus
/// any client that belongs to a disabled device.
fn enumerate_visible_ports(client: &Client) -> (Vec<PortInfo>, Vec<PortInfo>) {
    let mine = client.name().to_string();
    let prefix = format!("{mine}:");
    let hidden = hidden_clients_cached();

    // Candidate inputs = other clients' OUTPUT (source) audio ports.
    let ins: Vec<PortInfo> = client
        .ports(None, Some("audio"), PortFlags::IS_OUTPUT)
        .into_iter()
        .filter(|n| !n.starts_with(&prefix))
        .filter(|n| port_visible(n, hidden.as_ref()))
        .map(|name| port_info(&name))
        .collect();

    // Candidate outputs = other clients' INPUT (sink) audio ports.
    let outs: Vec<PortInfo> = client
        .ports(None, Some("audio"), PortFlags::IS_INPUT)
        .into_iter()
        .filter(|n| !n.starts_with(&prefix))
        .filter(|n| port_visible(n, hidden.as_ref()))
        .map(|name| port_info(&name))
        .collect();

    (ins, outs)
}

/// True when a full port name (`client:port`) should be shown.
fn port_visible(full_name: &str, hidden: Option<&std::collections::HashSet<String>>) -> bool {
    match hidden {
        Some(set) => !set.contains(full_name.split(':').next().unwrap_or("")),
        None => true, // can't determine device state → show everything
    }
}

/// Cached "hidden clients" set. The pactl/pw-cli lookups cost a few process
/// spawns, so the result is cached for two seconds (device state rarely
/// changes, and port lists refresh on registration events anyway).
fn hidden_clients_cached() -> Option<std::collections::HashSet<String>> {
    // pactl/pw-cli are PipeWire-on-Linux tools. On Windows (native JACK
    // mode, W12f) there is no such state to consult — and spawning two
    // missing processes every 2 s would be rude. Show everything.
    if !cfg!(unix) {
        return None;
    }
    use std::time::{Duration, Instant};
    static CACHE: std::sync::Mutex<Option<(Instant, Option<std::collections::HashSet<String>>)>> =
        std::sync::Mutex::new(None);

    // Fresh enough? (The lock is released at the end of this statement.)
    if let Some((t, set)) = CACHE.lock().unwrap().as_ref() {
        if t.elapsed() < Duration::from_secs(2) {
            return set.clone();
        }
    }

    // Compute outside the lock (it spawns processes), then store.
    let fresh = crate::ports::hidden_clients();
    *CACHE.lock().unwrap() = Some((Instant::now(), fresh.clone()));
    fresh
}

fn port_info(full_name: &str) -> PortInfo {
    let client = full_name.split(':').next().unwrap_or("").to_string();
    PortInfo {
        name: full_name.to_string(),
        client,
    }
}

/// Live jack backend for `manager_loop` (WINDOWS-PORT W5): owns the
/// activated client + the manager-thread patch state `apply_layout` keeps.
struct JackSession {
    client: Option<AsyncClient<Notif, JackProc>>,
    cur_in: Vec<Option<String>>,
    cur_out: Vec<Option<(String, u8)>>,
    mask_in: Arc<AtomicU64>,
    mask_out_l: Arc<AtomicU64>,
    mask_out_r: Arc<AtomicU64>,
    midi_sources: Vec<String>,
}

impl JackSession {
    fn client(&self) -> &Client {
        self.client
            .as_ref()
            .expect("jack session used after teardown")
            .as_client()
    }
}

impl ManagerBackend for JackSession {
    fn refresh(&mut self, tx: &SyncSender<Evt>) {
        send_ports(self.client(), tx);
    }
    fn apply(&mut self, ins: &[String], outs: &[(String, u8)], tx: &SyncSender<Evt>) {
        // destructure so the client borrow stays field-disjoint from the patch
        // state `apply_layout` mutates (borrow-checker-visible, WINDOWS-PORT W5)
        let JackSession {
            client,
            cur_in,
            cur_out,
            mask_in,
            mask_out_l,
            mask_out_r,
            ..
        } = self;
        match client {
            Some(c) => apply_layout(
                c.as_client(),
                ins,
                outs,
                cur_in,
                cur_out,
                mask_in,
                mask_out_l,
                mask_out_r,
                tx,
            ),
            // session used after teardown: nothing live to apply to
            None => {
                let _ = tx.try_send(Evt::Error("audio backend already shut down".into()));
            }
        }
    }
    fn idle(&mut self, tx: &SyncSender<Evt>) {
        let JackSession {
            client,
            midi_sources,
            ..
        } = self;
        if let Some(c) = client.as_ref() {
            rescan_midi_sources(c.as_client(), tx, midi_sources);
        }
    }
    fn teardown(&mut self) {
        // Teardown: null out every callback before the handler context is freed.
        // The jack crate's deactivate only clears thread-init/process, so without
        // this a late notification would call into freed memory on PipeWire-JACK.
        unsafe { clear_all_callbacks(self.client().raw()) };
        // Nulling the pointers does NOT cancel an invocation PipeWire-JACK has
        // already dispatched: its thread may have loaded the callback + context
        // pointer microseconds ago. Closing the client right now frees the jack
        // crate's Arc'd context underneath that in-flight call — a use-after-free
        // that glibc reports (in whichever thread frees first) as "corrupted size
        // vs. prev_size" and aborts. Let a dozen audio cycles elapse so every
        // dispatched callback runs against still-live state, then close.
        std::thread::sleep(std::time::Duration::from_millis(250));

        if let Some(client) = self.client.take() {
            drop(client); // deactivates + closes the client
        }
    }
}

/// Notification handler: keeps the GUI port list live and counts xruns.
struct Notif {
    tx: SyncSender<Evt>,
    xruns: Arc<AtomicU64>,
}

impl NotificationHandler for Notif {
    fn port_registration(&mut self, client: &Client, _port_id: PortId, _is_registered: bool) {
        // Remote ports appeared/disappeared — refresh the GUI list.
        // try_send: this runs on JACK's client thread; a full list is carried in
        // every Ports event, so dropping stale ones while the GUI lags is safe.
        send_ports_try(client, &self.tx);
    }

    fn client_registration(&mut self, client: &Client, _name: &str, _is_registered: bool) {
        send_ports_try(client, &self.tx);
    }

    fn xrun(&mut self, _client: &Client) -> Control {
        self.xruns.fetch_add(1, Ordering::Relaxed);
        // try_send: never block JACK's client thread on a full channel.
        let _ = self.tx.try_send(Evt::Xrun);
        Control::Continue
    }
}

/// Raw JACK port pointers of the fixed slot pool. The pool ports are never
/// unregistered, so the pointers stay valid for the whole client lifetime;
/// `Send` is safe to assert (they are only ever read from the RT thread).
/// `Copy` so the `BlockIo` shim can be built per cycle without borrowing
/// `Proc` (WINDOWS-PORT W4).
#[derive(Clone, Copy)]
struct RawPorts {
    ins: [*mut j::jack_port_t; MAX_SLOTS],
    outs: [*mut j::jack_port_t; MAX_SLOTS],
}

unsafe impl Send for RawPorts {}

/// The jack backend's `ProcessHandler`: pure plumbing around the neutral
/// engine core (W4 seam, W5 split). `Proc` holds ALL mix/transport state;
/// this struct only owns the port handles + event scratch the shim reads
/// per cycle.
struct JackProc {
    proc: Proc,
    midi_in: Port<MidiIn>,
    ports: RawPorts,
    midi_ev_scratch: Vec<(u32, Vec<u8>)>,
}

/// JACK shim (WINDOWS-PORT W4): the ONLY job left here is JACK plumbing —
/// frame count, materializing borrowed MIDI events into the owned scratch,
/// and the slot-buffer addresses. All mix/transport/take logic lives in
/// `Proc::process_block`; WASAPI (W5) builds its own shim against the same
/// entry point and `crate::backend::BlockIo`.
impl ProcessHandler for JackProc {
    fn process(&mut self, _client: &Client, ps: &ProcessScope) -> Control {
        // The borrowed `RawMidi`s die with this loop; the engine sees owned
        // events (same one-`Vec`-per-event allocation count as before W4).
        let mut evs = std::mem::take(&mut self.midi_ev_scratch);
        evs.clear();
        for e in self.midi_in.iter(ps) {
            evs.push((e.time, e.bytes.to_vec()));
        }
        let n = ps.n_frames() as usize;
        let io = JackIo {
            ports: self.ports,
            ps,
        };
        self.proc.process_block(n, &mut evs, &io);
        self.midi_ev_scratch = evs;
        Control::Continue
    }
}

/// `BlockIo` over the fixed JACK port pool + this cycle's `ProcessScope`.
/// A null return means "no buffer for this slot" — the same signal the old
/// in-function null-port check produced. `jack_port_get_buffer` never
/// allocates per call (JACK API), so this stays RT-safe.
struct JackIo<'ps> {
    ports: RawPorts,
    ps: &'ps ProcessScope,
}

impl crate::backend::BlockIo for JackIo<'_> {
    fn in_buf(&self, slot: usize) -> *const f32 {
        let p = self.ports.ins[slot];
        if p.is_null() {
            std::ptr::null()
        } else {
            (unsafe { j::jack_port_get_buffer(p, self.ps.n_frames()) }) as *const f32
        }
    }
    fn out_buf(&self, slot: usize) -> *mut f32 {
        let p = self.ports.outs[slot];
        if p.is_null() {
            std::ptr::null_mut()
        } else {
            unsafe { j::jack_port_get_buffer(p, self.ps.n_frames()) as *mut f32 }
        }
    }
}

/// Null out every callback that `activate_async` registered (the non-slow-sync
/// set), so no notification can reach freed handler memory after the context
/// is dropped. The jack crate's own `clear_callbacks` only covers
/// thread-init/process — not enough on PipeWire-JACK.
unsafe fn clear_all_callbacks(client: *mut j::jack_client_t) {
    let null = std::ptr::null_mut();
    j::jack_set_thread_init_callback(client, None, null);
    j::jack_on_info_shutdown(client, None, null);
    j::jack_set_process_callback(client, None, null);
    j::jack_set_freewheel_callback(client, None, null);
    j::jack_set_buffer_size_callback(client, None, null);
    j::jack_set_sample_rate_callback(client, None, null);
    j::jack_set_client_registration_callback(client, None, null);
    j::jack_set_port_registration_callback(client, None, null);
    j::jack_set_port_connect_callback(client, None, null);
    j::jack_set_graph_order_callback(client, None, null);
    j::jack_set_xrun_callback(client, None, null);
}

/// Format a JACK error for display.
fn describe_err(e: ::jack::Error) -> String {
    e.to_string()
}

// ==========================================================================
// W12f: native JACK on Windows — DLL discovery.
//
// The jack crate loads libjack at RUNTIME on every OS (jack-sys' DEFAULT
// `dynamic_loading` feature dlopens `JACK_LIB` on first use — that is why
// the Linux binary never links libjack either, cf. the W13 preamble).
// Windows has no pkg-config and no rpath, so the only thing needed to make
// that runtime load work is making the BARE name (`libjack64.dll`)
// resolvable: find the DLL, prove it is really a JACK library, and point
// the process DLL search at its directory — all BEFORE the first jack-sys
// call (its lazy loader panics if the name lookup fails).
//
// The path logic is pure (explicit inputs) so Linux pins prove the rules;
// only the cfg(windows) leaves touch the OS. Linux never calls into here
// (JACK is THE backend), hence the blanket dead-code allow.
#[allow(dead_code)] // W12f: the pure core is consumed on Windows + pinned on Linux
pub mod dll {
    use std::path::{Path, PathBuf};

    /// The DLL name jack-sys will LoadLibrary (arch-correct: x86 finds
    /// `libjack.dll`, x86_64 `libjack64.dll`).
    pub fn dll_name() -> &'static str {
        ::jack::jack_sys::JACK_LIB
    }

    /// Pure selection rule: the explicit override file (SLOOPER_JACK_DLL,
    /// a full path) wins; otherwise the first directory containing a file
    /// named EXACTLY `dll_name` (search order as given). The override must
    /// carry the expected file name: jack-sys loads by the BARE `JACK_LIB`
    /// name, so an override with any other file name could never be
    /// resolved back by the OS loader — reject it loudly instead of
    /// half-working.
    pub fn pick(override_file: Option<&Path>, dirs: &[PathBuf], dll_name: &str) -> Option<PathBuf> {
        if let Some(p) = override_file {
            if p.is_file() {
                if p.file_name().and_then(|n| n.to_str()) == Some(dll_name) {
                    return Some(p.to_path_buf());
                }
                eprintln!(
                    "[slooper] SLOOPER_JACK_DLL={} ignored: file name must be {dll_name} \
                     (jack-sys dlopens that bare name)",
                    p.display()
                );
            }
        }
        dirs.iter().map(|d| d.join(dll_name)).find(|c| c.is_file())
    }

    /// Candidate directories under a Program Files root for jack2 /
    /// jackbridge installs: every top-level `Jack*` dir plus the `lib64`,
    /// `bin` and `lib` subdirs its installers have historically used,
    /// sorted for deterministic order across machines.
    pub fn jack_dirs_in(program_files: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(program_files) else {
            return Vec::new();
        };
        let mut roots: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.to_ascii_lowercase().starts_with("jack"))
                        .unwrap_or(false)
            })
            .collect();
        roots.sort();
        let mut out = Vec::new();
        for r in roots {
            out.push(r.clone());
            for sub in ["lib64", "bin", "lib"] {
                out.push(r.join(sub));
            }
        }
        out
    }

    /// On-disk proof that `path` is a usable JACK library WITHOUT entering
    /// jack-sys (whose lazy loader would panic on a bad load): dlopen it
    /// and require the `jack_client_open` export. A good handle is LEAKED
    /// on purpose (repo rule: foreign audio libraries are never dlclosed).
    #[cfg(windows)]
    pub fn validate(path: &Path) -> bool {
        match unsafe { libloading::Library::new(path) } {
            Ok(lib) => {
                let ok = unsafe {
                    lib.get::<unsafe extern "C" fn()>(b"jack_client_open")
                        .is_ok()
                };
                if ok {
                    std::mem::forget(lib);
                } else {
                    eprintln!("[slooper] {} does not export jack_client_open", path.display());
                }
                ok
            }
            Err(e) => {
                eprintln!("[slooper] {} does not load: {e}", path.display());
                false
            }
        }
    }

    /// Make the BARE-name `LoadLibraryW(JACK_LIB)` that jack-sys performs
    /// resolve to `path`: SetDllDirectoryW redirects the process-wide
    /// search immediately; the PATH prepend is belt-and-braces and carries
    /// the directory to child processes as well.
    #[cfg(windows)]
    fn make_findable(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        let Some(dir) = path.parent() else { return };
        let wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let _ok = unsafe {
            windows::Win32::System::LibraryLoader::SetDllDirectoryW(windows::core::PCWSTR(
                wide.as_ptr(),
            ))
        };
        let dir_s = dir.display().to_string();
        let old = std::env::var("PATH").unwrap_or_default();
        if !old
            .split(';')
            .any(|d| d.trim_end_matches('\\').eq_ignore_ascii_case(dir_s.trim_end_matches('\\')))
        {
            std::env::set_var("PATH", format!("{dir_s};{old}"));
        }
    }

    /// Discovery order on Windows: SLOOPER_JACK_DLL, then every PATH dir,
    /// then `%PROGRAMFILES%\Jack*` install layouts — first hit that
    /// `validate`s wins. No env reads beyond the caller-provided override
    /// and PATH/PROGRAMFILES (tests pass synthetic dirs via `pick`).
    #[cfg(windows)]
    pub fn discover(override_file: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
        let name = dll_name();
        let mut dirs: Vec<PathBuf> =
            std::env::split_paths(&std::env::var("PATH").unwrap_or_default())
                .filter(|d| !d.as_os_str().is_empty())
                .collect();
        if let Some(pf) = std::env::var_os("PROGRAMFILES") {
            dirs.extend(jack_dirs_in(Path::new(&pf)));
        }
        let hit = pick(override_file.map(Path::new), &dirs, name)?;
        if !validate(&hit) {
            eprintln!("[slooper] {} found but is not a usable JACK library", hit.display());
            return None;
        }
        eprintln!("[slooper] native JACK: {}", hit.display());
        Some(hit)
    }

    /// The one probe behind backend resolution AND the settings selector
    /// (they must agree). Runs at most once per process; its side effects
    /// (PATH prepend + leaked handle) happen only on success and only
    /// before any jack-sys call, which is exactly the contract the loader
    /// needs.
    #[cfg(windows)]
    pub fn found() -> Option<&'static Path> {
        static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        FOUND
            .get_or_init(|| {
                let p = discover(std::env::var_os("SLOOPER_JACK_DLL").as_deref())?;
                make_findable(&p);
                Some(p)
            })
            .as_deref()
    }

    /// Linux twin of the probe: discovery is a Windows concept (JACK is
    /// THE backend here), so it always reports "not found". Exists so the
    /// OS-neutral selector code compiles and pins identically on both.
    #[cfg(not(windows))]
    pub fn found() -> Option<&'static Path> {
        None
    }
}
