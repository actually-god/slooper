//! End-to-end tests for the slooper DAW backend.
//!
//! These drive the real `Backend` (manager thread + realtime handler) against
//! a live JACK/PipeWire-JACK server, using a second JACK client as test
//! source:
//! - audio: two constant sources are summed into one sink through the applied
//!   port layout (`out = Σ in × gain`);
//! - transport: the global play/stop/seek + loop wrap/park behaviour, track
//!   mute flags, and the record-take pipeline (`Evt::Take`) are verified
//!   acoustically at a sink;
//! - metronome: click grid, three-tone pattern and immediate downbeat start
//!   are captured at a sink;
//! - export: a placed project renders through `Cmd::Export{Track,Project}`
//!   to a decodable WAV;
//! - midi: messages are sent to `slooper:midi_in` and must come back parsed.
//!
//! WINDOWS-PORT W5: the whole suite is Linux-only by construction — it drives
//! a LIVE JACK server and a second JACK client, both absent on Windows
//! (there the headless `SLOOPER_NULL_AUDIO` lib suite is the CI vehicle).
#![cfg(not(windows))]
//! If no JACK/PipeWire server is running, the tests skip themselves.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jack::{
    AudioIn, AudioOut, Client, ClientOptions, Control, MidiOut, Port, ProcessHandler, ProcessScope,
    RawMidi,
};

use slooper::arrange::{Arrangement, Clip};
use slooper::audio::{Backend, Cmd, Evt, FxSlot, MidiEv, TrackFx};
use slooper::midi::MidiMsg;

/// Serialize the e2e tests. Each drives the live audio server in real time
/// and asserts on sample-accurate timing; running them concurrently starves
/// the JACK clients and produces spurious "ensemble stalled" gaps.
static E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the e2e lock, surviving a poisoned guard from an earlier failed test.
fn e2e_lock() -> std::sync::MutexGuard<'static, ()> {
    E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Poll `b.evt_rx` until an event satisfies `pred` (or the timeout expires).
fn wait_for(b: &Backend, pred: impl Fn(&Evt) -> bool, timeout: Duration) -> Option<Evt> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match b.evt_rx.recv_timeout(remaining) {
            Ok(evt) if pred(&evt) => return Some(evt),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Start the backend and wait until it is ready (or give up -> `None`).
/// Returns the backend, the JACK client name (for building port names) and
/// the sample rate.
fn start_backend() -> Option<(Backend, String, u32)> {
    let b = Backend::start();
    match wait_for(
        &b,
        |e| matches!(e, Evt::Ready { .. } | Evt::Unavailable(_) | Evt::Error(_)),
        Duration::from_secs(5),
    ) {
        Some(Evt::Ready {
            client_name,
            sample_rate,
        }) => Some((b, client_name, sample_rate)),
        Some(other) => {
            eprintln!("skipping: backend not ready ({other:?})");
            None
        }
        None => {
            eprintln!("skipping: no JACK/PipeWire server (timed out waiting for backend)");
            None
        }
    }
}

// ---------------------------------------------------------------- audio mix --

struct MixProc {
    a: Port<AudioOut>,
    b: Port<AudioOut>,
    sink: Port<AudioIn>,
    last_sample: Arc<AtomicU32>, // f32 bits of the newest sink sample
    frames: Arc<AtomicU64>,      // process cycles handled
}

impl ProcessHandler for MixProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;
        if n == 0 {
            return Control::Continue;
        }
        let a = self.a.as_mut_slice(ps);
        let b = self.b.as_mut_slice(ps);
        for i in 0..n {
            a[i] = 0.25;
            b[i] = -0.125;
        }
        let sink = self.sink.as_slice(ps);
        self.last_sample
            .store(sink[n - 1].to_bits(), Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
        Control::Continue
    }
}

#[test]
fn e2e_audio_mixing() {
    let _g = e2e_lock();
    let Some((b, _client_name, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Test source client: two constant sources + one sink.
    let (client, _status) =
        Client::new("mixer_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let out_b = client.register_port("out_b", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();

    let last_sample = Arc::new(AtomicU32::new(f32::NAN.to_bits()));
    let frames = Arc::new(AtomicU64::new(0));
    let client_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            MixProc {
                a: out_a,
                b: out_b,
                sink,
                last_sample: last_sample.clone(),
                frames: frames.clone(),
            },
        )
        .unwrap();

    // Apply the layout through the backend's command channel.
    // Route 3 = mono downmix (the historical output behaviour).
    b.cmd(Cmd::Apply {
        ins: vec![
            format!("{client_name}:out_a"),
            format!("{client_name}:out_b"),
        ],
        outs: vec![(format!("{client_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 2, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the 2-in/1-out layout"
    );

    // Wait until the summed constant actually shows up at the sink.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let observed = f32::from_bits(last_sample.load(Ordering::Relaxed));
        if (observed - 0.125).abs() < 1e-4 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sink never reached 0.125 (last: {observed}, frames: {})",
            frames.load(Ordering::Relaxed)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(frames.load(Ordering::Relaxed) > 0);

    // Teardown: stop the backend's audio, drop clients.
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// ------------------------------------------------------------------ midi ----

/// One message per process cycle, cycling through a fixed list.
struct MidiSrcProc {
    out: Port<MidiOut>,
    step: u32,
}

const MESSAGES: &[&[u8]] = &[
    &[0x90, 60, 100], // NOTE ON  ch 1  C4  vel 100
    &[0xB1, 7, 128],  // CC       ch 2  volume 128
    &[0xC2, 11],      // PC       ch 3  program 11
];

impl ProcessHandler for MidiSrcProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let msg = MESSAGES[(self.step % MESSAGES.len() as u32) as usize];
        // One message per cycle at the cycle head; advance regardless so a
        // transient full buffer can't wedge us on one message.
        let _ = self.out.writer(ps).write(&RawMidi {
            time: 0,
            bytes: msg,
        });
        self.step += 1;
        Control::Continue
    }
}

#[test]
fn e2e_midi_monitor() {
    let _g = e2e_lock();
    let Some((b, client_name, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Source client with a MIDI out wired to slooper:midi_in.
    let (client, _status) =
        Client::new("midi_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let midi_out = client
        .register_port("midi_out", MidiOut::default())
        .unwrap();
    let src_name = client.name().to_string();
    client
        .connect_ports_by_name(
            &format!("{src_name}:midi_out"),
            &format!("{client_name}:midi_in"),
        )
        .expect("connect to slooper:midi_in");
    let active = client
        .activate_async(
            (),
            MidiSrcProc {
                out: midi_out,
                step: 0,
            },
        )
        .unwrap();

    // Collect events until every expected message has been seen (parsed).
    let mut seen: Vec<MidiMsg> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.len() < MESSAGES.len() && Instant::now() < deadline {
        match b.evt_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Evt::Midi(raw)) => {
                if let Some(msg) = MidiMsg::parse(&raw.bytes) {
                    seen.push(msg);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(
        seen.len() >= MESSAGES.len(),
        "expected at least {} MIDI events, got {}: {:?}",
        MESSAGES.len(),
        seen.len(),
        seen
    );

    let note_on = seen.iter().find(|m| matches!(m, MidiMsg::NoteOn { .. }));
    assert_eq!(
        note_on,
        Some(&MidiMsg::NoteOn {
            channel: 0,
            note: 60,
            velocity: 100
        })
    );
    let cc = seen
        .iter()
        .find(|m| matches!(m, MidiMsg::ControlChange { .. }));
    assert_eq!(
        cc,
        Some(&MidiMsg::ControlChange {
            channel: 1,
            number: 7,
            value: 128
        })
    );
    let pc = seen
        .iter()
        .find(|m| matches!(m, MidiMsg::ProgramChange { .. }));
    assert_eq!(
        pc,
        Some(&MidiMsg::ProgramChange {
            channel: 2,
            program: 11
        })
    );

    // Display formatting sanity (what the GUI shows).
    let note_on = note_on.unwrap();
    assert_eq!(note_on.type_name(), "NOTE ON");
    assert!(
        note_on.describe().contains("ch 01"),
        "{}",
        note_on.describe()
    );
    assert!(note_on.describe().contains("C4"), "{}", note_on.describe());

    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

/// The exact pattern the user's SINCO MIDI 1 controller sends (seen on aseq
/// port 32): Program Changes with values 1 and 2, cycling channels 0–3.
const SINCO_MESSAGES: &[&[u8]] = &[
    &[0xC0, 1], // PC  ch1  program 1
    &[0xC1, 2], // PC  ch2  program 2
    &[0xC2, 1], // PC  ch3  program 1
    &[0xC3, 2], // PC  ch4  program 2
];

struct SincoSrcProc {
    out: Port<MidiOut>,
    step: u32,
}

impl ProcessHandler for SincoSrcProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let msg = SINCO_MESSAGES[(self.step % SINCO_MESSAGES.len() as u32) as usize];
        let _ = self.out.writer(ps).write(&RawMidi {
            time: 0,
            bytes: msg,
        });
        self.step += 1;
        Control::Continue
    }
}

// ------------------------------------------------------------------ midi ----

/// Full capture → fire round trip through the real JACK MIDI path, emulating
/// the SINCO MIDI 1 device. The armed binding cell must register the first PC
/// (the "listening" box), and a repeated matching message must then fire the
/// bound action — here "select loop 2", observable via `App::selected_loop`.
#[test]
fn e2e_midi_capture_and_fire_sinco() {
    let _g = e2e_lock();
    let Some((b, client_name, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Isolate the config file so App does not read/write the user's real one.
    let dir = std::env::temp_dir().join(format!("slooper-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &dir);

    // Arm the "select track 2" slot (Select(1) in the action table) so the
    // fired action is observable via `App::selected_loop`. The next bindable
    // MIDI message is captured.
    let mut app = slooper::ui::App::new(b);
    let slot = slooper::midi::MidiAction::idx(slooper::midi::MidiAction::Select(1));
    app.arm_midi_slot(slot);
    app.drain_events();

    // Source client emulating SINCO MIDI 1, wired to slooper:midi_in.
    let (client, _status) =
        Client::new("sinco_emu", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let midi_out = client
        .register_port("midi_out", MidiOut::default())
        .unwrap();
    let src_name = client.name().to_string();
    client
        .connect_ports_by_name(
            &format!("{src_name}:midi_out"),
            &format!("{client_name}:midi_in"),
        )
        .expect("connect to slooper:midi_in");
    let active = client
        .activate_async(
            (),
            SincoSrcProc {
                out: midi_out,
                step: 0,
            },
        )
        .unwrap();

    // Pump App events until the first PC is captured and a repeated PC fires.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        app.drain_events();
        if app.midi_bind_at(slot).is_some() && app.selected_loop() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // The armed slot must have captured a message from the device's pattern.
    // Which of the four PCs arrives first depends on JACK graph scheduling, so
    // accept any PC with value 1 or 2 on channels 0–3 — exactly what the SINCO
    // MIDI 1 sends.
    let expected: &[slooper::midi::MidiBind] = &[
        slooper::midi::MidiBind {
            ty: 2,
            ch: 0,
            d1: 1,
        },
        slooper::midi::MidiBind {
            ty: 2,
            ch: 1,
            d1: 2,
        },
        slooper::midi::MidiBind {
            ty: 2,
            ch: 2,
            d1: 1,
        },
        slooper::midi::MidiBind {
            ty: 2,
            ch: 3,
            d1: 2,
        },
    ];
    let captured = app.midi_bind_at(slot);
    assert!(
        expected.contains(&captured.unwrap_or(slooper::midi::MidiBind {
            ty: 0,
            ch: 99,
            d1: 99
        })),
        "armed slot must capture one of the SINCO PC patterns, got {captured:?}"
    );
    assert_eq!(
        app.selected_loop(),
        1,
        "a repeated matching PC must fire the bound action (select loop 2)"
    );

    app.backend().cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// ----------------------------------------------- DAW transport & takes --

/// Source client: a controllable sample generator (constant or PRNG ±0.2)
/// plus a lock-free capture ring of the sink (what slooper wrote back).
struct LoopSrcProc {
    out: Port<AudioOut>,
    sink: Port<AudioIn>,
    /// 0 = constant `const_val`, 1 = xorshift PRNG ±0.2.
    mode: Arc<AtomicU8>,
    const_val: Arc<AtomicU32>, // f32 bits
    seed: u32,
    ring: Arc<Vec<AtomicU32>>, // sink samples (f32 bits)
    widx: Arc<AtomicU64>,      // absolute write position in the ring
}

impl ProcessHandler for LoopSrcProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;
        if n == 0 {
            return Control::Continue;
        }
        let out = self.out.as_mut_slice(ps);
        let prng = self.mode.load(Ordering::Relaxed) == 1;
        let c = f32::from_bits(self.const_val.load(Ordering::Relaxed));
        if prng {
            for s in out.iter_mut() {
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 17;
                self.seed ^= self.seed << 5;
                *s = if self.seed & 1 == 0 { 0.2 } else { -0.2 };
            }
        } else {
            out.fill(c);
        }
        let sink = self.sink.as_slice(ps);
        let base = self.widx.load(Ordering::Relaxed) as usize;
        for (i, s) in sink.iter().enumerate() {
            self.ring[(base + i) % self.ring.len()].store(s.to_bits(), Ordering::Relaxed);
        }
        self.widx.fetch_add(n as u64, Ordering::Relaxed);
        Control::Continue
    }
}

/// Wait until at least `need` new sink samples have been captured, then
/// return the most recent `need` of them.
fn read_ring(
    ring: &Arc<Vec<AtomicU32>>,
    widx: &AtomicU64,
    need: usize,
    timeout: Duration,
) -> Option<Vec<f32>> {
    let start = widx.load(Ordering::Relaxed);
    let deadline = Instant::now() + timeout;
    loop {
        if widx.load(Ordering::Relaxed).saturating_sub(start) as usize >= need {
            break;
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let end = widx.load(Ordering::Relaxed);
    let n = ring.len() as u64;
    Some(
        (0..need)
            .map(|i| {
                f32::from_bits(
                    ring[((end - need as u64 + i as u64) % n) as usize].load(Ordering::Relaxed),
                )
            })
            .collect::<Vec<f32>>(),
    )
}

/// Mean of the most recent `n` captured sink samples.
fn ring_mean(ring: &Arc<Vec<AtomicU32>>, widx: &AtomicU64, n: usize) -> Option<f32> {
    read_ring(ring, widx, n, Duration::from_millis(500)).map(|x| x.iter().sum::<f32>() / n as f32)
}

/// Peak |sample| of the most recent `n` captured sink samples.
fn ring_peak(ring: &Arc<Vec<AtomicU32>>, widx: &AtomicU64, n: usize) -> Option<f32> {
    read_ring(ring, widx, n, Duration::from_millis(500))
        .map(|x| x.iter().fold(0.0f32, |m, s| m.max(s.abs())))
}

/// Poll `pred` until true or the timeout expires (panics on timeout).
fn wait_until(pred: impl Fn() -> bool, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn e2e_transport_and_clips() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let mode = Arc::new(AtomicU8::new(0));
    let const_val = Arc::new(AtomicU32::new(0.0f32.to_bits()));

    let (client, _status) =
        Client::new("daw_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            LoopSrcProc {
                out: out_a,
                sink,
                mode: mode.clone(),
                const_val: const_val.clone(),
                seed: 0x1234_5678,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();

    b.cmd(Cmd::Apply {
        ins: vec![format!("{src_name}:out_a")],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 1, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the 1-in/1-out layout"
    );

    // ---- a project: one 0.5 asset on track 0 at t = 0 ----------------------
    let a0: Arc<[f32]> = Arc::from(vec![0.5f32; 16_000]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    let clip_len = 16_000u64;
    let total = 32_000u64; // two clip lengths: a real "project end"
    let table: Arc<Vec<Arc<[f32]>>> = Arc::new(vec![a0.clone()]);
    b.cmd(Cmd::SetProject {
        assets: table,
        arr: Arc::new(arr),
        len: total,
    });
    b.cmd(Cmd::SetLoop(true));

    // The transport is the only thing that makes clip sound: silent before.
    std::thread::sleep(Duration::from_millis(150));
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| m.abs() < 0.02)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
        "silence before the transport starts",
    );

    // ---- play: the clip plays from position 0 -------------------------------
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "transport playing the 0.5 clip",
    );
    assert!(
        b.engine.transport_on(),
        "PlayAll did not set the transport flag"
    );

    // ---- a stopped track contributes nothing (clips stay placed) ------------
    b.cmd(Cmd::ToggleTrackOn { i: 0 });
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| m.abs() < 0.02)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "muting track 0 silences the mix",
    );
    b.cmd(Cmd::ToggleTrackOn { i: 0 });
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "un-muting track 0 restores the clip",
    );

    // ---- stop holds the position; play resumes from it ----------------------
    b.cmd(Cmd::StopAll);
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Transport { on: false }),
            Duration::from_secs(2)
        )
        .is_some(),
        "no Transport(off) event from StopAll"
    );
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| m.abs() < 0.02)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
        "silence after StopAll",
    );
    let held = b.engine.pos();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(b.engine.pos(), held, "position moved while stopped");
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "play again resumes from the held position (still inside the clip)",
    );

    // ---- seek past the clip end: silence, playhead still running ------------
    b.cmd(Cmd::Seek {
        pos: clip_len + 4_000,
    });
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| m.abs() < 0.02)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
        "silence after seeking past the clip end",
    );
    // Seek back inside: sound returns immediately (scrub-scan works).
    b.cmd(Cmd::Seek { pos: 2_000 });
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
        "sound after scrubbing back into the clip",
    );

    // ---- loop wrap: the playhead rolls over instead of stopping -------------
    // Leave ~125 ms of runway, give the Seek command time to land, then
    // sample: 250 ms later the playhead MUST have passed the end and
    // reappeared at the top.
    b.cmd(Cmd::Seek {
        pos: total - sr as u64 / 8,
    });
    std::thread::sleep(Duration::from_millis(50));
    let p0 = b.engine.pos();
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        b.engine.transport_on(),
        "looping transport must not stop at the end"
    );
    let p1 = b.engine.pos();
    assert!(p1 < p0, "no wrap: pos went {p0} -> {p1} with loop on");

    // ---- loop off: park at the end and stop ---------------------------------
    b.cmd(Cmd::SetLoop(false));
    b.cmd(Cmd::Seek {
        pos: total - sr as u64 / 4,
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Transport { on: false }),
            Duration::from_secs(3)
        )
        .is_some(),
        "non-looping transport did not stop at the project end"
    );
    let parked = b.engine.pos();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(b.engine.pos(), parked, "parked playhead must hold");
    assert!(
        parked >= total - sr as u64 / 2,
        "parked far before the end: {parked}"
    );
    b.cmd(Cmd::SetLoop(true));

    // ---- recording produces a take (Evt::Take), not loop state --------------
    let at0 = b.engine.pos();
    const_val.store(0.25f32.to_bits(), Ordering::Relaxed);
    b.cmd(Cmd::ToggleRecord { i: 0 });
    std::thread::sleep(Duration::from_millis(300));
    b.cmd(Cmd::ToggleRecord { i: 0 });
    let take = wait_for(
        &b,
        |e| matches!(e, Evt::Take { track: 0, .. }),
        Duration::from_secs(3),
    );
    const_val.store(0.0f32.to_bits(), Ordering::Relaxed);
    let Evt::Take { at, data, .. } = take.expect("no Evt::Take from record toggle") else {
        unreachable!()
    };
    assert!(
        data.len() >= (sr as f32 * 0.2) as usize && data.len() <= (sr as f32 * 0.6) as usize,
        "take length {} samples, expected ~0.3 s",
        data.len()
    );
    let mean = data.iter().sum::<f32>() / data.len() as f32;
    assert!(
        (mean - 0.25).abs() < 0.03,
        "take captured mean {mean}, expected 0.25"
    );
    assert!(
        at.saturating_sub(at0) < sr as u64,
        "take start {at} far from playhead {at0}"
    );

    b.cmd(Cmd::StopAll);
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// --------------------------------------------------------------- metronome --

/// Sink-only capture client: records everything arriving at the applied
/// slooper output into a ring buffer.
struct CapProc {
    sink: Port<AudioIn>,
    ring: Arc<Vec<AtomicU32>>,
    widx: Arc<AtomicU64>,
}

impl ProcessHandler for CapProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;
        if n == 0 {
            return Control::Continue;
        }
        let sink = self.sink.as_slice(ps);
        let nring = self.ring.len() as u64;
        let mut w = self.widx.load(Ordering::Relaxed);
        for s in sink {
            self.ring[(w % nring) as usize].store(s.to_bits(), Ordering::Relaxed);
            w += 1;
        }
        self.widx.store(w, Ordering::Relaxed);
        Control::Continue
    }
}

/// Detect metronome clicks in a capture. Returns `(onset sample, tone)` per
/// click, where the tone is classified from the zero-crossing rate over the
/// first 15 ms of the click: 0 = downbeat (1760 Hz), 1 = quarter (1175 Hz),
/// 2 = subdivision (880 Hz).
fn metro_clicks(x: &[f32], sr: u32) -> Vec<(usize, u8)> {
    let thr = 0.08f32;
    let win = (sr as usize * 15 / 1000).max(16);
    let quiet_need = (sr as usize / 500).max(8); // 2 ms of quiet re-arms
    let mut out: Vec<(usize, u8)> = Vec::new();
    let mut armed = true;
    let mut quiet = 0usize;
    let mut i = 0usize;
    while i < x.len() {
        if armed && x[i].abs() > thr {
            let end = (i + win).min(x.len());
            let zc = (i + 1..end)
                .filter(|&k| (x[k] >= 0.0) != (x[k - 1] >= 0.0))
                .count();
            let f = zc as f32 * sr as f32 / (2.0 * (end - i) as f32);
            let tone = if f > 1450.0 {
                0
            } else if f > 1000.0 {
                1
            } else {
                2
            };
            out.push((i, tone));
            armed = false;
            quiet = 0;
            i = end; // skip the rest of this click
        } else {
            // Re-arm only after a sustained quiet run, so zero crossings
            // inside one click cannot split it into two onsets.
            if x[i].abs() < thr * 0.3 {
                quiet += 1;
            } else {
                quiet = 0;
            }
            if quiet >= quiet_need {
                armed = true;
            }
            i += 1;
        }
    }
    out
}

#[test]
fn e2e_midi_clip_plays_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Sink client: slooper's mix must appear at our capture ring.
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("midi_live", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            LoopSrcProc {
                out: out_a,
                sink,
                mode: Arc::new(AtomicU8::new(0)),
                const_val: Arc::new(AtomicU32::new(0.0f32.to_bits())),
                seed: 0x9e37_79b9,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the sink layout"
    );

    // The shared default synth, installed exactly as the app does it.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.cmd(Cmd::SetDefaultSynth(Some(Arc::new(
        slooper::audio::SynthHandle::Script(inst),
    ))));

    // A MIDI-only project: one bar of quarter notes at 120 BPM on track 0.
    // No audio asset exists at index 0 — the clip resolves through the MIDI
    // table only (a missing audio asset must skip, not crash the RT mix).
    let mut pat = slooper::midipat::Pattern::new(120);
    for (i, note) in [60u8, 64, 67, 72].iter().enumerate() {
        pat.notes.push(slooper::midipat::Note {
            ch: 0,
            note: *note,
            vel: 110,
            press: 0,
            start: (i as u32) * slooper::midipat::PPQ as u32,
            len: slooper::midipat::PPQ as u32 / 2,
        });
    }
    pat.sort();
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    // Project length = exactly the pattern's bar (2 s), so the looping
    // transport re-enters the clip after every wrap.
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: sr as u64 * 2,
    });
    b.cmd(Cmd::SetLoop(true));

    // Silent while the transport is stopped.
    std::thread::sleep(Duration::from_millis(150));
    wait_until(
        || {
            ring_peak(&ring, &widx, 960)
                .map(|p| p < 0.002)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "silence before the transport starts",
    );

    // Play: the notes must sound through the default piano. This is the
    // path that used to abort the RT thread (midi scratch rows were never
    // allocated): the first staged note panicked inside `process`.
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "MIDI clip audible through the default synth",
    );

    // Still alive mid-bar and after a transport wrap: notes land in many
    // different blocks, and the wrapped playhead re-arms the whole event
    // queue — a dead RT thread stops cycling the graph entirely.
    // Windows are 333 ms so they can never fall inside the 250 ms gap
    // between the last note-off and the wrap.
    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        ring_peak(&ring, &widx, 16000)
            .map(|p| p > 0.002)
            .unwrap_or(false),
        "MIDI render died mid-bar",
    );
    let after = widx.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        widx.load(Ordering::Relaxed) > after,
        "audio graph stopped cycling (RT thread died?)"
    );
    assert!(
        ring_peak(&ring, &widx, 16000)
            .map(|p| p > 0.002)
            .unwrap_or(false),
        "no sound after the transport wrapped through the loop point",
    );

    b.cmd(Cmd::StopAll);
    wait_for(
        &b,
        |e| matches!(e, Evt::Transport { on: false }),
        Duration::from_secs(2),
    );
    std::thread::sleep(Duration::from_millis(700)); // let the tail ring out
    wait_until(
        || {
            ring_peak(&ring, &widx, 2400)
                .map(|p| p < 0.002)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "silence after StopAll (transport gate closes the MIDI render)",
    );
    let _ = active;
}

#[test]
fn e2e_metronome() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));

    let (client, _status) =
        Client::new("metro_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();

    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "metronome test: output not applied"
    );
    // Let the sink settle on silence before arming the click.
    std::thread::sleep(Duration::from_millis(150));

    let read_range = |a: u64, c: u64| -> Vec<f32> {
        let n = ring_n as u64;
        (a..c)
            .map(|i| f32::from_bits(ring[(i % n) as usize].load(Ordering::Relaxed)))
            .collect()
    };

    // --- Phase 1: activation starts immediately on the downbeat ------------
    // 120 BPM, eighth-note subdivision, 4 beats per measure.
    let beat = sr as usize / 2; // samples per quarter note
    let start = widx.load(Ordering::Relaxed);
    b.cmd(Cmd::SetMetro {
        on: true,
        bpm: 120,
        sub: 2,
        num: 4,
    });
    std::thread::sleep(Duration::from_millis(2600)); // ~5 beats
    let x = read_range(start, widx.load(Ordering::Relaxed));
    let clicks = metro_clicks(&x, sr);
    assert!(!clicks.is_empty(), "metronome produced no clicks");
    assert!(
        clicks[0].0 < sr as usize / 10,
        "first click {:.0} ms after activation — must start directly on the beat",
        clicks[0].0 as f32 * 1000.0 / sr as f32
    );

    // Expected tone pattern for 4/4 with eighth notes:
    // downbeat, sub, quarter, sub, quarter, sub, quarter, sub, downbeat, sub.
    let expect = [0u8, 2, 1, 2, 1, 2, 1, 2, 0, 2];
    assert!(
        clicks.len() >= expect.len(),
        "expected at least {} clicks in 2.6 s, got {}",
        expect.len(),
        clicks.len()
    );
    for (k, &want) in expect.iter().enumerate() {
        assert_eq!(
            clicks[k].1,
            want,
            "click {k} has tone {} ({} Hz-class), expected {want} at {:.3} s",
            clicks[k].1,
            [1760, 1175, 880][clicks[k].1 as usize],
            clicks[k].0 as f32 / sr as f32
        );
    }
    // Clicks land on the eighth-note grid (every half beat, ±10%).
    for k in 1..expect.len() {
        let d = clicks[k].0 - clicks[k - 1].0;
        let want = beat / 2;
        assert!(
            (d as isize - want as isize).abs() < (want / 10) as isize,
            "click {k} off grid: interval {d}, expected ~{want}"
        );
    }

    // Playhead freeze: a bare metronome (transport off) must never drag
    // the playhead along — regression for "the metronome forces the
    // playback bar to advance".
    let p0 = b.engine.pos();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(b.engine.pos(), p0, "bare metronome advanced the playhead");

    // --- Phase 2: subdivision change applies on the next quarter note ------
    let start2 = widx.load(Ordering::Relaxed);
    b.cmd(Cmd::SetMetro {
        on: true,
        bpm: 120,
        sub: 4,
        num: 4,
    });
    std::thread::sleep(Duration::from_millis(1500)); // 3 more beats
    let end2 = widx.load(Ordering::Relaxed);
    let x2 = read_range(start2, end2);
    // Analyze only the last ~0.8 s: fully inside the sixteenth-note grid.
    let tail_at = x2.len().saturating_sub(sr as usize * 8 / 10);
    let clicks2 = metro_clicks(&x2[tail_at..], sr);
    assert!(
        clicks2.len() >= 4,
        "sixteenth subdivision not applied after the change: {} clicks in 0.8 s",
        clicks2.len()
    );
    for k in 1..clicks2.len() {
        let d = clicks2[k].0 - clicks2[k - 1].0;
        let want = beat / 4;
        assert!(
            (d as isize - want as isize).abs() < (want / 6) as isize,
            "post-change click {k} off the sixteenth grid: interval {d}, expected ~{want}"
        );
    }

    // --- Phase 3: stop silences the click -----------------------------------
    b.cmd(Cmd::SetMetro {
        on: false,
        bpm: 120,
        sub: 4,
        num: 4,
    });
    std::thread::sleep(Duration::from_millis(400));
    let start3 = widx.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(600));
    let x3 = read_range(start3, widx.load(Ordering::Relaxed));
    assert!(
        metro_clicks(&x3, sr).is_empty(),
        "metronome still clicking after stop"
    );

    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// ------------------------------------------------------------------ export --

/// The Export button's whole path: placed project -> Cmd::ExportTrack /
/// Cmd::ExportProject -> decodable WAV on disk, extension appended from the
/// format (the picker path never carries one), plus explicit "no clips"
/// errors for empty tracks and empty projects.
#[test]
fn e2e_export_writes_file() {
    use slooper::audio_io::ExportFormat;
    let _g = e2e_lock();
    let Some((b, _cn, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    // A project the engine can render without any live ports: a 0.5 asset on
    // track 0, plus a child-lane clip at half level on the same track.
    let a0: Arc<[f32]> = Arc::from(vec![0.5f32; 8_000]);
    let a1: Arc<[f32]> = Arc::from(vec![0.25f32; 4_000]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    arr.insert(0, 1, Clip::new(1, 4_000));
    let table: Arc<Vec<Arc<[f32]>>> = Arc::new(vec![a0, a1]);
    b.cmd(Cmd::SetProject {
        assets: table,
        arr: Arc::new(arr),
        len: 12_000,
    });

    let dir = std::env::temp_dir().join(format!("slooper-e2e-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let stem = dir.join("mix");

    // Track export: both lanes of track 0, engine-rate mixdown.
    b.cmd(Cmd::ExportTrack {
        i: 0,
        path: stem.clone(),
        fmt: ExportFormat::Wav,
    });
    let got = wait_for(
        &b,
        |e| matches!(e, Evt::Notice(m) if m.contains("exported")),
        Duration::from_secs(5),
    );
    assert!(got.is_some(), "no export Notice for a track with clips");
    let file = stem.with_extension("wav"); // extension from the format
    let samples = slooper::audio_io::decode_to_mono(&file, sr).expect("exported wav decodes");
    assert!(
        samples.len() >= 12_000,
        "export shorter than the project: {}",
        samples.len()
    );
    // [0,4000): only the 0.5 clip; [4000,8000): both lanes summed; then silence.
    let seg = |a: usize, c: usize| samples[a..c].iter().sum::<f32>() / (c - a) as f32;
    assert!((seg(0, 4_000) - 0.5).abs() < 0.02, "lane 0 only");
    assert!(
        (seg(4_000, 8_000) - 0.75).abs() < 0.02,
        "lanes stack additively"
    );
    assert!(
        seg(8_000, 12_000).abs() < 1e-6,
        "silence past the last clip"
    );

    // v34 R10 probe: export WHILE the transport rolls, project starts at
    // nonzero seek, clipped + looped clips — placement must still be exact.
    b.cmd(Cmd::Seek { pos: 6_321 });
    b.cmd(Cmd::PlayAll);
    std::thread::sleep(Duration::from_millis(80));
    b.cmd(Cmd::ExportTrack {
        i: 0,
        path: dir.join("rolling"),
        fmt: ExportFormat::Wav,
    });
    let got_r = wait_for(
        &b,
        |e| matches!(e, Evt::Notice(m) if m.contains("exported")),
        Duration::from_secs(5),
    );
    assert!(got_r.is_some(), "no Notice exporting while rolling");
    let roll =
        slooper::audio_io::decode_to_mono(&dir.join("rolling.wav"), sr).expect("rolling wav");
    let seg_r = |a: usize, c: usize| roll[a..c].iter().sum::<f32>() / (c - a) as f32;
    assert!(
        (seg_r(0, 4_000) - 0.5).abs() < 0.02,
        "rolling export segment 1"
    );
    assert!(
        (seg_r(4_000, 8_000) - 0.75).abs() < 0.02,
        "rolling export segment 2"
    );
    assert!(seg_r(8_000, 12_000).abs() < 1e-6, "rolling export tail");
    b.cmd(Cmd::StopAll);

    // Whole-project export: nothing is playing, but placement is the truth.
    b.cmd(Cmd::ExportProject {
        path: dir.join("whole"),
        fmt: ExportFormat::Wav,
    });
    let got = wait_for(
        &b,
        |e| matches!(e, Evt::Notice(m) if m.contains("exported project")),
        Duration::from_secs(5),
    );
    assert!(got.is_some(), "no Notice for the whole-project export");
    assert!(dir.join("whole.wav").exists(), "project wav missing");

    // Empty track / empty project must fail loudly, not write silence.
    b.cmd(Cmd::ExportTrack {
        i: 7,
        path: dir.join("none"),
        fmt: ExportFormat::Wav,
    });
    let err = wait_for(
        &b,
        |e| matches!(e, Evt::Error(m) if m.contains("no placed clips")),
        Duration::from_secs(2),
    );
    assert!(
        err.is_some(),
        "empty-track export must report 'no placed clips'"
    );
    assert!(
        !dir.join("none.wav").exists(),
        "no file may be written for an empty track"
    );

    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(Arrangement::new()),
        len: 4_800,
    });
    b.cmd(Cmd::ExportProject {
        path: dir.join("empty"),
        fmt: ExportFormat::Wav,
    });
    let err = wait_for(
        &b,
        |e| matches!(e, Evt::Error(m) if m.contains("no clips placed")),
        Duration::from_secs(2),
    );
    assert!(err.is_some(), "empty-project export must report an error");
    assert!(
        !dir.join("empty.wav").exists(),
        "no file for an empty project"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// -------------------------------------------------------------- monitoring --

/// Live input pass-through follows the SELECTED track: input x (vol x gain)
/// of the selected track, equal-power panned across the routed outputs.
/// Nothing needs to be recording for this to hold — that is the whole point
/// of "monitoring rides the selection".
#[test]
fn e2e_selected_loop_monitoring() {
    let _g = e2e_lock();
    let Some((b, _cn, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 16;
    let ring_l: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let ring_r: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));

    struct MonCapProc {
        out: Port<AudioOut>,
        sink_l: Port<AudioIn>,
        sink_r: Port<AudioIn>,
        rl: Arc<Vec<AtomicU32>>,
        rr: Arc<Vec<AtomicU32>>,
        widx: Arc<AtomicU64>,
    }
    impl ProcessHandler for MonCapProc {
        fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
            self.out.as_mut_slice(ps).fill(0.5);
            let l = self.sink_l.as_slice(ps);
            let r = self.sink_r.as_slice(ps);
            let base = self.widx.load(Ordering::Relaxed) as usize;
            for (i, s) in l.iter().enumerate() {
                self.rl[(base + i) % self.rl.len()].store(s.to_bits(), Ordering::Relaxed);
            }
            for (i, s) in r.iter().enumerate() {
                self.rr[(base + i) % self.rr.len()].store(s.to_bits(), Ordering::Relaxed);
            }
            self.widx.fetch_add(ps.n_frames() as u64, Ordering::Relaxed);
            Control::Continue
        }
    }

    let (client, _status) =
        Client::new("mon_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let sink_l = client.register_port("sink_l", AudioIn::default()).unwrap();
    let sink_r = client.register_port("sink_r", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            MonCapProc {
                out: out_a,
                sink_l,
                sink_r,
                rl: ring_l.clone(),
                rr: ring_r.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();

    // Selected loop 0 at vol 0.5, gain 1.0, pan hard right (1.0):
    // expect L = 0, R = 0.5 x 0.5 = 0.25 on the monitored signal.
    b.engine.set_loop_vol(0, 0.5);
    b.engine.set_loop_gain(0, 1.0);
    b.engine.set_loop_pan(0, 1.0);
    b.cmd(Cmd::SetSelected(0));
    b.cmd(Cmd::Apply {
        ins: vec![format!("{src_name}:out_a")],
        outs: vec![
            (format!("{src_name}:sink_l"), 1),
            (format!("{src_name}:sink_r"), 2),
        ],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { .. }),
            Duration::from_secs(5)
        )
        .is_some(),
        "layout not applied"
    );

    let expect = |ring: &Arc<Vec<AtomicU32>>, want: f32, what: &str| {
        let mut seen = None;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(30));
            if let Some(x) = read_ring(ring, &widx, 480, Duration::from_millis(200)) {
                let m = x.iter().sum::<f32>() / x.len() as f32;
                seen = Some(m);
                if (m - want).abs() < 0.02 {
                    return;
                }
            }
        }
        panic!("{what}: want mean {want}, last saw {seen:?}");
    };
    expect(&ring_l, 0.0, "left bus (pan hard right)");
    expect(&ring_r, 0.25, "right bus (input x selected-track vol)");

    // Switching the selection to loop 1 (untouched: vol 1, gain 1, centre)
    // re-shapes monitoring immediately: both buses at 0.5 x 0.7071 ≈ 0.354.
    b.engine.set_loop_vol(1, 1.0);
    b.engine.set_loop_gain(1, 1.0);
    b.engine.set_loop_pan(1, 0.5);
    b.cmd(Cmd::SetSelected(1));
    let want = 0.5f32 * std::f32::consts::FRAC_1_SQRT_2;
    expect(&ring_l, want, "left bus after selection switch");
    expect(&ring_r, want, "right bus after selection switch");

    // Settings → live input monitoring (Cmd::SetMonitor): off mutes the
    // pass-through while every level stays put; on brings it straight back.
    b.cmd(Cmd::SetMonitor(false));
    expect(&ring_l, 0.0, "left bus after monitoring off");
    expect(&ring_r, 0.0, "right bus after monitoring off");
    b.cmd(Cmd::SetMonitor(true));
    let want = 0.5f32 * std::f32::consts::FRAC_1_SQRT_2;
    expect(&ring_l, want, "left bus after monitoring back on");
    expect(&ring_r, want, "right bus after monitoring back on");

    // Silence via volume must silence monitoring too.
    b.engine.set_loop_vol(1, 0.0);
    expect(&ring_l, 0.0, "left bus after vol 0");
    expect(&ring_r, 0.0, "right bus after vol 0");

    b.engine.set_loop_vol(0, 1.0);
    b.engine.set_loop_pan(0, 0.5);
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![],
    });
    let _ = active;
}

// ------------------------------------------------------------------- vst ---

/// VST inserts on the export path. A constant burst on a track carrying a
/// real reverb plugin must produce a tail after the clip end — exactly where
/// the same render without the insert is silent — proving the private-copy
/// render path opens, processes, and parameterises real VST2/VST3 binaries.
/// Skips itself when no plugin is installed (CI).
#[test]
fn e2e_vst_insert_export() {
    use slooper::audio_io::ExportFormat;
    use slooper::vsthost::{open, VstKind};
    let _g = e2e_lock();
    let home = std::env::var("HOME").unwrap_or_default();
    let plugin = [
        format!("{home}/.vst3/DragonflyHallReverb.vst3"),
        format!("{home}/.vst/DragonflyHallReverb-vst.so"),
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .find(|p| p.exists());
    let Some(plugin) = plugin else { return }; // no plugin installed: skip
    let kind = if plugin.extension().is_some_and(|e| e == "vst3") {
        VstKind::V3
    } else {
        VstKind::V2
    };
    let Some((b, _cn, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // 4k-sample constant burst on track 0; 16k-sample project leaves a 12k
    // tail window that is provably dry-silent without the insert.
    let a0: Arc<[f32]> = Arc::from(vec![0.3f32; 4_000]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![a0]),
        arr: Arc::new(arr),
        len: 16_000,
    });

    let dir = std::env::temp_dir().join(format!("slooper-e2e-vst-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let export = |name: &str| {
        b.cmd(Cmd::ExportTrack {
            i: 0,
            path: dir.join(name),
            fmt: ExportFormat::Wav,
        });
        let got = wait_for(
            &b,
            |e| matches!(e, Evt::Notice(m) if m.contains("exported")),
            Duration::from_secs(5),
        );
        assert!(got.is_some(), "no export Notice ({name})");
        slooper::audio_io::decode_to_mono(&dir.join(format!("{name}.wav")), sr)
            .expect("exported wav decodes")
    };

    let dry = export("dry");
    let energy = |x: &[f32]| x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
    assert!(energy(&dry[8_000..]) < 1e-9, "dry tail must be silence");

    // Attach the reverb (same open path the UI uses) and export again.
    let load = open(&plugin, kind, sr, None).expect("open reverb plugin");
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx {
            slots: Arc::new(vec![FxSlot::vst(load.entry.clone())]),
        },
    });
    // Same command queue: the export below necessarily runs after the swap.

    let wet = export("wet");
    assert!(wet.iter().all(|v| v.is_finite()), "non-finite samples");
    assert!(energy(&wet[..4_000]) > 1e-4, "wet body must carry audio");
    assert!(
        energy(&wet[8_000..]) > 1e-8,
        "reverb insert must leave a tail where the dry render is silent"
    );

    // Removing the insert restores the dry export.
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx::default(),
    });
    drop(load);
    let back = export("back");
    assert!(
        energy(&back[8_000..]) < 1e-9,
        "tail gone after removing insert"
    );

    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(Arrangement::new()),
        len: 4_800,
    });
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------- midi sustain --

/// Build a one-note pattern (start 0, `len_secs` long) at 120 BPM.
fn one_note_pattern(sr: u32, note: u8, len_secs: f64) -> slooper::midipat::Pattern {
    let mut pat = slooper::midipat::Pattern::new(120);
    let ticks = (len_secs * 2.0 * slooper::midipat::PPQ as f64) as u32;
    pat.notes.push(slooper::midipat::Note {
        ch: 0,
        note,
        vel: 110,
        press: 0,
        start: 0,
        len: ticks,
    });
    pat.len_ticks = ticks;
    pat.sort();
    let _ = sr;
    pat
}

/// RMS of a capture window.
fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// A single LONG note must keep sounding across the blocks where the
/// schedule carries NO events — the v0.7.1 regression that made every MIDI
/// track a short blip: `render_midi_block` skipped advancing the synth
/// whenever a block had no staged events, so only the attack block of each
/// note ever rendered. The body of the note lives INSIDE the instrument.
#[test]
fn e2e_midi_note_sustains_across_blocks() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("midi_sus", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Default piano; one whole-note pattern (2 s), NOT a run of quarter
    // notes — attacks alone must not be able to pass this test.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.cmd(Cmd::SetDefaultSynth(Some(Arc::new(
        slooper::audio::SynthHandle::Script(inst),
    ))));
    let pat = one_note_pattern(sr, 57, 2.0);
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: (sr as u64) * 3,
    });
    b.cmd(Cmd::SetLoop(false));

    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "note attack audible",
    );
    let head = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("head window");
    let head_rms = rms(&head);
    assert!(head_rms > 1e-3, "attack body must carry audio: {head_rms}");

    // ~1.1 s into the 2 s note: no event has been staged for ~100 blocks.
    // The note must still be sounding. (Before the v0.7.1 fix this window
    // was exact silence: the synth sat frozen between events.)
    std::thread::sleep(Duration::from_millis(950));
    let mid = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("mid-note window");
    let mid_rms = rms(&mid);
    assert!(
        mid_rms > 2e-3,
        "sustained note must keep sounding mid-body: {mid_rms}"
    );
    assert!(
        mid_rms > 0.2 * head_rms,
        "note body collapsed to a blip: mid {mid_rms} vs head {head_rms}"
    );

    // 0.5 s after the note ended (damper on): back to quiet.
    std::thread::sleep(Duration::from_millis(1400));
    let tail = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("release window");
    let tail_rms = rms(&tail);
    assert!(
        tail_rms < 0.25 * mid_rms.max(1e-9),
        "released note must decay: tail {tail_rms} vs mid {mid_rms}"
    );

    b.cmd(Cmd::StopAll);
    std::thread::sleep(Duration::from_millis(500));
    wait_until(
        || {
            ring_peak(&ring, &widx, 2400)
                .map(|p| p < 0.002)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "silence after StopAll (sustain state must reset on stop)",
    );
    let _ = active;
}

/// The VSTi path of the same contract: a plugin instrument must keep being
/// PROCESSED through event-free blocks (its output lives in the host staging
/// ring; pausing starves it), StopAll must flush the ring AND kill plugin
/// voices so nothing leaks into the next run. Skips without the plugin.
#[test]
fn e2e_midi_vsti_sustains_live() {
    let _g = e2e_lock();
    // v37 R4: plugin under test is selectable — point SLOOPER_E2E_VSTI at
    // any .vst3/.so instrument to replay the full sustain contract on it
    // (default: Vitalium).
    let path = match std::env::var("SLOOPER_E2E_VSTI") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => match std::env::var("HOME") {
            Ok(h) => std::path::PathBuf::from(h).join(".vst3/vitalium.vst3"),
            Err(_) => return,
        },
    };
    if !path.exists() {
        eprintln!("skipping: {} not installed", path.display());
        return;
    }
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("vsti_sus", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    let kind = if path.extension().map(|e| e == "vst3").unwrap_or(false) {
        slooper::vsthost::VstKind::V3
    } else {
        slooper::vsthost::VstKind::V2
    };
    let load = slooper::vsthost::open(&path, kind, sr, None)
        .unwrap_or_else(|_| panic!("{} must load", path.display()));
    b.cmd(Cmd::SetTrackSynth {
        i: 0,
        synth: Some(Arc::new(slooper::audio::SynthHandle::Vst(
            load.entry.clone(),
        ))),
    });
    let pat = one_note_pattern(sr, 60, 2.0);
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: (sr as u64) * 3,
    });
    b.cmd(Cmd::SetLoop(false));

    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(6),
        "VSTi note audible",
    );
    let head = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("head window");
    let head_rms = rms(&head);
    assert!(head_rms > 1e-3, "VSTi attack must carry audio: {head_rms}");

    std::thread::sleep(Duration::from_millis(950));
    let mid = read_ring(&ring, &widx, (sr as usize) / 10, Duration::from_millis(500))
        .expect("mid-note window");
    let mid_rms = rms(&mid);
    assert!(
        mid_rms > 2e-3,
        "VSTi must keep producing through event-free blocks: {mid_rms}"
    );
    assert!(
        mid_rms > 0.2 * head_rms,
        "VSTi note collapsed to a blip: mid {mid_rms} vs head {head_rms}"
    );

    b.cmd(Cmd::StopAll);
    std::thread::sleep(Duration::from_millis(600));
    wait_until(
        || {
            ring_peak(&ring, &widx, 2400)
                .map(|p| p < 0.002)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "StopAll must silence the plugin (flush ring + all-notes-off)",
    );
    let _ = active;
}

/// The audition/preview chain end to end: `bounce_track_preview` must
/// render a SUSTAINED tone (not an attack blip) through the track's synth,
/// and the rendered buffer must actually reach the master output via
/// `Cmd::PreviewPlay` while the transport is stopped, then auto-stop.
#[test]
fn e2e_preview_renders_and_plays() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("preview_e2e", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Track 0 gets the bundled piano; audition one staged note (on @0,
    // off @1.5 s) through the exact UI preview path.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg.create_instance("piano", sr as f64, &[]).expect("piano");
    b.cmd(Cmd::SetTrackSynth {
        i: 0,
        synth: Some(Arc::new(slooper::audio::SynthHandle::Script(inst))),
    });
    std::thread::sleep(Duration::from_millis(200));

    let n = (sr as usize) * 3;
    let mut ev = vec![
        (0u64, 3u8, 0u8, 57u8, 110u8),
        ((sr as u64) * 3 / 2, 0, 0, 57, 0),
    ];
    let buf = b
        .engine
        .bounce_track_preview(0, &mut ev, n)
        .expect("preview bounce (track 0)");
    let win = (sr as usize) / 10;
    let head_rms = rms(&buf[(sr as usize) / 20..(sr as usize) / 20 + win]);
    let mid_rms = rms(&buf[(sr as usize)..(sr as usize) + win]);
    assert!(head_rms > 1e-3, "preview head must carry audio: {head_rms}");
    // "Not a blip": the note lasts 1.5 s, so 0.5–1.0 s must still carry it.
    // The recorded piano DECAYS (a real grand loses roughly half its level
    // in the first second), so the bar is a blip-proof floor, not the flat
    // sustain the old oscillator piano made easy: a 30 ms blip leaves
    // mid_rms ~1e-4 here.
    assert!(
        mid_rms > 0.05 * head_rms && mid_rms > 2e-3,
        "preview must sustain, not blip: mid {mid_rms} vs head {head_rms}"
    );

    // Play it: audible at the sink with the transport STOPPED, then silent
    // once the buffer ran out (auto-stop).
    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PreviewPlay {
        id: 7,
        samples: Arc::from(buf),
    });
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "preview audible at the master out",
    );
    std::thread::sleep(Duration::from_millis(3100));
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p < 0.002)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "preview auto-stops at the end of its buffer",
    );

    // The user's real setting: audition THROUGH A TRACK whose instrument is
    // a VSTi (`preview_track` points at it from the piano roll). The
    // offline plugin render must also produce a sustained tone, not a blip.
    let vst_path = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h).join(".vst3/vitalium.vst3"),
        Err(_) => std::path::PathBuf::from("/nonexistent"),
    };
    if vst_path.exists() {
        let load = slooper::vsthost::open(&vst_path, slooper::vsthost::VstKind::V3, sr, None)
            .expect("vitalium must load");
        b.cmd(Cmd::SetTrackSynth {
            i: 0,
            synth: Some(Arc::new(slooper::audio::SynthHandle::Vst(
                load.entry.clone(),
            ))),
        });
        std::thread::sleep(Duration::from_millis(200));
        let mut ev = vec![
            (0u64, 3u8, 0u8, 60u8, 110u8),
            ((sr as u64) * 3 / 2, 0, 0, 60, 0),
        ];
        let buf = b
            .engine
            .bounce_track_preview(0, &mut ev, n)
            .expect("VSTi preview bounce");
        let head_rms = rms(&buf[(sr as usize) / 20..(sr as usize) / 20 + win]);
        let mid_rms = rms(&buf[(sr as usize)..(sr as usize) + win]);
        assert!(head_rms > 1e-3, "VSTi preview head: {head_rms}");
        assert!(
            mid_rms > 0.2 * head_rms && mid_rms > 2e-3,
            "VSTi preview must sustain through the whole window: mid {mid_rms} vs head {head_rms}"
        );
    }
    let _ = active;
}

/// Seeking away from a LONG held note must silence it: the seek handler
/// all-`off`s every synth AND drops the sustain bookkeeping, so a held
/// note cannot keep rendering after its timeline window is gone.
#[test]
fn e2e_midi_seek_silences_held_note() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("midi_seek", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg.create_instance("piano", sr as f64, &[]).expect("piano");
    b.cmd(Cmd::SetDefaultSynth(Some(Arc::new(
        slooper::audio::SynthHandle::Script(inst),
    ))));
    let pat = one_note_pattern(sr, 57, 4.0);
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: (sr as u64) * 10,
    });
    b.cmd(Cmd::SetLoop(false));

    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "note audible before the seek",
    );
    // Jump to 6 s — past the clip end, mid-note. Nothing may keep ringing.
    b.cmd(Cmd::Seek {
        pos: (sr as u64) * 6,
    });
    wait_until(
        || {
            ring_peak(&ring, &widx, 2400)
                .map(|p| p < 0.002)
                .unwrap_or(false)
        },
        Duration::from_secs(4),
        "seek must silence the held note (sustain state resets with the position)",
    );
    // And stay silent while the transport keeps running over empty bars.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        ring_peak(&ring, &widx, 4800)
            .map(|p| p < 0.002)
            .unwrap_or(false),
        "held note resurrected after the seek (sustain kept advancing)"
    );
    b.cmd(Cmd::StopAll);
    let _ = active;
}

// ===================== v0.10 acceptance pins =====================

/// AC8 (realtime, honest, v40 semantics): a new preview REPLACES the
/// voice in flight — a silent second start silences the playing one —
/// and when a voice runs out of samples the RT thread retires the pool
/// (auto-stop + drain flag) so previews can never "run constantly".
#[test]
fn e2e_preview_replaces_and_drains() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("additive_e2e", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Two phase-independent voices (sines an octave apart) at equal level:
    // one voice peaks at ~A*F1S2; a sum of two clearly beats A*F1S2 by 3 dB.
    let mk = |f: f32| -> Arc<[f32]> {
        let n = sr as usize;
        Arc::from(
            (0..n)
                .map(|k| (2.0 * std::f32::consts::PI * f * k as f32 / sr as f32).sin() * 0.4)
                .collect::<Vec<f32>>(),
        )
    };
    b.cmd(Cmd::PreviewPlay {
        id: 11,
        samples: mk(440.0),
    });
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.2)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "first preview audible",
    );
    let solo = ring_peak(&ring, &widx, 4800).unwrap_or(0.0);
    // v40 REPLACE: a silent voice starts WHILE the 1 s sine still runs —
    // the sine must go silent within a block or two, NOT keep mixing over
    // (the old additive pool kept both voices; that stacking is exactly
    // the "preview runs constantly" behavior this batch retired).
    let silent: Arc<[f32]> = Arc::from(vec![0.0f32; sr as usize]);
    b.cmd(Cmd::PreviewPlay { id: 12, samples: silent });
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p < 0.002).unwrap_or(false),
        Duration::from_millis(500),
        "the new start replaced the sine mid-voice (silence took over)",
    );
    // Drain retirement: the silent voice ends on its own; the RT thread
    // clears `preview_on` and arms the UI retire flag. (The engine-side
    // pool itself is cleared by the UI frame loop / next start — App::logic
    // has its own headless pins.)
    wait_until(
        || !b.engine.preview_active(),
        Duration::from_secs(3),
        "preview auto-stopped at end of buffer",
    );
    assert!(
        b.engine.take_preview_drained(),
        "RT armed the pool-retire flag when the voice drained"
    );

    // Same asset twice = one restarting voice, never a doubled sum (v40).
    let a = mk(660.0);
    b.engine.play_preview(31, a.clone());
    b.engine.play_preview(31, a);
    assert_eq!(
        b.engine.preview_snapshot().len(),
        1,
        "a repeat replaces, it does not stack"
    );
    let mut best2 = 0.0f32;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(30));
        best2 = best2.max(ring_peak(&ring, &widx, 4800).unwrap_or(0.0));
    }
    assert!(
        best2 < 0.42,
        "single voice must NOT double its peak: {best2} (solo was {solo})"
    );
    b.engine.stop_preview();

    // AC6.2: starting the transport must clear every audition voice —
    // auditions never double over real playback (manager-path wiring).
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p < 0.002 && b.engine.preview_snapshot().is_empty())
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "PlayAll clears every audition voice",
    );
}

/// AC3: a VST2 EFFECT on a track chain passes audio (effOpen handshake).
/// Before the fix, every VST2 effect zeroed its track — the whole point of
/// the "VST2 plugins silence everything" bug report.
#[test]
fn e2e_vst2_effect_not_silent() {
    let _g = e2e_lock();
    let home = std::env::var("HOME").unwrap_or_default();
    let dll = std::path::PathBuf::from(&home).join(".vst/wolf-shaper-vst.so");
    if !dll.is_file() {
        eprintln!("no wolf-shaper on this host: skipping");
        return;
    }
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("vst2fx_e2e", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Loud-ish square through the plugin (wolf-shaper needs drive to move).
    let a: Arc<[f32]> = Arc::from(
        (0..(sr as usize))
            .map(|k| if (k / 80) % 2 == 0 { 0.55f32 } else { -0.55 })
            .collect::<Vec<f32>>(),
    );
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![a]),
        arr: Arc::new(arr),
        len: sr as u64,
    });

    // DRY reference first (no chain): proves the clip itself is audible.
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.2)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "dry clip audible",
    );
    b.cmd(Cmd::StopAll);

    // Now the same clip behind the VST2 effect.
    let load = slooper::vsthost::open(&dll, slooper::vsthost::VstKind::V2, sr, None)
        .expect("open wolf-shaper");
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: slooper::audio::TrackFx {
            slots: std::sync::Arc::new(vec![slooper::audio::FxSlot::vst(load.entry.clone())]),
        },
    });
    std::thread::sleep(Duration::from_millis(200));
    b.cmd(Cmd::Seek { pos: 0 });
    b.cmd(Cmd::PlayAll);
    let mut peak = 0.0f32;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(30));
        peak = peak.max(ring_peak(&ring, &widx, 4800).unwrap_or(0.0));
    }
    b.cmd(Cmd::StopAll);
    assert!(
        peak > 0.05,
        "VST2 effect muted the track (peak {peak}) — effOpen regression?"
    );
}

/// v13 AC3.1: the user drags a NEW sample onto the sampler while MIDI is
/// actively playing. The patch must change live — the v0.12 report ("the
/// sampler only picks up a new sample after a restart") must never return.
/// Repro shape: asset A (loud) is bound, notes fire every 250 ms; while the
/// transport runs, a second asset B (near-silent) lands in the rack (same
/// Cmd::SetProject push `ensure_asset` performs) and param 0 is set (what
/// the synth-window drop zone does). Output must fall to B's level without
/// any stop/restart.
#[test]
fn e2e_v13_sampler_patch_swap_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("samp_swap", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Sampler on track 0, asset 0 = a loud flat buffer (0.9 DC).
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("sampler", sr as f64, &[0.0, 48.0, 1.0])
        .expect("bundled sampler must compile and load");
    b.cmd(Cmd::SetTrackSynth {
        i: 0,
        synth: Some(Arc::new(slooper::audio::SynthHandle::Script(inst.clone()))),
    });

    // 12 short notes, one every 250 ms — events keep arriving for 3 s, so
    // "actively playing MIDI" for the whole swap window.
    let mut pat = slooper::midipat::Pattern::new(120);
    let step = (0.25 * 2.0 * slooper::midipat::PPQ as f64) as u32;
    for i in 0..12u32 {
        pat.notes.push(slooper::midipat::Note {
            ch: 0,
            note: 60,
            vel: 110,
            press: 0,
            start: i * step,
            len: step / 2,
        });
    }
    pat.len_ticks = 12 * step;
    pat.sort();
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    let loud: Arc<[f32]> = Arc::from(vec![0.9f32; 4096]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![loud.clone()]),
        arr: Arc::new(arr.clone()),
        len: (sr as u64) * 4,
    });
    b.cmd(Cmd::SetLoop(false));
    std::thread::sleep(Duration::from_millis(150));
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 2400)
                .map(|p| p > 0.3)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "asset A (loud) audible while notes fire",
    );

    // Mid-playback: the rack gains asset 1 (a near-silent take) and the
    // user drops it on the sampler — exactly the two host-side effects of
    // `ensure_asset` + the drop zone's set_param.
    let quiet: Arc<[f32]> = Arc::from(vec![0.02f32; 4096]);
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![loud, quiet]),
        arr: Arc::new(arr),
        len: (sr as u64) * 4,
    });
    std::thread::sleep(Duration::from_millis(250)); // manager applies SetProject
    inst.set_param(0, 1.0); // the drop

    std::thread::sleep(Duration::from_millis(400));
    let win = read_ring(&ring, &widx, (sr as usize) / 4, Duration::from_millis(500))
        .expect("swap window");
    let peak = win.iter().fold(0f32, |m, s| m.max(s.abs()));
    b.cmd(Cmd::StopAll);
    assert!(
        peak < 0.15,
        "sampler kept playing the OLD sample after a live patch swap: peak {peak} \
         (asset B is 0.02) — the drop did not reach the live synth"
    );
    let _ = active;
}

// ---------------------------------------------------------------------------
// v23 editor crash guard — child protocol (issue 3: "ADLplug briefly shows a
// solid blue window, then crash the entire program").
//
// The guard's whole promise is that a vendor editor SIGSEGV lands on a
// THROWAWAY child, never the app. That promise lives in the real binary's
// main() dispatch (`vstedit::probe_child`), which a test harness cannot
// emulate — so this spawns the actual `slooper` binary as the probe child.
// The strongest signal is implicit: this test process must still be alive
// to make the assertion.
// ---------------------------------------------------------------------------

fn v23_probe_child(path: &std::path::Path, kind: &str) -> std::process::ExitStatus {
    use std::process::{Command, Stdio};
    let exe = std::env::var("CARGO_BIN_EXE_slooper").expect("cargo bin env");
    Command::new(exe)
        .env("SLOOPER_EDITOR_PROBE", kind)
        .env("SLOOPER_EDITOR_PROBE_PATH", path)
        .env("SLOOPER_NULL_AUDIO", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn probe child")
}

#[test]
fn v23_probe_child_dies_instead_of_the_host() {
    if !slooper::vstedit::x11_available() {
        eprintln!("skip: no X display for editor probes");
        return;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let adl = std::path::Path::new(&home).join(".vst/ADLplug.so");
    if adl.is_file() {
        let st = v23_probe_child(&adl, "V2");
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        let crashed = st.signal().is_some() || st.code() == Some(4);
        assert!(
            crashed,
            "ADLplug's editor must take down ONLY the child (status {st:?})"
        );
    }
    let wolf = std::path::Path::new(&home).join(".vst3/wolf-shaper.vst3");
    if wolf.is_dir() {
        let st = v23_probe_child(&wolf, "V3");
        // Exit 2 = did not load in the child = NOT an editor verdict; that
        // is a skip, a crash-or-hang verdict against a healthy editor is a
        // test failure.
        if st.code() != Some(2) {
            assert!(
                st.success(),
                "wolf-shaper's editor is healthy; the probe said {st:?}"
            );
        }
    }
}

/// v32: MIDI source + audio sink in one client. `stage` commands the note:
/// 0 idle, 1 write note-on next cycle, 2 write note-off next cycle; the
/// sink copies slooper's output into the shared capture ring.
struct LiveMidiCapProc {
    out: Port<MidiOut>,
    stage: Arc<AtomicUsize>,
    sink: Port<AudioIn>,
    ring: Arc<Vec<AtomicU32>>,
    widx: Arc<AtomicU64>,
}

impl ProcessHandler for LiveMidiCapProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        match self.stage.swap(0, Ordering::SeqCst) {
            1 => {
                let _ = self.out.writer(ps).write(&RawMidi {
                    time: 0,
                    bytes: &[0x90, 60, 100],
                });
            }
            2 => {
                let _ = self.out.writer(ps).write(&RawMidi {
                    time: 0,
                    bytes: &[0x80, 60, 0],
                });
            }
            _ => {}
        }
        let n = ps.n_frames() as usize;
        if n == 0 {
            return Control::Continue;
        }
        let sink = self.sink.as_slice(ps);
        let nring = self.ring.len() as u64;
        let mut w = self.widx.load(Ordering::Relaxed);
        for s in sink {
            self.ring[(w % nring) as usize].store(s.to_bits(), Ordering::Relaxed);
            w += 1;
        }
        self.widx.store(w, Ordering::Relaxed);
        Control::Continue
    }
}

/// The bug this pins: MIDI arrived, the monitor blinked, bindings fired,
/// recording worked — and no synthesizer ever sounded live, for any synth
/// type, because the realtime path forwarded messages to the UI and nobody
/// fed the instruments. Playing the keyboard must play through the
/// selected track's synth with the transport STOPPED (the "live feedback
/// while recording" the user never heard), a held key must keep sounding,
/// a BOUND note must stay silent (bound = control traffic, the rule MIDI
/// recording already used), and the settings gate must gate.
#[test]
fn e2e_live_midi_input_plays_synths() {
    let _g = e2e_lock();
    let Some((b, client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let stage = Arc::new(AtomicUsize::new(0));
    let (client, _status) =
        Client::new("midi_live", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let midi_out = client
        .register_port("midi_out", MidiOut::default())
        .unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    client
        .connect_ports_by_name(
            &format!("{src_name}:midi_out"),
            &format!("{client_name}:midi_in"),
        )
        .expect("connect to slooper:midi_in");
    let active = client
        .activate_async(
            (),
            LiveMidiCapProc {
                out: midi_out,
                stage: stage.clone(),
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // The bundled piano answers on the default-selected track. NO clips
    // and the transport never starts: everything this test hears is the
    // live feed, which pre-v32 did not exist.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.cmd(Cmd::SetDefaultSynth(Some(Arc::new(
        slooper::audio::SynthHandle::Script(inst),
    ))));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        ring_peak(&ring, &widx, 4800)
            .map(|p| p < 0.002)
            .unwrap_or(false),
        "idle before playing: silent"
    );

    // Play. The attack is audible within a few blocks...
    stage.store(1, Ordering::SeqCst);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "live note-on plays the selected track's synth (transport STOPPED)",
    );
    let head = read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("head");
    let head_rms = rms(&head);
    // ...and the held key keeps sounding long after the block its message
    // arrived in (the synth is still timed, not timed out).
    std::thread::sleep(Duration::from_millis(500));
    let mid = read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("mid");
    let mid_rms = rms(&mid);
    assert!(
        mid_rms > 0.1 * head_rms,
        "held key keeps sounding: mid {mid_rms} vs head {head_rms}"
    );
    // Release: it decays.
    stage.store(2, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(1400));
    let tail = read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("tail");
    assert!(
        rms(&tail) < 0.25 * mid_rms,
        "released key decays: {} vs {mid_rms}",
        rms(&tail)
    );

    // Bound note = control traffic: note 60 bound to an action must not
    // sound live (its release passes through untouched — never a stuck
    // note).
    b.engine.set_midi_bound(slooper::audio::BoundSet {
        exact: vec![slooper::audio::bind_key(0, 0, 60)],
        axes: vec![],
    });
    std::thread::sleep(Duration::from_millis(150));
    stage.store(1, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(400));
    stage.store(2, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        ring_peak(&ring, &widx, sr as usize / 4)
            .map(|p| p < 0.002)
            .unwrap_or(false),
        "a bound note is control traffic: it must not play the synth"
    );

    // The settings gate: unbound, gate OFF → silent; gate ON → plays again.
    b.engine.set_midi_bound(slooper::audio::BoundSet::default());
    b.engine.set_midi_live(false);
    std::thread::sleep(Duration::from_millis(60));
    stage.store(1, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        ring_peak(&ring, &widx, sr as usize / 4)
            .map(|p| p < 0.002)
            .unwrap_or(false),
        "live MIDI switched off: the keyboard is silent"
    );
    stage.store(2, Ordering::SeqCst);
    b.engine.set_midi_live(true);
    std::thread::sleep(Duration::from_millis(120));
    stage.store(1, Ordering::SeqCst);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "gate back on: the keyboard plays again",
    );
    stage.store(2, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    let _ = active;
}

/// The bug this pins: gain changes were block-CONSTANT multipliers, so
/// loop start/stop, mutes and volume jumps cut the waveform mid-swing at
/// a block edge — the "static", everyone heard, on every asset and synth.
/// With a constant-value (DC) clip the un-smoothed path produces one
/// 0.5-sized SAMPLE-TO-SAMPLE JUMP per event, while the v32 ramps cap the
/// delta at amp/block-size. Detecting |Δ| is therefore exact, not fussy.
#[test]
fn e2e_no_click_gain_steps() {
    let _g = e2e_lock();
    let Some((b, _client_name, _sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("click_test", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // One constant 0.5 clip: any discontinuity is a single huge Δ.
    let a0: Arc<[f32]> = Arc::from(vec![0.5f32; 16_000]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    let table: Arc<Vec<Arc<[f32]>>> = Arc::new(vec![a0]);
    b.cmd(Cmd::SetProject {
        assets: table,
        arr: Arc::new(arr),
        len: 32_000,
    });
    b.cmd(Cmd::SetLoop(true));

    let max_delta =
        |x: &[f32]| -> f32 { x.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs())) };
    // Capture a SPANK that straddles a command: ring positions are exact,
    // so the transition itself (not just the aftermath) lands inside.
    let read_span =
        |ring: &Arc<Vec<AtomicU32>>, widx: &AtomicU64, w0: u64, back: u64, fwd: u64| -> Vec<f32> {
            let n = ring.len() as u64;
            while widx.load(Ordering::Relaxed).saturating_sub(w0) < fwd {
                std::thread::sleep(Duration::from_millis(5));
            }
            ((w0 - back)..(w0 + fwd))
                .map(|i| f32::from_bits(ring[(i % n) as usize].load(Ordering::Relaxed)))
                .collect()
        };

    // Play: steady DC, zero deltas (float-exact except the start ramp).
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "DC clip playing",
    );
    let steady = read_ring(&ring, &widx, 4800, Duration::from_millis(500)).expect("steady");
    assert!(
        max_delta(&steady) < 0.02,
        "steady playback shows steps: {}",
        max_delta(&steady)
    );

    // STOP: the fade must take the level to zero in small steps — measure
    // the window that CONTAINS the stop command.
    let w0 = widx.load(Ordering::Relaxed);
    b.cmd(Cmd::StopAll);
    let stop = read_span(&ring, &widx, w0, 480, 4800);
    assert!(
        max_delta(&stop) < 0.02,
        "transport STOP clicked: max sample delta {}",
        max_delta(&stop)
    );
    assert!(
        stop[stop.len() - 100..].iter().all(|s| s.abs() < 0.02),
        "stop did not fade to silence"
    );

    // Play again (the start edge clicked too, pre-v32), then MUTE.
    let w1 = widx.load(Ordering::Relaxed);
    b.cmd(Cmd::PlayAll);
    let play = read_span(&ring, &widx, w1, 480, 4800);
    assert!(
        max_delta(&play) < 0.02,
        "transport PLAY clicked: max sample delta {}",
        max_delta(&play)
    );
    wait_until(
        || {
            ring_mean(&ring, &widx, 480)
                .map(|m| (m - 0.5).abs() < 0.05)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "playing again",
    );
    let w2 = widx.load(Ordering::Relaxed);
    b.cmd(Cmd::ToggleTrackOn { i: 0 });
    let mute = read_span(&ring, &widx, w2, 480, 4800);
    assert!(
        max_delta(&mute) < 0.02,
        "track MUTE clicked: max sample delta {}",
        max_delta(&mute)
    );
    b.cmd(Cmd::StopAll);
    std::thread::sleep(Duration::from_millis(80));
    let _ = active;
}

/// v32 R5: sidechaining works end to end. Track 0 plays a quiet 110 Hz
/// key; track 1 plays a steady tone through the stock ABI-v2 Sidechain
/// Compressor. With the send routed (bit 0 of track 1's mask) the mixed
/// output must drop — proof that the post-fader tap feeds the SC bus and
/// a v2 script actually receives it. Unrouted, the tone is untouched.
#[test]
fn e2e_sidechain_ducks_target_track() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Compile/load the stock sidechain compressor through the REAL
    // registry (first run compiles with rustc, then the hash cache hits).
    let dir = std::env::temp_dir().join(format!("slooper-e2e-sc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut reg = slooper::fx::Registry::open(dir.clone());
    reg.rescan();
    // threshold -30 dB, ratio 4, attack 2 ms, release 100 ms, knee 6, depth 1.
    let vals = [-30.0f32, 4.0, 2.0, 100.0, 6.0, 1.0];
    let desc = reg
        .desc("sidechain_comp")
        .expect("stock sidechain_comp present")
        .clone();
    let inst = reg
        .create_instance("sidechain_comp", sr as f64, &vals)
        .expect("sidechain_comp compiles and instantiates");

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("sc_duck", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    // Probe the RIGHT bus only: the key track is panned hard left so its
    // own energy never touches the measured signal — the sink hears the
    // target tone alone, which is exactly what the duck must move.
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 2)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Key: 110 Hz sine, hard LEFT (invisible to the probe, but the SC tap
    // is taken pre-pan). Target: soft DC tone hard RIGHT, -34 dBFS = under
    // the -30 dB threshold, so it rides unity until the key arrives.
    let n = 24_000usize;
    let key: Arc<[f32]> = Arc::from(
        (0..n)
            .map(|i| 0.3 * (2.0 * std::f32::consts::PI * 110.0 * i as f32 / sr as f32).sin())
            .collect::<Vec<f32>>(),
    );
    let tone: Arc<[f32]> = Arc::from(vec![0.02f32; n]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0)); // key on track 0
    arr.insert(1, 0, Clip::new(0, 1)); // tone on track 1 (the target)
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![key, tone]),
        arr: Arc::new(arr),
        len: n as u64,
    });
    b.cmd(Cmd::SetLoop(true));
    b.engine.set_loop_pan(0, 0.0); // key hard left
    b.engine.set_loop_pan(1, 1.0); // tone hard right
    b.cmd(Cmd::SetTrackEffects {
        i: 1,
        chain: TrackFx {
            slots: Arc::new(vec![FxSlot::scripted(inst, desc)]),
        },
    });
    // Layout writes race the transport; land them before measuring.
    std::thread::sleep(Duration::from_millis(150));

    let rms = |ms: u64| -> f32 {
        let x = read_ring(
            &ring,
            &widx,
            (sr as u64 * ms / 1000) as usize,
            Duration::from_millis(500),
        )
        .expect("ring");
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    };

    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 480)
                .map(|m| m > 0.005)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "target tone audible on the probe bus",
    );
    std::thread::sleep(Duration::from_millis(150));
    let open = rms(200);
    assert!(
        open > 0.005,
        "tone must ride the probe bus unkeyed (got {open})"
    );

    // Route the key: track 0 now keys track 1's chain (bit 0).
    b.engine.set_sc_send(1, 1 << 0);
    // SC bus lags one block; the compressor needs attack + settle time.
    std::thread::sleep(Duration::from_millis(400));
    let ducked = rms(200);
    b.cmd(Cmd::StopAll);

    // Key sits 20 dB over threshold: ratio 4 puts the steady-state gain
    // near 0.1x. Anything past a 30% drop can only be real ducking.
    assert!(
        ducked < open * 0.7,
        "SC routing must attenuate the target: open rms {open}, ducked rms {ducked}"
    );
    // And it must be REVERSIBLE: dropping the send restores the tone.
    b.engine.set_sc_send(1, 0);
    b.cmd(Cmd::PlayAll);
    std::thread::sleep(Duration::from_millis(500));
    let restored = rms(200);
    b.cmd(Cmd::StopAll);
    assert!(
        restored > ducked * 1.5,
        "clearing the send must un-duck: ducked {ducked}, restored {restored}"
    );
}

/// v37 R5: while ANY recorder is armed the transport runs STRAIGHT THROUGH
/// the project end: no park, no auto-stop (this is what truncated every
/// kind of recording at the arrangement length). When the last recorder
/// disarms, the normal park/auto-stop behaviour resumes.
#[test]
fn e2e_v37_recording_suppresses_park_and_autostop() {
    let _g = e2e_lock();
    let Some((b, _client_name, _sr)) = start_backend() else {
        return; // no audio server: skip
    };
    // The RT callback only runs once the client owns ports (every transport
    // pin in this file wires a dummy pair first) — without this the
    // playhead never advances and the test would only measure JACK.
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let mode = Arc::new(AtomicU8::new(0));
    let const_val = Arc::new(AtomicU32::new(0.0f32.to_bits()));
    let (client, _status) =
        Client::new("daw_test_v37", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let out_a = client.register_port("out_a", AudioOut::default()).unwrap();
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            LoopSrcProc {
                out: out_a,
                sink,
                mode: mode.clone(),
                const_val: const_val.clone(),
                seed: 0x1234_5678,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![format!("{src_name}:out_a")],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 1, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the layout"
    );
    let table: Arc<Vec<Arc<[f32]>>> = Arc::new(vec![]);
    b.cmd(Cmd::SetProject {
        assets: table,
        arr: Arc::new(Arrangement::new()),
        len: 24_000, // half a second at 48k
    });
    // Park/auto-stop only applies outside loop mode (a looping transport
    // legally never stops) — this test is about the END-park truncation.
    b.cmd(Cmd::SetLoop(false));
    std::thread::sleep(Duration::from_millis(120));
    b.engine.set_recording(true);
    b.cmd(Cmd::PlayAll);
    // Well past the project end: the playhead must keep running and the
    // transport must NOT have auto-stopped.
    wait_until(
        || b.engine.pos() > 3 * 24_000,
        Duration::from_secs(4),
        "playhead runs past the project end while recording",
    );
    assert!(b.engine.transport_on(), "no auto-stop during a take");
    // Disarm: normal behaviour resumes at the next block — the transport
    // parks at the project end and auto-stops.
    b.engine.set_recording(false);
    wait_until(
        || !b.engine.transport_on(),
        Duration::from_secs(4),
        "auto-stop fires once nothing records",
    );
    drop(active);
    b.cmd(Cmd::StopAll);
}

// =============== v38 R1: roll audition voices (live path) =================

/// Transport STOPPED, track play flag OFF: a staged audition voice must
/// sound through the track's OWN instrument (bound, not the default) and
/// its chain — the "place a note, hear the instrument NOW" promise —
/// and the playing flag must drain itself when the voice retires.
#[test]
fn e2e_roll_audition_note_sounds_with_transport_stopped() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("audlive", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // Script instrument bound to TRACK 0 specifically (not the project
    // default) — the audition must route through the track's binding.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.engine
        .set_track_synth(0, Some(Arc::new(slooper::audio::SynthHandle::Script(inst))));
    // Track play flag OFF: an audition ignores it by design (A1).
    b.cmd(Cmd::SetTrackOn { i: 0, on: false });
    std::thread::sleep(Duration::from_millis(150));
    assert!(!b.engine.transport_on(), "transport stays stopped");

    let ev: Arc<[MidiEv]> = vec![
        (0u64, 3u8, 0u8, 57u8, 110u8),
        (sr as u64, 0, 0, 57, 0), // one second of note
    ]
    .into();
    b.engine.play_audition(0, ev, None);
    assert!(b.engine.audition_active(), "voice armed");
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "audition note audible with the transport STOPPED",
    );
    let body =
        read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("body window");
    let body_rms = rms(&body);
    assert!(
        body_rms > 2e-3,
        "audition body must carry audio: {body_rms}"
    );

    // After the note-off lands and the pool retires, the flag drains —
    // the roll's STOP button flips back on its own (A2).
    wait_until(
        || !b.engine.audition_active(),
        Duration::from_secs(6),
        "audition flag drains when voices retire",
    );
    std::thread::sleep(Duration::from_millis(400));
    let tail =
        read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("tail window");
    let tail_rms = rms(&tail);
    assert!(
        tail_rms < 0.25 * body_rms.max(1e-9),
        "retired audition must decay: tail {tail_rms} vs body {body_rms}"
    );
    drop(active);
    b.cmd(Cmd::StopAll);
}

/// A held audition note that is STOPPED mid-voice (STOP button / SPACE /
/// roll close) must not keep ringing: the purge all-notes-offs the track
/// within a few blocks and never resurrects.
#[test]
fn e2e_roll_audition_stop_silences_held_note() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("audstop", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.engine
        .set_track_synth(0, Some(Arc::new(slooper::audio::SynthHandle::Script(inst))));
    std::thread::sleep(Duration::from_millis(150));

    // A 3-second note; STOP it ~0.6 s in, while it is held.
    let ev: Arc<[MidiEv]> =
        vec![(0u64, 3u8, 0u8, 50u8, 120u8), (3 * sr as u64, 0, 0, 50, 0)].into();
    b.engine.play_audition(0, ev, None);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "audition sounding before STOP",
    );
    let body =
        read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("body window");
    let body_rms = rms(&body);
    assert!(body_rms > 2e-3, "held note audible: {body_rms}");

    b.engine.stop_audition();
    // a few blocks + damper decay
    std::thread::sleep(Duration::from_millis(600));
    let after = read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500))
        .expect("post-stop window");
    let after_rms = rms(&after);
    assert!(
        after_rms < 0.15 * body_rms.max(1e-9),
        "STOP must kill the held note: {after_rms} vs {body_rms}"
    );
    // …and it must stay dead (no resurrect from staged-but-purged offs).
    std::thread::sleep(Duration::from_millis(500));
    let after2 =
        read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("late window");
    let after2_rms = rms(&after2);
    assert!(
        after2_rms < 0.15 * body_rms.max(1e-9),
        "purged voice must stay silent: {after2_rms} vs {body_rms}"
    );
    drop(active);
    b.cmd(Cmd::StopAll);
}

/// v46: an audition voice that carries its OWN synth copy renders through
/// that copy — the note sounds while held (transport stopped) and dies
/// within a few blocks of `stop_audition` (the reported stuck-note bug:
/// a held note-outlived its voice object and nothing could silence it).
#[test]
fn v46_e2e_copy_audition_release_silences() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("audcopy", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let live = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.engine
        .set_track_synth(0, Some(Arc::new(slooper::audio::SynthHandle::Script(live))));
    let copy = slooper::audio::spawn_synth_copy(
        &slooper::audio::SynthHandle::Script(
            reg.create_instance("piano", sr as f64, &[]).unwrap(),
        ),
        sr,
    )
    .expect("headless copy spawn");
    std::thread::sleep(Duration::from_millis(150));

    // A HELD note (no note-off ever) through the copy.
    let ev: Arc<[MidiEv]> = vec![(0u64, 3u8, 0u8, 57u8, 110u8)].into();
    b.engine.play_audition(0, ev, Some(copy));
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "held copy note audible with the transport stopped",
    );
    let body =
        read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("body window");
    let body_rms = rms(&body);
    assert!(body_rms > 2e-3, "copy audition body: {body_rms}");

    b.engine.stop_audition();
    std::thread::sleep(Duration::from_millis(600));
    let after =
        read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("post window");
    let after_rms = rms(&after);
    assert!(
        after_rms < 0.15 * body_rms.max(1e-9),
        "copy stop must kill the held note: {after_rms} vs {body_rms}"
    );
    std::thread::sleep(Duration::from_millis(500));
    let after2 = rms(
        &read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500))
            .expect("late window"),
    );
    assert!(
        after2 < 0.15 * body_rms.max(1e-9),
        "released copy note must STAY dead: {after2} vs {body_rms}"
    );
    drop(active);
    b.cmd(Cmd::StopAll);
}

/// v46: the DEFAULT (no-track) audition route — the engine's `AUD_PV`
/// pseudo-slot. Same hold/release contract, centre mix, no track or
/// mixer involved at all.
#[test]
fn v46_e2e_default_route_hold_and_release() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("audpv", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let pv = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    std::thread::sleep(Duration::from_millis(150));

    let ev: Arc<[MidiEv]> = vec![(0u64, 3u8, 0u8, 57u8, 110u8)].into();
    b.engine.play_audition(
        slooper::audio::AUD_PV,
        ev,
        Some(Arc::new(slooper::audio::SynthHandle::Script(pv))),
    );
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(5),
        "default-route hold audible",
    );
    let body =
        read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500)).expect("body window");
    let body_rms = rms(&body);
    assert!(body_rms > 2e-3, "default-route body: {body_rms}");
    b.engine.stop_audition();
    std::thread::sleep(Duration::from_millis(600));
    let after = rms(
        &read_ring(&ring, &widx, sr as usize / 10, Duration::from_millis(500))
            .expect("post window"),
    );
    assert!(
        after < 0.15 * body_rms.max(1e-9),
        "default-route release must silence: {after} vs {body_rms}"
    );
    drop(active);
    b.cmd(Cmd::StopAll);
}

/// v46 R2: a POLY track renders every child lane through its OWN synth
/// copy on the live RT thread — audio still reaches the master out, and
/// flipping the mode back to mono (row gone mid-playback) keeps sounding
/// through the shared instance instead of dying.
#[test]
fn v46_e2e_poly_track_renders_per_lane_copies() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("polyrt", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the sink layout"
    );

    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let mk = || {
        Arc::new(slooper::audio::SynthHandle::Script(
            reg.create_instance("piano", sr as f64, &[])
                .expect("bundled piano must compile and load"),
        ))
    };
    let master = mk();
    b.cmd(Cmd::SetDefaultSynth(Some(master.clone())));

    // Two lanes, one bar each, distinct notes; both start at zero.
    let pat = |notes: [u8; 2]| {
        let mut p = slooper::midipat::Pattern::new(120);
        for (i, n) in notes.iter().enumerate() {
            p.notes.push(slooper::midipat::Note {
                ch: 0,
                note: *n,
                vel: 110,
                press: 0,
                start: (i as u32) * slooper::midipat::PPQ as u32 * 2,
                // Sustain to the end of the bar: the flip-back-to-mono
                // window below always falls inside sounding notes.
                len: slooper::midipat::PPQ as u32 * (2 - i as u32),
            });
        }
        p.sort();
        p
    };
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![
        Some(Arc::new(pat([60, 62]))),
        Some(Arc::new(pat([67, 71]))),
    ])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    arr.insert(0, 1, Clip::new(1, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: sr as u64 * 2,
    });

    // Monophonic baseline: the merged render is audible.
    b.cmd(Cmd::PlayAll);
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p > 0.01).unwrap_or(false),
        Duration::from_secs(5),
        "mono baseline audible",
    );
    b.cmd(Cmd::StopAll);
    std::thread::sleep(Duration::from_millis(700));
    wait_until(
        || ring_peak(&ring, &widx, 2400).map(|p| p < 0.002).unwrap_or(false),
        Duration::from_secs(5),
        "silence after StopAll",
    );

    // POLY: one exact copy of the instrument per child lane. If the RT
    // poly pass were broken (row ignored, copies never fed), the track
    // would go silent — every lane event now routes away from the live
    // instance into the copy row.
    b.engine
        .set_track_poly(0, true, vec![slooper::audio::spawn_synth_copy(&master, sr), slooper::audio::spawn_synth_copy(&master, sr)]);
    b.cmd(Cmd::PlayAll);
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p > 0.01).unwrap_or(false),
        Duration::from_secs(5),
        "poly track audible through the lane copies",
    );

    // Flipping back to mono mid-playback (row cleared under the RT
    // thread) must fall back to the shared instance, not stay silent:
    // the live instance missed the notes already sounding, so it re-
    // attacks at the NEXT pattern onset (correct — onsets never replay
    // mid-span) — therefore wait for audible, not a fixed window.
    b.engine.set_track_poly(0, false, Vec::new());
    wait_until(
        || ring_peak(&ring, &widx, 4800).map(|p| p > 0.01).unwrap_or(false),
        Duration::from_secs(4),
        "mono fallback audible after the poly row was cleared mid-playback",
    );
    b.cmd(Cmd::StopAll);
    drop(active);
}

// ---------------------------------------------------- FL-suite tier 2 live --

/// FL-Suite tier 2: the four new bundled synths (tri_osc, pluck, fm,
/// granular) and the new Convolver must work through the exact public
/// command queue the UI uses. Synths come from the SEEDED user folder
/// (same registry path every other e2e uses); the convolver from the
/// repo's `effects/` — the identical source that gets embedded and
/// seeded. Proof has two halves: (a) each synth carries a sustained note
/// through the production preview bounce, (b) tri_osc + Convolver on the
/// live RT thread produce audio at the master out while the transport
/// runs — real dlopen of the scripted cdylib, real process() per block.
#[test]
fn e2e_tier2_new_bundled_plugins_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("tier2_live", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 1 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "sink layout not applied"
    );

    // A real CONSTANT-ENERGY asset. Granular maps note number to a
    // playhead position (and scan drifts it), so the buffer must carry
    // energy EVERYWHERE: a sine leaves zero-crossing neighborhoods where
    // 40 ms grains genuinely render silence (measured body = 0). Noise
    // with a tonal carrier is what a granular source really is.
    let n = (sr as usize) * 2;
    let sample: Arc<[f32]> = Arc::from(
        (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let tone = (2.0 * std::f32::consts::PI * 220.0 * t).sin();
                // deterministic "noise" (integer-multiply LCG, no rand dep)
                let h = (i as u32).wrapping_mul(2654435761);
                let noise = (h as f32 / u32::MAX as f32) * 2.0 - 1.0;
                (tone * 0.5 + noise * 0.5) * 0.5
            })
            .collect::<Vec<f32>>(),
    );
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![sample.clone()]),
        arr: arr.into(),
        len: (sr as u64) * 4,
    });

    // (a) every tier-2 synth carries a held note through the UI preview path.
    let sreg = slooper::synth::Registry::open(slooper::synth::default_dir());
    for name in ["tri_osc", "pluck", "fm", "granular"] {
        let inst = sreg
            .create_instance(name, sr as f64, &[])
            .unwrap_or_else(|| panic!("{name}: bundled synth must compile and load"));
        b.cmd(Cmd::SetTrackSynth {
            i: 0,
            synth: Some(Arc::new(slooper::audio::SynthHandle::Script(inst))),
        });
        std::thread::sleep(Duration::from_millis(120));
        let mut ev = vec![
            (0u64, 3u8, 0u8, 57u8, 110u8),
            ((sr as u64) * 3 / 2, 0, 0, 57, 0),
        ];
        let buf = b
            .engine
            .bounce_track_preview(0, &mut ev, (sr as usize) * 3)
            .unwrap_or_else(|| panic!("{name}: preview bounce"));
        assert!(buf.iter().all(|v| v.is_finite()), "{name}: non-finite");
        let body = rms(&buf[(sr as usize) / 2..(sr as usize) / 2 + sr as usize / 4]);
        assert!(
            body > 1e-3,
            "{name}: held note must carry audio at 0.5 s (rms {body})"
        );
    }

    // (b) convolver on the live RT thread: tri_osc through the scripted
    // insert, notes firing, master out non-silent while PLAYING.
    let mut freg =
        slooper::fx::Registry::open(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("effects"));
    freg.rescan(); // compiles scripts (content-hashed cache: instant after first run)
    let conv = freg
        .create_instance("convolver", sr as f64, &[100.0, 50.0, 40.0, 100.0])
        .expect("bundled convolver must compile and load (repo effects/)");
    let desc = freg.desc("convolver").expect("convolver desc").clone();
    let inst = sreg
        .create_instance("tri_osc", sr as f64, &[])
        .expect("tri_osc instance");
    b.cmd(Cmd::SetTrackSynth {
        i: 0,
        synth: Some(Arc::new(slooper::audio::SynthHandle::Script(inst))),
    });
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx {
            slots: Arc::new(vec![FxSlot::scripted(conv.clone(), desc)]),
        },
    });
    let mut pat = slooper::midipat::Pattern::new(120);
    let step = (0.25 * 2.0 * slooper::midipat::PPQ as f64) as u32;
    for i in 0..12u32 {
        pat.notes.push(slooper::midipat::Note {
            ch: 0,
            note: 57,
            vel: 110,
            press: 0,
            start: i * step,
            len: step,
        });
    }
    pat.len_ticks = 12 * step;
    pat.sort();
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));
    b.cmd(Cmd::Seek { pos: 0 });
    b.cmd(Cmd::PlayAll);
    let mut peak = 0.0f32;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(30));
        peak = peak.max(ring_peak(&ring, &widx, 4800).unwrap_or(0.0));
    }
    b.cmd(Cmd::StopAll);
    assert!(
        peak > 0.02,
        "tri_osc + convolver chain must sound on the live RT thread (peak {peak})"
    );
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx::default(),
    });
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arrangement::new().into(),
        len: 4_800,
    });
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![])));
    b.cmd(Cmd::StopAll);
}

/// Tier-3: an ABI-v3 script produces a REAL stereo image at the master
/// out. Track 0 loops a constant-energy asset through the stock `spread`
/// script; the probe captures bus L and bus R sample-synchronously. With
/// the chain on, the L/R difference is strong (sideband energy only a v3
/// slot can create — every mono stage returns (y, y)); with the chain
/// cleared, L and R are identical within float noise, proving the mono
/// fold stays bit-true for every older script.
struct DiffProc {
    sink_l: Port<AudioIn>,
    sink_r: Port<AudioIn>,
    ring_sum: Arc<Vec<AtomicU32>>,
    ring_dif: Arc<Vec<AtomicU32>>,
    widx: Arc<AtomicU64>,
}

impl ProcessHandler for DiffProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;
        if n == 0 {
            return Control::Continue;
        }
        let l = self.sink_l.as_slice(ps);
        let r = self.sink_r.as_slice(ps);
        let nring = self.ring_sum.len() as u64;
        let mut w = self.widx.load(Ordering::Relaxed);
        for i in 0..n {
            let (sl, sr) = (l.get(i).copied().unwrap_or(0.0), r.get(i).copied().unwrap_or(0.0));
            self.ring_sum[(w % nring) as usize].store(((sl + sr) * 0.5).to_bits(), Ordering::Relaxed);
            self.ring_dif[(w % nring) as usize].store(((sl - sr) * 0.5).to_bits(), Ordering::Relaxed);
            w += 1;
        }
        self.widx.store(w, Ordering::Relaxed);
        Control::Continue
    }
}

#[test]
fn e2e_tier3_stereo_spread_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Load the stock spread script through the REAL registry.
    let dir = std::env::temp_dir().join(format!("slooper-e2e-st-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut reg = slooper::fx::Registry::open(dir.clone());
    reg.rescan();
    let desc = reg
        .desc("spread")
        .expect("stock spread present")
        .clone();
    assert!(desc.stereo, "spread must load as an ABI-v3 stereo script");
    let inst = reg
        .create_instance("spread", sr as f64, &[])
        .expect("spread compiles and instantiates");

    let ring_n = 1usize << 19;
    let ring_sum: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let ring_dif: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("st_live", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink_l = client.register_port("sink_l", AudioIn::default()).unwrap();
    let sink_r = client.register_port("sink_r", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            DiffProc {
                sink_l,
                sink_r,
                ring_sum: ring_sum.clone(),
                ring_dif: ring_dif.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![
            (format!("{src_name}:sink_l"), 1),
            (format!("{src_name}:sink_r"), 2),
        ],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 2 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "dual-bus layout not applied"
    );

    // Constant-energy looping asset (toned noise).
    let n = (sr as usize) * 2;
    let sample: Arc<[f32]> = Arc::from(
        (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let tone = (2.0 * std::f32::consts::PI * 220.0 * t).sin();
                let h = (i as u32).wrapping_mul(2654435761);
                let noise = (h as f32 / u32::MAX as f32) * 2.0 - 1.0;
                0.4 * (0.6 * tone + 0.4 * noise)
            })
            .collect::<Vec<f32>>(),
    );
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![sample]),
        arr: Arc::new(arr),
        len: n as u64,
    });
    b.cmd(Cmd::SetLoop(true));
    b.engine.set_loop_pan(0, 0.5); // dead centre: any L/R split is the effect's
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx {
            slots: Arc::new(vec![FxSlot::scripted(inst, desc)]),
        },
    });
    std::thread::sleep(Duration::from_millis(150));

    let rms = |ring: &Arc<Vec<AtomicU32>>| -> f32 {
        let x = read_ring(ring, &widx, (sr as u64 * 200 / 1000) as usize, Duration::from_millis(800))
            .expect("ring");
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    };

    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring_sum, &widx, 480)
                .map(|m| m > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "loop audible on master",
    );
    std::thread::sleep(Duration::from_millis(200));
    let s_on = rms(&ring_sum);
    let d_on = rms(&ring_dif);
    assert!(s_on > 0.01, "loop must be audible (sum rms {s_on})");
    assert!(
        d_on > s_on * 0.15,
        "spread must put real side energy on the master buses (dif {d_on} vs sum {s_on})"
    );

    // Clear the chain: the mono fold must be identical L/R again.
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx {
            slots: Arc::new(vec![]),
        },
    });
    std::thread::sleep(Duration::from_millis(250));
    let s_off = rms(&ring_sum);
    let d_off = rms(&ring_dif);
    b.cmd(Cmd::StopAll);
    assert!(s_off > 0.01, "loop still audible after clearing (rms {s_off})");
    assert!(
        d_off < s_off * 1e-3,
        "mono chain must produce bit-identical L/R buses (dif {d_off} vs sum {s_off})"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tier-4 live proof (docs/FL-SUITE.md rework): the upgraded v3 Echo 3
/// (ping-pong) and Chorus Doubler (per-side tap banks) must put REAL
/// side energy on the master buses through the production RT chain —
/// registry load, FxSlot attach, looping asset, live JACK transport.
#[test]
fn e2e_tier4_stereo_echo_chorus_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Load the stock scripts through the REAL registry.
    let dir = std::env::temp_dir().join(format!("slooper-e2e-t4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut reg = slooper::fx::Registry::open(dir.clone());
    reg.rescan();
    for stem in ["echo3", "chorus"] {
        let d = reg.desc(stem).expect("stock script present").clone();
        assert!(d.stereo, "{stem} must load as an ABI-v3 stereo script");
    }
    let de = reg.desc("echo3").unwrap().clone();
    let dech = reg.desc("chorus").unwrap().clone();
    let i_echo = reg.create_instance("echo3", sr as f64, &[]).expect("echo3 compiles");
    let i_cho = reg.create_instance("chorus", sr as f64, &[]).expect("chorus compiles");

    let ring_n = 1usize << 19;
    let ring_sum: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let ring_dif: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("t4_live", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink_l = client.register_port("sink_l", AudioIn::default()).unwrap();
    let sink_r = client.register_port("sink_r", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            DiffProc {
                sink_l,
                sink_r,
                ring_sum: ring_sum.clone(),
                ring_dif: ring_dif.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![
            (format!("{src_name}:sink_l"), 1),
            (format!("{src_name}:sink_r"), 2),
        ],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 2 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "dual-bus layout not applied"
    );

    // Constant-energy looping asset (toned noise).
    let n = (sr as usize) * 2;
    let sample: Arc<[f32]> = Arc::from(
        (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let tone = (2.0 * std::f32::consts::PI * 220.0 * t).sin();
                let h = (i as u32).wrapping_mul(2654435761);
                let noise = (h as f32 / u32::MAX as f32) * 2.0 - 1.0;
                0.4 * (0.6 * tone + 0.4 * noise)
            })
            .collect::<Vec<f32>>(),
    );
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![sample]),
        arr: Arc::new(arr),
        len: n as u64,
    });
    b.cmd(Cmd::SetLoop(true));
    b.engine.set_loop_pan(0, 0.5); // dead centre: any L/R split is the effect's
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx {
            slots: Arc::new(vec![FxSlot::scripted(i_echo, de)]),
        },
    });
    std::thread::sleep(Duration::from_millis(150));

    let rms = |ring: &Arc<Vec<AtomicU32>>| -> f32 {
        let x = read_ring(ring, &widx, (sr as u64 * 200 / 1000) as usize, Duration::from_millis(800))
            .expect("ring");
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    };

    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring_sum, &widx, 480)
                .map(|m| m > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "loop audible on master",
    );
    std::thread::sleep(Duration::from_millis(400));

    // Phase A: ping-pong delay decorrelates the master pair.
    let s_a = rms(&ring_sum);
    let d_a = rms(&ring_dif);
    assert!(s_a > 0.01, "loop must be audible (sum rms {s_a})");
    assert!(
        d_a > s_a * 0.05,
        "ping-pong echo3 must put side energy on the master buses (dif {d_a} vs sum {s_a})"
    );

    // Phase B: swap to the stereo chorus in the same live chain.
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx {
            slots: Arc::new(vec![FxSlot::scripted(i_cho, dech)]),
        },
    });
    std::thread::sleep(Duration::from_millis(400));
    let s_b = rms(&ring_sum);
    let d_b = rms(&ring_dif);
    assert!(s_b > 0.01, "loop still audible with chorus (rms {s_b})");
    assert!(
        d_b > s_b * 0.1,
        "stereo chorus must decorrelate on the master buses (dif {d_b} vs sum {s_b})"
    );

    b.cmd(Cmd::StopAll);
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx { slots: Arc::new(vec![]) },
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// FL Suite tier 5 live gate (docs/FL-SUITE.md): space_echo went ABI-v3.
/// Headless pins prove the DSP; this proves the script survives the REAL
/// RT thread in a live chain — per-sample calls at RT block sizes, the
/// engine pair bus feeding process_st, and decorrelated energy actually
/// reaching the master L/R JACK buses (sum audible, difference non-zero).
#[test]
fn e2e_tier5_stereo_reverb_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    // Fresh folder => seed_api writes every stock script; real loader.
    let dir = std::env::temp_dir().join(format!("slooper-e2e-t5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut reg = slooper::fx::Registry::open(dir.clone());
    reg.rescan();
    let d = reg.desc("space_echo").expect("space_echo present").clone();
    assert!(d.stereo, "space_echo must load as an ABI-v3 stereo script");
    let inst = reg.create_instance("space_echo", sr as f64, &[]).expect("compiles");

    let ring_n = 1usize << 19;
    let ring_sum: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let ring_dif: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("t5_live", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink_l = client.register_port("sink_l", AudioIn::default()).unwrap();
    let sink_r = client.register_port("sink_r", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            DiffProc {
                sink_l,
                sink_r,
                ring_sum: ring_sum.clone(),
                ring_dif: ring_dif.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![
            (format!("{src_name}:sink_l"), 1),
            (format!("{src_name}:sink_r"), 2),
        ],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 2 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "dual-bus layout not applied"
    );

    // Constant-energy looping asset (toned noise), same fixture as tier 4.
    let n = (sr as usize) * 2;
    let sample: Arc<[f32]> = Arc::from(
        (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let tone = (2.0 * std::f32::consts::PI * 220.0 * t).sin();
                let h = (i as u32).wrapping_mul(2654435761);
                let noise = (h as f32 / u32::MAX as f32) * 2.0 - 1.0;
                0.4 * (0.6 * tone + 0.4 * noise)
            })
            .collect::<Vec<f32>>(),
    );
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![sample]),
        arr: Arc::new(arr),
        len: n as u64,
    });
    b.cmd(Cmd::SetLoop(true));
    b.engine.set_loop_pan(0, 0.5); // dead centre: any L/R split is the effect's
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx {
            slots: Arc::new(vec![FxSlot::scripted(inst, d)]),
        },
    });
    std::thread::sleep(Duration::from_millis(150));

    let rms = |ring: &Arc<Vec<AtomicU32>>| -> f32 {
        let x = read_ring(ring, &widx, (sr as u64 * 200 / 1000) as usize, Duration::from_millis(800))
            .expect("ring");
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    };

    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring_sum, &widx, 480)
                .map(|m| m > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(3),
        "loop audible on master",
    );
    // Let the room charge (decay at default 0.84) before measuring.
    std::thread::sleep(Duration::from_millis(600));

    let s = rms(&ring_sum);
    let dd = rms(&ring_dif);
    assert!(s > 0.01, "loop must be audible through the reverb (sum rms {s})");
    assert!(
        dd > s * 0.05,
        "stereo room must put side energy on the master buses (dif {dd} vs sum {s})"
    );

    b.cmd(Cmd::StopAll);
    b.cmd(Cmd::SetTrackEffects {
        i: 0,
        chain: TrackFx { slots: Arc::new(vec![]) },
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// FL Suite tier 5b live-RT proof (docs/FL-SUITE.md): shifter, gater,
/// pitchshft and vocoder upgraded to ABI-v3 must put real SIDE energy on
/// the LIVE master buses. For the vocoder (the only key-consuming script
/// of the four) a one-bit self-send keys its envelopes off the track's
/// own dry sum (the sc tap is pre-FX — no feedback path). The source is
/// hard-centred, so ANY L/R difference is the effect.
#[test]
fn e2e_tier5b_stereo_mods_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let dir = std::env::temp_dir().join(format!("slooper-e2e-t5b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut reg = slooper::fx::Registry::open(dir.clone());
    reg.rescan();
    let stems = ["shifter", "gater", "pitchshft", "vocoder"];
    for stem in stems {
        let d = reg.desc(stem).expect("stock script present").clone();
        assert!(d.stereo, "{stem} must load as an ABI-v3 stereo script");
    }

    let ring_n = 1usize << 19;
    let ring_sum: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let ring_dif: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("t5b_live", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink_l = client.register_port("sink_l", AudioIn::default()).unwrap();
    let sink_r = client.register_port("sink_r", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            DiffProc {
                sink_l,
                sink_r,
                ring_sum: ring_sum.clone(),
                ring_dif: ring_dif.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![
            (format!("{src_name}:sink_l"), 1),
            (format!("{src_name}:sink_r"), 2),
        ],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 2 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "dual-bus layout not applied"
    );

    // Constant-energy looping asset (toned noise), same fixture as tier 4/5.
    let n = (sr as usize) * 2;
    let sample: Arc<[f32]> = Arc::from(
        (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let tone = (2.0 * std::f32::consts::PI * 220.0 * t).sin();
                let h = (i as u32).wrapping_mul(2654435761);
                let noise = (h as f32 / u32::MAX as f32) * 2.0 - 1.0;
                0.4 * (0.6 * tone + 0.4 * noise)
            })
            .collect::<Vec<f32>>(),
    );
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    // Track 1 carries the same asset as the vocoder's KEY source:
    // `set_sc_send` masks the target's own bit, so a self-send is
    // impossible by construction (no feedback path, ever) — the key
    // must come from another track. Silent (vol 0) on every other pass;
    // the tap is post-trim, so 0 vol doubles as "no key".
    arr.insert(1, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![sample]),
        arr: Arc::new(arr),
        len: n as u64,
    });
    b.cmd(Cmd::SetLoop(true));
    b.engine.set_loop_pan(0, 0.5); // dead centre: any L/R split is the effect's

    let rms = |ring: &Arc<Vec<AtomicU32>>| -> f32 {
        let x = read_ring(ring, &widx, (sr as u64 * 200 / 1000) as usize, Duration::from_millis(800))
            .expect("ring");
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    };

    for stem in stems {
        let d = reg.desc(stem).unwrap().clone();
        // pitchshft at default shift (semitones 0) has travel 0.5 samples —
        // the spread needs REAL tap movement, same seed as the unit pin.
        let mut dp = vec![0.0f32; 32];
        for (k, pd) in d.params.iter().enumerate() {
            dp[k] = pd.default;
        }
        if stem == "pitchshft" {
            dp[1] = 5.0;
        }
        if stem == "vocoder" {
            dp[8] = 1.0; // full band-interleave: assert at maximum decorrelation
        }
        let inst = reg
            .create_instance(stem, sr as f64, &dp)
            .unwrap_or_else(|| panic!("{stem} compiles"));
        // Vocoder keying: the track-1 asset (see above) is the modulator;
        // its volume doubles as the key gate since the tap is post-trim.
        let keying = stem == "vocoder";
        b.engine.set_loop_vol(1, if keying { 1.0 } else { 0.0 });
        b.engine.set_loop_pan(1, 0.5);
        b.engine.set_sc_send(0, if keying { 1 << 1 } else { 0 });
        b.cmd(Cmd::SetTrackEffects {
            i: 0,
            chain: TrackFx {
                slots: Arc::new(vec![FxSlot::scripted(inst, d)]),
            },
        });
        std::thread::sleep(Duration::from_millis(120));

        b.cmd(Cmd::PlayAll);
        wait_until(
            || {
                ring_peak(&ring_sum, &widx, 480)
                    .map(|m| m > 0.01)
                    .unwrap_or(false)
            },
            Duration::from_secs(3),
            &format!("{stem} audible on master"),
        );
        std::thread::sleep(Duration::from_millis(300));

        let s = rms(&ring_sum);
        let dd = rms(&ring_dif);
        eprintln!("T5b {stem}: sum {s:.6} dif {dd:.6} D/S {:.4}", dd / s);
        assert!(s > 0.01, "{stem}: loop must be audible through the effect (sum rms {s})");
        assert!(
            dd > s * 0.05,
            "{stem}: stereo script must put side energy on the master buses (dif {dd} vs sum {s})"
        );

        b.cmd(Cmd::StopAll);
        b.cmd(Cmd::SetTrackEffects {
            i: 0,
            chain: TrackFx { slots: Arc::new(vec![]) },
        });
        std::thread::sleep(Duration::from_millis(80));
    }
    b.engine.set_sc_send(0, 0);
    b.engine.set_loop_vol(1, 0.0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// FL Suite tier-6 LIVE pin (docs/FL-SUITE.md tier-6 log): the gater in
/// `host = song` mode runs on the REAL RT thread — the per-block
/// `refresh_sync -> push_sync` plumbing must lock the gate to SetMetro's
/// BPM: 0.5 cycles/beat measures a 1.000 s period at 120 BPM and 2.000 s
/// at 60 BPM (autocorrelation of the captured master-bus envelope).
/// This is the only test that exercises the sync PUSH itself (unit pins
/// cover the instance side; Proc cannot be built headless).
#[test]
fn e2e_tier6_host_sync_live() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let dir = std::env::temp_dir().join(format!("slooper-e2e-t6sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut reg = slooper::fx::Registry::open(dir.clone());
    reg.rescan();
    let desc = reg.desc("gater").expect("stock gater present").clone();

    let ring_n = 1usize << 20;
    let ring_sum: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let ring_dif: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("t6sync", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink_l = client.register_port("sink_l", AudioIn::default()).unwrap();
    let sink_r = client.register_port("sink_r", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let _active = client
        .activate_async(
            (),
            DiffProc {
                sink_l,
                sink_r,
                ring_sum: ring_sum.clone(),
                ring_dif: ring_dif.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![
            (format!("{src_name}:sink_l"), 1),
            (format!("{src_name}:sink_r"), 2),
        ],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { ins: 0, outs: 2 }),
            Duration::from_secs(5)
        )
        .is_some(),
        "bus layout not applied"
    );

    // Constant-energy looping asset (toned noise, same fixture family).
    let n = (sr as usize) * 2;
    let sample: Arc<[f32]> = Arc::from(
        (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let tone = (2.0 * std::f32::consts::PI * 220.0 * t).sin();
                let h = (i as u32).wrapping_mul(2654435761);
                let noise = (h as f32 / u32::MAX as f32) * 2.0 - 1.0;
                0.4 * (0.6 * tone + 0.4 * noise)
            })
            .collect::<Vec<f32>>(),
    );
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![sample]),
        arr: Arc::new(arr),
        len: n as u64,
    });
    b.cmd(Cmd::SetLoop(true));
    b.engine.set_loop_pan(0, 0.5);

    let mut dp = vec![0.0f32; 32];
    for (k, pd) in desc.params.iter().enumerate() {
        dp[k] = pd.default;
    }
    dp[0] = 0.5; // song mode: 0.5 cycles per beat
    dp[1] = 2.0; // pulse
    dp[2] = 0.5; // duty
    dp[3] = 0.1; // nearly hard edges
    dp[7] = 0.0; // mono (spread off)
    dp[8] = 1.0; // HOST = SONG

    // One measured run at a BPM: play, settle, capture 6 s, dominant
    // envelope period via normalized autocorrelation, stop.
    let measure = |bpm: u32| -> f64 {
        b.cmd(Cmd::SetMetro {
            on: false,
            bpm,
            sub: 4,
            num: 4,
        });
        b.cmd(Cmd::PlayAll);
        wait_until(
            || {
                ring_peak(&ring_sum, &widx, 960)
                    .map(|m| m > 0.05)
                    .unwrap_or(false)
            },
            Duration::from_secs(4),
            "gater song output audible",
        );
        std::thread::sleep(Duration::from_millis(250));
        let need = (sr as usize) * 6;
        let x = read_ring(&ring_sum, &widx, need, Duration::from_millis(9500)).expect("ring");
        // 20 ms RMS envelope, then autocorrelate over 0.25..3.2 s lags.
        let w = (sr as usize / 50).max(1);
        let env: Vec<f32> = x
            .chunks(w)
            .map(|c| (c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32).sqrt())
            .collect();
        let mean = env.iter().sum::<f32>() / env.len() as f32;
        let d: Vec<f32> = env.iter().map(|v| v - mean).collect();
        let lo = 12usize; // 0.25 s at 20 ms resolution
        let hi = (env.len() - 2).min(160); // <= 3.2 s
        let mut best = (f64::NAN, -1e9f32);
        for lag in lo..=hi {
            let m = d.len() - lag;
            let num: f32 = (0..m).map(|i| d[i] * d[i + lag]).sum();
            let na: f32 = d[..m].iter().map(|v| v * v).sum();
            let nb: f32 = d[lag..].iter().map(|v| v * v).sum();
            let c = num / (na.sqrt() * nb.sqrt()).max(1e-12);
            if c > best.1 {
                best = (lag as f64 / 50.0, c);
            }
        }
        b.cmd(Cmd::StopAll);
        b.cmd(Cmd::SetTrackEffects {
            i: 0,
            chain: TrackFx {
                slots: Arc::new(vec![]),
            },
        });
        std::thread::sleep(Duration::from_millis(80));
        eprintln!("T6 bpm {bpm}: period {:.3} s corr {:.3}", best.0, best.1);
        assert!(
            best.1 > 0.25,
            "no periodic gating found at bpm {bpm} (best corr {:.3})",
            best.1
        );
        best.0
    };

    let p120 = {
        let inst = reg.create_instance("gater", sr as f64, &dp).expect("gater compiles");
        b.cmd(Cmd::SetTrackEffects {
            i: 0,
            chain: TrackFx {
                slots: Arc::new(vec![FxSlot::scripted(inst, desc.clone())]),
            },
        });
        std::thread::sleep(Duration::from_millis(120));
        measure(120)
    };
    let p60 = {
        let inst = reg.create_instance("gater", sr as f64, &dp).expect("gater compiles");
        b.cmd(Cmd::SetTrackEffects {
            i: 0,
            chain: TrackFx {
                slots: Arc::new(vec![FxSlot::scripted(inst, desc)]),
            },
        });
        std::thread::sleep(Duration::from_millis(120));
        measure(60)
    };

    assert!(
        (p120 - 1.0).abs() < 0.18,
        "song-mode gate at 120 BPM measured {p120:.3} s, expected 1.000 s (0.5 cyc/beat)"
    );
    assert!(
        (p60 - 2.0).abs() < 0.35,
        "song-mode gate at 60 BPM measured {p60:.3} s, expected 2.000 s (0.5 cyc/beat)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- v50 R3: scrubbing the playhead while STOPPED must be silent ----------
// The reported bug: dragging the playback bar over a clip (transport never
// started) sounded the clip's audio / fired the pattern's notes. Only the
// transport may start sound; automation alone follows the playhead.
#[test]
fn e2e_scrub_while_stopped_is_silent() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };

    let ring_n = 1usize << 19;
    let ring: Arc<Vec<AtomicU32>> = Arc::new((0..ring_n).map(|_| AtomicU32::new(0)).collect());
    let widx = Arc::new(AtomicU64::new(0));
    let (client, _status) =
        Client::new("scrub_stop", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sink = client.register_port("sink", AudioIn::default()).unwrap();
    let src_name = client.name().to_string();
    let active = client
        .activate_async(
            (),
            CapProc {
                sink,
                ring: ring.clone(),
                widx: widx.clone(),
            },
        )
        .unwrap();
    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{src_name}:sink"), 3)],
    });
    assert!(
        wait_for(
            &b,
            |e| matches!(e, Evt::Applied { .. }),
            Duration::from_secs(5)
        )
        .is_some(),
        "backend did not apply the sink layout"
    );

    // Default synth so a MIDI clip COULD sound if the gate were open.
    let reg = slooper::synth::Registry::open(slooper::synth::default_dir());
    let inst = reg
        .create_instance("piano", sr as f64, &[])
        .expect("bundled piano must compile and load");
    b.cmd(Cmd::SetDefaultSynth(Some(Arc::new(
        slooper::audio::SynthHandle::Script(inst),
    ))));
    let mut pat = slooper::midipat::Pattern::new(120);
    for (i, note) in [60u8, 64, 67, 72].iter().enumerate() {
        pat.notes.push(slooper::midipat::Note {
            ch: 0,
            note: *note,
            vel: 110,
            press: 0,
            start: (i as u32) * slooper::midipat::PPQ as u32,
            len: slooper::midipat::PPQ as u32 * 3 / 4,
        });
    }
    pat.sort();
    b.cmd(Cmd::SetMidiAssets(Arc::new(vec![Some(Arc::new(pat))])));

    // Audio asset 0 (loud constant) plays on track 0; the MIDI pattern is
    // asset index 1 on track 1 (its table slot is the MIDI table).
    let a0: Arc<[f32]> = Arc::from(vec![0.5f32; 24_000]);
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    arr.insert(1, 0, Clip::new(1, 0));
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![a0.clone()]),
        arr: Arc::new(arr),
        len: sr as u64 * 4,
    });
    assert!(!b.engine.transport_on(), "test needs the transport stopped");

    // Scrub the playhead like the user does: a storm of Seeks sweeping the
    // clips both directions, then park inside the audio clip.
    for pos in [
        0u64, 4_000, 9_000, 15_000, 22_000, 23_900, 18_000, 12_000, 6_000, 2_000, 11_000,
    ] {
        b.cmd(Cmd::Seek { pos });
        std::thread::sleep(Duration::from_millis(20));
    }
    // Give the ring a clean window AFTER the last seek.
    std::thread::sleep(Duration::from_millis(150));
    let parked = b.engine.pos();
    assert!(parked > 0, "a seek while stopped did not move the playhead");
    assert!(
        ring_peak(&ring, &widx, 9600)
            .map(|p| p < 0.002)
            .unwrap_or(false),
        "scrubbing the stopped transport made sound (audio clip or MIDI notes leaked)"
    );

    // Gate sanity: the SAME project must be audible once the transport runs.
    b.cmd(Cmd::PlayAll);
    wait_until(
        || {
            ring_peak(&ring, &widx, 4800)
                .map(|p| p > 0.01)
                .unwrap_or(false)
        },
        Duration::from_secs(4),
        "transport playing the project",
    );
    b.cmd(Cmd::StopAll);
    std::thread::sleep(Duration::from_millis(700));
    let _ = active;
}

// ---- v50 R3: automation follows the playhead even while STOPPED -----------
#[test]
fn e2e_automation_follows_seek_while_stopped() {
    let _g = e2e_lock();
    let Some((b, _client_name, sr)) = start_backend() else {
        return; // no audio server: skip
    };
    let _ = sr;

    // A curve asset (table slot 1) riding track 0's volume: 0.25 flat over
    // 2 s. NO audio asset at slot 1 — only the automation acts.
    use slooper::automation::{Automation, Node, TARGET_TRACK_VOL};
    let auto = Arc::new(Automation {
        target: TARGET_TRACK_VOL.into(),
        len_samples: 96_000,
        nodes: vec![Node { t: 0, v: 0.25, c: 0.0 }, Node { t: 96_000, v: 0.25, c: 0.0 }],
    });
    b.cmd(Cmd::SetAutoAssets(Arc::new(vec![None, Some(auto)])));
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(1, sr as u64)); // auto clip at 1 s
    b.cmd(Cmd::SetProject {
        assets: Arc::new(vec![]),
        arr: Arc::new(arr),
        len: sr as u64 * 8,
    });
    std::thread::sleep(Duration::from_millis(120));
    assert!(!b.engine.transport_on());

    let vol0 = || f32::from_bits(b.engine.vol[0].load(Ordering::Relaxed));
    let manual = vol0();
    // Park before the clip: untouched manual value.
    b.cmd(Cmd::Seek { pos: 500 });
    std::thread::sleep(Duration::from_millis(120));
    assert!(
        (vol0() - manual).abs() < 1e-4,
        "seek BEFORE the curve must leave the manual volume"
    );
    // Scrub INSIDE the curve: while STOPPED, the curve value must land on
    // the real control (this is the "automation follows at all times" fix).
    b.cmd(Cmd::Seek { pos: 1_500 });
    wait_until(
        || (vol0() - 0.25).abs() < 1e-4,
        Duration::from_secs(2),
        "curve did not drive track volume while stopped",
    );
    // And back off the curve: manual value restored.
    b.cmd(Cmd::Seek { pos: 7_000 });
    wait_until(
        || (vol0() - manual).abs() < 1e-4,
        Duration::from_secs(2),
        "leaving the curve must restore the manual volume",
    );
}
