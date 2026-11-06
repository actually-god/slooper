//! slooper — a minimalist clip-based audio DAW for Linux (JACK / PipeWire).
//!
//! Sixteen tracks on a global, draggable playhead with a global loop region
//! (area / current longest / whole song). Asset-rack drag & drop onto a
//! metronome-defined bar grid, a right-docked Mixer aligned per track,
//! clip copy/paste, additive child lanes, takes that record into rack assets
//! and auto-placed clips, metronome-anchored quantize/trim, a bundled TR-808
//! sample pack, `.slooper` projects, per-track effects, MIDI action
//! mapping plus keyboard shortcuts, live mix passthrough and a MIDI monitor.

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
        eprintln!("[slooper] panic: {info}");
        std::process::abort();
    }));
    // v23: editor crash-guard child (see `vstedit::probe_child`). This
    // process only ever tests ONE plugin window and exits — no JACK, no
    // eframe, no config, nothing that could be confused for a second DAW.
    if let Some(spec) = std::env::var_os("SLOOPER_EDITOR_PROBE") {
        std::process::exit(slooper::vstedit::probe_child(
            spec.to_string_lossy().as_ref(),
        ));
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

    #[test]
    fn icon_decodes() {
        let icon = load_icon().expect("bundled icon must decode");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }
}
