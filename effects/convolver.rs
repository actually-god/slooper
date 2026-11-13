// ============================================================ slooper ==
// convolver.rs — convolution reverb.
//
// Capability: fixed-folder impulse scan (docs/EFFECT-SUITE.md capability log).
// Any .wav file dropped into the impulses folder becomes an IR:
//
//     $SLOOPER_IMPULSES_DIR   (env override, tests/dev)
//     Linux:   $XDG_DATA_HOME/slooper/impulses  or  ~/.local/share/...
//     Windows: %LOCALAPPDATA%\slooper\impulses
//
// The folder (plus a README) is created on first use. Index 0 is ALWAYS
// the built-in synthetic hall so the effect works with zero files. Up to
// 7 files load (sorted by file name); they are parsed (PCM 16/24/32,
// IEEE float 32, mono/stereo), resampled to the engine rate, faded, and
// peak-normalized at create — nothing allocates in `process`.
//
// DSP: partitioned convolution (overlap-save, N = 2048 / hop = 1024,
// radix-2 FFT with precomputed tables). Latency = one hop (~21 ms at
// 48k) — irrelevant for a reverb tail. Output = dry*(1-mix) + wet*gain,
// with an optional input low-pass (damping: a dark room needs a dark IR
// feed, exactly like the hardware units this models).
//
// Params:
//   ir   — impulse index (see the editor list; 0 = built-in hall)
//   mix  — dry/wet (0 = dry, 1 = wet)
//   gain — wet trim
//   damp — input low-pass Hz feeding the convolver (100..20000)
//
// No sidechain (v1). NaN-guarded; a non-finite blast resets the engine.
// ---------------------------------------------------------------------------

include!("effect_api.rs");

use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

const FN: usize = 2048; // FFT size
const FL: usize = 1024; // hop (block size); latency = FL samples
const MAX_IR: usize = 8; // incl. built-in slot 0
const IR_MAX_S: f64 = 1.2; // IR truncation cap (fade-out applied)

// ---------------------------------------------------------------- WAV ----

fn rd_u16(b: &[u8], p: usize) -> u16 {
    u16::from_le_bytes([b[p], b[p + 1]])
}
fn rd_u32(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]])
}

/// Parse a WAV file into mono f64 samples + its sample rate.
fn load_wav(path: &std::path::Path) -> Option<(Vec<f64>, f64)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut fmt = 1u16;
    let mut ch = 1u16;
    let mut hz = 44100u32;
    let mut bits = 16u16;
    let mut data: Option<(usize, usize)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let sz = rd_u32(&bytes, pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            fmt = rd_u16(&bytes, body);
            ch = rd_u16(&bytes, body + 2).max(1);
            hz = rd_u32(&bytes, body + 4).max(1);
            bits = rd_u16(&bytes, body + 14);
        } else if id == b"data" {
            data = Some((body, (body + sz).min(bytes.len())));
        }
        pos = body + sz + (sz & 1);
    }
    let (mut p, end) = data?;
    let ch = ch.min(8).max(1) as usize;
    let bytes_per = (bits / 8) as usize;
    if bytes_per == 0 || p >= end {
        return None;
    }
    let mut out: Vec<f64> = Vec::new();
    let frame = bytes_per * ch;
    while p + frame <= end {
        let mut acc = 0.0f64;
        for c in 0..ch {
            let q = p + c * bytes_per;
            let s = match (fmt, bits) {
                (3, 32) => f32::from_le_bytes([bytes[q], bytes[q + 1], bytes[q + 2], bytes[q + 3]]) as f64,
                (3, 64) => f64::from_le_bytes([
                    bytes[q], bytes[q + 1], bytes[q + 2], bytes[q + 3],
                    bytes[q + 4], bytes[q + 5], bytes[q + 6], bytes[q + 7],
                ]),
                (1, 16) => i16::from_le_bytes([bytes[q], bytes[q + 1]]) as f64 / 32768.0,
                (1, 32) => i32::from_le_bytes([bytes[q], bytes[q + 1], bytes[q + 2], bytes[q + 3]]) as f64 / 2147483648.0,
                (1, 8) => (bytes[q] as f64 - 128.0) / 128.0,
                (1, 24) => {
                    let v = (bytes[q] as i32) | ((bytes[q + 1] as i32) << 8) | ((bytes[q + 2] as i32) << 16);
                    (if v & 0x800000 != 0 { v | !0xFFFFFF } else { v }) as f64 / 8388608.0
                }
                _ => return None,
            };
            acc += s;
        }
        out.push(acc / ch as f64);
        p += frame;
    }
    Some((out, hz as f64))
}

fn resample(x: &[f64], sr_from: f64, sr_to: f64) -> Vec<f64> {
    if x.is_empty() || (sr_from - sr_to).abs() < 0.5 {
        return x.to_vec();
    }
    let n_out = ((x.len() as f64) * sr_to / sr_from) as usize;
    let mut out = Vec::with_capacity(n_out);
    let ratio = sr_from / sr_to;
    for i in 0..n_out {
        let pos = i as f64 * ratio;
        let i0 = pos as usize;
        let f = pos - i0 as f64;
        let a = x[i0.min(x.len() - 1)];
        let b = x[(i0 + 1).min(x.len() - 1)];
        out.push(a + (b - a) * f);
    }
    out
}

/// Fade the tail (only long IRs need it) and energy-normalize. Energy
/// normalization — not peak — keeps the wet path at dry-like loudness
/// whatever the IR length: a delta stays itself, a 1 s hall stays at
/// the input's level. Returns the (possibly shortened) IR.
fn tidy(mut x: Vec<f64>, sr: f64) -> Vec<f64> {
    let cap = (IR_MAX_S * sr) as usize;
    x.truncate(cap.max(FL));
    if x.len() >= 4 * FL {
        let fade = (x.len() / 7).max(1);
        let n = x.len();
        for k in 0..fade {
            let i = n - 1 - k;
            let t = k as f64 / fade as f64;
            x[i] *= 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
        }
    }
    let e = x.iter().fold(0.0f64, |m, v| m + v * v).sqrt();
    if e > 1.0e-9 {
        let g = 1.0 / e;
        for v in x.iter_mut() {
            *v *= g;
        }
    }
    x
}

/// Built-in hall: lowpassed noise with an 8 ms rise and 1 s T60, plus a
/// 12 ms pre-delay. Deterministic (seeded LCG) so it is the same "room"
/// everywhere.
fn builtin_ir(sr: f64) -> Vec<f64> {
    let n = (1.15 * sr) as usize;
    let mut out = Vec::with_capacity(n);
    let mut rng: u32 = 0x2545_F491;
    let mut lp1 = 0.0;
    let mut lp2 = 0.0;
    let att = (0.008 * sr) as usize;
    let pre = (0.012 * sr) as usize;
    let k = (-9.21034 / (1.0 * sr)).exp();
    for i in 0..n {
        rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = ((rng >> 8) as f64 / 8_388_608.0) - 1.0;
        lp1 += (noise - lp1) * 0.25;
        lp2 += (lp1 - lp2) * 0.55;
        let env = if i < pre {
            0.0
        } else if i < pre + att {
            (i - pre) as f64 / att as f64
        } else {
            k.powi((i - pre - att) as i32)
        };
        out.push(lp2 * env * 0.6);
    }
    out
}

// ----------------------------------------------------------------- FFT ---

fn build_tables() -> (Vec<u32>, Vec<f64>, Vec<f64>) {
    let log2n = FN.trailing_zeros() as usize;
    let mut rev = vec![0u32; FN];
    for i in 0..FN {
        let mut r = 0usize;
        let mut x = i;
        for _ in 0..log2n {
            r = (r << 1) | (x & 1);
            x >>= 1;
        }
        rev[i] = r as u32;
    }
    let mut rc = vec![0.0; FN / 2];
    let mut rs = vec![0.0; FN / 2];
    for k in 0..FN / 2 {
        let a = 2.0 * core::f64::consts::PI * k as f64 / FN as f64;
        rc[k] = a.cos();
        rs[k] = a.sin();
    }
    (rev, rc, rs)
}

/// In-place radix-2 Cooley-Tukey. `inv` = inverse (conjugate twiddles + 1/N).
fn fft(re: &mut [f64], im: &mut [f64], inv: bool, rev: &[u32], rc: &[f64], rs: &[f64]) {
    for i in 0..FN {
        let j = rev[i] as usize;
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut size = 2usize;
    while size <= FN {
        let half = size / 2;
        let step = FN / size;
        let mut start = 0;
        while start < FN {
            let mut k = 0usize;
            for idx in start..start + half {
                let wr = rc[k];
                let wi = if inv { rs[k] } else { -rs[k] };
                let ar = re[idx];
                let ai = im[idx];
                let br = re[idx + half];
                let bi = im[idx + half];
                let tr = br * wr - bi * wi;
                let ti = br * wi + bi * wr;
                re[idx] = ar + tr;
                im[idx] = ai + ti;
                re[idx + half] = ar - tr;
                im[idx + half] = ai - ti;
                k += step;
            }
            start += size;
        }
        size <<= 1;
    }
    if inv {
        let g = 1.0 / FN as f64;
        for v in re.iter_mut() {
            *v *= g;
        }
        for v in im.iter_mut() {
            *v *= g;
        }
    }
}

// ----------------------------------------------------------- Convolver ---

struct Ir {
    name: String,
    spectra: Vec<(Vec<f64>, Vec<f64>)>, // one FFT-spectrum per FL-hop partition
}

struct Conv {
    sr: f64,
    irs: Vec<Ir>,
    cur: AtomicUsize,
    rev: Vec<u32>,
    rc: Vec<f64>,
    rs: Vec<f64>,
    // Runtime history: spectra of the last PBUF input windows.
    pbuf: usize,
    xring: Vec<(Vec<f64>, Vec<f64>)>,
    slot: usize,
    xbuf: Vec<f64>, // last FN input samples (circular)
    wp: usize,
    block: Vec<f64>, // FL wet outputs for the current hop
    bk: usize,
    // Scratch
    tr: Vec<f64>,
    ti: Vec<f64>,
    wr: Vec<f64>,
    wi: Vec<f64>,
    lp: f64, // damping one-pole state
    // smoothed params
    mix: f64,
    gain: f64,
}

fn impulses_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("SLOOPER_IMPULSES_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    // Keep in sync with crate::paths::impulses_dir() in the host (this
    // script compiles standalone and cannot call host code).
    #[cfg(windows)]
    let base = std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|p| !p.is_empty())
                .map(|h| std::path::PathBuf::from(h).join("AppData").join("Local"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    #[cfg(not(windows))]
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local")
                .join("share")
        });
    base.join("slooper").join("impulses")
}

fn make_ir(name: &str, x: &[f64]) -> Option<Ir> {
    if x.is_empty() {
        return None;
    }
    let (mut re, mut im) = (vec![0.0f64; FN], vec![0.0f64; FN]);
    let mut spectra = Vec::new();
    let (rev, rc, rs) = build_tables();
    let mut p = 0usize;
    while p * FL < x.len() {
        for v in re.iter_mut() {
            *v = 0.0;
        }
        for v in im.iter_mut() {
            *v = 0.0;
        }
        let start = p * FL;
        let end = ((p + 1) * FL).min(x.len());
        re[..end - start].copy_from_slice(&x[start..end]);
        fft(&mut re, &mut im, false, &rev, &rc, &rs);
        spectra.push((re.clone(), im.clone()));
        p += 1;
    }
    Some(Ir {
        name: name.to_string(),
        spectra,
    })
}

extern "C" fn create(sample_rate: f64) -> *mut core::ffi::c_void {
    let sr = if sample_rate > 1.0 { sample_rate } else { 48000.0 };

    // Folder capability: create + drop a README so users can find it.
    let dir = impulses_dir();
    let _ = std::fs::create_dir_all(&dir);
    let readme = dir.join("README.txt");
    if !readme.exists() {
        let _ = std::fs::write(
            &readme,
            "slooper convolver impulses\n\
             \n\
             Drop any .wav file here and rescan the effects (or reopen the\n\
             convolver editor): the convolver effect lists every file and the\n\
             `ir` slider picks one. Files are resampled to the engine rate,\n\
             truncated to 1.2 s (with a fade) and peak-normalized.\n\
             \n\
             Index 0 is always the built-in synthetic hall. The first 7 .wav\n\
             files in this folder (sorted by name) load as indexes 1..7.\n\
             Stereo IRs are summed to mono. PCM 8/16/24/32 and IEEE float.\n",
        );
    }

    let mut irs = Vec::new();
    // Index 0: built-in hall.
    let hall = tidy(builtin_ir(sr), sr);
    if let Some(ir) = make_ir("built-in hall", &hall) {
        irs.push(ir);
    }
    // Files.
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.to_ascii_lowercase() == "wav")
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    for path in files.into_iter().take(MAX_IR - 1) {
        if let Some((x, hz)) = load_wav(&path) {
            let mono = resample(&x, hz, sr);
            let mono = tidy(mono, sr);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "ir".into());
            if let Some(ir) = make_ir(&name, &mono) {
                irs.push(ir);
            }
        }
    }
    if irs.is_empty() {
        return core::ptr::null_mut();
    }

    let max_parts = irs.iter().map(|i| i.spectra.len()).max().unwrap_or(1);
    let pbuf = max_parts + 1;
    let c = Box::new(Conv {
        sr,
        irs,
        cur: AtomicUsize::new(0),
        rev: build_tables().0,
        rc: build_tables().1,
        rs: build_tables().2,
        pbuf,
        xring: (0..pbuf)
            .map(|_| (vec![0.0f64; FN], vec![0.0f64; FN]))
            .collect(),
        slot: 0,
        xbuf: vec![0.0; FN],
        wp: 0,
        block: vec![0.0; FL],
        bk: FL,
        tr: vec![0.0; FN],
        ti: vec![0.0; FN],
        wr: vec![0.0; FN],
        wi: vec![0.0; FN],
        lp: 0.0,
        mix: 0.35,
        gain: 1.0,
    });
    Box::into_raw(c) as *mut core::ffi::c_void
}

extern "C" fn destroy(state: *mut core::ffi::c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut Conv) });
    }
}

fn reset(c: &mut Conv) {
    for v in c.xbuf.iter_mut() {
        *v = 0.0;
    }
    for v in c.block.iter_mut() {
        *v = 0.0;
    }
    for (re, im) in c.xring.iter_mut() {
        for v in re.iter_mut() {
            *v = 0.0;
        }
        for v in im.iter_mut() {
            *v = 0.0;
        }
    }
    c.lp = 0.0;
}

/// One hop boundary: FFT the newest window, accumulate all partition
/// contributions into `block` (the wet outputs for the NEXT hop).
fn process_block(c: &mut Conv) {
    let cur = c.cur.load(Relaxed).min(c.irs.len() - 1);
    // newest FN samples -> window
    for j in 0..FN {
        c.wr[j] = c.xbuf[(c.wp + j) & (FN - 1)];
        c.wi[j] = 0.0;
    }
    fft(&mut c.wr, &mut c.wi, false, &c.rev, &c.rc, &c.rs);
    c.slot = (c.slot + 1) % c.pbuf;
    {
        let (re, im) = &mut c.xring[c.slot];
        re.copy_from_slice(&c.wr);
        im.copy_from_slice(&c.wi);
    }
    for v in c.block.iter_mut() {
        *v = 0.0;
    }
    let n_part = c.irs[cur].spectra.len().min(c.pbuf);
    for i in 0..n_part {
        let slot = (c.slot + c.pbuf - i) % c.pbuf;
        let (hr, hi) = &c.irs[cur].spectra[i];
        let (xr, xi) = &c.xring[slot];
        for k in 0..FN {
            let ar = xr[k];
            let ai = xi[k];
            let br = hr[k];
            let bi = hi[k];
            c.tr[k] = ar * br - ai * bi;
            c.ti[k] = ar * bi + ai * br;
        }
        fft(&mut c.tr, &mut c.ti, true, &c.rev, &c.rc, &c.rs);
        for k in FL..FN {
            c.block[k - FL] += c.tr[k];
        }
    }
    c.bk = 0;
}

extern "C" fn process(state: *mut core::ffi::c_void, input: f32, params: *const f32) -> f32 {
    if state.is_null() {
        return input;
    }
    let c = unsafe { &mut *(state as *mut Conv) };
    let mix_t = unsafe { *params.add(1) }.clamp(0.0, 1.0) as f64;
    let gain_t = unsafe { *params.add(2) }.clamp(0.0, 2.0) as f64;
    let damp = unsafe { *params.add(3) }.clamp(100.0, 20000.0) as f64;
    let want = unsafe { *params.add(0) } as usize;
    let want = want.min(c.irs.len() - 1);
    if want != c.cur.load(Relaxed) {
        c.cur.store(want, Relaxed);
    }

    // Input damping (true-Hz one-pole) feeds the convolver; dry stays clean.
    let g = (-(2.0 * core::f64::consts::PI) * damp / c.sr).exp();
    c.lp += (input as f64 - c.lp) * (1.0 - g);
    c.xbuf[c.wp] = c.lp;
    c.wp = (c.wp + 1) & (FN - 1);

    if c.bk >= FL {
        process_block(c);
    }
    let wet = c.block[c.bk];
    c.bk += 1;

    // Smooth mix/gain within the hop so slider steps never click.
    c.mix += (mix_t - c.mix) * 0.002;
    c.gain += (gain_t - c.gain) * 0.002;

    let out = (1.0 - c.mix) * input as f64 + c.mix * wet * c.gain;
    if out.is_finite() {
        out as f32
    } else {
        reset(c);
        0.0
    }
}

static PARAMS: [EffectParamV1; 4] = [
    EffectParamV1 { label: c"ir (index)".as_ptr(), min: 0.0, max: (MAX_IR - 1) as f32, default: 0.0 },
    EffectParamV1 { label: c"mix".as_ptr(), min: 0.0, max: 1.0, default: 0.35 },
    EffectParamV1 { label: c"gain".as_ptr(), min: 0.0, max: 2.0, default: 1.0 },
    EffectParamV1 { label: c"damp Hz".as_ptr(), min: 100.0, max: 20000.0, default: 14000.0 },
];

pub static DESC: EffectInfoV1 = EffectInfoV1 {
    abi: SLOOPER_EFFECT_ABI,
    name: c"convolver".as_ptr(),
    author: c"slooper".as_ptr(),
    description:
        c"Convolution reverb. Drop .wav impulses into ~/.local/share/slooper/impulses - index 0 is a built-in hall. Editor lists what loaded.".as_ptr(),
    n_params: 4,
    params: PARAMS.as_ptr(),
    create,
    destroy,
    process,
};

#[no_mangle]
pub extern "C" fn slooper_effect_info() -> *const EffectInfoV1 {
    &DESC
}

// ------------------------------------------------------------ child GUI --

extern "C" fn gui_create(_sample_rate: f64) -> *mut core::ffi::c_void {
    std::ptr::null_mut()
}

extern "C" fn gui_destroy(_gs: *mut core::ffi::c_void) {}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s.chars().filter(|c| c.is_ascii()).collect::<String>()).unwrap()
}

extern "C" fn gui_draw(
    _gs: *mut core::ffi::c_void,
    state: *mut core::ffi::c_void,
    frame: *const GuiFrameV1,
    api: *const GuiApiV1,
) {
    if state.is_null() || api.is_null() {
        return;
    }
    let f = unsafe { &*frame };
    let api = unsafe { &*api };
    let c = unsafe { &*(state as *const Conv) }; // immutable reads + atomics only
    let cur = c.cur.load(Relaxed).min(c.irs.len() - 1);

    let head = cstr(&format!("Convolver - {} impulse(s) loaded", c.irs.len()));
    (api.text)(api.ctx, 14.0, 18.0, 15.0, 0, 0.95, 0.95, 0.95, 1.0, head.as_ptr());
    let sel = cstr(&format!("IR {}: {}", cur, c.irs[cur].name));
    (api.text)(api.ctx, 14.0, 40.0, 13.0, 0, 0.6, 0.85, 0.6, 1.0, sel.as_ptr());
    let dir = cstr(&format!("folder: {}", impulses_dir().display()));
    (api.text)(api.ctx, 14.0, 58.0, 11.0, 0, 0.6, 0.6, 0.65, 1.0, dir.as_ptr());
    (api.text)(
        api.ctx,
        14.0,
        74.0,
        11.0,
        0,
        0.55,
        0.55,
        0.6,
        1.0,
        c"drop .wav files there, then Rescan the Effects window".as_ptr(),
    );

    // Slider row: ir, mix, gain, damp.
    let mut v = (api.get_param)(api.ctx, 0);
    if (api.slider)(
        api.ctx, 14.0, 100.0, f.width - 30.0, 26.0,
        c"ir index".as_ptr(), &mut v, 0.0, (c.irs.len() - 1) as f32, 1.0,
    ) == 1
    {
        (api.set_param)(api.ctx, 0, v.round().max(0.0));
    }
    let mut v = (api.get_param)(api.ctx, 1);
    if (api.slider)(api.ctx, 14.0, 132.0, f.width - 30.0, 26.0, c"mix".as_ptr(), &mut v, 0.0, 1.0, 1.0) == 1 {
        (api.set_param)(api.ctx, 1, v);
    }
    let mut v = (api.get_param)(api.ctx, 2);
    if (api.slider)(api.ctx, 14.0, 164.0, f.width - 30.0, 26.0, c"gain".as_ptr(), &mut v, 0.0, 2.0, 1.0) == 1 {
        (api.set_param)(api.ctx, 2, v);
    }
    let mut v = (api.get_param)(api.ctx, 3);
    if (api.slider)(api.ctx, 14.0, 196.0, f.width - 30.0, 26.0, c"damp Hz".as_ptr(), &mut v, 100.0, 20000.0, 0.0) == 1 {
        (api.set_param)(api.ctx, 3, v);
    }

    // IR name list.
    for (i, ir) in c.irs.iter().enumerate().take(MAX_IR) {
        let y = 236.0 + i as f32 * 16.0;
        if y > f.height - 8.0 {
            break;
        }
        let nm = cstr(&format!("{}  {}", if i == cur { ">" } else { " " }, ir.name));
        if i == cur {
            (api.text)(api.ctx, 14.0, y, 12.0, 0, 0.95, 0.9, 0.5, 1.0, nm.as_ptr());
        } else {
            (api.text)(api.ctx, 14.0, y, 12.0, 0, 0.7, 0.7, 0.75, 1.0, nm.as_ptr());
        }
    }
}

pub static GUI: EffectGuiV1 = EffectGuiV1 {
    abi: SLOOPER_EFFECT_GUI_ABI,
    want_width: 460.0,
    want_height: 400.0,
    create: gui_create,
    draw: gui_draw,
    destroy: gui_destroy,
};

#[no_mangle]
pub extern "C" fn slooper_effect_gui() -> *const EffectGuiV1 {
    &GUI
}
