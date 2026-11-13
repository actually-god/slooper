// ============================================================ slooper ==
// slooper-synth-version: 6
// granular.rs — granular cloud engine for rack samples.
//
// Granular pads and textures in one engine. Like a granulator's
// keyboard mapping, each
// held NOTE owns a playhead through the asset buffer: the note number
// selects its start position (0..127 spans the clip), and the `scan`
// slider sends playheads walking forward. Every held note spawns its own
// grain cloud — chords are clusters. Grains are windowed fragments with
// random pitch scatter, spray and size jitter.
//
// Buffer sync mirrors the bundled sampler: the rack asset (by id) is
// re-copied at block boundaries when its identity moves, so editing the
// sample live-refreshes the cloud.
//
// Params:
//   asset   — rack asset id to granulate
//   gain    — output trim
//   size    — grain length in ms (2..200)
//   density — grains per second per note (1..120)
//   spray   — random read-position jitter, ms (0..100)
//   pitch   — base grain pitch in semitones (-24..24)
//   scatter — random pitch jitter, semitones (0..12)
//   scan    — playhead speed: 0 = frozen, 1 = real-time forward (0..2)
//   shape   — grain window: 0 = square, 1 = Hann (0..1)
//
// 8 note-voices / 32 grains, allocation-free DSP (asset copy at block
// boundary only), NaN-guarded.
// ---------------------------------------------------------------------------

include!("synth_api.rs");

const VOICES: usize = 8;
const GRAINS: usize = 32;

struct Grain {
    on: bool,
    pre: f64,    // samples of delayed start (sub-block spawn timing)
    pos: f64,    // read position (fractional samples)
    rate: f64,   // playback rate (pitch)
    age: f64,    // seconds
    len: f64,    // seconds
    amp: f64,
}

#[derive(Clone, Copy)]
struct Voice {
    held: bool,
    note: u8,
    pos: f64,  // playhead (fractional samples)
    timer: f64, // seconds until next grain
}

struct Granular {
    sr: f64,
    host: *const SynthHostV1,
    asset: i32,
    data_ptr: usize,
    data_len: u32,
    buf: Vec<f32>,
    buf_sr: f64,
    voices: [Voice; VOICES],
    grains: Vec<Grain>,
    rng: u32,
}

fn lcg(s: &mut u32) -> f64 {
    *s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    (*s >> 8) as f64 / 16_777_216.0
}

extern "C" fn create(sr: f64, host: *const SynthHostV1) -> *mut core::ffi::c_void {
    let s = Box::new(Granular {
        sr: if sr > 1.0 { sr } else { 48000.0 },
        host,
        asset: -1,
        data_ptr: 0,
        data_len: 0,
        buf: Vec::new(),
        buf_sr: 48000.0,
        voices: [Voice { held: false, note: 0, pos: 0.0, timer: 0.0 }; VOICES],
        grains: (0..GRAINS)
            .map(|_| Grain { on: false, pre: 0.0, pos: 0.0, rate: 1.0, age: 0.0, len: 0.0, amp: 0.0 })
            .collect(),
        rng: 0x5bd1_e995,
    });
    Box::into_raw(s) as *mut core::ffi::c_void
}

extern "C" fn destroy(s: *mut core::ffi::c_void) {
    if !s.is_null() {
        drop(unsafe { Box::from_raw(s as *mut Granular) });
    }
}

/// (Re-)copy the selected rack asset when its identity changed. Block-
/// boundary only (same contract as the bundled sampler).
fn sync_asset(s: &mut Granular, want: i32) {
    if s.host.is_null() {
        return;
    }
    let host = unsafe { &*s.host };
    let mut info = SynthAssetV1::default();
    let ok = (host.load_asset)(want.max(0) as u32, &mut info) == 1;
    if !ok || info.len == 0 {
        if s.asset != -2 {
            s.asset = -2;
            s.buf.clear();
        }
        return;
    }
    let ptr = info.data as usize;
    if s.asset == want && s.data_ptr == ptr && s.data_len == info.len {
        return; // same buffer, nothing to copy
    }
    let n = info.len as usize;
    s.buf.resize(n, 0.0);
    unsafe {
        std::ptr::copy_nonoverlapping(info.data, s.buf.as_mut_ptr(), n);
    }
    s.asset = want;
    s.data_ptr = ptr;
    s.data_len = info.len;
    s.buf_sr = if info.sample_rate > 1 { info.sample_rate as f64 } else { s.sr };
}

extern "C" fn note_on(s: *mut core::ffi::c_void, _ch: u8, note: u8, _vel: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Granular) };
    for v in &mut p.voices {
        if !v.held {
            v.held = true;
            v.note = note;
            v.pos = 0.0; // positioned from the note in process (needs buf)
            v.timer = 0.0; // spawn first grain immediately
            break;
        }
    }
}

extern "C" fn note_off(s: *mut core::ffi::c_void, _ch: u8, note: u8) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Granular) };
    for v in &mut p.voices {
        if v.held && v.note == note {
            v.held = false;
        }
    }
}

extern "C" fn all_off(s: *mut core::ffi::c_void) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Granular) };
    for v in &mut p.voices {
        v.held = false;
    }
    for g in &mut p.grains {
        g.on = false;
    }
}

extern "C" fn cc(_s: *mut core::ffi::c_void, _ch: u8, _num: u8, _val: u8) {}
extern "C" fn press(_s: *mut core::ffi::c_void, _ch: u8, _val: u8) {}

#[inline]
fn read(buf: &[f32], pos: f64) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let len = buf.len();
    let mut p = pos;
    while p >= len as f64 {
        p -= len as f64;
    }
    while p < 0.0 {
        p += len as f64;
    }
    let i = p as usize;
    let f = p - i as f64;
    let a = buf[i] as f64;
    let b = buf[(i + 1).min(len - 1)] as f64;
    a + (b - a) * f
}

extern "C" fn process(s: *mut core::ffi::c_void, out: *mut f32, n: usize, params: *const f32) {
    if s.is_null() {
        return;
    }
    let p = unsafe { &mut *(s as *mut Granular) };
    let sr = p.sr;

    let asset = unsafe { *params.add(0) }.clamp(0.0, 999.0) as i32;
    let gain = unsafe { *params.add(1) }.clamp(0.0, 1.0) as f64;
    let size = unsafe { *params.add(2) }.clamp(2.0, 200.0) as f64 / 1000.0;
    let density = unsafe { *params.add(3) }.clamp(1.0, 120.0) as f64;
    let spray = unsafe { *params.add(4) }.clamp(0.0, 100.0) as f64 / 1000.0;
    let pitch = unsafe { *params.add(5) }.clamp(-24.0, 24.0) as f64;
    let scatter = unsafe { *params.add(6) }.clamp(0.0, 12.0) as f64;
    let scan = unsafe { *params.add(7) }.clamp(0.0, 2.0) as f64;
    let shape = unsafe { *params.add(8) }.clamp(0.0, 1.0) as f64;

    sync_asset(p, asset);
    let blen = p.buf.len() as f64;
    let ratio = if p.buf_sr > 1.0 { p.buf_sr / sr } else { 1.0 };

    let spawn_every = 1.0 / density;
    let base_rate = ratio * 2.0f64.powf(pitch / 12.0);

    // One pass per block: schedule grains + advance playheads (block-rate
    // granularity is fine for grain spawn timing).
    let dt = n as f64 / sr;
    for v in &mut p.voices {
        if !v.held {
            continue;
        }
        if v.pos == 0.0 && blen > 0.0 {
            // First frame of this note: place the playhead by note number
            // (granular keyboard = position mapping).
            v.pos = (v.note as f64 / 127.0) * blen * 0.999;
        }
        while v.timer <= dt {
            v.timer += spawn_every;
            // grab a free grain
            if let Some(g) = p.grains.iter_mut().find(|g| !g.on) {
                let spr = (lcg(&mut p.rng) * 2.0 - 1.0) * spray * sr;
                let sc = (lcg(&mut p.rng) * 2.0 - 1.0) * scatter;
                g.on = true;
                // Sub-block spawn offset: host blocks are NOT 1 ms — with
                // block-rate timing a 1.5 s preview bounce collapsed every
                // grain into one burst at the block head (body of the note
                // rendered silence). `pre` delays start to the exact time.
                g.pre = (v.timer - spawn_every).max(0.0) * sr;
                g.pos = (v.pos + spr).rem_euclid(blen.max(1.0));
                g.rate = base_rate * 2.0f64.powf(sc / 12.0);
                g.age = 0.0;
                g.len = size * (0.7 + 0.6 * lcg(&mut p.rng));
                g.amp = 0.5 + 0.5 * lcg(&mut p.rng);
            }
        }
        v.timer -= dt;
        v.pos += scan * sr * dt;
        if blen > 0.0 {
            v.pos %= blen;
        }
    }

    for k in 0..n {
        let mut acc = 0.0f64;
        // Field-split borrows: grains mutate while the buffer is read.
        let grains = &mut p.grains;
        let buf = &p.buf;
        for g in grains.iter_mut() {
            if !g.on {
                continue;
            }
            if g.pre > 0.0 {
                g.pre -= 1.0;
                continue;
            }
            let t = g.age / g.len;
            if t >= 1.0 {
                g.on = false;
                continue;
            }
            // Window: 1 = Hann, 0 = square with 1 ms ramps to avoid clicks.
            let win = 0.5 * (1.0 - (2.0 * core::f64::consts::PI * t).cos());
            let ramp = 0.001 / g.len;
            let sq = if t < ramp || t > 1.0 - ramp {
                t.min(1.0 - t) / ramp
            } else {
                1.0
            };
            let env = shape * win + (1.0 - shape) * sq;
            acc += read(buf, g.pos) * env * g.amp;
            g.pos += g.rate;
            g.age += 1.0 / sr;
        }
        let smp = acc * gain * 0.45;
        if smp.is_finite() {
            unsafe { *out.add(k) += smp as f32 };
        }
    }
}

static PARAMS: [SynthParamV1; 9] = [
    SynthParamV1 { label: c"asset".as_ptr(), min: 0.0, max: 999.0, default: 0.0 },
    SynthParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 1.0, default: 0.8 },
    SynthParamV1 { label: c"grain ms".as_ptr(), min: 2.0, max: 200.0, default: 40.0 },
    SynthParamV1 { label: c"density Hz".as_ptr(), min: 1.0, max: 120.0, default: 30.0 },
    SynthParamV1 { label: c"spray ms".as_ptr(), min: 0.0, max: 100.0, default: 5.0 },
    SynthParamV1 { label: c"pitch st".as_ptr(), min: -24.0, max: 24.0, default: 0.0 },
    SynthParamV1 { label: c"scatter st".as_ptr(), min: 0.0, max: 12.0, default: 1.0 },
    SynthParamV1 { label: c"scan speed".as_ptr(), min: 0.0, max: 2.0, default: 0.1 },
    SynthParamV1 { label: c"window shape".as_ptr(), min: 0.0, max: 1.0, default: 1.0 },
];

pub static DESC: SynthInfoV1 = SynthInfoV1 {
    abi: SLOOPER_SYNTH_ABI,
    name: c"granular".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Granular clouds over a rack sample: notes are playheads, chords are clusters; spray, scatter, scan, window shape.".as_ptr(),
    n_params: 9,
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
