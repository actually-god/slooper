//! v33 MIDI soak against the oracle plugin (host-level ground truth).
//!
//! The oracle dumps per-process() JSON stats to $ORACLE_STATS, so every
//! number below is what the PLUGIN actually received — not what the
//! monitor pane showed. Sections pin the v33 fixes:
//!   A  fresh open writes NO parameters back (yabridge tax + silence trap)
//!   B  events land at their EXACT sample offsets (delta_sum == Σ offsets)
//!   C  1500-event burst: nothing dropped, no cap, voices release
//!   D  fast retriggers keep sub-block ordering/deltas
//!   E  held notes stay alive across event-free blocks (sustain/pedal)
//!   F  legacy feed_midi+process still works (chain rows use it)
use std::process::Command;
use std::{fs, path::Path};

const ORACLE_SO: &str = "/tmp/oracle.so";
const ORACLE_SRC: &str = "examples/oracle.c";
const STATS: &str = "/tmp/oracle_stats.json";

fn stats() -> Vec<(String, f64)> {
    let s = fs::read_to_string(STATS).unwrap_or_default();
    let mut out = Vec::new();
    for kv in s.trim().trim_matches(['{', '}']).split(',') {
        let mut it = kv.split(':');
        let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
        if let Ok(v) = v.trim().parse::<f64>() {
            out.push((k.trim_matches('"').to_string(), v));
        }
    }
    out
}

fn get(s: &[(String, f64)], k: &str) -> f64 {
    s.iter().find(|(kk, _)| kk == k).map(|(_, v)| *v).unwrap_or(f64::NAN)
}

fn main() {
    // Build the oracle if missing/stale.
    let need = !Path::new(ORACLE_SO).exists()
        || std::fs::metadata(ORACLE_SO)
            .ok()
            .and_then(|m| m.modified().ok())
            < std::fs::metadata(ORACLE_SRC).ok().and_then(|m| m.modified().ok());
    if need {
        let st = Command::new("cc")
            .args(["-shared", "-fPIC", "-O1", "-o", ORACLE_SO, ORACLE_SRC])
            .status()
            .expect("cc");
        assert!(st.success(), "oracle build failed");
    }
    std::env::set_var("ORACLE_STATS", STATS);
    let _ = fs::remove_file(STATS);

    let load = slooper::vsthost::open(Path::new(ORACLE_SO), slooper::vsthost::VstKind::V2, 48_000, None)
        .expect("oracle open");
    let e = load.entry;
    let sil = vec![0f32; 512];

    let run = |e: &slooper::vsthost::RtStageEntry, blocks: usize| {
        let mut out = vec![0f32; 512];
        for _ in 0..blocks {
            e.process(&sil, &mut out);
        }
    };
    // v33 engine path: one call, events stamped at exact sample offsets.
    let pm = |e: &slooper::vsthost::RtStageEntry, evs: &[(usize, [u8; 3])]| {
        let mut out = vec![0f32; 512];
        e.process_with_midi(&sil, &mut out, evs);
    };

    // A — fresh open: zero parameter writes (restore must be the only writer).
    run(&e, 1); // force a stats dump (the oracle dumps per process call)
    let a = stats();
    println!(
        "A open: param_sets={:.0} (want 0) opens={:.0} (want 1)",
        get(&a, "param_sets"),
        get(&a, "opens")
    );

    // B — 8-note chord at DISTINCT exact offsets: every delta preserved.
    let chord: Vec<(usize, [u8; 3])> = (0..8usize)
        .map(|k| (k * 7, [0x90u8, 48 + k as u8, 100]))
        .collect();
    let want_delta: usize = (0..8).map(|k| k * 7).sum();
    pm(&e, &chord);
    run(&e, 24);
    let s = stats();
    println!(
        "B chord: note_on={:.0} (want 8) voices={:.0} (want 8) ev_total={:.0} (want 8) \
         delta_sum={:.0} (want {want_delta} — 0 or low means offsets were LOST)",
        get(&s, "note_on"),
        get(&s, "voices"),
        get(&s, "ev_total"),
        get(&s, "delta_sum"),
    );

    // C — 1500-event burst (the "dense chord overload" report). Every
    // event must reach the plugin; voices must all release.
    let burst: Vec<(usize, [u8; 3])> = (0..750)
        .map(|i| {
            let note = 20 + (i % 108) as u8;
            let off = i % 511; // keep on/off pairs in order below
            (off, [0x90u8, note, 90])
        })
        .chain((0..750).map(|i| {
            let note = 20 + (i % 108) as u8;
            (2 + i % 511, [0x80u8, note, 0])
        }))
        .collect();
    pm(&e, &burst);
    run(&e, 48);
    let s = stats();
    println!(
        "C burst: ev_total={:.0} (want 1508) note_on={:.0} (want 758) \
         voices={:.0} (want 0 — released) drops-cap={:.0} max_batch={:.0}",
        get(&s, "ev_total"),
        get(&s, "note_on"),
        get(&s, "voices"),
        1508.0 - get(&s, "ev_total"),
        get(&s, "max_batch"),
    );

    // D — fast retrigger: on/off pairs 6 samples apart, 40 pairs. The sum
    // of delivered deltas must equal the sum of staged offsets, i.e. no
    // event was smeared to the block start.
    let pairs: Vec<(usize, [u8; 3])> = (0..40)
        .flat_map(|i| {
            let o = i * 12;
            vec![(o, [0x90u8, 60, 100]), (o + 4, [0x80u8, 60, 0])]
        })
        .collect();
    let want_d: f64 = (0..40).map(|i| (i * 12 + i * 12 + 4) as f64).sum::<f64>();
    let before = get(&stats(), "delta_sum");
    let on_before = get(&stats(), "note_on");
    pm(&e, &pairs);
    run(&e, 12);
    let s = stats();
    println!(
        "D retrigger: delta_sum+{:.0} (want {want_d:.0}) note_on+{:.0} (want 40)",
        get(&s, "delta_sum") - before,
        get(&s, "note_on") - on_before,
    );

    // E — sustain: hold 4 notes, then 100 event-free blocks. The engine
    // must keep calling process() (v33 always-run) and voices must stay
    // held the whole time; CC64 (sustain) must be delivered cleanly.
    pm(&e, &[
        (3, [0x90u8, 60, 100]),
        (11, [0x90u8, 64, 100]),
        (19, [0x90u8, 67, 100]),
        (27, [0x90u8, 72, 100]),
    ]);
    run(&e, 4);
    let mid1 = get(&stats(), "voices");
    run(&e, 100);
    let s = stats();
    let vc = get(&s, "voices");
    let pc = get(&s, "process_calls");
    println!(
        "E sustain: voices={vc:.0} (want 4 held through 100 idle blocks) \
         pc after idle={pc:.0}"
    );
    // Sustain pedal CC + a note that NEVER gets an off: pedal semantics
    // live in the plugin; the oracle must simply see the CC and the on.
    pm(&e, &[(5, [0xB0u8, 64, 127]), (9, [0x90u8, 65, 90])]);
    run(&e, 32);
    let s = stats();
    println!(
        "E2 pedal: cc={:.0} (want 1) voices={:.0} (want 5 — held note survives)",
        get(&s, "cc"),
        get(&s, "voices"),
    );
    // Release everything (engine all-notes-off path).
    e.all_notes_off();
    run(&e, 8);
    let s = stats();
    println!("E3 release: voices={:.0} (want 0)", get(&s, "voices"));
    let _ = mid1;

    // F — legacy feed_midi + process (chain-row path) still delivers.
    let on_b = get(&stats(), "note_on");
    e.feed_midi([0x90, 70, 100]);
    run(&e, 4);
    e.feed_midi([0x80, 70, 0]);
    run(&e, 4);
    let s = stats();
    println!(
        "F legacy: note_on+{:.0} (want 1) voices={:.0} (want 0)",
        get(&s, "note_on") - on_b,
        get(&s, "voices")
    );

    println!("(final process_calls = {:.0})", get(&stats(), "process_calls"));
    // Never drop plugin instances in probes (skill rule).
    std::mem::forget(e);
}
