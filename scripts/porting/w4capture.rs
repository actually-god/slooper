//! WINDOWS-PORT W4 bit-true capture harness (canonical copy; copy to
//!
//! tests/ to run: W4_TAG=x cargo test --release --test w4capture --
//! --nocapture). Drives the REAL RT path (manager thread + Proc over the live
//! PipeWire-JACK server) with a fully deterministic project (two LCG-noise
//! clips, defaults everywhere) and captures the raw stereo bus output at a
//! sink client. Writes the canonical capture window (aligned on the
//! clip-start correlation marker so wall-clock timing cancels) to
//! /tmp/w4_win_$W4_TAG.pcm — sha256 of HEAD vs refactor must match.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jack::{AudioIn, Client, ClientOptions, Control, ProcessHandler, ProcessScope};

use slooper::arrange::{Arrangement, Clip};
use slooper::audio::{Backend, Cmd, Evt};

struct Capture {
    pairs: VecDeque<(f32, f32)>, // ring of the newest CAP_LEN pairs
    total: u64,
}

impl Capture {
    fn len(&self) -> usize {
        self.pairs.len()
    }
}

struct SinkProc {
    l: jack::Port<AudioIn>,
    r: jack::Port<AudioIn>,
    cap: Arc<Mutex<Capture>>,
    stop: Arc<AtomicBool>,
    samples_needed: Arc<AtomicU64>,
}

const CAP_LEN: usize = 520_000; // ring capacity (~10.8 s @48k)

impl ProcessHandler for SinkProc {
    fn process(&mut self, _c: &Client, ps: &ProcessScope) -> Control {
        let n = ps.n_frames() as usize;
        if n == 0 || self.stop.load(Ordering::Relaxed) {
            return Control::Continue;
        }
        let l = self.l.as_slice(ps);
        let r = self.r.as_slice(ps);
        {
            let mut cap = self.cap.lock().unwrap();
            for i in 0..n {
                if cap.len() == CAP_LEN {
                    cap.pairs.pop_front();
                }
                cap.pairs.push_back((l[i], r[i]));
                cap.total += 1;
            }
        }
        if cap_total(&self.cap) >= self.samples_needed.load(Ordering::Relaxed) {
            self.stop.store(true, Ordering::Relaxed);
        }
        Control::Continue
    }
}

fn cap_total(cap: &Arc<Mutex<Capture>>) -> u64 {
    cap.lock().unwrap().total
}

fn start_backend() -> Option<(Backend, String, u32)> {
    let b = Backend::start();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return None;
        }
        match b.evt_rx.recv_timeout(rem) {
            Ok(Evt::Ready { client_name, sample_rate }) => {
                return Some((b, client_name, sample_rate))
            }
            Ok(Evt::Unavailable(_) | Evt::Error(_)) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Deterministic white noise (the dsp-audio-probe LCG, XOR variant).
fn lcg_noise(seed: u32, len: usize) -> Vec<f32> {
    let mut s = seed;
    (0..len)
        .map(|_| {
            s = (1103515245u32.wrapping_mul(s) ^ 12345) & 0x7FFF_FFFF;
            (s as f32 / 0x7FFF_FFFF as f32 - 0.5) * 0.5
        })
        .collect()
}

#[test]
fn w4_bit_true_capture() {
    let Some((_b, _cn, sr)) = start_backend() else {
        eprintln!("SKIP: no JACK server");
        return;
    };
    let b = _b;
    let tag = std::env::var("W4_TAG").unwrap_or_else(|_| "run".into());

    // ---- capture sink client ---------------------------------------------------
    let (client, _status) =
        Client::new("w4cap", ClientOptions::NO_START_SERVER).expect("no JACK server");
    let sl = client.register_port("sl", AudioIn::default()).unwrap();
    let srr = client.register_port("sr", AudioIn::default()).unwrap();
    let name = client.name().to_string();
    let cap = Arc::new(Mutex::new(Capture {
        pairs: VecDeque::with_capacity(CAP_LEN),
        total: 0,
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let needed = Arc::new(AtomicU64::new(240_000)); // 5 s of pairs
    let active = client
        .activate_async(
            (),
            SinkProc {
                l: sl,
                r: srr,
                cap: cap.clone(),
                stop: stop.clone(),
                samples_needed: needed.clone(),
            },
        )
        .unwrap();

    b.cmd(Cmd::Apply {
        ins: vec![],
        outs: vec![(format!("{name}:sl"), 1), (format!("{name}:sr"), 2)],
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match b.evt_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Evt::Applied { outs: 2, .. }) => break,
            Ok(_) => {}
            Err(_) => assert!(Instant::now() < deadline, "layout never applied"),
        }
    }

    // ---- deterministic project: two clips, everything default ------------------
    const CLIP: usize = 100_000;
    let a0: Arc<[f32]> = Arc::from(lcg_noise(0x1234_5678, CLIP).into_boxed_slice());
    let a1: Arc<[f32]> = Arc::from(lcg_noise(0x9E37_79B9, CLIP).into_boxed_slice());
    let mut arr = Arrangement::new();
    arr.insert(0, 0, Clip::new(0, 0));
    arr.insert(1, 0, Clip::new(1, 30_000));
    let table: Arc<Vec<Arc<[f32]>>> = Arc::new(vec![a0.clone(), a1.clone()]);
    b.cmd(Cmd::SetProject {
        assets: table,
        arr: Arc::new(arr),
        len: 130_000,
    });
    b.cmd(Cmd::SetLoop(false)); // play once, park: exactly one marker
    std::thread::sleep(Duration::from_millis(250));
    b.cmd(Cmd::PlayAll);

    let deadline = Instant::now() + Duration::from_secs(12);
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(stop.load(Ordering::Relaxed), "capture never completed");
    std::thread::sleep(Duration::from_millis(100));
    b.cmd(Cmd::StopAll);
    b.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
    drop(active);

    // ---- align on the clip-start marker ----------------------------------------
    let pairs: Vec<(f32, f32)> = {
        let mut c = cap.lock().unwrap();
        c.pairs.drain(..).collect()
    };
    let n = pairs.len();
    assert!(n > 180_000, "capture too short: {n}");
    // Template: a slice of the t0 asset taken from the PURE zone (before t1
    // enters and after the onset attack ramp); the mix scales it, so use
    // normalized correlation. Loop is OFF: the clip plays exactly once.
    const T: usize = 2_048;
    const TOFF: usize = 4_000; // template offset inside the t0 asset
    let tmpl: Vec<f64> = a0[TOFF..TOFF + T].iter().map(|&x| x as f64).collect();
    let tnorm: f64 = tmpl.iter().map(|x| x * x).sum::<f64>().sqrt();
    let mut best = (0usize, -1e9f64);
    for start in 2_000..n.saturating_sub(T + 500) {
        let mut dot = 0.0f64;
        let mut e = 0.0f64;
        for k in 0..T {
            let v = pairs[start + k].0 as f64;
            dot += v * tmpl[k];
            e += v * v;
        }
        if e <= 1e-20 {
            continue;
        }
        let corr = dot / (e.sqrt() * tnorm);
        if corr > best.1 {
            best = (start, corr);
        }
    }
    let (marker, corr) = best;
    // ---- canonical window file ---------------------------------------------------
    let mut f = File::create(PathBuf::from(format!("/tmp/w4_win_{tag}.pcm"))).unwrap();
    let mut g = File::create(PathBuf::from(format!("/tmp/w4_all_{tag}.pcm"))).unwrap();
    // Window anchored on the CLIP START (marker - template offset).
    let win_start = marker.saturating_sub(TOFF);
    let win = &pairs[win_start..(win_start + 190_000).min(n)];
    for (l, r) in win.iter() {
        f.write_all(&l.to_le_bytes()).unwrap();
        f.write_all(&r.to_le_bytes()).unwrap();
    }
    for (l, r) in pairs.iter() {
        g.write_all(&l.to_le_bytes()).unwrap();
        g.write_all(&r.to_le_bytes()).unwrap();
    }
    drop(f);
    drop(g);
    println!(
        "W4CAP tag={tag} sr={sr} marker={marker} corr={corr:.6} pairs={n} win={} win_start={win_start}",
        win.len()
    );
    assert!(corr > 0.995, "no clean clip-start marker (corr={corr})");
}
