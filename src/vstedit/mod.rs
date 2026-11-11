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
//!   the shared vst2-gui thread — never the UI thread. W8b adds EMBED mode
//!   (`SLOOPER_WIN_EMBED`): the container is a `WS_CHILD` under the winit
//!   HWND with slooper-drawn chrome; the X11 twin carries the inert seam.
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
            // W8b embed seam (win32 = machinery, x11 = documented inert).
            "embed_requested",
            "set_embed_parent",
            "set_container_rect",
            "main_thread_hwnd",
            // W8b p2-b: hide policy + coordinate-origin seam for the v47
            // dock integration.
            "set_container_visible",
            "embed_parent_origin",
            // W8b p2-c: detach/re-dock toggle (win32 = the SetParent dance,
            // x11 = documented inert; the live window style is the state).
            "detach_to_toplevel",
            "redock_to_embed",
            "container_is_detached",
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
    #[cfg(unix)] // DISPLAY-only gate is the LINUX truth (win32 stub answers always-on)
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

    // ------------------------------------------------------------ W8b embed
    //
    // The embed seam: Windows grows the machinery, Linux stays the frozen
    // documented inert — each OS asserts its own truth (standing rule).

    /// Linux: embedding is structurally impossible — every embed entry
    /// point answers inert, EVEN with the Windows env flag set, so no
    /// stray environment can ever route the shipped X11 path down a
    /// half-implemented embed road.
    #[cfg(unix)]
    #[test]
    fn w8b_x11_embed_api_inert() {
        let had = std::env::var_os("SLOOPER_WIN_EMBED");
        std::env::set_var("SLOOPER_WIN_EMBED", "1");
        assert!(!crate::vstedit::embed_requested(), "Linux must never embed");
        assert_eq!(crate::vstedit::main_thread_hwnd(), None);
        // Inert calls never panic, always refuse.
        crate::vstedit::set_embed_parent(0x1234);
        assert!(!crate::vstedit::set_container_rect(0x1234, 0, 0, 10, 10));
        crate::vstedit::set_embed_parent(0);
        // W8b p2-b additions answer inert too (the sync pass that calls
        // them never runs on Linux, and even if it did: no-ops).
        assert!(!crate::vstedit::set_container_visible(0x1234, false));
        assert_eq!(crate::vstedit::embed_parent_origin(0x1234), (0, 0));
        // W8b p2-c: the detach toggle is equally inert — there is no embed
        // machinery here, so nothing detaches, nothing re-docks, and
        // nothing ever reads as detached.
        assert!(!crate::vstedit::detach_to_toplevel(0x1234));
        assert!(!crate::vstedit::redock_to_embed(0x1234));
        assert!(!crate::vstedit::container_is_detached(0x1234));
        match had {
            Some(v) => std::env::set_var("SLOOPER_WIN_EMBED", v),
            None => std::env::remove_var("SLOOPER_WIN_EMBED"),
        }
    }

    /// Windows: the env gate reads live (empty = unset, the paths rule),
    /// and the null handle still answers honestly.
    #[cfg(windows)]
    #[test]
    fn w8b_windows_embed_gate_reads_env() {
        let had = std::env::var_os("SLOOPER_WIN_EMBED");
        std::env::remove_var("SLOOPER_WIN_EMBED");
        assert!(!crate::vstedit::embed_requested());
        std::env::set_var("SLOOPER_WIN_EMBED", "");
        assert!(!crate::vstedit::embed_requested(), "empty = unset");
        std::env::set_var("SLOOPER_WIN_EMBED", "1");
        assert!(crate::vstedit::embed_requested());
        std::env::set_var("SLOOPER_WIN_EMBED", "0");
        assert!(
            crate::vstedit::embed_requested(),
            "any non-empty value enables (parity with SLOOPER_SCAN_INLINE)"
        );
        assert!(!crate::vstedit::set_container_rect(0, 1, 2, 3, 4));
        // W8b p2-b honest refusals on the null HWND.
        assert!(!crate::vstedit::set_container_visible(0, true));
        assert_eq!(crate::vstedit::embed_parent_origin(0), (0, 0));
        // W8b p2-c honest refusals on the null HWND (the full round trip
        // lives in the win32 `w8c_*` functional pins).
        assert!(!crate::vstedit::detach_to_toplevel(0));
        assert!(!crate::vstedit::redock_to_embed(0));
        assert!(!crate::vstedit::container_is_detached(0));
        match had {
            Some(v) => std::env::set_var("SLOOPER_WIN_EMBED", v),
            None => std::env::remove_var("SLOOPER_WIN_EMBED"),
        }
    }

    /// Source pins for the embed wiring:
    /// * win32 creates containers through the ONE embed-aware helper, in
    ///   BOTH generations, and its child mode really uses `WS_CHILD`;
    /// * ui.rs gates the pass on `embed_requested`, grabs the parent via
    ///   `main_thread_hwnd`, wraps opens with `set_embed_parent`, and
    ///   NEVER calls `set_container_rect` outside a `vsthost::post`
    ///   closure (direct cross-thread `SetWindowPos` is the deadlock the
    ///   module docs forbid).
    #[test]
    fn w8b_embed_wiring_is_seamed() {
        let w = code_lines(&twin_path("win32"));
        assert!(w.contains("fn create_container"), "win32 lost create_container");
        assert!(w.contains("WS_CHILD"), "child mode lost WS_CHILD");
        assert!(
            w.matches("create_container(").count() >= 3,
            "create_container must serve both generations + its definition"
        );

        let ui = code_lines(&format!("{}/src/ui.rs", env!("CARGO_MANIFEST_DIR")));
        assert!(ui.contains("embed_wanted()"), "ui lost the embed predicate");
        assert!(
            ui.contains("fn embed_wanted") && ui.contains("self.win_embed"),
            "the predicate must ride the Config-backed switch"
        );
        assert!(ui.contains("embed_requested()"), "ui lost the env override");
        assert!(ui.contains("main_thread_hwnd()"), "ui lost the embed parent grab");
        assert!(ui.contains("set_embed_parent(p)"), "ui lost the open wrap");
        // W8b p2-a: the Config switch is persisted and offered (the
        // settings row is Windows-gated, but the seam lives cross-OS).
        assert!(
            ui.contains("win_editor_embed: self.win_embed"),
            "the Settings switch must persist into Config"
        );
        let cfg = code_lines(&format!("{}/src/config.rs", env!("CARGO_MANIFEST_DIR")));
        assert!(
            cfg.contains("pub win_editor_embed: bool"),
            "Config lost the win_editor_embed field"
        );
        let pos = ui.find("set_container_rect(").expect("ui must sync embed rects");
        let window = &ui[pos.saturating_sub(240)..pos];
        assert!(
            window.contains("vsthost::post"),
            "set_container_rect must run on the vst2-gui thread via vsthost::post"
        );
        // W8b p2-b: dock integration + hide policy wiring.
        assert!(
            ui.contains("fn embed_sync_pass"),
            "the geometry/visibility sync must be its own pass"
        );
        let sf = ui.find("side_frames(ui, self);").expect("side_frames call site");
        let after_sf = &ui[sf..];
        let cp = after_sf
            .find("central_panels(ui, self);")
            .expect("central_panels call site");
        assert!(
            after_sf[..cp].contains("embed_sync_pass("),
            "embed_sync_pass must land between side_frames and central_panels \
             (docked bodies record their slot in side_frames)"
        );
        assert!(ui.contains("embed_parent_origin("), "chrome must subtract the parent client origin");
        let vis = ui.find("set_container_visible(").expect("hide policy must call set_container_visible");
        let visw = &ui[vis.saturating_sub(240)..vis];
        assert!(
            visw.contains("vsthost::post"),
            "set_container_visible must run on the vst2-gui thread via vsthost::post"
        );
        // Dock membership: BOTH chrome kinds registered as dock members and
        // dispatched to the shared body. Assert the BODY-CALL marker (not a
        // bare "DynKind::Vst(t) =>", which the key()/from_key() match arms
        // also contain and would satisfy the check vacuously).
        assert!(
            ui.contains("DynKind::Vst(t),") && ui.contains("DynKind::VstFx(k.0, k.1),"),
            "embed pass must register Vst/VstFx dock members"
        );
        assert!(
            ui.matches("app.embed_body(ui, xid, size, painted)").count() >= 2,
            "dyn_body must dispatch both native-editor kinds through embed_body"
        );
        // The chrome's own float + close suppression wiring.
        assert!(
            ui.contains("self.embed_chrome(ctx, idx, kind, xid, size, painted)")
                && ui.contains("self.dyn_dock_skip(idx)"),
            "embed pass must skip the float chrome when docked and draw it otherwise"
        );
        assert!(
            ui.contains("fn chrome_closing_close")
                && ui.contains("chrome_closing.contains(&xid)"),
            "close-in-flight suppression must gate both the float and dock paths"
        );
        // Hide policy inputs (v1): any open menu/popup area or a carry.
        assert!(
            ui.contains("egui::Order::Foreground") && ui.contains("free_drag.is_some()"),
            "hide policy must consider open popups and window carries"
        );
        // W8b p2-c: the detach toggle. Placeholder body gates on the LIVE
        // style and posts the re-dock; the slot menu row goes through the
        // kind filter; the sync pass must keep both hands off detached
        // containers. As in p2-b, assert CALLS (thread-rule window
        // included), not patterns a definition could satisfy vacuously.
        let eb = ui.find("fn embed_body").expect("embed body");
        let ebw = &ui[eb..(eb + 1800).min(ui.len())];
        assert!(
            ebw.contains("crate::vstedit::container_is_detached(xid)"),
            "embed_body must read the live style for the placeholder"
        );
        let rd = ebw.find("redock_to_embed(").expect("placeholder must offer the way back");
        assert!(
            ebw[..rd].contains("vsthost::post"),
            "redock_to_embed must run on the vst2-gui thread via vsthost::post"
        );
        assert!(
            ebw[..rd].contains("dock back into slooper"),
            "the placeholder must carry its own label"
        );
        let sm = ui.find("fn window_slot_menu").expect("slot menu");
        let smw = &ui[sm..(sm + 3600).min(ui.len())];
        assert!(
            smw.contains("native_member_container(app, idx)"),
            "slot menu must offer detach through the kind filter"
        );
        let dt = smw
            .find("detach_to_toplevel(")
            .expect("slot menu must detach through the seam");
        assert!(
            smw[..dt].contains("vsthost::post"),
            "detach_to_toplevel must run on the vst2-gui thread via vsthost::post"
        );
        assert!(
            smw.contains("detach to native window"),
            "the detach row must carry its own label"
        );
        let sp = ui.find("fn embed_sync_pass").expect("sync pass");
        let spw = &ui[sp..(sp + 2600).min(ui.len())];
        assert!(
            spw.matches("container_is_detached(").count() >= 2,
            "sync pass must skip detached containers for BOTH geometry and \
             visibility (a parent-client coord would teleport a top-level)"
        );
        assert!(
            spw.contains("self.embed_sync.remove(&xid)"),
            "detaching must drop the geometry-diff entry so re-dock re-lands the slot rect"
        );
    }
}
