//! Hosting VST2 custom editor windows (X11 embedding).
//!
//! # The standard protocol (VST2.4 spec, verified against Ardour, Carla,
//! JUCE, LMMS and yabridge host implementations)
//!
//! * `effEditOpen` (opcode 14) takes a native window handle in `ptr`. On
//!   X11 that is the **XID** of the parent window cast to a pointer; the
//!   plugin creates its editor window as a child of it (reparenting works
//!   because X object IDs are server-global — the plugin uses its own X
//!   connection and the window manager keeps decorating our container).
//! * `effEditIdle` (opcode 19) must be pumped from the GUI thread at a
//!   steady rate while the editor is open; FLTK- and DPF-based editors run
//!   their event handling from it.
//! * `effEditGetRect` (opcode 13) receives an `ERect**` — the plugin writes
//!   the address of its rect there; `ERect` holds 16-bit
//!   `top/left/bottom/right`. The host sizes its container to it; that is
//!   the resize path for fixed-size editors, while VST2.4 resizable plugins
//!   additionally call back with `audioMasterSizeWindow`.
//! * `effEditClose` (opcode 15) must be dispatched before the plugin is
//!   unloaded; the editor thread owns that ordering.
//!
//! The open is VERIFIED, not trusted: the plugin must either reparent a
//! child window with real geometry into the container or paint pixels on
//! it (compared against a sentinel background — never "non-black", the
//! container's own background is not content). Nothing appearing within
//! ~3 s means the editor is refused, the window destroyed, and the UI
//! falls back to sliders — a blank, dead panel is never left on screen.
//! The container pumps X events and honours `WM_DELETE_WINDOW` /
//! `DestroyNotify`, so the window-manager close button always works.
//!
//! THREADING: every plugin control-plane call — instantiation included —
//! runs on ONE shared "vst2-gui" thread (see `vsthost::open`), while the
//! audio thread keeps processing. This is not optional: GUI toolkits
//! (JUCE's `MessageManager`, FLTK) bind their global event loop to the
//! thread that first touches the plugin and silently refuse to show any
//! window opened elsewhere. Opening editors on a per-editor thread while
//! instantiation ran on the UI thread left every JUCE editor permanently
//! unmapped (the v10.2 "no plugin GUIs at all" report). One thread makes
//! the affinity assumption true by construction; editors are pumped
//! there in rotation (verify + idle) as state machines rather than
//! blocking loops, so several editors coexist.
//!
//! VST3 editors ARE hosted here since v0.12 — but not through the running
//! instance: the `vst3-host` realtime runner confines the plugin to its
//! processing thread. The runner owns the sounding instance; the editor
//! embeds a SHADOW instance of the same plugin (see `Editor3` below),
//! which never processes audio and reports its edits back into the shared
//! parameter cells. The crate's own `EmbeddedEditor` is unused: it wants
//! an egui parent window, while slooper hosts every generation in its own
//! verified-paint X11 container.

use std::ffi::{c_void, CString};
use std::os::raw::c_int;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::vsthost::Vst2Handle;

// ------------------------------------------------------------------ xlib FFI
#[repr(C)]
struct Display {
    _private: [u8; 0],
}
type XWindow = u64;
type Atom = u64;

#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(name: *const i8) -> *mut Display;
    fn XCloseDisplay(d: *mut Display) -> c_int;
    fn XCreateSimpleWindow(
        d: *mut Display,
        parent: XWindow,
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
        border: c_int,
        border_color: u64,
        background: u64,
    ) -> XWindow;
    fn XDestroyWindow(d: *mut Display, w: XWindow) -> c_int;
    fn XDestroySubwindows(d: *mut Display, w: XWindow) -> c_int;
    fn XMapWindow(d: *mut Display, w: XWindow) -> c_int;
    fn XSelectInput(d: *mut Display, w: XWindow, mask: u64) -> c_int;
    fn XStoreName(d: *mut Display, w: XWindow, name: *const i8) -> c_int;
    fn XInternAtom(d: *mut Display, name: *const i8, only_if_exists: c_int) -> Atom;
    fn XSetWMProtocols(d: *mut Display, w: XWindow, protocols: *mut Atom, count: c_int) -> c_int;
    fn XSync(d: *mut Display, discard: c_int) -> c_int;
    fn XFlush(d: *mut Display) -> c_int;
    fn XPending(d: *mut Display) -> c_int;
    fn XNextEvent(d: *mut Display, ev: *mut [u64; 24]) -> c_int;
    fn XResizeWindow(d: *mut Display, w: XWindow, width: c_int, height: c_int) -> c_int;
    fn XQueryTree(
        d: *mut Display,
        w: XWindow,
        root: *mut XWindow,
        parent: *mut XWindow,
        children: *mut *mut XWindow,
        n_children: *mut c_int,
    ) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
    fn XGetGeometry(
        d: *mut Display,
        w: XWindow,
        root: *mut XWindow,
        x: *mut c_int,
        y: *mut c_int,
        ww: *mut u32,
        hh: *mut u32,
        border: *mut u32,
        depth: *mut u32,
    ) -> c_int;
    fn XGetWindowAttributes(d: *mut Display, w: XWindow, attrs: *mut XWindowAttributes) -> c_int;
    fn XSetErrorHandler(
        handler: Option<extern "C" fn(*mut c_void, *mut c_void) -> c_int>,
    ) -> Option<extern "C" fn(*mut c_void, *mut c_void) -> c_int>;
    fn XSendEvent(
        d: *mut Display,
        w: XWindow,
        propagate: c_int,
        mask: u64,
        ev: *const u64,
    ) -> c_int;
}

extern "C" fn x_error_noop(_display: *mut c_void, _event: *mut c_void) -> c_int {
    // X races (a plugin window vanishing mid-query) must never kill the
    // host; standard practice for plugin wrappers.
    0
}

/// Install the process-wide no-op X error handler (v16). The default
/// Xlib handler calls `exit()` — a `BadMatch` from an editor-window race
/// (XGetImage on a just-destroyed window — reproduced in the v16 perf
/// probe) took the whole app down. Plugin hosting runs on a dedicated
/// thread with dozens of foreign windows under a WM/compositor; a dead
/// window must be a no-op, not a funeral. Idempotent; called before any
/// editor display is created.
fn install_x_error_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        XSetErrorHandler(Some(x_error_noop));
    });
}

/// Live editor windows across both generations (the pump thread reads
/// this through `vsthost::gui_tx` to pick its tick: fast while editors
/// exist, idle-cost-zero while none do).
static EDITORS_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// How many plugin editor windows the GUI thread is serving right now.
pub fn editors_active() -> usize {
    EDITORS_ACTIVE.load(Ordering::Relaxed)
}

// X11 event masks (X.h). v0.11 NOTE: this constant used to read `1 << 20`,
// which is SubstructureRedirectMask -- the container never actually
// received StructureNotify events, and WM-driven resizes went unnoticed
// (close-button handling survived only because ClientMessage delivery is
// not mask-gated). 1 << 17 is the real StructureNotifyMask.
const STRUCTURE_NOTIFY_MASK: u64 = 1 << 17;
const KEY_PRESS_MASK: u64 = 1 << 0;
const KEY_RELEASE_MASK: u64 = 1 << 1;
const FOCUS_CHANGE_MASK: u64 = 1 << 21;
// X protocol event codes (XSendEvent(7X)): 17 DestroyNotify, 18 UnmapNotify,
// 21 ReparentNotify, 22 ConfigureNotify, 33 ClientMessage. (An earlier
// comment block here called 18 "ReparentNotify" — 18 is UnmapNotify.)
const EVENT_KEY_PRESS: i32 = 2;
const EVENT_KEY_RELEASE: i32 = 3;
const EVENT_FOCUS_IN: i32 = 9;
const EVENT_FOCUS_OUT: i32 = 10;
const EVENT_EXPOSE: i32 = 12;
const EVENT_DESTROY_NOTIFY: i32 = 17;
const EVENT_REPARENT_NOTIFY: i32 = 21;
const EVENT_CONFIGURE_NOTIFY: i32 = 22;
const EVENT_CLIENT_MESSAGE: i32 = 33;

// VST2.4 editor resize negotiation (aeff.h).
const EFF_EDIT_CAN_SIZE: i32 = 79;
const EFF_EDIT_SIZE: i32 = 78;

/// Distinctive container background. The "did the plugin paint?" probe
/// compares pixels against THIS, never against black: the old probe
/// treated the window's own grey background as painted content, so a
/// broken plugin that never drew a pixel was accepted and the user got a
/// permanent blank panel (the amsynth report, 2026-10). Black is equally
/// unusable — a mapped-but-never-exposed window reads all-zero under a
/// compositing manager, indistinguishable from drawn black.
const CONTAINER_BG: u64 = 0x2A16B4;

// VST2 editor opcodes (aeff.h / vestige.h, cross-checked against the
// vendored vst2 crate's OpCode enum).
const EFF_EDIT_GET_RECT: i32 = 13;
const EFF_EDIT_OPEN: i32 = 14;
const EFF_EDIT_CLOSE: i32 = 15;
const EFF_EDIT_IDLE: i32 = 19;

/// `ERect` from the VST2 SDK: four 16-bit screen coordinates.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ERect {
    top: i16,
    left: i16,
    bottom: i16,
    right: i16,
}

/// Opaque XImage handle (ZPixmap snapshot of the editor container).
#[repr(C)]
struct XImage {
    _private: [u8; 0],
}

#[link(name = "X11")]
extern "C" {
    fn XGetImage(
        d: *mut Display,
        w: XWindow,
        x: c_int,
        y: c_int,
        w2: u32,
        h2: u32,
        planemask: u64,
        format: c_int,
    ) -> *mut XImage;
    fn XGetPixel(img: *mut XImage, x: c_int, y: c_int) -> u64;
    fn XDestroyImage(img: *mut XImage) -> c_int;
}

/// `XWindowAttributes` (X11/X.h) treated as an opaque 136-byte buffer on
/// x86_64: we read only two fields, `width` (offset 8) and `map_state`
/// (offset 92), whose layout is pinned by the `v102_xattr_offsets_pinned`
/// test against the live X server. Declaring the real field types instead
/// would depend on a 40-year-old header layout staying frozen.
#[repr(C, align(8))]
struct XWindowAttributes {
    raw: [u8; 136],
}

/// X.h map-state enum.
const IS_VIEWABLE: i32 = 2;

impl XWindowAttributes {
    fn new() -> Self {
        Self { raw: [0; 136] }
    }
    fn width(&self) -> i32 {
        i32::from_ne_bytes(self.raw[8..12].try_into().unwrap())
    }
    fn map_state(&self) -> i32 {
        i32::from_ne_bytes(self.raw[92..96].try_into().unwrap())
    }
}

/// True when `win` exists and is currently mapped/visible (a plugin
/// child that was created and resized but never MAPPED is not evidence
/// of a live editor — the amsynth "solid blue forever" report: its
/// editor throws mid-build and leaves an unmapped sized child that
/// `XGetGeometry` alone happily accepted).
unsafe fn is_viewable(d: *mut Display, win: XWindow) -> bool {
    let mut attrs = XWindowAttributes::new();
    XGetWindowAttributes(d, win, &mut attrs) != 0
        && attrs.map_state() == IS_VIEWABLE
        && attrs.width() > 0
}

/// Drain and DISPATCH every pending X event on `d`, returning true when
/// one of them says the window must close: a `WM_DELETE_WINDOW` client
/// message (the window manager's close button), or the container's own
/// DestroyNotify (xkill / forced close).
///
/// The old pump broke on `XNextEvent`'s return value — XNextEvent returns
/// 0 on SUCCESS, so every event it read was popped off the queue and
/// dropped before the match, and the close button did nothing (the
/// "cannot close plugin window" report). XNextEvent blocking is not a
/// risk either: the caller only calls it while `XPending > 0`.
/// Pump the container's event queue. Returns true when the user closed
/// the editor. `resized` collects the newest CONTAINER size change (the
/// window manager's answer to the user dragging the frame). Keyboard and
/// focus events are forwarded to the plugin's top child window with
/// `send_event=True`: child-window toolkits (JUCE and friends) sit behind
/// an unmanaged X window that never receives the WM's focus, so without
/// this relay their editors are click-but-not-type deaf (v0.11 R1b).
unsafe fn drain_events(
    d: *mut Display,
    win: XWindow,
    wm_protocols: Atom,
    wm_delete: Atom,
    resized: &mut Option<(u32, u32)>,
) -> bool {
    let mut drained = 0;
    while XPending(d) > 0 && drained < 256 {
        let mut ev = [0u64; 24];
        XNextEvent(d, &mut ev);
        drained += 1;
        match (ev[0] as u32) as i32 {
            EVENT_KEY_PRESS | EVENT_KEY_RELEASE | EVENT_FOCUS_IN | EVENT_FOCUS_OUT => {
                // Relay to the plugin's own top window (XKeyEvent window
                // field lives at byte 32 -> ev[4]; send_event at byte 16).
                if let Some((k, _, _)) = first_sized_child(d, win) {
                    ev[4] = k;
                    ev[2] = 1;
                    XSendEvent(d, k, 0, 0, ev.as_ptr());
                }
            }
            EVENT_CONFIGURE_NOTIFY if ev[5] == win => {
                // XConfigureEvent: window@40 -> ev[5], width@64 -> ev[8],
                // height@72 -> ev[9]. Only OUR window's resizes.
                *resized = Some((ev[8] as u32, ev[9] as u32));
            }
            EVENT_EXPOSE => {
                // Direct-draw plugins run their OWN Xlib connection and
                // receive their own Expose events; nothing to relay.
            }
            EVENT_CLIENT_MESSAGE => {
                // XClientMessageEvent on x86_64: message_type atom
                // (unsigned long) at byte 40 -> ev[5]; data.l[0]
                // at byte 56 -> ev[7].
                if ev[5] == wm_protocols && ev[7] == wm_delete {
                    return true;
                }
            }
            EVENT_DESTROY_NOTIFY => {
                // XDestroyWindowEvent: the destroyed window is at byte 32.
                if ev[4] == win {
                    return true;
                }
            }
            EVENT_CONFIGURE_NOTIFY | EVENT_REPARENT_NOTIFY => {
                // Container resizes and plugin window churn: the child
                // count and rect refresh in the caller track them.
            }
            _ => {}
        }
    }
    false
}

/// Send a `WM_DELETE_WINDOW` client message to an X window from a fresh
/// connection — exactly what a window manager does when the user clicks
/// the close button. Used by the editor close-button regression pins.
pub fn request_wm_delete(xid: u64) -> bool {
    unsafe {
        let d = XOpenDisplay(ptr::null());
        if d.is_null() {
            return false;
        }
        XSetErrorHandler(Some(x_error_noop));
        let wm_delete = XInternAtom(d, c"WM_DELETE_WINDOW".as_ptr(), 0);
        let wm_protocols = XInternAtom(d, c"WM_PROTOCOLS".as_ptr(), 0);
        // XClientMessageEvent as u64 words (x86_64): [0] type (ClientMessage
        // = 33, int), [2] send_event, [4] window, [5] message_type,
        // [6] format = 32, [7] data.l[0] = the protocol atom.
        let mut ev = [0u64; 24];
        ev[0] = EVENT_CLIENT_MESSAGE as u64;
        ev[2] = 1;
        ev[4] = xid;
        ev[5] = wm_protocols;
        ev[6] = 32;
        ev[7] = wm_delete;
        XSendEvent(d, xid, 0, 0, ev.as_ptr());
        XFlush(d);
        XCloseDisplay(d);
        true
    }
}

/// True when a corner probe of `win` holds any pixel that is neither the
/// sentinel background nor the all-zero "nothing painted yet" state —
/// proof the plugin painted DIRECTLY on the container (VSTGUI-style
/// editors never create a child window, so child polling alone
/// false-negatives them).
///
/// v16: guarded — an unmapped/0×0 window answers `XGetImage` with
/// `BadMatch`, and even with the no-op error handler installed the probe
/// is pointless there; skip the round-trip instead.
unsafe fn container_painted(d: *mut Display, win: XWindow) -> bool {
    const ALL_PLANES: u64 = !0u64;
    const Z_PIXMAP: c_int = 2;
    if !is_viewable(d, win) {
        return false;
    }
    let img = XGetImage(d, win, 0, 0, 24, 24, ALL_PLANES, Z_PIXMAP);
    if img.is_null() {
        return false;
    }
    let mut hit = false;
    for y in 0..24 {
        for x in 0..24 {
            let p = XGetPixel(img, x, y);
            if p != CONTAINER_BG && p != 0 {
                hit = true;
                break;
            }
        }
        if hit {
            break;
        }
    }
    XDestroyImage(img);
    hit
}

/// Query the plugin's preferred editor size via `effEditGetRect`.
pub fn edit_rect(handle: &Vst2Handle) -> Option<(i32, i32)> {
    let mut rect: *mut ERect = ptr::null_mut();
    handle.dispatch(
        EFF_EDIT_GET_RECT,
        0,
        0,
        &mut rect as *mut *mut ERect as *mut c_void,
        0.0,
    );
    if rect.is_null() {
        return None;
    }
    let r = unsafe { *rect };
    let w = (r.right as i32 - r.left as i32).max(0);
    let h = (r.bottom as i32 - r.top as i32).max(0);
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

/// Shared state between an `EditorWindow` handle and its `Editor` running
/// on the VST GUI thread.
#[derive(Clone)]
struct Shared {
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    open_failed: Arc<AtomicBool>,
    painted: Arc<AtomicBool>,
    children: Arc<AtomicUsize>,
    width: Arc<AtomicI32>,
    height: Arc<AtomicI32>,
    xid: Arc<AtomicUsize>,
    /// v20 (VST3 editors): the shadow's serialized component state, captured
    /// at teardown when the user changed something. The host harvests it via
    /// `EditorWindow::take_state` and rebuilds the sounding instance, so a
    /// preset picked inside the plugin's own GUI reaches the audio.
    state: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            open_failed: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(None)),
            painted: Arc::new(AtomicBool::new(false)),
            children: Arc::new(AtomicUsize::new(0)),
            width: Arc::new(AtomicI32::new(0)),
            height: Arc::new(AtomicI32::new(0)),
            xid: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// A hosted VST2 editor. Every editor operation (open, idle pump, resize,
/// close) runs on the ONE shared VST GUI thread (see `vsthost::open`) —
/// the same thread that instantiated the plugin, because GUI toolkits
/// bind their event loop to that thread and nowhere else.
pub struct EditorWindow {
    path: String,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    open_failed: Arc<AtomicBool>,
    /// The plugin's editor window appeared and was sized — real pixels.
    painted: Arc<AtomicBool>,
    children: Arc<AtomicUsize>,
    width: Arc<AtomicI32>,
    height: Arc<AtomicI32>,
    /// The host container's XID (0 until the GUI thread creates it).
    xid: Arc<AtomicUsize>,
    /// v20: VST3 shadow-editor state hand-off (see `Shared::state`).
    state: Arc<Mutex<Option<Vec<u8>>>>,
}

impl EditorWindow {
    /// v20: take the VST3 shadow's state chunk if the user changed the
    /// plugin through its own GUI (presets, patch data beyond automatable
    /// params). Non-empty ONLY after a VST3 editor retired with changes;
    /// VST2 editors edit the live instance and never produce one.
    pub fn take_state(&mut self) -> Option<Vec<u8>> {
        self.state.lock().ok().and_then(|mut c| c.take())
    }

    /// Plugin path this editor was opened for (stale-editor detection).
    pub fn path(&self) -> &str {
        &self.path
    }
    /// True once the editor has retired (user closed the window, the
    /// plugin refused to open, or [`Self::close`] completed).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    /// The plugin accepted `effEditOpen` but its editor returned failure
    /// (or it created no window at all).
    pub fn open_failed(&self) -> bool {
        self.open_failed.load(Ordering::SeqCst)
    }
    /// True once the plugin's own editor window has been seen mapped with
    /// real geometry (the container is showing live plugin UI, not a
    /// blank panel). False while waiting, or forever on broken editors.
    pub fn painted(&self) -> bool {
        self.painted.load(Ordering::SeqCst)
    }
    /// Windows the plugin has parented into our container. Zero a while
    /// after opening means the plugin did not create an editor.
    pub fn child_count(&self) -> usize {
        self.children.load(Ordering::SeqCst)
    }
    /// Editor size as last reported by the plugin (`effEditGetRect`).
    pub fn editor_size(&self) -> (i32, i32) {
        (
            self.width.load(Ordering::SeqCst),
            self.height.load(Ordering::SeqCst),
        )
    }
    /// The host container window's XID, or 0 before the GUI thread has
    /// created it (integration tests / WM-protocol close injection).
    pub fn xid(&self) -> u64 {
        self.xid.load(Ordering::SeqCst) as u64
    }
    /// Retire the editor: ask the GUI thread to close it and wait (with a
    /// bound) for the `effEditClose` + window teardown to finish, so the
    /// plugin instance may be dropped safely afterwards. Idempotent.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // v16: wake the pump thread immediately. Without the post the
        // hand-off waits a full idle tick (≥30 ms, the old fixed cadence);
        // with it, close settles on the next iteration.
        crate::vsthost::post(move || ());
        for _ in 0..500 {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Only reachable if the GUI thread is wedged inside plugin code;
        // report rather than hang the host forever.
        eprintln!("[vstedit] {} close did not settle in 5 s", self.path);
    }
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        // Wait so `effEditClose` finishes before the plugin instance may
        // be dropped by the caller.
        self.close();
    }
}

/// Open a VST2 plugin's editor in a new X11 window. `ae` is the raw
/// `AEffect` dispatcher handle from `vsthost`; the GUI thread keeps
/// using it, so the plugin instance must outlive the returned handle
/// (slooper closes editors before unloading, see `ui.rs`).
pub fn open_editor(handle: &Vst2Handle, path: &str, title: &str) -> Result<EditorWindow, String> {
    if !x11_available() {
        return Err("no X11 display (VST2 editors need X11/XWayland)".into());
    }
    let shared = Shared::new();
    let ae = *handle;
    let title = title.to_string();
    let s2 = shared.clone();
    // Creation (container window + effEditOpen) happens ON the GUI
    // thread; blocking here means the returned handle already has an
    // XID and either a live verification state machine or a verdict.
    crate::vsthost::on_gui_thread(move || {
        REG.with(|reg| {
            // SAFETY: runs on the GUI thread, the sole owner of all
            // editor state (see module docs).
            if let Some(e) = unsafe { make_editor(ae, &title, s2) } {
                reg.borrow_mut().push(e);
            }
        });
    })?;
    Ok(EditorWindow {
        path: path.to_string(),
        stop: shared.stop.clone(),
        closed: shared.closed.clone(),
        open_failed: shared.open_failed.clone(),
        painted: shared.painted.clone(),
        children: shared.children.clone(),
        width: shared.width.clone(),
        height: shared.height.clone(),
        xid: shared.xid.clone(),
        state: shared.state.clone(),
    })
}

// ---------------------------------------------------------------- registry
//
// The live editors, owned exclusively by the VST GUI thread; every
// `pump_editors` call advances their state machines once (the shared
// 30 ms cadence the toolkits expect from `effEditIdle`).

thread_local! {
    static REG: std::cell::RefCell<Vec<Editor>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// One step for every open editor; retires finished ones. Called by the
/// VST GUI thread between jobs — never from another thread. Serves both
/// generations: VST2 state machines get `effEditIdle`, VST3 shadow
/// editors get `service_run_loop` (same 30 ms cadence both expect).
pub fn pump_editors() {
    pump_vst3_editors();
    REG.with(|reg| {
        let mut eds = reg.borrow_mut();
        for e in eds.iter_mut() {
            if e.shared.stop.load(Ordering::SeqCst) {
                if !matches!(e.phase, Phase::Done) {
                    e.teardown(false);
                }
                continue;
            }
            e.step();
        }
        eds.retain(|e| !matches!(e.phase, Phase::Done));
    });
}

enum Phase {
    /// Waiting for the plugin to prove it painted — the per-frame form
    /// of what used to be `editor_main`'s verification loop.
    Verify {
        it: u32,
    },
    Live {
        last_size: (i32, i32),
        /// Container size last reported by the window manager (R1b).
        last_container: (u32, u32),
    },
    Done,
}

/// v16: window-system housekeeping (child tree, paint watchdog, plugin
/// rect) runs on a timer, not per pump step. At the old 30 ms tick the
/// per-step XQueryTree/XGetWindowAttributes/XGetImage storm was ~0.2-1 ms
/// per editor every step — with the 6 ms responsiveness tick it would
/// dominate, and healthy editors do not need 160 Hz existence checks.
const HOUSEKEEPING: Duration = Duration::from_millis(500);
/// ~4 s of housekeeping ticks without proof of life means the editor
/// died (same patience as the old 120 × 30 ms step counting).
const DEAD_TICKS_PATIENCE: u32 = 8;
/// Verify patience: real time for the plugin to show a window.
const VERIFY_PATIENCE: Duration = Duration::from_secs(3);

struct Editor {
    handle: Vst2Handle,
    d: *mut Display,
    win: XWindow,
    title: String,
    wm_protocols: Atom,
    wm_delete: Atom,
    phase: Phase,
    shared: Shared,
    dbg: bool,
    created: Instant,
    house: Instant,
    dead_ticks: u32,
}

/// Create the container, map it, and dispatch `effEditOpen` — all on the
/// GUI thread. `None` (with the failure flags already set) when X or the
/// plugin refuses outright.
unsafe fn make_editor(handle: Vst2Handle, title: &str, shared: Shared) -> Option<Editor> {
    // NO XInitThreads: hosts that host plugin UIs on a dedicated thread
    // (Carla, Y/Plugins) deliberately leave libX11 unthreaded — JUCE's
    // X initialization inside plugins breaks with it (verified here:
    // editors never map the window while XInitThreads has been called).
    // Our plugin-side Xlib use is strictly the GUI thread; the UI thread
    // only opens short-lived independent connections (WM-delete probes).
    install_x_error_handler();
    let d = XOpenDisplay(ptr::null());
    if d.is_null() {
        shared.open_failed.store(true, Ordering::SeqCst);
        shared.closed.store(true, Ordering::SeqCst);
        return None;
    }
    let root = root_window(d);
    // (bisect) fixed seed size
    let (w0, h0) = (640i32, 420i32);
    // Border width 0 and a SENTINEL background/border (see
    // CONTAINER_BG): the pixel probe must be able to tell "plugin
    // drew here" apart from "empty window furniture".
    let win = XCreateSimpleWindow(d, root, 0, 0, w0, h0, 0, CONTAINER_BG, CONTAINER_BG);
    // The container is mapped BARE — no window name, no WM protocols,
    // no event selection — and that is load-bearing: any of those
    // requests issued before `effEditOpen` change how the window manager
    // engages the window in time, and plugin editors (JUCE verified,
    // FLTK consistent) then create and size their child but never map
    // it, leaving the sentinel panel forever (the v10.2 solid-blue
    // report). Decoration lands after the editor proves itself live,
    // in `Editor::decorate`.
    shared.xid.store(win as usize, Ordering::SeqCst);
    let wm_delete = XInternAtom(d, c"WM_DELETE_WINDOW".as_ptr(), 0);
    let wm_protocols = XInternAtom(d, c"WM_PROTOCOLS".as_ptr(), 0);
    XMapWindow(d, win);
    XSync(d, 0);
    let dbg = std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some();
    let opened = handle.dispatch(EFF_EDIT_OPEN, 0, 0, win as usize as *mut c_void, 0.0);
    if dbg {
        eprintln!("[vstedit] {title}: effEditOpen -> {opened} (win {win:#x}, gui thread)");
    }
    // The VST2 spec says 0 == success, but plenty of toolkits return a
    // "true" flag (1) for a SUCCESSFUL open — the verification below is
    // the real judge: if the plugin draws, it opened, whatever it said.
    EDITORS_ACTIVE.fetch_add(1, Ordering::Relaxed);
    Some(Editor {
        handle,
        d,
        win,
        title: title.to_string(),
        wm_protocols,
        wm_delete,
        phase: Phase::Verify { it: 0 },
        shared,
        dbg,
        created: Instant::now(),
        house: Instant::now(),
        dead_ticks: 0,
    })
}

/// `edit_rect` under the loader lock (every GUI-thread AEffect call
/// serializes with instantiation through it).
fn edit_rect_locked(handle: &Vst2Handle) -> Option<(i32, i32)> {
    let _loader = crate::vsthost::vst_loader_lock();
    edit_rect(handle)
}

impl Editor {
    /// One pump iteration: X events, idle dispatch, state advance.
    fn step(&mut self) {
        unsafe {
            match self.phase {
                Phase::Done => {}
                Phase::Verify { it } => {
                    // The user can hit the close button while we are
                    // still verifying: honour it instead of holding a
                    // doomed window.
                    let mut _resized = None;
                    if drain_events(
                        self.d,
                        self.win,
                        self.wm_protocols,
                        self.wm_delete,
                        &mut _resized,
                    ) {
                        self.teardown(false);
                        return;
                    }
                    self.handle
                        .dispatch(EFF_EDIT_IDLE, 0, 0, ptr::null_mut(), 0.0);
                    // A child counts as a live editor only when MAPPED
                    // with real geometry: created-and-sized-but-unmapped
                    // children are what a mid-build crash leaves behind,
                    // and accepting one kept the sentinel-blue container
                    // on screen forever (v10.2 report). Direct-draw
                    // editors (VSTGUI et al.) paint ON the container and
                    // never create a child, so check real pixels too.
                    let child_live = first_sized_child(self.d, self.win)
                        .is_some_and(|(k, w, h)| w >= 8 && h >= 8 && is_viewable(self.d, k));
                    let px = container_painted(self.d, self.win);
                    if self.dbg && (it < 3 || it % 10 == 0) {
                        let (kid, w, h) = first_sized_child(self.d, self.win).unwrap_or((0, 0, 0));
                        eprintln!(
                            "[vstedit] verify {it}: child=({kid:x} {w}x{h}) px={px} live={child_live}"
                        );
                    }
                    if child_live || px {
                        self.shared.painted.store(true, Ordering::SeqCst);
                        self.decorate();
                        self.phase = Phase::Live {
                            last_size: (0, 0),
                            last_container: (0, 0),
                        };
                        return;
                    }
                    // v16: real-time patience (the pump tick changed with
                    // the responsiveness pass; step counting did not).
                    if self.created.elapsed() >= VERIFY_PATIENCE {
                        // No live editor appeared: close cleanly so NO
                        // blank panel is left behind; the UI falls back
                        // to the slider panel.
                        self.teardown(true);
                    } else {
                        self.phase = Phase::Verify { it: it + 1 };
                    }
                }
                Phase::Live {
                    ref mut last_size,
                    ref mut last_container,
                } => {
                    // Dispatch X events (close button / window
                    // destruction); bounded per step.
                    let mut resized = None;
                    if drain_events(
                        self.d,
                        self.win,
                        self.wm_protocols,
                        self.wm_delete,
                        &mut resized,
                    ) {
                        self.teardown(false);
                        return;
                    }
                    // v0.11 R1b: the user dragged the editor frame (or the
                    // WM auto-maximised it). Route that through the plugin:
                    // VST2.4 editors with effEditCanSizeWindow take it as
                    // effEditSize; stubborn child-window toolkits get the
                    // child resized plus a synthetic ConfigureNotify so
                    // their layout code runs. Without this the plugin kept
                    // painting its old size inside a new-size frame --
                    // clipped or sentinel-coloured strips (the "GUIs don't
                    // fully work" report).
                    if let Some((w, h)) = resized {
                        if (w, h) != *last_container
                            && w >= 8
                            && h >= 8
                            && (last_container.0 != 0 || (w, h) != (640, 420))
                        {
                            *last_container = (w, h);
                            let can = {
                                let _loader = crate::vsthost::vst_loader_lock();
                                self.handle.dispatch(
                                    EFF_EDIT_CAN_SIZE,
                                    0,
                                    0,
                                    ptr::null_mut(),
                                    0.0,
                                ) != 0
                                    // aeff.h (VST 2.4): 'value' = new width,
                                // 'opt' = new height; returns 1 if handled.
                                && self.handle.dispatch(
                                    EFF_EDIT_SIZE,
                                    0,
                                    w as isize,
                                    ptr::null_mut(),
                                    h as f32,
                                ) != 0
                            };
                            if !can {
                                if let Some((k, _, _)) = first_sized_child(self.d, self.win) {
                                    XResizeWindow(self.d, k, w as i32, h as i32);
                                    // Synthetic ConfigureNotify for the child.
                                    let mut cn = [0u64; 24];
                                    cn[0] = EVENT_CONFIGURE_NOTIFY as u64;
                                    cn[2] = 1; // send_event
                                    cn[3] = self.d as u64; // display
                                    cn[4] = k; // event window
                                    cn[5] = k; // window
                                    cn[8] = w as u64;
                                    cn[9] = h as u64;
                                    XSendEvent(self.d, k, 0, 0, cn.as_ptr());
                                    XFlush(self.d);
                                }
                            }
                        }
                    }
                    // The plugin's editor event loop runs here — this is
                    // what the fast tick buys: mouse/key/timer service at
                    // ~1/EDITOR_TICK_MS instead of the old 33 Hz.
                    self.handle
                        .dispatch(EFF_EDIT_IDLE, 0, 0, ptr::null_mut(), 0.0);
                    // Window-system housekeeping on a timer (v16): child
                    // count for the UI, the dead-editor watchdog, and the
                    // plugin's preferred rect. Healthy editors do not
                    // need four X round-trips per pump step.
                    if self.house.elapsed() >= HOUSEKEEPING {
                        self.house = Instant::now();
                        let kids = count_children_local(self.d, self.win);
                        self.shared.children.store(kids, Ordering::SeqCst);
                        // Dead-editor watchdog: nothing viewable for
                        // ~4 s straight means the editor crashed or tore
                        // itself down — close cleanly instead of keeping
                        // a dead sentinel panel around.
                        let alive = first_sized_child(self.d, self.win)
                            .is_some_and(|(k, w, h)| w >= 8 && h >= 8 && is_viewable(self.d, k))
                            || container_painted(self.d, self.win);
                        self.dead_ticks = if alive { 0 } else { self.dead_ticks + 1 };
                        if self.dead_ticks > DEAD_TICKS_PATIENCE {
                            self.teardown(true);
                            return;
                        }
                        // Rect + size handling; the plugin owns its size.
                        if let Some((w, h)) = edit_rect_locked(&self.handle) {
                            self.shared.width.store(w, Ordering::SeqCst);
                            self.shared.height.store(h, Ordering::SeqCst);
                            if (w, h) != *last_size && w > 0 && h > 0 {
                                *last_size = (w, h);
                                XResizeWindow(self.d, self.win, w, h);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Name, WM close protocol, and destroy-notification — added only
    /// once the editor has proven itself (see the bare-container note in
    /// `make_editor`: issuing these before `effEditOpen` breaks plugin
    /// window mapping).
    unsafe fn decorate(&mut self) {
        // v0.11 R1b: StructureNotify for WM resizes, plus keyboard/focus
        // so `drain_events` can relay input to the plugin's child window.
        // Exposure is NOT selected: direct-draw plugins listen on their own
        // connection and synthetic Expose floods confuse them.
        XSelectInput(
            self.d,
            self.win,
            STRUCTURE_NOTIFY_MASK | KEY_PRESS_MASK | KEY_RELEASE_MASK | FOCUS_CHANGE_MASK,
        );
        if let Ok(cname) = CString::new(format!("slooper \u{2014} {}", self.title)) {
            XStoreName(self.d, self.win, cname.as_ptr());
        }
        let mut protocols = [self.wm_delete];
        XSetWMProtocols(self.d, self.win, protocols.as_mut_ptr(), 1);
        XFlush(self.d);
    }

    /// `effEditClose`, destroy the container, release the display, mark
    /// retired. `refused` reports the death as an open failure so the UI
    /// drops to (or keeps) the slider panel.
    fn teardown(&mut self, refused: bool) {
        unsafe {
            if !self.d.is_null() {
                // v16: one pump tick faster for the whole app if this was
                // the last open editor (lets the idle tick take over).
                EDITORS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
                {
                    let _loader = crate::vsthost::vst_loader_lock();
                    self.handle
                        .dispatch(EFF_EDIT_CLOSE, 0, 0, ptr::null_mut(), 0.0);
                }
                XDestroySubwindows(self.d, self.win);
                XDestroyWindow(self.d, self.win);
                XFlush(self.d);
                XCloseDisplay(self.d);
                self.d = ptr::null_mut();
            }
            self.shared.open_failed.store(refused, Ordering::SeqCst);
            self.shared.closed.store(true, Ordering::SeqCst);
            self.phase = Phase::Done;
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Every retire path runs teardown() before removal from REG;
        // this is the belt for any path that forgets — never leak the
        // display, and never leave the container alive without an owner.
        if !matches!(self.phase, Phase::Done) {
            self.teardown(true);
        }
    }
}

// ------------------------------------------------------------ VST3 editors
//
// The sounding VST3 instance lives inside the `vst3-host` realtime
// runner, and the crate confines it to the processing thread — no editor
// API reaches it from the UI side. So the editor hosts a SHADOW instance:
// the same plugin loaded again on the GUI thread, never processing audio,
// primed with the live parameter cells' current values. Every gesture the
// plugin's own UI reports (IComponentHandler::performEdit, drained via
// `get_parameter_changes`) is written into the SAME `VstParams` cell the
// sliders and automation write — generation bump, RT pick-up, sound
// changes: the identical path a slider move takes. Reverse sync (RT
// automation moving the open GUI's knobs) is deliberately out of scope
// for v0.12; docs/PLUGINS.md says so.

struct Editor3 {
    /// The shadow instance, owned by the GUI thread. `Option` so
    /// teardown can retire it under the loader lock exactly once.
    plugin: Option<vst3_host::Plugin>,
    /// VST3 parameter ids in `VstParams`-cell order: cell index =
    /// position, value = id (`performEdit` reports ids).
    ids: Vec<u32>,
    params: Arc<crate::vsthost::VstParams>,
    /// Load-time sample rate for the teardown state flush (see
    /// `teardown`): host-applied values only enter a plugin's
    /// serializable state once it has run audio with them.
    sr: u32,
    d: *mut Display,
    win: XWindow,
    title: String,
    wm_protocols: Atom,
    wm_delete: Atom,
    phase: Phase,
    shared: Shared,
    dbg: bool,
    created: Instant,
    house: Instant,
    dead_ticks: u32,
    /// v20: set when the user has demonstrably edited through this shadow
    /// (a performEdit reached us, or the plugin raised a restartComponent
    /// request) — only then is its state chunk worth harvesting at close.
    changed: bool,
}

/// Open a VST3 plugin's own editor window. Mirrors [`open_editor`] exactly
/// from the caller's side — same handle type, same verified-paint rule
/// (nothing appears within ~3 s means refused: container destroyed,
/// `open_failed` set, honest slider-less status), same WM-close behaviour,
/// same GUI-thread ownership. `initial` carries (id, normalized value)
/// pairs copied from the sounding instance's parameter cells; `ids` is the
/// cell-order id list used to route the plugin's edits back.
pub fn open_vst3_editor(
    path: &str,
    title: &str,
    sample_rate: u32,
    initial: Vec<(u32, f64)>,
    state: Option<Vec<u8>>,
    ids: Vec<u32>,
    params: Arc<crate::vsthost::VstParams>,
) -> Result<EditorWindow, String> {
    if !x11_available() {
        return Err("no X11 display (VST3 editors need X11/XWayland)".into());
    }
    let shared = Shared::new();
    let title = title.to_string();
    let path_h = path.to_string();
    let path_j = path.to_string();
    let s2 = shared.clone();
    crate::vsthost::on_gui_thread(move || {
        REG3.with(|reg| {
            // SAFETY: runs on the GUI thread, the sole owner of all
            // editor state (see module docs).
            if let Some(e) =
                unsafe { make_editor3(path_j, title, sample_rate, initial, state, ids, params, s2) }
            {
                reg.borrow_mut().push(e);
            }
        });
    })?;
    Ok(EditorWindow {
        path: path_h,
        stop: shared.stop.clone(),
        closed: shared.closed.clone(),
        open_failed: shared.open_failed.clone(),
        painted: shared.painted.clone(),
        children: shared.children.clone(),
        width: shared.width.clone(),
        height: shared.height.clone(),
        xid: shared.xid.clone(),
        state: shared.state.clone(),
    })
}

thread_local! {
    static REG3: std::cell::RefCell<Vec<Editor3>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Route every edit the plugin UI reported into the shared parameter
/// cells (slider path: atomic write + generation bump). Free function: it
/// borrows the plugin and the cells separately (the state machine holds
/// the plugin mutably while pumping).
fn drain_param_edits3(
    plugin: &vst3_host::Plugin,
    ids: &[u32],
    params: &crate::vsthost::VstParams,
) -> bool {
    let changes = plugin.get_parameter_changes();
    let touched = !changes.is_empty();
    route_param_edits3(changes, ids, params);
    touched
}

/// The host half of VST3 editor→sound sync: an edit reported as
/// (parameter id, normalized value) lands in the `VstParams` CELL with
/// that id; unknown ids are dropped. Ids and cells are separate spaces —
/// the plugin speaks ids, automation and sliders speak cell indices.
pub(crate) fn route_param_edits3(
    changes: Vec<(u32, f64)>,
    ids: &[u32],
    params: &crate::vsthost::VstParams,
) {
    for (id, value) in changes {
        if let Some(cell) = ids.iter().position(|x| *x == id) {
            params.set(cell, value as f32);
        }
    }
}

/// Advance every open VST3 editor one step; called by `pump_editors` on
/// the shared GUI-thread cadence — `service_run_loop` IS the VST3 editor's
/// idle pump (timers + X fds registered with the host frame).
fn pump_vst3_editors() {
    REG3.with(|reg| {
        let mut eds = reg.borrow_mut();
        for e in eds.iter_mut() {
            if e.shared.stop.load(Ordering::SeqCst) {
                if !matches!(e.phase, Phase::Done) {
                    e.teardown(false);
                }
                continue;
            }
            e.step();
        }
        eds.retain(|e| !matches!(e.phase, Phase::Done));
    });
}

/// Load the shadow instance, size and map a BARE container (the VST2 rule
/// applies to every plugin toolkit we have measured: WM property requests
/// before the attach break child-window mapping), attach the editor view,
/// and hand back the state machine. `None` retires with the failure flags
/// already set on `shared`.
unsafe fn make_editor3(
    path: String,
    title: String,
    sample_rate: u32,
    initial: Vec<(u32, f64)>,
    state: Option<Vec<u8>>,
    ids: Vec<u32>,
    params: Arc<crate::vsthost::VstParams>,
    shared: Shared,
) -> Option<Editor3> {
    install_x_error_handler();
    let d = XOpenDisplay(ptr::null());
    if d.is_null() {
        shared.open_failed.store(true, Ordering::SeqCst);
        shared.closed.store(true, Ordering::SeqCst);
        return None;
    }
    let fail = |d: *mut Display, win: Option<XWindow>| -> Option<Editor3> {
        if let Some(w) = win {
            XDestroySubwindows(d, w);
            XDestroyWindow(d, w);
        }
        XCloseDisplay(d);
        shared.open_failed.store(true, Ordering::SeqCst);
        shared.closed.store(true, Ordering::SeqCst);
        None
    };
    // Shadow instantiation runs under the process-wide loader lock, on
    // the GUI thread — the same discipline as every other plugin load
    // (unsynchronized toolkit globals).
    let mut plugin = {
        let _loader = crate::vsthost::vst_loader_lock();
        match vst3_host::simple::load_plugin_with_settings(
            &path,
            sample_rate as f64,
            crate::vsthost::VST_BLOCK,
        ) {
            Ok(p) => p,
            Err(e) => {
                if std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some() {
                    eprintln!("[vstedit3] {title}: shadow load failed: {e}");
                }
                return fail(d, None);
            }
        }
    };
    // v20: restore the live instance's state chunk FIRST when it has one
    // (the instance was (re)opened from a harvested preset), then layer the
    // cell mirror on top: cells hold everything the user moved since, and
    // both come from the SAME instance the previous editor kept in sync.
    // A refused chunk (vendor quirks) degrades to cell-only priming.
    if let Some(bytes) = &state {
        if let Err(e) = plugin.load_state(bytes) {
            if std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some() {
                eprintln!("[vstedit3] {title}: shadow load_state refused ({e}) — cell priming only");
            }
        }
    }
    // Prime from the sounding instance BEFORE the view exists, so the
    // editor paints its real current state, and adopt the preferred size.
    for (id, v) in &initial {
        let _ = plugin.set_parameter(*id, *v);
    }
    let (w0, h0) = plugin.get_editor_size().unwrap_or((720, 480));
    let (w0, h0) = (w0.clamp(200, 3840), h0.clamp(150, 2160));
    // The container opens at exactly this size — publish it now; the
    // Live phase keeps it honest against the attached view.
    shared.width.store(w0, Ordering::SeqCst);
    shared.height.store(h0, Ordering::SeqCst);
    let root = root_window(d);
    let win = XCreateSimpleWindow(d, root, 0, 0, w0, h0, 0, CONTAINER_BG, CONTAINER_BG);
    shared.xid.store(win as usize, Ordering::SeqCst);
    let wm_delete = XInternAtom(d, c"WM_DELETE_WINDOW".as_ptr(), 0);
    let wm_protocols = XInternAtom(d, c"WM_PROTOCOLS".as_ptr(), 0);
    XMapWindow(d, win);
    XSync(d, 0);
    {
        let _loader = crate::vsthost::vst_loader_lock();
        if let Err(e) = plugin.open_editor(vst3_host::WindowHandle::from_x11(win as u32)) {
            if std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some() {
                eprintln!("[vstedit3] {title}: open_editor refused: {e}");
            }
            return fail(d, Some(win));
        }
    }
    let dbg = std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some();
    if dbg {
        eprintln!("[vstedit3] {title}: view attached to {win:#x} ({w0}x{h0})");
    }
    EDITORS_ACTIVE.fetch_add(1, Ordering::Relaxed);
    Some(Editor3 {
        plugin: Some(plugin),
        ids,
        params,
        sr: sample_rate,
        d,
        win,
        title,
        wm_protocols,
        wm_delete,
        phase: Phase::Verify { it: 0 },
        shared,
        dbg,
        created: Instant::now(),
        house: Instant::now(),
        dead_ticks: 0,
        changed: false,
    })
}

impl Editor3 {
    /// One pump: X events, `service_run_loop`, editor→host parameter
    /// drain, size tracking.
    fn step(&mut self) {
        unsafe {
            let Some(plugin) = self.plugin.as_mut() else {
                self.teardown(true);
                return;
            };
            match self.phase {
                Phase::Done => {}
                Phase::Verify { it } => {
                    let mut _resized = None;
                    if drain_events(
                        self.d,
                        self.win,
                        self.wm_protocols,
                        self.wm_delete,
                        &mut _resized,
                    ) {
                        self.teardown(false);
                        return;
                    }
                    plugin.service_run_loop();
                    if drain_param_edits3(plugin, &self.ids, &self.params) {
                        self.changed = true;
                    }
                    // Steinberg's X11 view creates a real child window;
                    // the same live-child / painted-pixel verdict as VST2.
                    let child_live = first_sized_child(self.d, self.win)
                        .is_some_and(|(k, w, h)| w >= 8 && h >= 8 && is_viewable(self.d, k));
                    let px = container_painted(self.d, self.win);
                    if self.dbg && (it < 3 || it % 10 == 0) {
                        let (kid, w, h) = first_sized_child(self.d, self.win).unwrap_or((0, 0, 0));
                        eprintln!(
                            "[vstedit3] verify {it}: child=({kid:x} {w}x{h}) px={px} live={child_live}"
                        );
                    }
                    if child_live || px {
                        // Views sometimes only commit their real rect once
                        // mapped: take the attached size before the borrow
                        // of the plugin is dropped for decorate/resize.
                        let attached = match plugin.attached_editor_size() {
                            Ok(Some((w, h))) if w > 0 && h > 0 => Some((w, h)),
                            _ => None,
                        };
                        self.shared.painted.store(true, Ordering::SeqCst);
                        self.decorate();
                        self.phase = match attached {
                            Some((w, h)) => {
                                self.shared.width.store(w, Ordering::SeqCst);
                                self.shared.height.store(h, Ordering::SeqCst);
                                XResizeWindow(self.d, self.win, w, h);
                                Phase::Live {
                                    last_size: (w, h),
                                    last_container: (0, 0),
                                }
                            }
                            None => Phase::Live {
                                last_size: (0, 0),
                                last_container: (0, 0),
                            },
                        };
                        return;
                    }
                    if self.created.elapsed() >= VERIFY_PATIENCE {
                        self.teardown(true);
                    } else {
                        self.phase = Phase::Verify { it: it + 1 };
                    }
                }
                Phase::Live {
                    ref mut last_size,
                    ref mut last_container,
                } => {
                    let mut resized = None;
                    if drain_events(
                        self.d,
                        self.win,
                        self.wm_protocols,
                        self.wm_delete,
                        &mut resized,
                    ) {
                        self.teardown(false);
                        return;
                    }
                    // The VST3 message pump: this IS the editor's idle
                    // service (timers + registered X fds), and the edit
                    // drain is the slider path back into the sound.
                    plugin.service_run_loop();
                    if drain_param_edits3(plugin, &self.ids, &self.params) {
                        self.changed = true;
                    }
                    // The plugin may request a size change (e.g. the
                    // view only commits its real rect once mapped).
                    if let Some((w, h)) = plugin.take_editor_resize_request() {
                        if w > 0 && h > 0 {
                            XResizeWindow(self.d, self.win, w, h);
                        }
                    }
                    // The window manager resized our frame: ask the view
                    // to relayout into it (R1b parity with VST2).
                    if let Some((w, h)) = resized {
                        if (w, h) != *last_container && w >= 8 && h >= 8 {
                            *last_container = (w, h);
                            let _ = plugin.resize_editor(w as i32, h as i32);
                        }
                    }
                    // Housekeeping on a timer (v16, VST2 parity): the
                    // watchdog and view-size tracking are not per-step
                    // operations.
                    if self.house.elapsed() >= HOUSEKEEPING {
                        self.house = Instant::now();
                        // v20: a plugin that changes patch data WITHOUT
                        // broadcasting performEdits (preset reload) still
                        // announces itself through restartComponent.
                        if !plugin.take_restart_flags().is_empty() {
                            self.changed = true;
                        }
                        let kids = count_children_local(self.d, self.win);
                        self.shared.children.store(kids, Ordering::SeqCst);
                        let alive = first_sized_child(self.d, self.win)
                            .is_some_and(|(k, w, h)| w >= 8 && h >= 8 && is_viewable(self.d, k))
                            || container_painted(self.d, self.win);
                        self.dead_ticks = if alive { 0 } else { self.dead_ticks + 1 };
                        if self.dead_ticks > DEAD_TICKS_PATIENCE {
                            self.teardown(true);
                            return;
                        }
                        // Track the view size; keep the container to it.
                        if let Ok(Some((w, h))) = plugin.attached_editor_size() {
                            if w > 0 && h > 0 {
                                self.shared.width.store(w, Ordering::SeqCst);
                                self.shared.height.store(h, Ordering::SeqCst);
                                if (w, h) != *last_size {
                                    *last_size = (w, h);
                                    XResizeWindow(self.d, self.win, w, h);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Name + WM close protocol + event selection — only after the view
    /// proved itself (see the bare-container rule in `make_editor3`).
    unsafe fn decorate(&mut self) {
        XSelectInput(
            self.d,
            self.win,
            STRUCTURE_NOTIFY_MASK | KEY_PRESS_MASK | KEY_RELEASE_MASK | FOCUS_CHANGE_MASK,
        );
        if let Ok(cname) = CString::new(format!("slooper \u{2014} {}", self.title)) {
            XStoreName(self.d, self.win, cname.as_ptr());
        }
        let mut protocols = [self.wm_delete];
        XSetWMProtocols(self.d, self.win, protocols.as_mut_ptr(), 1);
        XFlush(self.d);
    }

    /// Detach the view, retire the shadow instance under the loader lock,
    /// destroy the container, mark the handle retired.
    fn teardown(&mut self, refused: bool) {
        unsafe {
            // v20: harvest BEFORE anything is torn down. If the user edited
            // through this shadow, its component state holds patch changes
            // the sounding instance never saw (preset browsers, non-param
            // state). Hand the chunk to the host via `EditorWindow::
            // take_state`; the host rebuilds the live instance from it.
            if self.changed {
                if let Some(p) = self.plugin.as_mut() {
                    // v20 PROBE FACT (v20_chunkdiff example): host-set
                    // parameter values are NOT part of a plugin's
                    // serializable state until the plugin has actually
                    // RUN audio with them — wolf-shaper and 3BandEQ wrote
                    // byte-identical chunks until one silent block ran
                    // (DPF's isLive deferral; the same pattern the VST3
                    // SDK teaches). The shadow never processes during
                    // its life, so push a few silent blocks through NOW,
                    // otherwise the chunk would lag every knob the user
                    // moved and rebuilding from it could UNDO edits.
                    if let Err(e) = p.start_processing() {
                        if self.dbg {
                            eprintln!("[vstedit3] {}: state-flush start failed: {e}", self.title);
                        }
                    } else {
                        let ch = p.output_channel_count().max(1).max(0) as usize;
                        let mut bufs = vst3_host::audio::AudioBuffers::new(
                            ch,
                            ch,
                            crate::vsthost::VST_BLOCK,
                            self.sr.max(1) as f64,
                        );
                        for _ in 0..4 {
                            if p.process_audio(&mut bufs).is_err() {
                                break;
                            }
                        }
                        let _ = p.stop_processing();
                    }
                    match p.save_state() {
                        Ok(bytes) => {
                            if let Ok(mut cell) = self.shared.state.lock() {
                                *cell = Some(bytes);
                            }
                        }
                        Err(e) => {
                            if self.dbg {
                                eprintln!("[vstedit3] {}: save_state failed: {e}", self.title);
                            }
                        }
                    }
                }
            }
            // View first, plugin second — close_editor must run before
            // the instance tears down.
            let plugin = self.plugin.take();
            if let Some(mut p) = plugin {
                {
                    let _loader = crate::vsthost::vst_loader_lock();
                    let _ = p.close_editor();
                }
                // Plugin drop tears the instance down; keep it on this
                // thread under the loader lock like every other teardown.
                drop(p);
            }
            if !self.d.is_null() {
                EDITORS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
                XDestroySubwindows(self.d, self.win);
                XDestroyWindow(self.d, self.win);
                XFlush(self.d);
                XCloseDisplay(self.d);
                self.d = ptr::null_mut();
            }
            self.shared.open_failed.store(refused, Ordering::SeqCst);
            self.shared.closed.store(true, Ordering::SeqCst);
            self.phase = Phase::Done;
        }
    }
}

impl Drop for Editor3 {
    fn drop(&mut self) {
        if !matches!(self.phase, Phase::Done) {
            self.teardown(true);
        }
    }
}

/// Whether an X display is configured for the session. Env-only by
/// design: opening a probe display here would drag the UI thread into
/// libX11 alongside the GUI thread's live connections.
pub fn x11_available() -> bool {
    std::env::var_os("DISPLAY").is_some()
}

/// Child count on the thread's own connection (XQueryTree needs no round
/// trip through a new connection here).
unsafe fn count_children_local(d: *mut Display, win: XWindow) -> usize {
    let mut root: XWindow = 0;
    let mut parent: XWindow = 0;
    let mut kids: *mut XWindow = ptr::null_mut();
    let mut n: c_int = 0;
    let ok = XQueryTree(d, win, &mut root, &mut parent, &mut kids, &mut n);
    let count = if ok != 0 { n.max(0) as usize } else { 0 };
    if !kids.is_null() {
        XFree(kids as *mut c_void);
    }
    count
}

/// First plugin child window with non-degenerate geometry (created AND
/// resized — an editor that created a window but crashed mid-build leaves
/// it 1x1, the tell-tale of the "blank editor window" bug).
unsafe fn first_sized_child(d: *mut Display, win: XWindow) -> Option<(XWindow, u32, u32)> {
    let mut root: XWindow = 0;
    let mut parent: XWindow = 0;
    let mut kids: *mut XWindow = ptr::null_mut();
    let mut n: c_int = 0;
    if XQueryTree(d, win, &mut root, &mut parent, &mut kids, &mut n) == 0 {
        return None;
    }
    let list = if kids.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(kids, n.max(0) as usize).to_vec()
    };
    if !kids.is_null() {
        XFree(kids as *mut c_void);
    }
    for k in &list {
        let mut rr: XWindow = 0;
        let (mut x, mut y, mut w, mut h, mut b, mut depth) = (0, 0, 0u32, 0u32, 0u32, 0u32);
        if XGetGeometry(
            d, *k, &mut rr, &mut x, &mut y, &mut w, &mut h, &mut b, &mut depth,
        ) != 0
            && w > 0
            && h > 0
        {
            return Some((*k, w, h));
        }
    }
    list.first().copied().map(|k| (k, 0, 0))
}

// `XRootWindow`/`XDefaultScreen` are legacy-but-exported libX11 functions
// (verified against the system libX11 symbol table); `DefaultRootWindow`
// itself is an Xlib macro, so go through the functions instead.
extern "C" {
    fn XRootWindow(d: *mut Display, screen: c_int) -> XWindow;
    fn XDefaultScreen(d: *mut Display) -> c_int;
}
unsafe fn root_window(d: *mut Display) -> XWindow {
    XRootWindow(d, XDefaultScreen(d))
}

// ---------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    /// Real-plugin editor smoke: loads the first VST2 present,
    /// opens its editor in a live X11 window, pumps idle, and verifies the
    /// plugin actually parented a window into our container. Gated on
    /// `SLOOPER_VST_SMOKE=1` + an installed plugin + DISPLAY, so CI skips
    /// it. AC4.2 of REQUIREMENTS.md.
    #[test]
    fn vst2_editor_smoke_if_present() {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" {
            eprintln!("skip: set SLOOPER_VST_SMOKE=1 to run");
            return;
        }
        if !x11_available() {
            eprintln!("skip: no X11 display");
            return;
        }
        let cand = [
            dirs_home().join(".vst/wolf-shaper-vst.so"),
            dirs_home().join(".vst/DragonflyHallReverb-vst.so"),
        ];
        let Some(path) = cand.iter().find(|p| p.exists()).cloned() else {
            eprintln!("skip: no known VST2 plugin installed");
            return;
        };
        let load = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None)
            .expect("plugin loads");
        let entry = &load.entry;
        assert!(
            entry.has_editor(),
            "{} advertises an editor",
            path.display()
        );
        let h = entry.vst2_handle().expect("vst2 handle");
        let mut win = match open_editor(&h, &path.to_string_lossy(), "smoke") {
            Ok(w) => w,
            Err(e) => panic!("open_editor failed: {e}"),
        };
        // Give the editor thread time to embed (verify) + pump.
        std::thread::sleep(Duration::from_millis(4200));
        // The invariant slooper guarantees: the window shows a LIVE
        // plugin editor, or it is gone with an honest failure — never a
        // stuck blank panel (the historical "uncloseable blank window"
        // complaint from a plugin that opened but never painted).
        if win.open_failed() {
            eprintln!(
                "note: {} refused/did not paint an editor (thread closed cleanly) \u{2014} \
                 UI will fall back to sliders",
                path.display()
            );
            assert!(win.is_closed(), "failed editor must close its window");
            assert_eq!(win.child_count(), 0, "failed editor must destroy children");
        } else {
            assert!(!win.is_closed(), "editor thread died while painted");
            assert!(win.painted(), "editor window shows no live plugin UI");
            assert!(win.editor_size().0 > 0, "no editor size negotiated");
        }
        win.close();
        assert!(win.is_closed());
    }

    fn dirs_home() -> std::path::PathBuf {
        std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
    }

    /// Shared gate for the live-X pins: explicit opt-in + X11 + a plugin.
    fn smoke_gate(plugin: &str) -> Option<std::path::PathBuf> {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" || !x11_available() {
            return None;
        }
        let p = dirs_home().join(plugin);
        p.exists().then_some(p)
    }

    /// v10.1 pin (originally the amsynth blank-panel report; that plugin
    /// has since been REMOVED from this host, so this pin now needs an
    /// explicit path): a plugin whose editor dies mid-build — child window
    /// stays 1x1 — must be REFUSED: container destroyed, thread exited,
    /// `open_failed` set so the UI falls back to sliders. The historical
    /// bug: the paint probe compared against black, and the container's
    /// own background counted as "painted", leaving a permanent blank,
    /// uncloseable window. Set `SLOOPER_BROKEN_EDITOR=/path/to/plugin.so`
    /// to run it against a known-broken editor; skipped otherwise.
    #[test]
    fn v101_vstedit_refuses_unpainted_editor() {
        let path = match std::env::var_os("SLOOPER_BROKEN_EDITOR").map(std::path::PathBuf::from) {
            Some(p) if p.is_file() => p,
            _ => {
                eprintln!("skip: SLOOPER_BROKEN_EDITOR=<broken .so> required");
                return;
            }
        };
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" || !x11_available() {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 + X11 display required");
            return;
        }
        let load = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None)
            .expect("plugin loads");
        let entry = &load.entry;
        let h = entry.vst2_handle().expect("vst2 handle");
        let mut win = open_editor(&h, &path.to_string_lossy(), "pin-refuse").expect("open_editor");
        // Verification is 100 x 30 ms (shared pump); slack ok.
        let mut waited = 0u32;
        while !win.open_failed() && !win.painted() && waited < 8000 {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
        }
        if win.painted() {
            eprintln!(
                "note: {} paints a live editor on this build; pin passes trivially",
                path.display()
            );
            win.close();
            return;
        }
        assert!(
            win.open_failed(),
            "unpainted editor must report open_failed"
        );
        assert!(
            win.is_closed(),
            "refused editor must destroy its window and exit"
        );
        assert_eq!(win.child_count(), 0, "no children may survive the refuse");
        win.close();
    }

    /// v10.1 pin (uncloseable-window report): a `WM_DELETE_WINDOW` client
    /// message — exactly what the window manager sends for the title-bar
    /// X — must close a painted editor within seconds. The old pump tested
    /// `XNextEvent(...) == 0` as an error, but 0 IS success, so every
    /// event was popped and dropped and the close button did nothing.
    #[test]
    fn v101_vstedit_wm_delete_closes_editor() {
        let Some(path) = smoke_gate(".vst/wolf-shaper-vst.so") else {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 + ~/.vst/wolf-shaper-vst.so required");
            return;
        };
        let load = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None)
            .expect("plugin loads");
        let entry = &load.entry;
        let h = entry.vst2_handle().expect("vst2 handle");
        let mut win = open_editor(&h, &path.to_string_lossy(), "pin-close").expect("open_editor");
        let mut waited = 0u32;
        while !win.painted() && !win.is_closed() && waited < 8000 {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
        }
        if !win.painted() {
            eprintln!("skip: wolf-shaper editor did not paint in 8 s (no X session?)");
            win.close();
            return;
        }
        let xid = win.xid();
        assert_ne!(xid, 0, "container XID must be published");
        assert!(request_wm_delete(xid), "WM_DELETE injection failed");
        let mut waited = 0u32;
        while !win.is_closed() && waited < 4000 {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
        }
        assert!(
            win.is_closed(),
            "editor must exit on WM_DELETE_WINDOW (close button)"
        );
        win.close();
    }

    /// v10.2 pin: `is_viewable` must distinguish a MAPPED window from a
    /// merely-created one, and the opaque `XWindowAttributes` field
    /// offsets (width@8, map_state@92) must match the live libX11 struct.
    /// A created-but-unmapped window is exactly what the amsynth crash
    /// leaves behind, and the old geometry-only probe accepted it.
    #[test]
    fn v102_xattr_map_state_pin() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skip: no X11 display");
            return;
        }
        unsafe {
            let d = XOpenDisplay(ptr::null());
            assert!(!d.is_null());
            XSetErrorHandler(Some(x_error_noop));
            let root = root_window(d);
            let mapped = XCreateSimpleWindow(d, root, 0, 0, 64, 48, 0, 0, 0);
            let unmapped = XCreateSimpleWindow(d, root, 0, 0, 64, 48, 0, 0, 0);
            XMapWindow(d, mapped);
            XSync(d, 0);
            // Map is asynchronous: poll briefly.
            let mut is_m = false;
            for _ in 0..50 {
                is_m = is_viewable(d, mapped);
                if is_m {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(is_m, "mapped window must read viewable (offsets pinned)");
            assert!(
                !is_viewable(d, unmapped),
                "unmapped window must NOT read viewable"
            );
            // Garbage XID must fail closed, not crash.
            assert!(!is_viewable(d, 0x0F00_0000_0000_0001));
            XDestroyWindow(d, mapped);
            XDestroyWindow(d, unmapped);
            XFlush(d);
            XCloseDisplay(d);
        }
    }

    /// v10.2 ACCEPTANCE pin (the "VST plugin GUIs don't work" report):
    /// every installed VST2 with an editor must reach PAINTED through the
    /// public API — proof that the shared GUI-thread model (load, open,
    /// idle all on one thread) actually shows plugin UI. This used to be
    /// impossible: editors opened on their own thread stayed unmapped
    /// forever because JUCE/FLTK had bound their event loops elsewhere.
    /// Fails (does not skip) when a present plugin never paints.
    #[test]
    fn v102_every_vst2_editor_paints() {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 required");
            return;
        }
        if !x11_available() {
            eprintln!("skip: no X11 display");
            return;
        }
        let dir = dirs_home().join(".vst");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            eprintln!("skip: no ~/.vst");
            return;
        };
        let mut files: Vec<_> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "so"))
            .collect();
        files.sort();
        let mut opened = 0usize;
        let mut fails = Vec::new();
        for path in files {
            let Ok(load) = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None)
            else {
                continue; // broken plugin: not this pin's problem
            };
            let entry = &load.entry;
            if !entry.has_editor() {
                continue;
            }
            let h = entry.vst2_handle().expect("vst2 handle");
            let mut win =
                open_editor(&h, &path.to_string_lossy(), "pin-paint").expect("open_editor");
            let mut waited = 0u32;
            while !win.painted() && !win.is_closed() && waited < 10_000 {
                std::thread::sleep(Duration::from_millis(100));
                waited += 100;
            }
            if !win.painted() {
                fails.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
            win.close();
            opened += 1;
        }
        assert_eq!(fails, Vec::<String>::new(), "editors that never painted");
        assert!(
            opened >= 3,
            "expected the installed VST2 editors, got {opened}"
        );
        eprintln!("[v102] {opened} VST2 editors painted through the shared GUI thread");
    }

    // ===================== v0.12 acceptance pins =====================

    /// R4 (host half, always on): VST3 editor edits speak PARAMETER IDS;
    /// the shared cells are addressed by index. The route must map id →
    /// cell exactly and drop unknown ids — this is what makes moving a knob
    /// in a hosted VST3 window change the sound (the plugin-side half,
    /// `get_parameter_changes`, is fed by `IComponentHandler::performEdit`).
    #[test]
    fn v12_vst3_param_routing_maps_ids_to_cells() {
        use crate::vsthost::{VstParamDesc, VstParams};
        let descs = [
            VstParamDesc {
                id: 5,
                name: "a".into(),
                default: 0.0,
                writable: true,
            },
            VstParamDesc {
                id: 9,
                name: "b".into(),
                default: 0.0,
                writable: true,
            },
        ];
        let params = VstParams::new(&descs);
        let ids: Vec<u32> = descs.iter().map(|d| d.id).collect();
        let g0 = params.generation();
        route_param_edits3(vec![(9, 0.42), (777, 0.5)], &ids, &params);
        assert_eq!(
            params.get(1).to_bits(),
            (0.42f32).to_bits(),
            "id 9 → cell 1"
        );
        assert_eq!(params.get(0).to_bits(), 0.0f32.to_bits(), "id 5 untouched");
        assert!(
            params.generation() > g0,
            "cell write must bump the generation (RT pick-up)"
        );
        // Unknown ids alone: no cell moves, no generation churn.
        let g1 = params.generation();
        route_param_edits3(vec![(12345, 0.9)], &ids, &params);
        assert_eq!(
            params.generation(),
            g1,
            "unknown id must not churn the cells"
        );
    }

    /// Shared live gate for the VST3 editor pins.
    fn vst3_smoke_gate() -> Option<std::path::PathBuf> {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" || !x11_available() {
            return None;
        }
        let p = dirs_home().join(".vst3/wolf-shaper.vst3");
        p.exists().then_some(p)
    }

    /// R2: a hosted VST3 plugin's OWN window must reach PAINTED through
    /// the same verified-paint machinery as VST2 (shadow instance inside):
    /// editor child mapped with real geometry, no blank panel on refusal,
    /// clean teardown on close.
    #[test]
    fn v12_vst3_editor_paints() {
        let Some(path) = vst3_smoke_gate() else {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 + ~/.vst3/wolf-shaper.vst3 required");
            return;
        };
        let load = crate::vsthost::open(&path, crate::vsthost::VstKind::V3, 48_000, None)
            .expect("wolf-shaper VST3 loads");
        let entry = &load.entry;
        assert!(entry.has_editor(), "wolf-shaper VST3 advertises an editor");
        let mut win = entry.open_vst3_editor(48_000).expect("shadow editor opens");
        let mut waited = 0u32;
        while !win.painted() && !win.is_closed() && waited < 8_000 {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
        }
        assert!(win.painted(), "VST3 editor never painted ({} ms)", waited);
        assert!(win.editor_size().0 > 0, "no editor size negotiated");
        win.close();
        assert!(win.is_closed(), "closed editor must retire");
    }

    /// R3: the window manager's close button works on VST3 windows too
    /// (WM_DELETE lands in the container's pump → view detached, container
    /// destroyed, handle retired — the historical VST2 "uncloseable panel"
    /// bug must not be reimported with the new generation).
    #[test]
    fn v12_vst3_wm_delete_closes_editor() {
        let Some(path) = vst3_smoke_gate() else {
            eprintln!("skip: gated");
            return;
        };
        let load =
            crate::vsthost::open(&path, crate::vsthost::VstKind::V3, 48_000, None).expect("loads");
        let entry = &load.entry;
        let win = entry.open_vst3_editor(48_000).expect("opens");
        let mut waited = 0u32;
        while !win.painted() && !win.is_closed() && waited < 8_000 {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
        }
        assert!(win.painted(), "paint first");
        let xid = win.xid();
        assert_ne!(xid, 0, "container XID published");
        assert!(request_wm_delete(xid), "WM_DELETE injected");
        let mut closed = 0u32;
        while !win.is_closed() && closed < 5_000 {
            std::thread::sleep(Duration::from_millis(50));
            closed += 50;
        }
        assert!(win.is_closed(), "WM close must retire a VST3 editor");
    }

    /// R3: the v0.11 crash report (close + reopen killed the program)
    /// must NOT be reimported along the VST3 path — the exact user cycle,
    /// three times, WM-close each round.
    #[test]
    fn v12_vst3_editor_reopen_cycles() {
        let Some(path) = vst3_smoke_gate() else {
            eprintln!("skip: gated");
            return;
        };
        let load =
            crate::vsthost::open(&path, crate::vsthost::VstKind::V3, 48_000, None).expect("loads");
        let entry = &load.entry;
        for cycle in 0..3 {
            let mut win = entry.open_vst3_editor(48_000).expect("open cycle");
            let mut waited = 0u32;
            while !win.painted() && !win.is_closed() && waited < 8_000 {
                std::thread::sleep(Duration::from_millis(100));
                waited += 100;
            }
            assert!(win.painted(), "cycle {cycle}: never painted");
            assert!(request_wm_delete(win.xid()), "cycle {cycle}: delete sent");
            let mut closed = 0u32;
            while !win.is_closed() && closed < 5_000 {
                std::thread::sleep(Duration::from_millis(50));
                closed += 50;
            }
            assert!(win.is_closed(), "cycle {cycle}: WM close ignored");
            // Explicit close AFTER the WM close retired it: must be a
            // no-op, never a double-free of view or container.
            win.close();
        }
    }

    // ================= v16 responsiveness pass pins ======================

    /// v16 helper: a real, PAINTED VST2 editor for the SMOKE pins, or
    /// `None` (skip) when this host can't provide one.
    fn v16_editor(tag: &str) -> Option<EditorWindow> {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1"
            || std::env::var_os("DISPLAY").is_none()
        {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 + DISPLAY required");
            return None;
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::path::PathBuf::from(format!("{home}/.vst/DragonflyHallReverb-vst.so"));
        if !path.is_file() {
            eprintln!("skip: Dragonfly VST2 not installed");
            return None;
        }
        let load = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None).ok()?;
        let h = load.entry.vst2_handle()?;
        let mut w = open_editor(&h, &path.to_string_lossy(), tag).ok()?;
        let t0 = std::time::Instant::now();
        while !w.painted()
            && !w.open_failed()
            && !w.is_closed()
            && t0.elapsed() < std::time::Duration::from_secs(12)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !w.painted() {
            eprintln!("skip: editor never painted");
            w.close();
            return None;
        }
        // Keep the instance alive next to its editor for the pin's whole
        // lifetime (the plugin instance must outlive `open_editor`).
        std::mem::forget(load);
        Some(w)
    }

    /// AC2 pin (SMOKE): Live housekeeping (child count, editor rect,
    /// painted probe, WM-watchdog) now runs on a ~500 ms timer instead of
    /// every pump step — a healthy editor must survive several cycles.
    /// The v10 crash this guards was exactly "housekeeping churn killed a
    /// live editor"; with the 6 ms fast tick a per-step refresh would run
    /// ~5x harder AND `XGetGeometry` storms on some plugins mis-reported
    /// the child as gone.
    #[test]
    fn v16_vstedit_live_survives_housekeeping() {
        let Some(mut w) = v16_editor("v16-ac2") else {
            return;
        };
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_secs(4) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !w.is_closed(),
            "time-based housekeeping must not retire an editor that is alive"
        );
        assert!(w.painted(), "the editor must still be painting after 4 s");
        w.close();
    }

    /// AC3 pin (SMOKE): `close()` posts a wakeup job, so the hand-off
    /// lands on the next pump iteration instead of waiting out a full
    /// idle tick plus scheduling jitter — must settle well under 150 ms.
    #[test]
    fn v16_vstedit_close_settles_fast() {
        let Some(mut w) = v16_editor("v16-ac3") else {
            return;
        };
        let t0 = std::time::Instant::now();
        w.close();
        let dt = t0.elapsed();
        assert!(w.is_closed(), "close() must settle the editor");
        assert!(
            dt < std::time::Duration::from_millis(150),
            "close() settled in {} ms — the wakeup hand-off regressed",
            dt.as_millis()
        );
    }

    /// AC7 pin: the painted-probe `XGetImage` on an unmapped or garbage
    /// window used to hit `BadMatch` and the Xlib DEFAULT handler killed
    /// the process (core dump reproduced in the v16 probe). Now: the
    /// no-op error handler is installed before any editor display, and
    /// the probe is guarded by `is_viewable` — both must fail closed.
    #[test]
    fn v16_vstedit_survives_x_errors() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skip: no X11 display");
            return;
        }
        unsafe {
            install_x_error_handler();
            let d = XOpenDisplay(ptr::null());
            assert!(!d.is_null());
            let root = root_window(d);
            let win = XCreateSimpleWindow(d, root, 0, 0, 64, 48, 0, 0, 0);
            XSync(d, 0);
            // Created-but-unmapped: exactly the BadMatch case from the
            // pump pin's core dump. Probe must answer false and LIVE.
            assert!(
                !container_painted(d, win),
                "unmapped window must fail closed"
            );
            // Garbage XID: BadWindow on the attribute fetch — also live.
            assert!(
                !container_painted(d, 0x0F00_0000_0000_0001),
                "bogus XID must fail closed"
            );
            // A WM_DELETE client message to a bogus window must be
            // swallowed the same way.
            assert!(request_wm_delete(0x0F00_0000_0000_0001), "send path ran");
            XDestroyWindow(d, win);
            XFlush(d);
            XCloseDisplay(d);
        }
    }
}
