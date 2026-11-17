//! Bundled TR-808 starter pack + the stock 808 drum-kit preset.
//!
//! Twenty-four one-shots sampled directly from a real Roland TR-808 Rhythm
//! Composer (serial 103852) by Michael Fischer / Technopolis in 1994 — the
//! classic "TR808WAV" set published on machines.hyperreal.org with **no
//! licensing restrictions** ("ABSOLUTELY FREE"), redistributed here via the
//! ISC-licensed `@fluid-music/tr-808` npm repackaging (see
//! `samples/808/CREDITS.md` for the full chain).
//!
//! The WAVs are embedded in the binary (`include_bytes!`) so every install —
//! package, desktop copy, or `cargo run` — ships them. On startup the app
//! seeds them into `$XDG_DATA_HOME/slooper/samples/808` (default
//! `~/.local/share/...`) — files are (re)written whenever the on-disk copy
//! is missing or its CONTENT differs from the embedded copy, so a deleted
//! or tampered file comes back but a live install never fights itself
//! (v81: the staleness test is byte-content, not size — a same-length edit
//! is detected too). The seeded folder becomes the asset-rack default when
//! the user has configured no rack folders, which is what makes the pack
//! the app's *default samples*: the first folder, so takes record into it
//! and every 808 voice is one drag away (under "808").
//!
//! v81 (S1) adds the kit side of the story: [`seed_stock_kit`] writes the
//! "808-Kit" drum-kit preset (v69 format) that arms 16 of the pack sounds
//! onto a track in one click from the kit bar.

use std::path::{Path, PathBuf};

/// Folder name (under the app data dir) the pack is seeded into.
pub const PACK_DIRNAME: &str = "808";

/// Every pack file: (file name, embedded 16-bit / 44.1 kHz / mono WAV).
pub const FILES: &[(&str, &[u8])] = &[
    (
        "808-kick.wav",
        include_bytes!("../samples/808/808-kick.wav"),
    ),
    (
        "808-kick-tight.wav",
        include_bytes!("../samples/808/808-kick-tight.wav"),
    ),
    ("808-sub.wav", include_bytes!("../samples/808/808-sub.wav")),
    (
        "808-sub-deep.wav",
        include_bytes!("../samples/808/808-sub-deep.wav"),
    ),
    (
        "808-snare.wav",
        include_bytes!("../samples/808/808-snare.wav"),
    ),
    (
        "808-snare-tight.wav",
        include_bytes!("../samples/808/808-snare-tight.wav"),
    ),
    (
        "808-snare-lo.wav",
        include_bytes!("../samples/808/808-snare-lo.wav"),
    ),
    (
        "808-snare-long.wav",
        include_bytes!("../samples/808/808-snare-long.wav"),
    ),
    (
        "808-clap.wav",
        include_bytes!("../samples/808/808-clap.wav"),
    ),
    (
        "808-hat-closed.wav",
        include_bytes!("../samples/808/808-hat-closed.wav"),
    ),
    (
        "808-hat-open.wav",
        include_bytes!("../samples/808/808-hat-open.wav"),
    ),
    (
        "808-hat-open-long.wav",
        include_bytes!("../samples/808/808-hat-open-long.wav"),
    ),
    (
        "808-crash.wav",
        include_bytes!("../samples/808/808-crash.wav"),
    ),
    (
        "808-cym-ping.wav",
        include_bytes!("../samples/808/808-cym-ping.wav"),
    ),
    (
        "808-cowbell.wav",
        include_bytes!("../samples/808/808-cowbell.wav"),
    ),
    (
        "808-tom-low.wav",
        include_bytes!("../samples/808/808-tom-low.wav"),
    ),
    (
        "808-tom-mid.wav",
        include_bytes!("../samples/808/808-tom-mid.wav"),
    ),
    (
        "808-tom-high.wav",
        include_bytes!("../samples/808/808-tom-high.wav"),
    ),
    (
        "808-rimshot.wav",
        include_bytes!("../samples/808/808-rimshot.wav"),
    ),
    (
        "808-clave.wav",
        include_bytes!("../samples/808/808-clave.wav"),
    ),
    (
        "808-maracas.wav",
        include_bytes!("../samples/808/808-maracas.wav"),
    ),
    (
        "808-conga-lo.wav",
        include_bytes!("../samples/808/808-conga-lo.wav"),
    ),
    (
        "808-conga-mid.wav",
        include_bytes!("../samples/808/808-conga-mid.wav"),
    ),
    (
        "808-conga-hi.wav",
        include_bytes!("../samples/808/808-conga-hi.wav"),
    ),
];

/// slooper data dir — `paths::data_dir()`: `$XDG_DATA_HOME/slooper`
/// (default `~/.local/share/slooper`) on Linux, `%LOCALAPPDATA%\slooper`
/// on Windows (WINDOWS-PORT W2).
pub fn data_dir() -> PathBuf {
    crate::paths::data_dir()
}

/// The folder the pack is seeded into.
pub fn pack_dir() -> PathBuf {
    data_dir().join("samples").join(PACK_DIRNAME)
}

/// The samples root — parent of the 808 pack and of every pack the user
/// drops in later. v28: this (not the pack folder) is the default rack
/// folder, so the recursive scan shows the whole library with each pack as
/// its own sub-folder group.
pub fn samples_root() -> PathBuf {
    data_dir().join("samples")
}

/// True when `path` is the seeded pack folder (compared trimmed/absolute).
pub fn is_pack_dir(path: &str) -> bool {
    let p = Path::new(path.trim());
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(p)
    } else {
        p.to_path_buf()
    };
    // Lexically normalise "./" and "../" segments (the folder string may
    // come from a config file written with a slightly different form).
    let norm = |q: &Path| -> PathBuf {
        let mut out = PathBuf::new();
        for c in q.components() {
            match c {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    };
    norm(&abs) == norm(&pack_dir())
}

/// Seed the pack: create the folder and (re)write every file whose CONTENT
/// differs from the embedded copy (missing → written; byte-identical → left
/// alone, no rewrite churn). v81: content compare replaced the old
/// size-only compare, so a same-length tamper is restored too. Returns the
/// folder path; unwritable files are skipped silently (the rack still
/// lists what exists).
pub fn seed_pack() -> PathBuf {
    let dir = pack_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return dir;
    }
    for (name, data) in FILES {
        let p = dir.join(name);
        let fresh = match std::fs::read(&p) {
            Ok(on) => on != *data,
            Err(_) => true,
        };
        if fresh {
            let _ = std::fs::write(&p, data);
        }
    }
    dir
}

// ---- v81 (TS-010 S1): the stock 808 kit preset ----------------------------
//
// The pack's sounds, pre-armed: a named drum-kit preset in the v69 format
// (`data_dir()/drum-kits/808-Kit.json`, same PresetStore folder the kit
// bar lists) that a fresh install ships and any track can load with one
// click. Content-diff staleness, exactly the synth-seed doctrine: missing
// → restored; drifted → the user's bytes are kept once as
// `808-Kit.json.bak` and the stock table rewritten; foreign-named kits
// are never touched. Serialization is `serde_json::to_string_pretty` —
// byte-identical to the v69 `PresetStore::write_json` writer, so re-saving
// the very same pads from the UI never trips the drift check.

/// Kit name (and file stem) of the bundled preset. Survives `sanitize()`
/// untouched (alphanumeric + hyphen), so the bar lists it exactly.
pub const STOCK_KIT_NAME: &str = "808-Kit";

/// The kit table: (pad name as the pattern builder shows it, pack file).
/// 16 pads = DRUM_MAX_PADS. The remaining pack sounds (sub-deep, snare-lo,
/// hat-open-long, cym-ping, maracas, conga lo/mid) stay rack variety for
/// drag-and-drop per-pad import.
const STOCK_KIT_PADS: &[(&str, &str)] = &[
    ("Kick", "808-kick.wav"),
    ("KickTight", "808-kick-tight.wav"),
    ("Sub", "808-sub.wav"),
    ("Snare", "808-snare.wav"),
    ("SnareTight", "808-snare-tight.wav"),
    ("Clap", "808-clap.wav"),
    ("Rim", "808-rimshot.wav"),
    ("Clave", "808-clave.wav"),
    ("Hat", "808-hat-closed.wav"),
    ("Open", "808-hat-open.wav"),
    ("Crash", "808-crash.wav"),
    ("Cow", "808-cowbell.wav"),
    ("TomLo", "808-tom-low.wav"),
    ("TomMid", "808-tom-mid.wav"),
    ("TomHi", "808-tom-high.wav"),
    ("Conga", "808-conga-hi.wav"),
];

/// The stock kit as a save record. Pad assets are absolute paths into the
/// seeded pack folder, built scan-form (chained single-component joins —
/// the Windows PathEq trap, WINDOWS-PORT), so they match rack-scan paths
/// byte-for-byte and re-link when the kit is loaded.
pub fn stock_kit() -> crate::config::DrumKitSave {
    crate::config::DrumKitSave {
        name: STOCK_KIT_NAME.to_string(),
        pads: STOCK_KIT_PADS
            .iter()
            .map(|(pad, file)| crate::config::DrumPadSave {
                name: (*pad).to_string(),
                asset: pack_dir().join(file).to_string_lossy().into_owned(),
                vel: 100,
                muted: false,
            })
            .collect(),
    }
}

/// Where the stock kit preset is seeded. Chained joins, one component
/// each — never `join("drum-kits/808-Kit.json")` (mixed-separator trap).
pub fn stock_kit_path() -> PathBuf {
    crate::paths::data_dir()
        .join("drum-kits")
        .join(format!("{STOCK_KIT_NAME}.json"))
}

/// Seed the stock kit preset (call next to [`seed_pack`] at boot).
/// Returns Some(path) once the file holds the stock table; None only when
/// the parent folder cannot be created. A drifted on-disk copy is backed
/// up to `<name>.json.bak` before the stock table is rewritten.
pub fn seed_stock_kit() -> Option<PathBuf> {
    let path = stock_kit_path();
    let dir = path.parent()?;
    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }
    let want = serde_json::to_string_pretty(&stock_kit()).ok()?;
    match std::fs::read_to_string(&path) {
        // Content matches: no write, no .bak, no churn.
        Ok(cur) if cur == want => {}
        Ok(cur) => {
            let bak = path.with_extension("json.bak");
            let _ = std::fs::write(&bak, cur);
            let _ = std::fs::write(&path, want);
        }
        Err(_) => {
            let _ = std::fs::write(&path, want);
        }
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal RIFF chunk walk: the int16 peak of the 'data' chunk in
    /// absolute dBFS. None when the file carries no parsable 16-bit data
    /// chunk (odd sizes too). Used by the level-window pins (dsp-audio-
    /// probe doctrine: ABSOLUTE window dBFS + a negative control).
    fn wav_peak_dbfs(data: &[u8]) -> Option<f64> {
        if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
            return None;
        }
        let mut i = 12usize;
        while i + 8 <= data.len() {
            let id = &data[i..i + 4];
            let size = u32::from_le_bytes([
                data[i + 4],
                data[i + 5],
                data[i + 6],
                data[i + 7],
            ]) as usize;
            let start = i + 8;
            let end = start.checked_add(size)?.min(data.len());
            if id == b"data" {
                let samples = &data[start..end];
                if samples.len() % 2 != 0 || samples.is_empty() {
                    return None;
                }
                let mut peak: i32 = 0;
                for s in samples.chunks_exact(2) {
                    let v = i16::from_le_bytes([s[0], s[1]]).unsigned_abs() as i32;
                    if v > peak {
                        peak = v;
                    }
                }
                if peak == 0 {
                    return Some(f64::NEG_INFINITY);
                }
                return Some(20.0 * (peak as f64 / 32768.0).log10());
            }
            // Chunks pad to even sizes.
            i = start + size + (size % 2);
        }
        None
    }

    /// fmt-chunk fields: (audio_format, channels, sample_rate, bits).
    fn wav_fmt(data: &[u8]) -> Option<(u16, u16, u32, u16)> {
        let mut i = 12usize;
        while i + 8 <= data.len() {
            let id = &data[i..i + 4];
            let size = u32::from_le_bytes([
                data[i + 4],
                data[i + 5],
                data[i + 6],
                data[i + 7],
            ]) as usize;
            let start = i + 8;
            if id == b"fmt " && start + 16 <= data.len() {
                let f = &data[start..start + 16];
                return Some((
                    u16::from_le_bytes([f[0], f[1]]),
                    u16::from_le_bytes([f[2], f[3]]),
                    u32::from_le_bytes([f[4], f[5], f[6], f[7]]),
                    u16::from_le_bytes([f[14], f[15]]),
                ));
            }
            i = start + size + (size % 2);
        }
        None
    }

    #[test]
    fn embedded_pack_is_valid_wav_audio() {
        assert_eq!(FILES.len(), 24);
        for (name, data) in FILES {
            assert!(name.starts_with("808-") && name.ends_with(".wav"), "{name}");
            assert!(data.len() > 44, "{name}: truncated");
            assert_eq!(&data[0..4], b"RIFF", "{name}: not a RIFF file");
            assert_eq!(&data[8..12], b"WAVE", "{name}: not a WAVE file");
            // Every entry distinct (no accidentally duplicated asset).
            assert_eq!(
                FILES.iter().filter(|(_, d)| *d == *data).count(),
                1,
                "{name}: duplicate content"
            );
        }
    }

    #[test]
    fn v81_pack_files_complete() {
        // R1: 24 entries, unique names, uniform 16-bit/44.1 kHz/mono
        // PCM, and every peak inside the absolute level window.
        assert_eq!(FILES.len(), 24);
        let mut names: Vec<&str> = FILES.iter().map(|(n, _)| *n).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 24, "duplicate file name");
        for (name, data) in FILES {
            let (fmt, ch, rate, bits) = wav_fmt(data).unwrap_or_else(|| panic!("{name}: no fmt chunk"));
            assert_eq!((fmt, ch, rate, bits), (1, 1, 44100, 16), "{name}: not 16-bit/44.1k/mono PCM");
            let db = wav_peak_dbfs(data).unwrap_or_else(|| panic!("{name}: no data chunk"));
            assert!(
                (-24.0..=-0.1).contains(&db),
                "{name}: peak {db:+.1} dBFS outside the level window"
            );
        }
        // Negative control (dsp-audio-probe rule): a silent synth file
        // must FAIL the same window — proves the check measures real
        // signal, not a vacuous pass.
        let mut silent = Vec::new();
        silent.extend_from_slice(b"RIFF\x24\x00\x01\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x44\xac\x00\x00\x88\x58\x01\x00\x02\x00\x10\x00data\x00\x00\x01\x00");
        silent.extend_from_slice(&[0u8; 65536]);
        let db = wav_peak_dbfs(&silent).expect("silent control parses");
        assert!(
            !(-24.0..=-0.1).contains(&db),
            "negative control (silent, {db:+.1} dBFS) must be rejected"
        );
    }

    #[test]
    fn v81_pack_size_budget() {
        // Acceptance: the embedded pack (and with it the ~13 MB release
        // binary) stays sane. 4 MiB is a generous ceiling — the actual
        // pack is ~2 MB.
        let sum: usize = FILES.iter().map(|(_, d)| d.len()).sum();
        assert!(sum < 4 * 1024 * 1024, "pack budget blown: {sum} bytes");
    }

    #[test]
    fn v81_pack_seed_content_stale() {
        // R1: seeding is CONTENT-driven, not size-driven: a same-length
        // different-content tamper comes back, identical bytes are never
        // rewritten, foreign files in the folder are never touched.
        let base = std::env::temp_dir().join(format!("sp-cstale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("XDG_DATA_HOME", &base);
        std::env::set_var(crate::paths::DATA_DIR_ENV, base.join("slooper"));
        let dir = seed_pack();
        // All 24 land BYTE-IDENTICAL (content, not just size).
        for (name, data) in FILES {
            let on = std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(on.as_slice(), *data, "{name}: seed content");
        }
        // Same LENGTH, different bytes: the old size compare would miss
        // this; the content rule must restore it.
        let t = dir.join("808-clave.wav");
        let mut cur = std::fs::read(&t).unwrap();
        let embedded = FILES.iter().find(|(n, _)| *n == "808-clave.wav").unwrap().1;
        cur[60] ^= 0xFF;
        assert_eq!(cur.len(), embedded.len(), "tamper must be same-size");
        assert_ne!(cur.as_slice(), embedded, "tamper changed content");
        std::fs::write(&t, &cur).unwrap();
        seed_pack();
        assert_eq!(std::fs::read(&t).unwrap().as_slice(), embedded, "content tamper restored");
        // A deleted file comes back byte-identical.
        std::fs::remove_file(dir.join("808-maracas.wav")).unwrap();
        seed_pack();
        let m = FILES.iter().find(|(n, _)| *n == "808-maracas.wav").unwrap().1;
        assert_eq!(std::fs::read(dir.join("808-maracas.wav")).unwrap().as_slice(), m);
        // A foreign-named file in the pack folder is never touched.
        let foreign = dir.join("my-kick-remix.wav");
        std::fs::write(&foreign, b"not ours").unwrap();
        seed_pack();
        assert_eq!(std::fs::read(&foreign).unwrap(), b"not ours");
        // Idempotent: matching content means NO write at all (mtime flat).
        let before = std::fs::metadata(dir.join("808-kick.wav")).unwrap().modified().unwrap();
        seed_pack();
        let after = std::fs::metadata(dir.join("808-kick.wav")).unwrap().modified().unwrap();
        assert_eq!(before, after, "identical content must not rewrite");
        let _ = std::fs::remove_dir_all(&base);
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var(crate::paths::DATA_DIR_ENV);
    }

    #[test]
    fn v81_stock_kit_seeded_and_stale() {
        // R3: boot seeds 808-Kit.json next to the pack; drift is restored
        // via a .bak of the user's bytes; foreign kits stay untouched.
        let base = std::env::temp_dir().join(format!("sp-skit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("XDG_DATA_HOME", &base);
        std::env::set_var(crate::paths::DATA_DIR_ENV, base.join("slooper"));
        seed_pack();
        let path = seed_stock_kit().expect("stock kit seeded");
        assert_eq!(
            path,
            base.join("slooper").join("drum-kits").join("808-Kit.json"),
            "kit lands in the PresetStore folder (chained joins)"
        );
        let want = serde_json::to_string_pretty(&stock_kit()).unwrap();
        let on = std::fs::read_to_string(&path).unwrap();
        // Same writer format as PresetStore::write_json → UI re-saves of
        // identical pads cannot read as drift.
        assert_eq!(on, want, "seed bytes match the store writer");
        let rec: crate::config::DrumKitSave = serde_json::from_str(&on).unwrap();
        assert_eq!(rec.name, STOCK_KIT_NAME);
        assert_eq!(rec.pads.len(), 16, "DRUM_MAX_PADS");
        for (pad_name, file) in STOCK_KIT_PADS {
            let pad = rec.pads.iter().find(|p| p.name == *pad_name).unwrap_or_else(|| panic!("pad {pad_name}"));
            assert_eq!(
                pad.asset,
                pack_dir().join(file).to_string_lossy(),
                "pad {pad_name} asset is the scan-form pack path"
            );
            assert!(pack_dir().join(file).is_file(), "referenced pack file seeded: {file}");
            assert_eq!(pad.vel, 100);
            assert!(!pad.muted);
        }
        let bak = path.with_extension("json.bak");
        // Idempotent re-seed: no write, no .bak.
        seed_stock_kit();
        assert!(!bak.exists(), "clean re-seed must not churn a .bak");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want);
        // Drift: the user's bytes are kept once as .bak, stock restored.
        let tampered = "{\"name\":\"808-Kit\",\"pads\":[]}";
        std::fs::write(&path, tampered).unwrap();
        seed_stock_kit();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), tampered, "drift backed up");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want, "stock restored");
        // Foreign-named kits are NEVER touched.
        let foreign = path.parent().unwrap().join("My-Kit.json");
        std::fs::write(&foreign, b"foreign kit").unwrap();
        seed_stock_kit();
        assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "foreign kit");
        // Missing → restored.
        std::fs::remove_file(&path).unwrap();
        seed_stock_kit();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), want, "missing kit restored");
        let _ = std::fs::remove_dir_all(&base);
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var(crate::paths::DATA_DIR_ENV);
    }

    #[test]
    fn pack_dir_follows_xdg_and_is_comparable() {
        let dir = pack_dir();
        assert!(dir.ends_with("samples/808"));
        let s = dir.to_string_lossy().into_owned();
        assert!(is_pack_dir(&s));
        assert!(is_pack_dir(&format!("{s}/")));
        assert!(!is_pack_dir("/somewhere/else"));
    }

    #[test]
    fn seeds_into_a_scratch_xdg_home() {
        // Redirect the data root so the test never touches the real data
        // dir. XDG is Unix-only; the OS-neutral sandbox is SLOOPER_DATA_DIR
        // (W2), pointed at what XDG would resolve (`base/slooper`) so the
        // assertions below are IDENTICAL on both OSes (W14, WINDOWS-PORT —
        // on Windows the XDG var alone was inert and the pack seeded into
        // the real %LOCALAPPDATA%).
        let base = std::env::temp_dir().join(format!("sp-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("XDG_DATA_HOME", &base);
        std::env::set_var(crate::paths::DATA_DIR_ENV, base.join("slooper"));
        let dir = seed_pack();
        assert_eq!(dir, base.join("slooper").join("samples").join("808"));
        for (name, data) in FILES {
            let p = dir.join(name);
            let on = std::fs::read(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(on.len(), data.len());
        }
        // Idempotent re-seed keeps files (content matches → no rewrite
        // churn).
        seed_pack();
        // A deleted file comes back.
        std::fs::remove_file(dir.join("808-kick.wav")).unwrap();
        seed_pack();
        assert!(dir.join("808-kick.wav").exists());
        let _ = std::fs::remove_dir_all(&base);
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var(crate::paths::DATA_DIR_ENV);
    }
}
