//! Hosting plugin custom editor windows — the OS seam (WINDOWS-PORT W8).
//!
//! One task per OS, one twin module, identical public surface:
//!
//! * `x11.rs` (`cfg(unix)`) — the X11 container path that has shipped since
//!   v0.2: an own-connection, initially BARE container window per editor,
//!   `effEditOpen(XID)` / VST3 `attached(X11EmbedWindowID)`, verified paint
//!   (live mapped child OR real pixels over a sentinel background), WM
//!   protocol close, XGetImage probes. This file is the frozen Linux truth:
//!   Windows work never edits it, so Linux behavior is byte-identical by
//!   construction (the WINDOWS-PORT ground rule).
//! * `win32.rs` (`cfg(windows)`) — the W8a twin: the SAME architecture
//!   rebuilt on Win32. Host-created top-level container window (own WNDCLASS,
//!   sentinel background brush), `effEditOpen(HWND)` / VST3
//!   `IPlugView::attached(HWND, kPlatformTypeHWND)`, verified paint (a
//!   visible sized child OR non-sentinel pixels on the container),
//!   `WM_CLOSE` as the close-button protocol, a Win32 message pump run on
//!   the shared vst2-gui thread — never the UI thread.
//!
//! Everything the rest of the tree touches (`EditorWindow`, `open_editor`,
//! `pump_editors`, `probe_child`, the shadow-edit test hooks, …) resolves
//! through the glob re-export below, so no call site knows there are two.
//! The W8 source pins in `seam_tests` keep the twins' surface in lockstep
//! and forbid X symbols from leaking into the Win32 twin (and vice versa).
//!
//! THREADING (both twins): every plugin control-plane call — instantiation
//! included — runs on the ONE shared "vst2-gui" thread (`vsthost::open`);
//! GUI toolkits bind their event loop to the thread that first touches the
//! plugin. Editors are pumped there as state machines, never blocking loops.

#[cfg(unix)]
mod x11;
#[cfg(unix)]
pub use x11::*;

#[cfg(windows)]
mod win32;
#[cfg(windows)]
pub use win32::*;

// ------------------------------------------------------------------ seam
//
// Source-consistency pins for the twin split (slooper-dev standing order:
// every seam carries sabotage-verifiable pins). These run on the PRIMARY
// OS (Linux) today and assert the STRUCTURE both twins must keep; the
// `#[cfg(windows)]` half executes in the W10 Windows CI job.
#[cfg(test)]
mod seam_tests {
    /// Strip `//` comment lines so doc prose cannot satisfy (or poison) a
    /// source-scan pin, and cut the `#[cfg(test)]` tail so the pins' OWN
    /// match-strings can never self-satisfy (the w5/w6/w7 pattern).
    fn code_lines(path: &str) -> String {
        let src = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {path}: {e}"));
        let body = src.split("\n#[cfg(test)]").next().unwrap_or("");
        body.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn twin_path(os: &str) -> String {
        format!(
            "{}/src/vstedit/{}.rs",
            env!("CARGO_MANIFEST_DIR"),
            os
        )
    }

    /// Every `pub`/`pub(crate)` item name the twin exports (top-level fns,
    /// types — not their methods).
    fn surface(code: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for l in code.lines() {
            let t = l.trim_start();
            for pre in ["pub fn ", "pub(crate) fn ", "pub struct ", "pub enum ", "pub type "] {
                if let Some(rest) = t.strip_prefix(pre) {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        out.insert(name);
                    }
                }
            }
        }
        out
    }

    /// The two twins must export the SAME public surface, and it must be
    /// the whole surface the tree calls (this list is the historical
    /// vstedit.rs surface — adding a twin item without the other, or
    /// dropping either, goes red).
    #[test]
    fn w8_twins_export_the_same_surface() {
        let a = surface(&code_lines(&twin_path("x11")));
        let b = surface(&code_lines(&twin_path("win32")));
        let required: std::collections::BTreeSet<String> = [
            "editors_active",
            "enter_probe_mode",
            "set_probe_mode",
            "container_is_viewable",
            "request_wm_delete",
            "edit_rect",
            "EditorWindow",
            "open_editor",
            "pump_editors",
            "open_vst3_editor",
            "inject_shadow_edit",
            "mutate_shadow_model",
            "route_param_edits3",
            "x11_available",
            "probe_child",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        for r in &required {
            assert!(a.contains(r), "x11 twin lost {r}");
            assert!(b.contains(r), "win32 twin lost {r}");
        }
        assert_eq!(a, b, "vstedit twin surfaces drifted apart");
    }

    /// The X11 FFI stays sealed inside the cfg(unix) twin, and the Win32
    /// twin never grows X or libc-fork code. The `from_x11` call (the sole
    /// W1-triaged gnullvm error) must exist ONLY in x11.rs.
    #[test]
    fn w8_x11_ffi_sealed_to_the_unix_twin() {
        let w = code_lines(&twin_path("win32"));
        for forbidden in
            ["XOpenDisplay", "X11", "from_x11", "libc::", "fork(", "XGetImage", "os::unix"]
        {
            assert!(
                !w.contains(forbidden),
                "win32 twin references {forbidden} — X/POSIX leaked across the seam"
            );
        }
        // mod.rs gates each twin by OS, and the x11 twin keeps the Linux
        // machinery it shipped with (split-day markers).
        let mc = code_lines(&format!("{}/src/vstedit/mod.rs", env!("CARGO_MANIFEST_DIR")));
        assert!(mc.contains("#[cfg(unix)]\nmod x11;"), "x11 twin ungated!");
        assert!(mc.contains("#[cfg(windows)]\nmod win32;"), "win32 twin ungated!");
        let x = code_lines(&twin_path("x11"));
        for marker in ["install_x_error_handler", "drain_events", "XGetImage", "from_x11"] {
            assert!(x.contains(marker), "x11 twin lost {marker}");
        }
    }

    /// Each OS asserts its own backend-availability truth. On Linux the
    /// gate is DISPLAY-env-only (never opens a probe display next to the
    /// GUI thread's connections); on Windows the Win32 window station is
    /// simply there.
    #[test]
    fn w8_linux_backend_gate() {
        let want = std::env::var_os("DISPLAY").is_some();
        assert_eq!(crate::vstedit::x11_available(), want);
    }

    #[cfg(windows)]
    #[test]
    fn w8_windows_backend_gate_and_shape() {
        // Win32 is always available; the name stays so shared call sites
        // (ui.rs / vsthost.rs editor gates) remain untouched.
        assert!(crate::vstedit::x11_available());
        // Handle-shape pins on the null HWND: honest refusals, no panics.
        assert_eq!(crate::vstedit::container_is_viewable(0), None);
        assert!(!crate::vstedit::request_wm_delete(0));
        // probe_child with no path env is the "nothing to test" verdict.
        assert_eq!(crate::vstedit::probe_child("V2"), 0);
    }
}
