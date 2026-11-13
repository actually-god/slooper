//! # paths — every base directory slooper touches, in ONE place (W2, Windows port)
//!
//! Before this module, XDG/HOME resolution was duplicated (with slightly
//! different edge-case rules!) across config.rs, fx.rs, synth.rs and
//! samples808.rs. The Windows port needs one seam to hang `%LOCALAPPDATA%` /
//! `%APPDATA%` on, so every call site now asks this module.
//!
//! Resolution order (identical on every OS, differing only in which env
//! vars feed it):
//!
//! 1. `SLOOPER_DATA_DIR` / `SLOOPER_CONFIG_DIR` — full-path override
//!    (test/CI convenience; replaces the whole app dir, wins over XDG).
//! 2. XDG vars (`XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`) on
//!    Unix; on Windows `%LOCALAPPDATA%` / `%APPDATA%`.
//! 3. Home-dir fallback (`~/.local/share`, `~/.config`, `~/.cache`;
//!    Windows: `%USERPROFILE%\AppData\{Local,Roaming}`).
//!
//! An env var set to the empty string counts as UNSET (XDG spec: invalid or
//! unset must be treated the same; the old duplicated sites disagreed about
//! this — the unified rule is the spec-correct one).
//!
//! Linux return values are byte-identical to the old per-file code:
//! `config_dir()`/`data_dir()` INCLUDE the `slooper` folder (call sites used
//! to `.join("slooper")` themselves), `cache_home()` does NOT (the synth
//! script cache lives in `<cache>/slooper-synth`, a name that predates this
//! module and stays).
//!
//! Windows: `config_dir()` = `%APPDATA%\slooper`, `data_dir()` =
//! `%LOCALAPPDATA%\slooper`. On Unix the "app data" and "config" homes
//! differ; Windows app data has no such split, and keeping config in
//! `%APPDATA%` (the roaming one) matches how most Windows DAWs ship.
//!
//! **No caching.** Every call re-reads the environment — the headless test
//! suite swaps `HOME`/`XDG_*` between tests (see `test_app`), so memoizing
//! here would silently freeze every sandboxed test onto the first value.

use std::path::{Path, PathBuf};

/// Env var: full replacement for [`data_dir`] (wins over XDG).
pub const DATA_DIR_ENV: &str = "SLOOPER_DATA_DIR";
/// Env var: full replacement for [`config_dir`] (wins over XDG).
pub const CONFIG_DIR_ENV: &str = "SLOOPER_CONFIG_DIR";

/// A set, non-empty environment variable as a path. Empty means unset.
fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var(var)
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// The user's home folder: `HOME` on Unix; `USERPROFILE` (then `HOME`) on
/// Windows; `.` when nothing is set (matches the pre-W2 fallbacks —
/// relative paths are preferable to panicking in a headless test).
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        env_path("USERPROFILE")
            .or_else(|| env_path("HOME"))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(not(windows))]
    {
        env_path("HOME").unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Unix: `$XDG_CONFIG_HOME` or `~/.config`, un-joined home of the fallback
/// chain only when needed. Windows: `%APPDATA%` or
/// `%USERPROFILE%\AppData\Roaming`.
fn config_home() -> PathBuf {
    #[cfg(windows)]
    {
        env_path("APPDATA").unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
    }
    #[cfg(not(windows))]
    {
        env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home_dir().join(".config"))
    }
}

/// Unix: `$XDG_DATA_HOME` or `~/.local/share`. Windows: `%LOCALAPPDATA%`
/// or `%USERPROFILE%\AppData\Local`.
fn data_home() -> PathBuf {
    #[cfg(windows)]
    {
        env_path("LOCALAPPDATA").unwrap_or_else(|| home_dir().join("AppData").join("Local"))
    }
    #[cfg(not(windows))]
    {
        env_path("XDG_DATA_HOME").unwrap_or_else(|| home_dir().join(".local").join("share"))
    }
}

/// Unix: `$XDG_CACHE_HOME` or `~/.cache`. Windows: `%LOCALAPPDATA%` (no
/// roaming cache). Returned WITHOUT the app folder — the one in-tree
/// consumer joins its own pre-W2 folder name (`slooper-synth`).
pub fn cache_home() -> PathBuf {
    #[cfg(windows)]
    {
        env_path("LOCALAPPDATA").unwrap_or_else(|| home_dir().join("AppData").join("Local"))
    }
    #[cfg(not(windows))]
    {
        env_path("XDG_CACHE_HOME").unwrap_or_else(|| home_dir().join(".cache"))
    }
}

/// Where slooper keeps its per-user data: `config.json` lives under
/// [`config_dir`], effects/synths/samples/impulses under this one.
///
/// Linux: `$XDG_DATA_HOME/slooper` (default `~/.local/share/slooper`).
/// Windows: `%LOCALAPPDATA%\slooper`.
pub fn data_dir() -> PathBuf {
    env_path(DATA_DIR_ENV).unwrap_or_else(|| data_home().join("slooper"))
}

/// Config home + `slooper`.
///
/// Linux: `$XDG_CONFIG_HOME/slooper` (default `~/.config/slooper`).
/// Windows: `%APPDATA%\slooper`.
pub fn config_dir() -> PathBuf {
    env_path(CONFIG_DIR_ENV).unwrap_or_else(|| config_home().join("slooper"))
}

/// `<data_dir>/impulses` — the convolver IR drop-folder (the bundled
/// convolver effect script re-implements this resolution standalone; keep
/// the two in sync — see `effects/convolver.rs`).
pub fn impulses_dir() -> PathBuf {
    data_dir().join("impulses")
}

/// `<home>/slooper-projects` — the default projects folder when
/// `Config.project_dir` is unset.
pub fn default_projects_dir() -> PathBuf {
    home_dir().join("slooper-projects")
}

/// Suffix (no dot) the platform gives a loadable shared object: `so` on
/// Unix, `dll` on Windows. Used by the two runtime-compile loaders
/// (`fx.rs`, `synth.rs`) so no call site hardcodes `.so` again — this is
/// the W3 step of the Windows port. The Linux value is unchanged.
pub fn dylib_ext() -> &'static str {
    if cfg!(windows) {
        "dll"
    } else {
        "so"
    }
}

/// Does this path string carry the *other* OS's absolute-path syntax?
/// (WINDOWS-PORT W12d — config round-trip.)
///
/// A config file carried between OSes holds absolute paths the receiving OS
/// can never host — and they are NOT syntactically invalid there: Windows
/// `C:\Users\x\rack` is a perfectly legal *relative* directory name on
/// Linux, and Linux `/home/x/rack` is a legal drive-rooted path on Windows.
/// Consumers that create-or-seed folders ([`crate::fx::prepare_effects_dir`],
/// project save, rack recording) would happily WRITE into such junk trees,
/// so `Config::load` drops these shapes and per-OS defaults come back.
///
/// Rules (checked on the trimmed string):
/// * Windows: foreign ⟺ starts with `/` (Unix-root syntax). Native shapes
///   stay: drive paths `X:\`/`X:/`, UNC `\\server\share` (backslashes!),
///   and relative paths of any kind.
/// * Unix: foreign ⟺ drive-letter root — `X:` alone or `X:` immediately
///   followed by `/` or `\` — or the UNC prefix `\\`. Backslashes inside
///   an otherwise relative name are legal POSIX filename characters and
///   stay untouched; drive-relative `C:foo` is deliberately NOT matched
///   (no picker ever writes it, and on Linux it is only a weird relative
///   name — harmless if it survives).
pub fn path_is_foreign(s: &str) -> bool {
    let s = s.trim();
    if cfg!(windows) {
        s.starts_with('/')
    } else {
        if s.starts_with("\\\\") {
            return true;
        }
        let b = s.as_bytes();
        if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
            return false;
        }
        // Drive-letter root: `X:` alone, or `X:` + separator.
        b.len() == 2 || b[2] == b'/' || b[2] == b'\\'
    }
}

/// A file next to `artifact` carrying `tag`:
/// `kick.so` → `kick.so.tmp` / `kick.so.failed`. Both compile loaders use
/// this for their staging and poison-marker files so those names follow
/// the platform suffix too. Linux names are unchanged.
pub fn build_sidecar(artifact: &Path, tag: &str) -> PathBuf {
    artifact.with_extension(format!("{}.{}", dylib_ext(), tag))
}

/// Env var: full replacement for [`crash_dir`] (W12e). Headless harnesses
/// and the functional pins set this so a crash never writes the real user
/// folder — the W12d `%APPDATA%` bleed rule, extended to crash artifacts.
pub const CRASH_DIR_ENV: &str = "SLOOPER_CRASH_DIR";

/// Where fatal-crash artifacts (minidumps + text notes) are written.
/// `SLOOPER_CRASH_DIR` override; else `<data_dir>/crashes` — i.e.
/// `%LOCALAPPDATA%\slooper\crashes` on Windows, `~/.local/share/slooper/crashes`
/// on Linux (where nothing writes them: the handler is Windows-only and
/// Linux keeps coredumpctl forensics — see `crash.rs`).
///
/// NOT created by resolution — only the writers create it, so merely
/// calling this has no side effect on any OS.
pub fn crash_dir() -> PathBuf {
    env_path(CRASH_DIR_ENV).unwrap_or_else(|| data_dir().join("crashes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvSandbox {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvSandbox {
        /// Sandboxes exactly these vars, pointing HOME/XDG at clearly
        /// distinct temp roots so precedence mistakes cannot pass.
        fn new(extra: &[&'static str]) -> Self {
            let mut keys: Vec<&'static str> = vec![
                "HOME",
                "USERPROFILE",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                DATA_DIR_ENV,
                CONFIG_DIR_ENV,
            ];
            keys.extend_from_slice(extra);
            let mut saved = Vec::new();
            for k in keys {
                saved.push((k, std::env::var(k).ok()));
            }
            // Wipe everything we own, then set a fake home.
            for (k, _) in &saved {
                std::env::remove_var(k);
            }
            let fake = std::env::temp_dir().join(format!("w2-paths-{}", std::process::id()));
            std::env::set_var("HOME", fake.join("home"));
            std::env::set_var("USERPROFILE", fake.join("userprofile"));
            Self { saved }
        }
    }

    impl Drop for EnvSandbox {
        fn drop(&mut self) {
            for (k, v) in self.saved.iter() {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[cfg(unix)] // Linux XDG/HOME folder semantics (Windows twin: windows_folder_shapes)
    #[test]
    fn w2_paths_xdg_beats_home_fallback() {
        let _sb = EnvSandbox::new(&["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"]);
        std::env::set_var("XDG_CONFIG_HOME", "/xdg/cfg");
        std::env::set_var("XDG_DATA_HOME", "/xdg/data");
        std::env::set_var("XDG_CACHE_HOME", "/xdg/cache");
        assert_eq!(config_dir(), PathBuf::from("/xdg/cfg/slooper"));
        assert_eq!(data_dir(), PathBuf::from("/xdg/data/slooper"));
        assert_eq!(cache_home(), PathBuf::from("/xdg/cache"));
        assert_eq!(impulses_dir(), PathBuf::from("/xdg/data/slooper/impulses"));
    }

    #[cfg(unix)]
    #[test]
    fn w2_paths_home_fallback_when_xdg_unset() {
        let _sb = EnvSandbox::new(&[]);
        std::env::set_var("HOME", "/home/zoinks");
        assert_eq!(config_dir(), PathBuf::from("/home/zoinks/.config/slooper"));
        assert_eq!(
            data_dir(),
            PathBuf::from("/home/zoinks/.local/share/slooper")
        );
        assert_eq!(cache_home(), PathBuf::from("/home/zoinks/.cache"));
        assert_eq!(
            default_projects_dir(),
            PathBuf::from("/home/zoinks/slooper-projects")
        );
    }

    #[cfg(unix)] // Linux XDG/HOME folder semantics (Windows twin: windows_folder_shapes)
    #[test]
    fn w2_paths_empty_env_vars_count_as_unset() {
        let _sb = EnvSandbox::new(&[]);
        std::env::set_var("HOME", "/home/zoinks");
        std::env::set_var("XDG_CONFIG_HOME", "");
        std::env::set_var("XDG_DATA_HOME", "");
        std::env::set_var(DATA_DIR_ENV, "");
        std::env::set_var(CONFIG_DIR_ENV, "");
        assert_eq!(config_dir(), PathBuf::from("/home/zoinks/.config/slooper"));
        assert_eq!(
            data_dir(),
            PathBuf::from("/home/zoinks/.local/share/slooper")
        );
    }

    #[test]
    fn w2_paths_slooper_overrides_beat_xdg() {
        let _sb = EnvSandbox::new(&["XDG_DATA_HOME", "XDG_CONFIG_HOME"]);
        std::env::set_var("XDG_DATA_HOME", "/xdg/data");
        std::env::set_var("XDG_CONFIG_HOME", "/xdg/cfg");
        std::env::set_var(DATA_DIR_ENV, "/sandbox/data");
        std::env::set_var(CONFIG_DIR_ENV, "/sandbox/cfg");
        // The override REPLACES the whole app dir — no joined "slooper".
        assert_eq!(data_dir(), PathBuf::from("/sandbox/data"));
        assert_eq!(config_dir(), PathBuf::from("/sandbox/cfg"));
        assert_eq!(impulses_dir(), PathBuf::from("/sandbox/data/impulses"));
    }

    #[cfg(unix)] // Linux XDG/HOME folder semantics (Windows twin: windows_folder_shapes)
    #[test]
    fn w2_paths_resolution_is_live_not_memoized() {
        // The test suite swaps HOME/XDG mid-suite (test_app); a memoized
        // base dir would freeze every sandboxed test onto the first value.
        let _sb = EnvSandbox::new(&["XDG_DATA_HOME"]);
        std::env::set_var("XDG_DATA_HOME", "/first/data");
        let a = data_dir();
        std::env::set_var("XDG_DATA_HOME", "/second/data");
        let b = data_dir();
        assert_eq!(a, PathBuf::from("/first/data/slooper"));
        assert_eq!(b, PathBuf::from("/second/data/slooper"));
        assert_ne!(a, b);
    }

    #[cfg(unix)] // Linux XDG/HOME folder semantics (Windows twin: windows_folder_shapes)
    #[test]
    fn w2_paths_config_json_lives_in_config_dir() {
        let _sb = EnvSandbox::new(&["XDG_CONFIG_HOME"]);
        std::env::set_var("XDG_CONFIG_HOME", "/xdg/cfg");
        assert_eq!(
            crate::config::Config::path(),
            PathBuf::from("/xdg/cfg/slooper/config.json")
        );
    }

    #[cfg(unix)] // Linux XDG/HOME folder semantics (Windows twin: windows_folder_shapes)
    #[test]
    fn w2_paths_effect_and_synth_dirs_follow_data_dir() {
        let _sb = EnvSandbox::new(&["XDG_DATA_HOME", "SLOOPER_SYNTHS_DIR"]);
        std::env::remove_var("SLOOPER_SYNTHS_DIR"); // the seam must NOT win here
                                                    // Temp root (not /xdg): the effects fallback best-effort-CREATES.
        let root = std::env::temp_dir().join(format!("w2-eff-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &root);
        assert_eq!(
            crate::synth::default_dir(),
            root.join("slooper").join("synths")
        );
        assert_eq!(
            crate::fx::default_effects_fallback_dir(),
            root.join("slooper").join("effects")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn w2_paths_samples_data_dir_is_the_shared_one() {
        let _sb = EnvSandbox::new(&["XDG_DATA_HOME"]);
        std::env::set_var("XDG_DATA_HOME", "/xdg/data");
        assert_eq!(crate::samples808::data_dir(), data_dir());
    }

    #[cfg(windows)]
    #[test]
    fn w2_paths_windows_folder_shapes() {
        let _sb = EnvSandbox::new(&["APPDATA", "LOCALAPPDATA"]);
        std::env::set_var("APPDATA", "C:\\Users\\u\\AppData\\Roaming");
        std::env::set_var("LOCALAPPDATA", "C:\\Users\\u\\AppData\\Local");
        assert_eq!(
            config_dir(),
            PathBuf::from("C:\\Users\\u\\AppData\\Roaming").join("slooper")
        );
        assert_eq!(
            data_dir(),
            PathBuf::from("C:\\Users\\u\\AppData\\Local").join("slooper")
        );
        assert_eq!(cache_home(), PathBuf::from("C:\\Users\\u\\AppData\\Local"));
    }

    #[test]
    fn w3_dylib_ext_matches_target() {
        // Each OS asserts its own truth (W3): the runtime-compile loaders
        // build and load `so` on Unix, `dll` on Windows.
        if cfg!(windows) {
            assert_eq!(dylib_ext(), "dll");
        } else {
            assert_eq!(dylib_ext(), "so");
        }
    }

    #[test]
    fn w3_build_sidecar_follows_target_ext() {
        let art = PathBuf::from("/x").join(format!("kick-abcd.{}", dylib_ext()));
        let p = build_sidecar(&art, "tmp");
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            format!("kick-abcd.{}.tmp", dylib_ext())
        );
        let p = build_sidecar(&art, "failed");
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            format!("kick-abcd.{}.failed", dylib_ext())
        );
    }

    // ---- W11: Windows release packaging (icon, subsystem, assets) ----

    /// Repo file as ONE string with CRLF normalized — Windows runners were
    /// proven (W10) to differ in line endings, and source pins must read
    /// identical on both OSes regardless of `.gitattributes` drift.
    fn w11_repo_file(rel: &str) -> String {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&p)
            .unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
            .replace("\r\n", "\n")
    }

    #[test]
    fn w11_icon_asset_is_a_multisize_ico() {
        // The .ico shipped for the winresource embed: ICONDIR header with
        // type=1, at least one real image per size class, and the 256-px
        // entry (directory width byte 0 means 256) so Explorer/Taskbar get
        // a crisp large icon. 16-px entries are width byte 16.
        let raw = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/icon.ico"))
            .expect("assets/icon.ico must exist (built from assets/icon.png)");
        assert!(raw.len() > 6 + 16, "ico too short");
        assert_eq!((&raw[0..4]), &[0, 0, 1, 0], "not an ICO (magic/type)");
        let count = u16::from_le_bytes([raw[4], raw[5]]) as usize;
        assert!(count >= 4, "ico has {count} images, want >= 4 sizes");
        let mut sizes: Vec<u32> = Vec::new();
        for i in 0..count {
            let e = 6 + i * 16;
            let w = if raw[e] == 0 { 256 } else { raw[e] as u32 };
            let h = if raw[e + 1] == 0 {
                256
            } else {
                raw[e + 1] as u32
            };
            assert_eq!(w, h, "ico entry {i} not square ({w}x{h})");
            assert!(w >= 16, "ico entry {w}px below 16");
            sizes.push(w);
        }
        assert!(sizes.contains(&256), "ico lacks the 256px entry: {sizes:?}");
        assert!(sizes.contains(&16), "ico lacks the 16px entry: {sizes:?}");
    }

    #[test]
    fn w11_build_rs_embeds_icon_for_target_windows_only() {
        let src = w11_repo_file("build.rs");
        // Gate is the TARGET OS env (a build script's cfg! is the HOST —
        // W11 trap), the icon is winresource's, and the ffiguard skip stays.
        assert!(
            src.contains(r#"CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")"#),
            "icon embed must gate on CARGO_CFG_TARGET_OS, not cfg!"
        );
        assert!(src.contains("winresource::WindowsResource::new"));
        assert!(src.contains(r#""assets/icon.ico""#));
        assert!(
            src.contains("catch_unwind"),
            "toolchain probing must stay best-effort"
        );
        assert!(src.contains("cargo:rerun-if-changed=assets/icon.ico"));
    }

    #[test]
    fn w11_main_rs_is_gui_subsystem_on_windows_only() {
        let src = w11_repo_file("src/main.rs");
        assert!(
            src.contains(r#"#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]"#),
            "main.rs must drop the console window ONLY on Windows targets"
        );
    }

    #[test]
    fn w11_release_workflow_ships_both_assets() {
        let wf = w11_repo_file(".github/workflows/release.yml");
        assert!(
            wf.contains("slooper-win64.zip"),
            "Windows asset name is contractual"
        );
        assert!(
            wf.contains("--zstd"),
            "Linux asset must be tar.zst (ledger contract)"
        );
        assert!(
            wf.contains("windows-latest"),
            "Windows ships from a native MSVC host (W10-proven)"
        );
        assert!(
            wf.contains("cargo build --release --locked"),
            "assets ride the shipping profile"
        );
        // Dispatch runs publish a DRAFT so the pipeline can be proven
        // without shipping anything.
        assert!(wf.contains(r#"draft: ${{ github.event_name == 'workflow_dispatch' }}"#));
    }
}
