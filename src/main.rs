//! slooper — a minimalist clip-based audio DAW for Linux (JACK / PipeWire).
//!
//! Sixteen tracks on a global, draggable playhead with a global loop region
//! (area / current longest / whole song). Asset-rack drag & drop onto a
//! metronome-defined bar grid, a right-docked Mixer aligned per track,
//! clip copy/paste, additive child lanes, takes that record into rack assets
//! and auto-placed clips, metronome-anchored quantize/trim, a bundled TR-808
//! sample pack, `.slooper` projects, per-track effects, MIDI action
//! mapping plus keyboard shortcuts, live mix passthrough and a MIDI monitor.

// W11 release polish: the shipped Windows exe is a GUI app — no console
// window on double-click (docs/INSTALL.md Windows smoke item 1). Panics
// still abort() after printing (visible under a debugger / an attached
// console); the W7 probe children stay capturable because the harness
// always pipes their stdout. Linux behavior unchanged (attribute is a
// no-op for non-Windows targets).
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/// Bundled application icon (placeholder for now — see assets/icon.png).
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
fn main() -> eframe::Result {
    // v42: release now compiles panic = "unwind" because the plugin FFI
    // firewall (src/ffiguard.rs) must be able to UNWIND C++ exceptions to
    // its catch. To keep the shipped failure mode exactly as loud as the
    // old panic = "abort", every app-level Rust panic aborts right after
    // printing (core-dump forensics unchanged: `strings core | grep
    // panicked`). C++ exceptions are not panics — they keep unwinding to
    // the guard.
    std::panic::set_hook(Box::new(|info| {
        // W12e: a console-less GUI-subsystem Windows exe loses eprintln to
        // the void — note the panic (text + backtrace) where the user can
        // find it BEFORE the v42 abort. Structurally inert on Linux.
        #[cfg(windows)]
        slooper::crash::note_panic(info);
        eprintln!("[slooper] panic: {info}");
        std::process::abort();
    }));
    // v45 GL policy (crash forensics attempt #4): run the WHOLE process on
    // software GL by default. Rationale in `gl_force_software`; the kill
    // chain we are removing: kernel rejects one Mesa command stream over a
    // VA-map conflict → Mesa's cs0 queue thread abort()s (non-robust
    // context) → DAW dead; when it does NOT abort, the UI thread hangs
    // forever in dri_flush instead — the same fault as a "freeze". With
    // llvmpipe there is no kernel command stream to reject: the entire
    // fault class becomes unrepresentable, editors and probe children
    // included (they inherit the environment).
    if gl_force_software(|k| std::env::var_os(k)) {
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        eprintln!(
            "[slooper] GL: llvmpipe (v45 — Mesa 26.2 CS-reject aborts kill \
             co-located DAWs; SLOOPER_GPU_GL=1 opts back onto GPU GL)"
        );
    } else if std::env::var_os("SLOOPER_GPU_GL").is_some() {
        std::env::remove_var("LIBGL_ALWAYS_SOFTWARE");
        eprintln!("[slooper] GL: GPU path per SLOOPER_GPU_GL (known abort risk)");
    }
    // v23: editor crash-guard child (see `vstedit::probe_child`). This
    // process only ever tests ONE plugin window and exits — no JACK, no
    // eframe, no config, nothing that could be confused for a second DAW.
    if let Some(spec) = std::env::var_os("SLOOPER_EDITOR_PROBE") {
        std::process::exit(slooper::vstedit::probe_child(
            spec.to_string_lossy().as_ref(),
        ));
    }
    // W7: plugin-scan probe child (Windows crash isolation; see
    // `vsthost::probe_child`). This process loads ONE plugin, prints one
    // JSON line on stdout, and exits — no audio backend, no eframe, no
    // config, no window: a probe can never touch JACK or draw anything,
    // and a hard crash inside the plugin takes only this process down.
    if let Some((kind, path)) = slooper_probe_invocation(&std::env::args_os().collect::<Vec<_>>()) {
        std::process::exit(slooper::vsthost::probe_child_main(kind, &path));
    }
    // v41 windowing policy (crash forensics 2026-11, seven SIGABRTs):
    // the main window used to be forced onto X11/XWayland so plugin
    // editors and the app shared one server. That co-located slooper's
    // OWN GLX/DRI3 context with every plugin editor's GPU traffic:
    // loading editor-heavy plugins (session restore auto-opens the track
    // synth GUI; a fresh mtime triggers a crash-guard probe; the user
    // clicks Edit) tickled a Mesa/amdgpu VA-map conflict storm, the
    // kernel rejected the command stream, and Mesa abort()s every DRI3
    // client it served — starting with the DAW. Running the main window
    // native-Wayland moves slooper's present path to EGL/Wayland, a
    // different driver code path, so an editor-side driver fault can
    // only take down that plugin's own X client, never slooper itself.
    // (Plugin editors remain plain X11 windows via vstedit's own X
    // connection; KWin stacks them over Wayland windows fine — the
    // v10.2 seam report predates KWin 6.) On a pure-X11 session nothing
    // changes; SLOOPER_FORCE_X11=1 opts back into the old whole-app-on-
    // X11 world (all plugin editors included). SLOOPER_FORCE_WAYLAND is
    // kept as the legacy override: it wins over FORCE_X11, as before.
    if x11_world_preferred(|k| std::env::var_os(k)) {
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("WAYLAND_SOCKET");
    }
    // W12e: from here on this IS the main process (the probe children
    // exited above and stay handler-free on purpose) — arm the Windows
    // fatal-fault story (minidump + note + box; Linux: inert no-op,
    // coredumpctl territory). See src/crash.rs.
    slooper::crash::install();
    // Start the JACK/PipeWire-JACK backend thread (NO_START: never auto-starts a server).
    let backend = slooper::audio::Backend::start();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1080.0, 640.0])
        .with_min_inner_size([780.0, 480.0]);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "slooper",
        options,
        Box::new(move |_cc| Ok(Box::new(slooper::ui::App::new(backend)))),
    )
}

/// v41: main-window backend policy (see `main` for the why). True =
/// strip the Wayland session vars so winit takes the X11 backend;
/// false = keep native Wayland for the main window.
fn x11_world_preferred(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    let wayland_session = env("WAYLAND_DISPLAY").is_some() || env("WAYLAND_SOCKET").is_some();
    let force_x11 = env("SLOOPER_FORCE_X11").is_some();
    let force_wayland = env("SLOOPER_FORCE_WAYLAND").is_some();
    if force_wayland {
        return false; // legacy override keeps Wayland, winning as before
    }
    force_x11 || !wayland_session
}

/// v45 GL backend policy (crash forensics, attempt #4 — kills the last
/// "no obvious cause" freeze/crash class). Mesa 26.2.x (installed
/// 2026-10-30 here) hard-kills a process whose GL command stream the
/// kernel rejects: `amdgpu_ctx_set_sw_reset_status` ends in `abort()`
/// whenever the context lacks `PIPE_CONTEXT_LOSE_CONTEXT_ON_RESET`, and
/// winit/eframe NEVER request robust/lose-on-reset contexts. One transient
/// amdgpu VA-map conflict — Mesa 26.2.5's own VA manager produced a 1-byte
/// overlap (`va ...03bc7` vs `...03bc8`) during a routine egui repaint on
/// slooper-bin e0ad6dd, kernel `bo ... conflict` line, then Mesa's cs0
/// thread aborted the whole DAW — is fatal. When the same fault does not
/// abort, the UI thread parks forever in `dri_flush`: that is the
/// "freeze" half of the reports. v41 moved the main window to native
/// Wayland to shrink blast radius, but slooper hosts GL-capable plugin
/// editors IN-PROCESS, so any co-located Mesa context can still reach the
/// kernel and abort the process. The durable fix is for the DAW process to
/// own NO kernel command streams at all: `LIBGL_ALWAYS_SOFTWARE=1` makes
/// `eglInitialize` and GLX create llvmpipe screens instead
/// (Mesa honors it on the Wayland path via `Options.ForceSoftware`), and
/// every in-process editor / forked probe / yabridge child inherits that.
/// Priority: `SLOOPER_GPU_GL` (any value) opts back onto GPU GL at the
/// user's own risk and beats everything; an externally-set
/// `LIBGL_ALWAYS_SOFTWARE` value is honored verbatim ("0"/"false" = GPU);
/// otherwise software GL is the default (true).
fn gl_force_software(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    if env("SLOOPER_GPU_GL").is_some() {
        return false;
    }
    match env("LIBGL_ALWAYS_SOFTWARE") {
        Some(v) => {
            let v = v.to_string_lossy().trim().to_ascii_lowercase();
            !(v == "0" || v == "false")
        }
        None => true,
    }
}

/// W7: recognize the hidden probe-child form `slooper --slooper-probe
/// <kind> <path>` (flag at argv[1] ONLY — probes are always spawned in
/// exactly this shape, so a stray flag later in a normal command line
/// can never turn the DAW into a probe child). `kind` is the strict
/// `vsthost::kind_code` vocabulary: 0 = VST2, 1 = VST3, 2 = CLAP.
fn slooper_probe_invocation(
    argv: &[std::ffi::OsString],
) -> Option<(u8, std::path::PathBuf)> {
    if argv.get(1)?.as_os_str() != std::ffi::OsStr::new(slooper::vsthost::PROBE_FLAG) {
        return None;
    }
    let kind = argv.get(2)?.to_str()?.trim().parse::<u8>().ok()?;
    if kind > 2 {
        return None;
    }
    Some((kind, std::path::PathBuf::from(argv.get(3)?)))
}

/// Decode the bundled icon into raw RGBA for the window title bar.
fn load_icon() -> Option<std::sync::Arc<egui::IconData>> {
    let img = image::ImageReader::new(std::io::Cursor::new(ICON_PNG))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .into_rgba8();
    Some(std::sync::Arc::new(egui::IconData {
        rgba: img.as_raw().to_vec(),
        width: img.width(),
        height: img.height(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envof(pairs: &[&str]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let owned: Vec<String> = pairs.iter().map(|k| k.to_string()).collect();
        move |k: &str| {
            owned
                .iter()
                .any(|name| name == k)
                .then(|| std::ffi::OsString::from("1"))
        }
    }

    #[test]
    fn v41_wayland_session_keeps_native_wayland() {
        // The flip: under a KDE/Wayland session the main window must NOT
        // be dragged onto XWayland anymore (crash blast-radius, v41).
        assert!(!x11_world_preferred(envof(&["WAYLAND_DISPLAY", "DISPLAY"])));
        assert!(!x11_world_preferred(envof(&["WAYLAND_SOCKET"])));
    }

    #[test]
    fn v41_force_x11_optes_back_in() {
        assert!(x11_world_preferred(envof(&["WAYLAND_DISPLAY", "SLOOPER_FORCE_X11"])));
        // Legacy override still wins over FORCE_X11, as before.
        assert!(!x11_world_preferred(envof(&[
            "WAYLAND_DISPLAY",
            "SLOOPER_FORCE_X11",
            "SLOOPER_FORCE_WAYLAND"
        ])));
    }

    #[test]
    fn v41_pure_x11_session_unchanged() {
        // No Wayland session vars: X11 as always.
        assert!(x11_world_preferred(envof(&["DISPLAY"])));
    }

    /// Value-aware env fixture (v45 policy needs to distinguish "0" vs "1").
    fn envv(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| std::ffi::OsString::from(v))
        }
    }

    #[test]
    fn v45_software_gl_is_the_default() {
        // A DAW must NEVER own kernel GL command streams by default: one
        // Mesa CS-reject aborts the process (or hangs dri_flush = the
        // "freeze" reports). Bare session and Wayland session both → soft.
        assert!(gl_force_software(envv(&[])));
        assert!(gl_force_software(envv(&[("WAYLAND_DISPLAY", "1"), ("DISPLAY", ":1")])));
    }

    #[test]
    fn v45_gpu_gl_optout_beats_everything() {
        // SLOOPER_GPU_GL is the documented escape hatch and wins even over
        // an explicit external LIBGL_ALWAYS_SOFTWARE=1.
        assert!(!gl_force_software(envv(&[("SLOOPER_GPU_GL", "1")])));
        assert!(!gl_force_software(envv(&[
            ("SLOOPER_GPU_GL", "1"),
            ("LIBGL_ALWAYS_SOFTWARE", "1"),
        ])));
    }

    #[test]
    fn v45_external_softgl_value_honored_verbatim() {
        // A launcher that already picked a side is respected as-is.
        assert!(gl_force_software(envv(&[("LIBGL_ALWAYS_SOFTWARE", "1")])));
        assert!(gl_force_software(envv(&[("LIBGL_ALWAYS_SOFTWARE", "true")])));
        assert!(!gl_force_software(envv(&[("LIBGL_ALWAYS_SOFTWARE", "0")])));
        assert!(!gl_force_software(envv(&[("LIBGL_ALWAYS_SOFTWARE", "false")])));
    }

    #[test]
    fn w7_probe_child_invocation_shape() {
        let argv = |a: &[&str]| -> Vec<std::ffi::OsString> {
            a.iter().map(std::ffi::OsString::from).collect()
        };
        // Normal launches (and slooper's lack of any CLI) must NOT trip.
        assert!(slooper_probe_invocation(&argv(&["slooper"])).is_none());
        assert!(slooper_probe_invocation(&argv(&["slooper", "--help"])).is_none());
        // Flag only counts at argv[1].
        assert!(slooper_probe_invocation(&argv(&[
            "slooper",
            "--foo",
            "--slooper-probe",
            "0",
            "/x"
        ]))
        .is_none());
        // The exact spawn shape, including a path with spaces.
        let hit = slooper_probe_invocation(&argv(&[
            "slooper",
            "--slooper-probe",
            "1",
            "/x y/Plug.vst3",
        ]))
        .expect("child form must parse");
        assert_eq!(hit.0, 1);
        assert_eq!(hit.1.to_string_lossy(), "/x y/Plug.vst3");
        // Malformed forms fall through to the normal (GUI) path instead
        // of half-starting a probe: missing parts, non-numeric kind, and
        // kinds outside the 0..=2 vocabulary.
        assert!(slooper_probe_invocation(&argv(&["slooper", "--slooper-probe", "0"])).is_none());
        assert!(slooper_probe_invocation(&argv(&["slooper", "--slooper-probe", "x", "/p"])).is_none());
        assert!(slooper_probe_invocation(&argv(&["slooper", "--slooper-probe", "3", "/p"])).is_none());
        assert!(slooper_probe_invocation(&argv(&["slooper", "--slooper-probe", "256", "/p"])).is_none());
    }

    #[test]
    fn icon_decodes() {
        let icon = load_icon().expect("bundled icon must decode");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }
}
