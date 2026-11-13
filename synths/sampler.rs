// ============================================================ slooper ==
// slooper-synth-version: 5
// sampler.rs — pitch-following audio-clip synthesizer (multi-sample).
//
// Plays ANY audio asset from the rack as an instrument. A note plays the
// mapped sample whose ROOT NOTE is closest to it, transposed by the
// semitone distance — the classic sampler trick, now with a whole KEY MAP:
// the sampler map window binds as many rack samples as you like, one root
// note each (drag a sample onto a piano key). With one sample the map is
// exactly the old single-asset behavior.
//
// Two selection modes (set in the map window):
//   hard cutoff — zones switch at the halfway point between neighbor
//                 roots; a note exactly halfway plays the LOWER sample.
//   crossfade   — the two nearest samples (one below, one above) are both
//                 bent to the note's pitch and mixed by inverse distance:
//                 samples on A2 and E3, note C3 = 3 st above A2 and 4
//                 below E3 -> A2 gets 4/7 of the volume, E3 gets 3/7.
//                 Notes outside the map's range play the extreme sample.
//
// Legacy params (the map is authoritative when it is non-empty):
//   asset     — rack asset id to sample (fallback single sample).
//   root note — MIDI note that fallback sample sounds at (default C3 = 48).
//   gain      — output trim.
//
// The map rides the host table (SynthHostV1::load_map): `process` polls
// it (cheap array compare) and copies any changed asset buffer out of the
// host at a block boundary (the host pointer is valid only briefly), so
// edits are live — no rebind, no restart. Buffers are cached per asset id
// and shared by every slot that references them. 24-voice poly.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 24;
const MAX_SLOTS: usize = 64;

/// One cached rack asset copy (identity = host pointer + length, so a
/// re-recorded/trimmed sample under the SAME id is picked up, v0.10 rule).
struct Buf {
    ptr: usize,
    len: u32,
    rate: f64,
    data: std::rc::Rc<std::vec::Vec<f32>>,
}

/// A mapped slot: rack asset + the note the sample IS.
#[derive(Clone, Copy)]
struct Slot {
    asset: u32,
    root: f64,
}

struct Voice {
    on: bool,
    pos: f64,
    rate: f64,
    vel: f64,
    buf: std::rc::Rc<std::vec::Vec<f32>>,
}

struct Sampler {
    host: *const SynthHostV1,
    sr: f64,
    // Legacy params from the last processed param frame — `note_on` runs
    // between blocks, so this is exact for anything the user can set.
    asset: i32,
    root: f64,
    // Last EFFECTIVE map seen (change detection; raw order = host order).
    raw: [u64; MAX_SLOTS],
    raw_n: usize,
    raw_mode: u32,
    has_map: bool,
    // Effective slots, SORTED ascending by root (params fallback merged).
    slots: std::vec::Vec<Slot>,
    mode: u32,
    cache: std::collections::BTreeMap<u32, Buf>,
    voices: [Voice; VOICES],
    next: usize,
}

extern "C" fn create(sr: f64, host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let s = Box::new(Sampler {
        host,
        sr: if sr > 1.0 { sr } else { 48000.0 },
        asset: -1,
        root: 48.0,
        raw: [0u64; MAX_SLOTS],
        raw_n: 0,
        raw_mode: 0,
        has_map: false,
        slots: std::vec::Vec::new(),
        mode: 0,
        cache: std::collections::BTreeMap::new(),
        voices: core::array::from_fn(|_| Voice {
            on: false,
            pos: 0.0,
            rate: 1.0,
            vel: 0.0,
            buf: std::rc::Rc::new(std::vec::Vec::new()),
        }),
        next: 0,
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Sampler) });
    }
}

/// Pull the current key map from the host and, if it (or the asset
/// contents under it) changed, rebuild the slot table + buffer cache.
/// Only touches the host when something actually moved, so calling it
/// from both `process` and `note_on` is cheap.
fn sync_map(s: &mut Sampler) {
    if s.host.is_null() {
        return;
    }
    let host = unsafe { &*s.host };
    if host.abi != SLOOPER_SYNTH_ABI {
        return;
    }
    let mut packed = [0u64; MAX_SLOTS];
    let mut bits = 0u32;
    let n = ((host.load_map)(packed.as_mut_ptr(), MAX_SLOTS as u32, &mut bits) as usize)
        .min(MAX_SLOTS);
    let has_map = bits & 0x8000_0000 != 0;
    let mode = bits & 0xFF;
    // Effective slot list: the map if the track has one, else the legacy
    // params as a single slot.
    let mut eff = [0u64; MAX_SLOTS];
    let mut en = 0usize;
    if has_map {
        eff[..n].copy_from_slice(&packed[..n]);
        en = n;
    } else if s.asset >= 0 {
        eff[0] = ((s.asset as u32) as u64) << 8 | ((s.root as u32) as u64) & 0xFF;
        en = 1;
    }
    if en == s.raw_n && mode == s.raw_mode && has_map == s.has_map && eff[..en] == s.raw[..en] {
        // Same set in the same order (the host publishes stably) — only
        // the asset CONTENTS under it may have moved.
        sync_bufs(s);
        return;
    }
    // Rebuild sorted slot list (insertion sort; stable, so two slots on
    // the same root keep their map order and the LOWER-indexed one wins
    // the cutoff scan).
    let mut slots: std::vec::Vec<Slot> = std::vec::Vec::with_capacity(en);
    for i in 0..en {
        slots.push(Slot {
            asset: (eff[i] >> 8) as u32,
            root: (eff[i] & 0xFF) as f64,
        });
    }
    slots.sort_by(|a, b| a.root.partial_cmp(&b.root).unwrap_or(core::cmp::Ordering::Equal));
    s.slots = slots;
    s.mode = mode;
    s.raw[..en].copy_from_slice(&eff[..en]);
    s.raw_n = en;
    s.raw_mode = mode;
    s.has_map = has_map;
    // Drop cache entries no slot references anymore, then refresh all.
    let want: std::collections::BTreeMap<u32, ()> =
        s.slots.iter().map(|sl| (sl.asset, ())).collect();
    s.cache.retain(|k, _| want.contains_key(k));
    sync_bufs(s);
}

/// Copy any changed asset out of the host into the cache.
fn sync_bufs(s: &mut Sampler) {
    let host = unsafe { &*s.host };
    for i in 0..s.slots.len() {
        let id = s.slots[i].asset;
        let mut info = SynthAssetV1::default();
        let ok = (host.load_asset)(id, &mut info) == 1 && info.len > 0 && !info.data.is_null();
        let ptr = if ok { info.data as usize } else { 0 };
        let known = s
            .cache
            .get(&id)
            .map(|b| (b.ptr, b.len))
            .unwrap_or((0, 0));
        if ok && ptr == known.0 && info.len == known.1 {
            continue; // nothing moved
        }
        if !ok {
            s.cache.remove(&id);
            continue;
        }
        let n = (info.len as usize).min(12 * 1024 * 1024);
        let mut data = std::vec::Vec::with_capacity(n);
        unsafe {
            data.extend_from_slice(std::slice::from_raw_parts(info.data, n));
        }
        let rate = if info.sample_rate > 1 {
            info.sample_rate as f64
        } else {
            s.sr
        };
        s.cache.insert(
            id,
            Buf {
                ptr,
                len: info.len,
                rate,
                data: std::rc::Rc::new(data),
            },
        );
    }
}

/// Start one voice on slot `idx` transposed to `note`, scaled by `w`.
fn start_voice(s: &mut Sampler, idx: usize, note: u8, vel: u8, w: f64) {
    let slot = s.slots[idx];
    let buf = match s.cache.get(&slot.asset) {
        Some(b) if !b.data.is_empty() => b.data.clone(),
        _ => return,
    };
    let mut vi = None;
    for k in 0..VOICES {
        if !s.voices[k].on {
            vi = Some(k);
            break;
        }
    }
    let vi = vi.unwrap_or_else(|| {
        let k = s.next;
        s.next = (s.next + 1) % VOICES;
        k
    });
    // Pitch = semitone distance from THIS slot's root note.
    let semis = note as f64 - slot.root;
    let native = {
        let rate = s
            .cache
            .get(&slot.asset)
            .map(|b| b.rate)
            .unwrap_or(s.sr);
        (rate / s.sr).clamp(0.25, 4.0)
    };
    let v = &mut s.voices[vi];
    v.on = true;
    v.pos = 0.0;
    v.rate = ((2.0f64).powf(semis / 12.0) * native).clamp(0.0625, 16.0);
    v.vel = (vel as f64 / 127.0) * w;
    v.buf = buf;
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, vel: u8) {
    if s.is_null() {
        return;
    }
    let sm = unsafe { &mut *(s as *mut Sampler) };
    // A map edited since the last block must answer THIS note (and the
    // very first note after a rebind may arrive before any process call —
    // the v0.10 "first note dropped" pin).
    sync_map(sm);
    if sm.slots.is_empty() {
        return;
    }
    let n = note as f64;
    if sm.mode == 1 {
        // Crossfade: nearest below + nearest above, inverse-distance mix.
        let mut lo = None; // last slot with root <= note
        let mut hi = None; // first slot with root > note
        for (i, sl) in sm.slots.iter().enumerate() {
            if sl.root <= n {
                lo = Some(i);
            } else {
                hi = Some(i);
                break;
            }
        }
        match (lo, hi) {
            (Some(i), _) if (sm.slots[i].root - n).abs() < 1e-6 => start_voice(sm, i, note, vel, 1.0),
            (Some(i), Some(j)) => {
                let span = sm.slots[j].root - sm.slots[i].root;
                let w_hi = if span > 1e-6 {
                    ((n - sm.slots[i].root) / span).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                start_voice(sm, i, note, vel, 1.0 - w_hi);
                if w_hi > 0.0 {
                    start_voice(sm, j, note, vel, w_hi);
                }
            }
            (Some(i), None) => start_voice(sm, i, note, vel, 1.0),
            (None, Some(j)) => start_voice(sm, j, note, vel, 1.0),
            (None, None) => {}
        }
    } else {
        // Hard cutoff: nearest root wins; an exact midpoint tie plays the
        // LOWER sample (strict `<` over the ascending scan keeps the first
        // = lower one on equal distance).
        let mut best = 0usize;
        let mut bd = f64::MAX;
        for (i, sl) in sm.slots.iter().enumerate() {
            let d = (sl.root - n).abs();
            if d < bd {
                bd = d;
                best = i;
            }
        }
        start_voice(sm, best, note, vel, 1.0);
    }
}

extern "C" fn note_off(_s: *mut core::ffi::c_void, _ch: u8, _note: u8) {
    // One-shots ring out; note_off has nothing to release.
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let sm = unsafe { &mut *(s as *mut Sampler) };
    for v in &mut sm.voices {
        v.on = false;
    }
}

extern "C" fn cc(_s: *mut core::ffi::c_void, _ch: u8, _num: u8, _val: u8) {}
extern "C" fn press(_s: *mut core::ffi::c_void, _ch: u8, _val: u8) {}

extern "C" fn process(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32) {
    if s.is_null() {
        return;
    }
    let sm = unsafe { &mut *(s as *mut Sampler) };
    sm.asset = unsafe { *params.add(0) }.round() as i32;
    sm.root = unsafe { *params.add(1) }.round() as f64;
    let gain = unsafe { *params.add(2) }.clamp(0.0, 1.0) as f64;
    sync_map(sm);
    if sm.slots.is_empty() || sm.cache.is_empty() {
        return;
    }
    for k in 0..n {
        let mut acc = 0.0f64;
        for v in &mut sm.voices {
            if !v.on {
                continue;
            }
            let buf = &v.buf;
            let i = v.pos as usize;
            if i + 1 >= buf.len() {
                v.on = false;
                continue;
            }
            let frac = v.pos - i as f64;
            let a = buf[i] as f64;
            let b = buf[i + 1] as f64;
            acc += (a + (b - a) * frac) * v.vel;
            v.pos += v.rate;
            if v.pos > buf.len() as f64 - 1.0 {
                v.on = false;
            }
        }
        unsafe {
            *out.add(k) += (acc * gain) as f32;
        }
    }
}

static PARAMS: [SynthParamV1; 3] = [
    SynthParamV1 { label: c"asset".as_ptr(), min: 0.0, max: 4095.0, default: 0.0 },
    SynthParamV1 { label: c"root note".as_ptr(), min: 12.0, max: 108.0, default: 48.0 },
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"sampler".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Play rack assets pitched across the keyboard; the map window binds many samples to key zones".as_ptr(),
    n_params: 3,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    note_on,
    note_off,
    all_off,
    cc,
    press,
    process,
};

#[no_mangle]
pub extern "C" fn slooper_synth_info() -> *const SynthInfoV1 {
    &DESC
}
