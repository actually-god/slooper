//! Asset model + rack folder scanning.
//!
//! An *asset* is a decoded mono audio buffer at the engine sample rate plus
//! the file it came from. The GUI owns the mutable list (drag-and-drop, the
//! asset rack and recordings all flow through it) and pushes
//! `Arc<Vec<Arc<[f32]>>>` snapshots (indexed by asset id) into the audio
//! engine — the realtime path only ever reads those Arcs.
//!
//! The rack scans *rack folders* (the first entry is the primary folder that
//! recordings are written to). Scanning is cheap — it only lists candidate
//! files; decoding happens once per file the first time it is needed and the
//! data is then cached in the asset list.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::audio_io::IMPORT_EXTENSIONS;
use crate::automation::Automation;
use crate::midipat::Pattern;

/// One decoded audio asset. `path` is where it is stored (or will be stored:
/// a fresh recording with no folder yet has an empty path and `dirty = true`).
pub struct Asset {
    pub id: u32,
    /// Display name (file stem).
    pub name: String,
    pub path: PathBuf,
    pub data: Arc<[f32]>,
    /// In-memory content no longer matches `path` (trim/derive): the next
    /// save writes it out before anything references the old file.
    pub dirty: bool,
    /// MIDI patterns live here (`data` stays empty); audio assets are None.
    pub pattern: Option<Arc<Pattern>>,
    /// Automation curves live here (v0.11); audio/MIDI assets are None.
    pub automation: Option<Arc<Automation>>,
}

impl Asset {
    pub fn samples(&self) -> u64 {
        self.data.len() as u64
    }

    /// Mono audio asset bound to a file (fresh imports / recordings).
    pub fn audio(id: u32, name: String, path: PathBuf, data: Arc<[f32]>, dirty: bool) -> Asset {
        Asset {
            id,
            name,
            path,
            data,
            dirty,
            pattern: None,
            automation: None,
        }
    }

    /// MIDI pattern asset (imported .mid, MIDI recording, piano roll).
    pub fn midi(id: u32, name: String, path: PathBuf, pattern: Arc<Pattern>, dirty: bool) -> Asset {
        Asset {
            id,
            name,
            path,
            data: Arc::from(Vec::new().into_boxed_slice()),
            dirty,
            pattern: Some(pattern),
            automation: None,
        }
    }

    /// Automation asset (v0.11): a curve + target, stored as `.slauto` JSON.
    pub fn automation_asset(
        id: u32,
        name: String,
        path: PathBuf,
        curve: Arc<Automation>,
        dirty: bool,
    ) -> Asset {
        Asset {
            id,
            name,
            path,
            data: Arc::from(Vec::new().into_boxed_slice()),
            dirty,
            pattern: None,
            automation: Some(curve),
        }
    }

    pub fn is_midi(&self) -> bool {
        self.pattern.is_some()
    }

    pub fn is_auto(&self) -> bool {
        self.automation.is_some()
    }
}

/// A decoded sample buffer shared between GUI and engine.
pub type AssetData = Arc<[f32]>;

/// One file offered by a rack folder.
#[derive(Clone, Debug, PartialEq)]
pub struct RackFile {
    pub name: String,
    pub path: PathBuf,
    /// v28: sub-folder relative to the rack folder root, "/"-separated
    /// (e.g. `"pearl-rock/kick"`). `""` = directly in the rack folder.
    pub dir: String,
    /// True for .mid/.midi entries (the rack's MIDI section).
    pub is_midi: bool,
    /// True for `.slauto` automation assets (v0.11).
    pub is_auto: bool,
}

/// True when a file's extension is a format the import pipeline decodes.
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMPORT_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// True when a file is a slooper automation asset (`.slauto` JSON).
pub fn is_auto_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("slauto"))
}

/// True when a file is a Standard MIDI File the pattern decoder accepts.
pub fn is_midi_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mid") || e.eq_ignore_ascii_case("midi"))
}

/// Max sub-folder nesting `scan_folder` descends into. Deep trees are
/// almost always app internals (`foo.vst3/Contents/...`-style layouts) or
/// archiving junk; four levels already covers pack/kit/voice/variant.
const MAX_DEPTH: usize = 4;

/// List the audio/MIDI/automation files in `dir` INCLUDING sub-folders
/// (v28 recursive scan), sorted by sub-folder then name, case-insensitive;
/// root-level files come first. Hidden entries (dot-prefix) are skipped.
/// A missing/unreadable `dir` is an error the rack shows inline instead of
/// silently disappearing — the folder may simply not exist right now.
/// Unreadable SUB-folders are skipped quietly (one broken branch must not
/// blank the whole folder's list).
pub fn scan_folder(dir: &Path) -> Result<Vec<RackFile>, String> {
    let mut files: Vec<RackFile> = Vec::new();
    scan_into(dir, "", 0, &mut files)?;
    files.sort_by_key(|f| (f.dir.to_lowercase(), f.name.to_lowercase()));
    Ok(files)
}

fn scan_into(dir: &Path, rel: &str, depth: usize, out: &mut Vec<RackFile>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in rd.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if fname.starts_with('.') {
            continue; // hidden files and folders never clutter a rack
        }
        let p = entry.path();
        if p.is_dir() {
            if depth < MAX_DEPTH {
                let sub = if rel.is_empty() {
                    fname.clone()
                } else {
                    format!("{rel}/{fname}")
                };
                let _ = scan_into(&p, &sub, depth + 1, out);
            }
            continue;
        }
        let is_midi = p.is_file() && is_midi_file(&p);
        let is_auto = p.is_file() && is_auto_file(&p);
        if p.is_file() && (is_audio_file(&p) || is_midi || is_auto) {
            let name = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(RackFile {
                name,
                path: p,
                dir: rel.to_string(),
                is_midi,
                is_auto,
            });
        }
    }
    Ok(())
}

/// A rack folder entry: the path plus its last scan result.
pub struct RackFolder {
    pub path: String,
    pub files: Vec<RackFile>,
    /// Populated when the folder could not be read.
    pub err: Option<String>,
}

impl RackFolder {
    pub fn new(path: impl Into<String>) -> Self {
        RackFolder {
            path: path.into(),
            files: Vec::new(),
            err: None,
        }
    }

    /// v29: sub-folders that exist in this scan, in DISPLAY order: the ones
    /// in `pref` first (in `pref` order, ignoring vanished dirs), then any
    /// new one alphabetically. Does NOT include the root (dir "").
    pub fn dirs_in_order(&self, pref: &[String]) -> Vec<String> {
        let mut present: std::collections::BTreeSet<&str> =
            self.files.iter().map(|f| f.dir.as_str()).filter(|d| !d.is_empty()).collect();
        let mut out: Vec<String> = Vec::new();
        for p in pref {
            if present.remove(p.as_str()) {
                out.push(p.clone());
            }
        }
        out.extend(present.into_iter().map(|d| d.to_string()));
        out
    }

    /// Re-scan this folder in place.
    pub fn rescan(&mut self) {
        let p = self.path.trim();
        if p.is_empty() {
            self.files.clear();
            self.err = None;
            return;
        }
        match scan_folder(Path::new(p)) {
            Ok(f) => {
                self.files = f;
                self.err = None;
            }
            Err(e) => {
                self.files.clear();
                self.err = Some(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rack_entry_construction_roundtrip() {
        let rf = RackFile {
            name: "a".into(),
            path: PathBuf::from("/x/a.mid"),
            dir: String::new(),
            is_midi: true,
            is_auto: false,
        };
        assert!(rf.is_midi);
    }

    #[test]
    fn extension_filter() {
        assert!(is_audio_file(Path::new("/x/y.WAV")));
        assert!(is_audio_file(Path::new("/x/y.flac")));
        assert!(!is_audio_file(Path::new("/x/y.rs")));
        assert!(!is_audio_file(Path::new("/x/noext")));
        assert!(is_midi_file(Path::new("/x/p.MID")));
        assert!(is_midi_file(Path::new("/x/p.midi")));
        assert!(!is_midi_file(Path::new("/x/p.wav")));
    }

    #[test]
    fn scan_sorts_filters_and_errors() {
        let dir = std::env::temp_dir().join(format!("sp-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["beta.wav", "Alpha.mp3", "notes.txt", "sub"] {
            let p = dir.join(name);
            if name == "sub" {
                std::fs::create_dir(&p).unwrap();
            } else {
                std::fs::write(&p, b"x").unwrap();
            }
        }
        let files = scan_folder(&dir).unwrap();
        assert_eq!(
            files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["Alpha", "beta"],
            "only audio files, sorted case-insensitively"
        );
        // A missing folder is an inline error, not a panic.
        let missing = dir.join("nope");
        assert!(scan_folder(&missing).is_err());
        let mut rf = RackFolder::new(dir.join("nope").display().to_string());
        rf.rescan();
        assert!(rf.err.is_some());
        rf.path = dir.display().to_string();
        rf.rescan();
        assert!(rf.err.is_none() && rf.files.len() == 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v28 R1/A1: the rack scan descends into sub-folders — a pack tree
    /// like `kit/kick/hit.wav` must surface, carry its relative dir for
    /// group rendering, sort dir-then-name with root files first, skip
    /// hidden folders and files, and stop at the depth limit.
    #[test]
    fn v28_scan_recurses_subfolders() {
        let dir = std::env::temp_dir().join(format!("sp-assets-v28-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mk = |rel: &str| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        };
        mk("top.wav");
        mk("kit/alpha.wav");
        mk("kit/snare/beta.wav");
        mk("kit/notes.mid");
        mk("kit/readme.txt"); // not an importable type
        mk(".hidden/sneaky.wav");
        mk("top/.dotfile.wav");
        mk("a/b/c/d/maxok.wav"); // 4 path segments == MAX_DEPTH, scanned
        mk("a/b/c/d/e/deep.wav"); // 5 segments > MAX_DEPTH, not scanned
        mk("a/b/c/ok.wav");

        let files = scan_folder(&dir).unwrap();
        let got: Vec<(String, String)> = files
            .iter()
            .map(|f| (f.dir.clone(), f.name.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("".into(), "top".into()),
                ("a/b/c".into(), "ok".into()),
                ("a/b/c/d".into(), "maxok".into()),
                ("kit".into(), "alpha".into()),
                ("kit".into(), "notes".into()),
                ("kit/snare".into(), "beta".into()),
            ],
            "recursive, dir-then-name sorted, root first, hidden + junk skipped, depth-bounded"
        );
        // Kind flags survive the recursion (the MIDI section needs them).
        let mid = files.iter().find(|f| f.name == "notes").unwrap();
        assert!(mid.is_midi && !mid.is_auto && mid.dir == "kit");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
