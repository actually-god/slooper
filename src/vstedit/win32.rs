//! Hosting plugin custom editor windows — Win32 twin of the X11 container
//! path (WINDOWS-PORT W8a).
//!
//! # Why this is a twin and not a refactor
//!
//! The Linux path (`x11.rs`) ships and works; the port ground rule is that
//! Linux behavior stays byte-identical, so nothing here is shared source.
//! What IS shared is the *architecture*, ported concept for concept:
//!
//! * **Container per editor.** The host creates its own top-level window
//!   (own `WNDCLASS`, sentinel background brush); the plugin paints into it
//!   as `effEditOpen`'s `ptr` (VST2) or `IPlugView::attached` parent
//!   (VST3, `kPlatformTypeHWND`). On Win32 the plugin toolkits (JUCE,
//!   VSTGUI, FLTK) create child HWNDs exactly like their X11 side creates
//!   child windows, so the same "live sized child OR real pixels over the
//!   sentinel" verification transfers 1:1. There is no window-manager
//!   handshake to time around, so the bare-container dance the X11 path
//!   needs is not replicated (styles/title are final at creation; a hidden
//!   probe container is "unmapped" by simply never calling `ShowWindow`).
//! * **One pump.** All containers share the thread's Win32 message queue:
//!   `pump_editors` drains it (`PeekMessageW`/`TranslateMessage`/
//!   `DispatchMessageW`, bounded per tick) on the shared vst2-gui thread —
//!   never the UI thread, for the same toolkit-affinity reason the X11
//!   module docs give. The plugin's child window procedures are dispatched
//!   from the same pump, so plugin editors are the only other thing this
//!   queue holds work for.
//! * **Close protocol.** `WM_CLOSE` on the container is the twin of
//!   `WM_DELETE_WINDOW`: the window procedure records the request and
//!   refuses to destroy anything itself — teardown order (`effEditClose`
//!   / `close_editor` first, `DestroyWindow` second) is host-owned, exactly
//!   like the X11 path.
//! * **Fixed-size pin (v36 parity).** A plugin that refuses
//!   `effEditCanSizeWindow` gets its frame locked via `WM_GETMINMAXINFO`
//!   (min==max track size), the Win32 equivalent of the WM size hints.
//! * **Verified paint, not trusted.** Same sentinel discipline: distinct
//!   background color, `GetPixel` corner probe over it, and the watchdog
//!   retirement for editors that die mid-life. `container_painted` reading
//!   child-covered regions is unreliable on Win32 (child HWNDs own their
//!   pixels), which is fine: the child-live branch covers toolkits and the
//!   pixel branch covers direct drawing on the container itself.
//!
//! Known W8a deviations from the X11 path (documented on purpose, none
//! silent): no `audioMasterSizeWindow` synthetic Configure routing (children
//! get real `WM_SIZE`s), no key/focus relay (Win32 focus is standard), no
//! per-editor display connection, and VST3 content-scale/DPI plumbing is
//! W12 polish — geometry follows `WM_DPICHANGED` but the plugin is not
//! re-scaled through the host frame.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, HINSTANCE, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, GetDC, GetPixel, ReleaseDC, PAINTSTRUCT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, CW_USEDEFAULT, DefWindowProcW, DestroyWindow,
    DispatchMessageW, EnumChildWindows, GetClientRect, IDC_ARROW, LoadCursorW, MINMAXINFO, MSG,
    PeekMessageW, PostMessageW, RegisterClassExW, SetWindowPos, SetWindowTextW, TranslateMessage,
    WNDCLASSEXW, WNDCLASS_STYLES, WINDOW_EX_STYLE, WINDOW_STYLE, CS_HREDRAW, CS_VREDRAW,
    IsWindow, IsWindowVisible, PM_REMOVE, SWP_NOACTIVATE, SWP_NOZORDER, WM_CLOSE, WM_DESTROY,
    WM_DPICHANGED, WM_GETMINMAXINFO, WM_PAINT, WM_SIZE, WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};

use crate::vsthost::Vst2Handle;

// ---------------------------------------------------------------- constants
//
// Timing values are deliberately identical to the X11 twin (VERIFY
// patience, housekeeping beat, dead-editor patience): the same plugin
// toolkits are being waited on.

/// VST2 editor opcodes (aeff.h / vestige.h, cross-checked against the
/// vendored vst2 crate's OpCode enum).
const EFF_EDIT_GET_RECT: i32 = 13;
const EFF_EDIT_OPEN: i32 = 14;
const EFF_EDIT_CLOSE: i32 = 15;
const EFF_EDIT_IDLE: i32 = 19;

// VST2.4 editor resize negotiation (aeff.h).
const EFF_EDIT_CAN_SIZE: i32 = 79;
const EFF_EDIT_SIZE: i32 = 80;

/// `ERect` from the VST2 SDK: four 16-bit screen coordinates.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ERect {
    top: i16,
    left: i16,
    bottom: i16,
    right: i16,
}

/// Distinctive container background (COLORREF 0x00BBGGRR of RGB(42,22,180)
/// — the X twin's `0x2A16B4`). The "did the plugin paint?" probe compares
/// pixels against THIS, never against black (the X twin documents why at
/// length; the reasoning is pixel-protocol-independent).
const CONTAINER_BG: u32 = 0x00B4_162A;
/// `GetPixel` failure / fully clipped answer.
const CLR_INVALID: u32 = 0xFFFF_FFFF;

/// Window-system housekeeping beat — see the X twin's v16 note: healthy
/// editors do not need existence checks every pump step.
const HOUSEKEEPING: Duration = Duration::from_millis(500);
/// Model-sync beat (VST3 shadow editors) — knob-freshness cadence.
const MODEL_SYNC: Duration = Duration::from_millis(150);
/// ~4 s of housekeeping ticks without proof of life means the editor died.
const DEAD_TICKS_PATIENCE: u32 = 8;
/// Verify patience: real time for the plugin to show a window.
const VERIFY_PATIENCE: Duration = Duration::from_secs(3);

/// Live editor windows across both generations (the pump thread reads this
/// through `vsthost::gui_tx` to pick its tick).
static EDITORS_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// How many plugin editor windows the GUI thread is serving right now.
pub fn editors_active() -> usize {
    EDITORS_ACTIVE.load(Ordering::Relaxed)
}

// --------------------------------------------------------------- probe mode
//
// Same crash-guard contract as the X twin: in the probe child every
// container is created WITHOUT `WS_VISIBLE` (never shown), so a safety
// check can never paint a window the user sees, while `effEditOpen` still
// runs the toolkit bootstrap the guard exists to catch.

static PROBE_MODE: AtomicBool = AtomicBool::new(false);

fn probe_mode() -> bool {
    PROBE_MODE.load(Ordering::SeqCst)
}

/// Mark this process as a throwaway editor probe (call before any
/// window exists; `probe_child` does).
pub fn enter_probe_mode() {
    PROBE_MODE.store(true, Ordering::SeqCst);
}

/// Test seam: set probe mode, return the previous value.
pub fn set_probe_mode(v: bool) -> bool {
    PROBE_MODE.swap(v, Ordering::SeqCst)
}

// ------------------------------------------------------------------ tables
//
// Window-scoped state the window procedure needs but must NOT call into
// the plugin for (a plugin/UI thread could be inside `SendMessage` when
// Windows calls back): close requests, destroy notices, the v36 fixed
// pin, and pending container resizes. Every message lands in the queue of
// the owning (vst2-gui) thread, so the map is written and read there — the
// mutex is for the `PostMessage`-from-UI-thread edge, never for contention.

struct Tables {
    /// Containers whose `WM_CLOSE` (or `WM_DESTROY`) was seen — the
    /// `WM_DELETE_WINDOW` twin. Teardown consumes.
    closed: HashSet<isize>,
    /// v36: fixed client sizes (plugin refused `effEditCanSizeWindow`).
    pinned: HashMap<isize, (i32, i32)>,
    /// Newest container client-size report per window (`WM_SIZE`),
    /// consumed by the Live phase — the ConfigureNotify twin.
    resizes: HashMap<isize, (u32, u32)>,
}

fn tables() -> &'static Mutex<Tables> {
    static T: OnceLock<Mutex<Tables>> = OnceLock::new();
    T.get_or_init(|| {
        Mutex::new(Tables {
            closed: HashSet::new(),
            pinned: HashMap::new(),
            resizes: HashMap::new(),
        })
    })
}

fn note_closed(win: HWND) {
    if let Ok(mut t) = tables().lock() {
        t.closed.insert(win.0 as isize);
    }
}

fn take_close_request(win: HWND) -> bool {
    tables()
        .lock()
        .map(|mut t| t.closed.remove(&(win.0 as isize)))
        .unwrap_or(false)
}

fn pinned_of(win: HWND) -> Option<(i32, i32)> {
    tables().lock().ok().and_then(|t| t.pinned.get(&(win.0 as isize)).copied())
}

fn take_resize(win: HWND) -> Option<(u32, u32)> {
    tables()
        .lock()
        .ok()
        .and_then(|mut t| t.resizes.remove(&(win.0 as isize)))
}

fn forget_window(win: HWND) {
    if let Ok(mut t) = tables().lock() {
        let k = win.0 as isize;
        t.closed.remove(&k);
        t.pinned.remove(&k);
        t.resizes.remove(&k);
    }
}

/// Frame style every container is created with (the pin math and
/// `WM_GETMINMAXINFO` must agree with this single constant).
fn frame_style() -> WINDOW_STYLE {
    WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN
}

/// The container window procedure. It records control-plane work into the
/// tables and defers every plugin interaction to the pump — the same
/// discipline the crate's own editor windows use.
unsafe extern "system" fn container_proc(
    win: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // Close asks never destroy here: teardown owns the order
        // (effEditClose / close_editor before DestroyWindow).
        WM_CLOSE => {
            note_closed(win);
            LRESULT(0)
        }
        WM_DESTROY => {
            // Something destroyed the container out from under us (the
            // DestroyNotify twin) — retire, and never double-destroy.
            note_closed(win);
            LRESULT(0)
        }
        WM_SIZE => {
            let w = (lparam.0 as u32) & 0xFFFF;
            let h = ((lparam.0 >> 16) as u32) & 0xFFFF;
            if w > 0 && h > 0 {
                if let Ok(mut t) = tables().lock() {
                    t.resizes.insert(win.0 as isize, (w, h));
                }
            }
            DefWindowProcW(win, msg, wparam, lparam)
        }
        WM_GETMINMAXINFO => {
            // v36 pin: a fixed-size plugin editor is unresizable —
            // min == max track size, so dragging the frame cannot expose
            // the sentinel background.
            if let Some((pw, ph)) = pinned_of(win) {
                let mmi = lparam.0 as *mut MINMAXINFO;
                let mut r = RECT {
                    left: 0,
                    top: 0,
                    right: pw.max(1),
                    bottom: ph.max(1),
                };
                if AdjustWindowRect(&mut r, frame_style(), false).is_ok() {
                    if let Some(mmi) = mmi.as_mut() {
                        let track = POINT {
                            x: r.right - r.left,
                            y: r.bottom - r.top,
                        };
                        mmi.ptMinTrackSize = track;
                        mmi.ptMaxTrackSize = track;
                    }
                    return LRESULT(0);
                }
            }
            DefWindowProcW(win, msg, wparam, lparam)
        }
        WM_DPICHANGED => {
            // Apply the suggested rectangle synchronously (SetWindowPos may
            // re-enter messages, so do it before any table lock).
            if lparam.0 != 0 {
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    win,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        WM_PAINT => {
            // Validate the region; the class brush paints the sentinel.
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(win, &mut ps);
            let _ = EndPaint(win, &ps);
            let _ = dc;
            LRESULT(0)
        }
        _ => DefWindowProcW(win, msg, wparam, lparam),
    }
}

// ------------------------------------------------------------ window class

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn class_name() -> PCWSTR {
    static CL: OnceLock<Vec<u16>> = OnceLock::new();
    PCWSTR(CL.get_or_init(|| utf16("SlooperVstEditorContainer")).as_ptr())
}

/// Register the container class once per process. The background brush is
/// the sentinel color and is intentionally leaked: a class brush lives as
/// long as the class, which lives as long as the process.
fn ensure_class() -> Result<HINSTANCE, String> {
    // Raw handle VALUE in the OnceLock: the windows-rs handle newtypes are
    // not Send, and a `static` must be.
    static INST: OnceLock<Result<isize, String>> = OnceLock::new();
    let h = INST
        .get_or_init(|| {
            let h = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(PCWSTR::null()) }
                .map_err(|e| e.to_string())?;
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: WNDCLASS_STYLES(CS_HREDRAW.0 | CS_VREDRAW.0),
                lpfnWndProc: Some(container_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: HINSTANCE(h.0),
                hIcon: windows::Win32::UI::WindowsAndMessaging::HICON(ptr::null_mut()),
                hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(|e| e.to_string())?,
                hbrBackground: unsafe {
                    CreateSolidBrush(windows::Win32::Foundation::COLORREF(CONTAINER_BG))
                },
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class_name(),
                hIconSm: windows::Win32::UI::WindowsAndMessaging::HICON(ptr::null_mut()),
            };
            if unsafe { RegisterClassExW(&wc) } == 0 {
                return Err(format!(
                    "RegisterClassExW: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(h.0 as isize)
        })
        .clone()
        .map_err(|e| e.clone())?;
    Ok(HINSTANCE(h as *mut c_void))
}

// ------------------------------------------------------------ win32 helpers

/// Drain and dispatch every pending message for this thread's windows,
/// bounded per call like the X twin's event pump. The plugin's child
/// window procedures run inside `DispatchMessageW` — a plugin that throws
/// a C++ exception through here is caught upstream (the caller wraps
/// `pump_editors` in the `ffiguard` firewall).
fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        let mut drained = 0;
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            drained += 1;
            if drained >= 512 {
                break;
            }
        }
    }
}

fn hwnd_of(handle: u64) -> HWND {
    HWND(handle as *mut c_void)
}

/// Test hook (v24 contract, both twins): is container `hwnd` currently
/// visible? Probe containers NEVER report `true` (never shown); a normal
/// host container reports `true` as soon as it is mapped. `None` if the
/// window is gone.
pub fn container_is_viewable(handle: u64) -> Option<bool> {
    let win = hwnd_of(handle);
    unsafe {
        if IsWindow(Some(win)).as_bool() {
            Some(IsWindowVisible(win).as_bool())
        } else {
            None
        }
    }
}

/// The close-button twin of the X11 path's `WM_DELETE_WINDOW` client
/// message: post `WM_CLOSE` to an arbitrary container. Our window
/// procedure records it; the pump tears down in the host's order.
pub fn request_wm_delete(handle: u64) -> bool {
    unsafe {
        PostMessageW(Some(hwnd_of(handle)), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok()
    }
}

unsafe fn child_windows(win: HWND) -> Vec<(HWND, u32, u32, bool)> {
    unsafe {
        let mut out: Vec<(HWND, u32, u32, bool)> = Vec::new();
        unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> windows::core::BOOL {
            let v = &mut *(lp.0 as *mut Vec<(HWND, u32, u32, bool)>);
            let mut rc = RECT::default();
            let (w, hh) = if GetClientRect(h, &mut rc).is_ok() {
                ((rc.right.max(0)) as u32, (rc.bottom.max(0)) as u32)
            } else {
                (0, 0)
            };
            v.push((h, w, hh, IsWindowVisible(h).as_bool()));
            windows::core::BOOL(1)
        }
        let _ = EnumChildWindows(Some(win), Some(collect), LPARAM(&mut out as *mut _ as isize));
        out
    }
}

/// First plugin child window with non-degenerate geometry (created AND
/// sized — the 1x1 child is the tell-tale of a mid-build crash, same rule
/// as the X twin).
unsafe fn first_sized_child(win: HWND) -> Option<(HWND, u32, u32, bool)> {
    child_windows(win)
        .into_iter()
        .find(|(_, w, h, _)| *w > 0 && *h > 0)
}

/// Child count for the UI (the XQueryTree twin).
unsafe fn count_children(win: HWND) -> usize {
    child_windows(win).len()
}

/// True when a corner probe of the container holds any pixel that is
/// neither the sentinel nor the all-zero "nothing painted yet" state —
/// proof of direct drawing on the container (see the module docs for why
/// the child-live branch carries most Windows plugins).
unsafe fn container_painted(win: HWND) -> bool {
    unsafe {
        if !IsWindow(Some(win)).as_bool() {
            return false;
        }
        let mut rc = RECT::default();
        if GetClientRect(win, &mut rc).is_err() {
            return false;
        }
        let (cw, ch) = (rc.right.min(24), rc.bottom.min(24));
        if cw <= 0 || ch <= 0 {
            return false;
        }
        let dc = GetDC(Some(win));
        if dc.is_invalid() {
            return false;
        }
        let mut hit = false;
        'probe: for y in 0..ch {
            for x in 0..cw {
                let p = GetPixel(dc, x, y).0;
                if p != CONTAINER_BG && p != 0 && p != CLR_INVALID {
                    hit = true;
                    break 'probe;
                }
            }
        }
        ReleaseDC(Some(win), dc);
        hit
    }
}

/// Resize so the CLIENT area is exactly `(w, h)` (plugin rects are client
/// rects; Win32 window sizes are frame rects).
unsafe fn set_client_size(win: HWND, w: i32, h: i32) {
    unsafe {
        let mut r = RECT {
            left: 0,
            top: 0,
            right: w.max(8),
            bottom: h.max(8),
        };
        if AdjustWindowRect(&mut r, frame_style(), false).is_ok() {
            let _ = SetWindowPos(
                win,
                None,
                0,
                0,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// v36: pin / release the fixed client size (the WM_NORMAL_HINTS twin).
fn lock_wm_size(win: HWND, size: Option<(i32, i32)>) {
    if let Ok(mut t) = tables().lock() {
        match size {
            Some(s) => {
                t.pinned.insert(win.0 as isize, s);
            }
            None => {
                t.pinned.remove(&(win.0 as isize));
            }
        }
    }
    if let Some((w, h)) = size {
        unsafe { set_client_size(win, w, h) };
    }
}

// ----------------------------------------------------------------- VST2 core

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

/// `edit_rect` under the loader lock (every GUI-thread AEffect call
/// serializes with instantiation through it).
fn edit_rect_locked(handle: &Vst2Handle) -> Option<(i32, i32)> {
    let _loader = crate::vsthost::vst_loader_lock();
    edit_rect(handle)
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
    hwnd: Arc<AtomicUsize>,
    /// v20 (VST3 editors): the shadow's serialized component state,
    /// captured at teardown when the user changed something.
    state: Arc<Mutex<Option<Vec<u8>>>>,
    /// v23: alongside the chunk, the shadow's own (parameter id,
    /// normalized value) list at harvest time.
    params: SharedPatch,
}

/// Teardown hand-off of the shadow's model parameter values.
type SharedPatch = Arc<Mutex<Option<Vec<(u32, f32)>>>>;

impl Shared {
    fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            open_failed: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(None)),
            params: Arc::new(Mutex::new(None)),
            painted: Arc::new(AtomicBool::new(false)),
            children: Arc::new(AtomicUsize::new(0)),
            width: Arc::new(AtomicI32::new(0)),
            height: Arc::new(AtomicI32::new(0)),
            hwnd: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// A hosted plugin editor. Every editor operation (open, idle pump,
/// resize, close) runs on the ONE shared VST GUI thread (see
/// `vsthost::open`) — the same thread that instantiated the plugin.
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
    /// The host container's HWND (0 until the GUI thread creates it).
    hwnd: Arc<AtomicUsize>,
    /// v20: VST3 shadow-editor state hand-off (see `Shared::state`).
    state: Arc<Mutex<Option<Vec<u8>>>>,
    /// v23: shadow parameter hand-off (see `Shared::params`).
    params: SharedPatch,
}

impl EditorWindow {
    /// v20: take the VST3 shadow's state chunk if the user changed the
    /// plugin through its own GUI.
    pub fn take_state(&mut self) -> Option<Vec<u8>> {
        self.state.lock().ok().and_then(|mut c| c.take())
    }

    /// v23: take the FULL harvest — state chunk plus the shadow's own
    /// parameter values at teardown.
    pub fn take_patch(&mut self) -> Option<crate::vsthost::VstPatch> {
        let chunk = self.take_state()?;
        let params = self
            .params
            .lock()
            .ok()
            .and_then(|mut p| p.take())
            .unwrap_or_default();
        Some(crate::vsthost::VstPatch { chunk, params })
    }

    /// v22: non-destructive peek at the harvested chunk (see `take_state`).
    pub fn state_snapshot(&self) -> Option<Vec<u8>> {
        self.state.lock().ok().and_then(|c| c.clone())
    }

    /// Plugin path this editor was opened for (stale-editor detection).
    pub fn path(&self) -> &str {
        &self.path
    }
    /// True once the editor has retired.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    /// The plugin accepted the open but its editor failed afterwards.
    pub fn open_failed(&self) -> bool {
        self.open_failed.load(Ordering::SeqCst)
    }
    /// True once the plugin's own editor window has been seen visible with
    /// real geometry (the container is showing live plugin UI).
    pub fn painted(&self) -> bool {
        self.painted.load(Ordering::SeqCst)
    }
    /// Windows the plugin has parented into our container.
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
    /// The host container window's HWND, or 0 before the GUI thread has
    /// created it (integration tests / close-button injection).
    pub fn xid(&self) -> u64 {
        self.hwnd.load(Ordering::SeqCst) as u64
    }
    /// Retire the editor: ask the GUI thread to close it and wait (with a
    /// bound) for the `effEditClose` + window teardown to finish.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // v16: wake the pump thread immediately (same hand-off trick as
        // the X twin — post a no-op job so the next tick comes early).
        crate::vsthost::post(move || ());
        for _ in 0..500 {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Only reachable if the GUI thread is wedged inside plugin code.
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

/// Open a VST2 plugin's editor in a new top-level Win32 container.
/// `ae` is the raw `AEffect` dispatcher handle from `vsthost`; the GUI
/// thread keeps using it, so the plugin instance must outlive the returned
/// handle (slooper closes editors before unloading, see `ui.rs`).
pub fn open_editor(
    handle: &Vst2Handle,
    path: &str,
    title: &str,
    cells: Arc<crate::vsthost::VstParams>,
    ncell: usize,
) -> Result<EditorWindow, String> {
    if !x11_available() {
        return Err("no Win32 window station (VST2 editors need a desktop session)".into());
    }
    let shared = Shared::new();
    let ae = *handle;
    let title = title.to_string();
    let s2 = shared.clone();
    // Creation (container window + effEditOpen) happens ON the GUI
    // thread; blocking here means the returned handle already has an HWND
    // and either a live verification state machine or a verdict.
    crate::vsthost::on_gui_thread(move || {
        REG.with(|reg| {
            // SAFETY: runs on the GUI thread, the sole owner of all
            // editor state (see module docs).
            if let Some(e) = unsafe { make_editor(ae, &title, s2, cells, ncell) } {
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
        hwnd: shared.hwnd.clone(),
        state: shared.state.clone(),
        params: shared.params.clone(),
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
/// VST GUI thread between jobs — never from another thread. Win32's single
/// per-thread queue is drained ONCE per tick; both generations then step.
pub fn pump_editors() {
    pump_messages();
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
    /// Waiting for the plugin to prove it painted.
    Verify {
        it: u32,
    },
    Live {
        last_size: (i32, i32),
        /// Container client size last routed to the plugin.
        last_container: (u32, u32),
    },
    Done,
}

struct Editor {
    handle: Vst2Handle,
    /// v36: `Some(size)` while the frame is pinned to a fixed editor size
    /// (plugin refused `effEditCanSizeWindow`). Same contract as the X11
    /// twin.
    fixed: Option<(i32, i32)>,
    /// v22: live readback plumbing (see the X twin's `sync_cells` note).
    cells: Arc<crate::vsthost::VstParams>,
    ncell: usize,
    win: HWND,
    title: String,
    phase: Phase,
    shared: Shared,
    dbg: bool,
    created: Instant,
    house: Instant,
    dead_ticks: u32,
}

/// Create the container and dispatch `effEditOpen` — all on the GUI
/// thread. `None` (with the failure flags already set) when the window
/// cannot be created.
unsafe fn make_editor(
    handle: Vst2Handle,
    title: &str,
    shared: Shared,
    cells: Arc<crate::vsthost::VstParams>,
    ncell: usize,
) -> Option<Editor> {
    let dbg = std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some();
    // (bisect) fixed seed size, same constant as the X twin.
    let (w0, h0) = (640i32, 420i32);
    let hinst = match ensure_class() {
        Ok(h) => h,
        Err(e) => {
            if dbg {
                eprintln!("[vstedit] {title}: class registration failed: {e}");
            }
            shared.open_failed.store(true, Ordering::SeqCst);
            shared.closed.store(true, Ordering::SeqCst);
            return None;
        }
    };
    let tname = utf16(&format!("slooper \u{2014} {title}"));
    // Probe containers are created UNSEEN (no WS_VISIBLE) — the
    // initially-unmapped container twin (v24).
    let mut style = frame_style();
    if !probe_mode() {
        style |= WS_VISIBLE;
    }
    let win = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name(),
            PCWSTR(tname.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w0,
            h0,
            None,
            None,
            Some(hinst),
            None,
        )
    } {
        Ok(w) => w,
        Err(e) => {
            if dbg {
                eprintln!("[vstedit] {title}: CreateWindowExW refused: {e}");
            }
            shared.open_failed.store(true, Ordering::SeqCst);
            shared.closed.store(true, Ordering::SeqCst);
            return None;
        }
    };
    shared.hwnd.store(win.0 as usize, Ordering::SeqCst);
    let opened = handle.dispatch(EFF_EDIT_OPEN, 0, 0, win.0, 0.0);
    if dbg {
        eprintln!("[vstedit] {title}: effEditOpen -> {opened} (hwnd {:?}, gui thread)", win.0);
    }
    // The VST2 spec says 0 == success, but plenty of toolkits return a
    // "true" flag (1) for a SUCCESSFUL open — the verification below is
    // the real judge: if the plugin draws, it opened, whatever it said.
    EDITORS_ACTIVE.fetch_add(1, Ordering::Relaxed);
    Some(Editor {
        handle,
        fixed: None,
        cells,
        ncell,
        win,
        title: title.to_string(),
        phase: Phase::Verify { it: 0 },
        shared,
        dbg,
        created: Instant::now(),
        house: Instant::now(),
        dead_ticks: 0,
    })
}

impl Editor {
    /// One pump iteration: message queue already drained by
    /// `pump_editors`; here: close verdict, idle dispatch, state advance.
    fn step(&mut self) {
        unsafe {
            match self.phase {
                Phase::Done => {}
                Phase::Verify { it } => {
                    // The user can hit the close button while we are
                    // still verifying: honour it instead of holding a
                    // doomed window.
                    if take_close_request(self.win) {
                        self.teardown(false);
                        return;
                    }
                    self.handle
                        .dispatch(EFF_EDIT_IDLE, 0, 0, ptr::null_mut(), 0.0);
                    // A child counts as a live editor only when VISIBLE
                    // with real geometry (the mapped+geometry rule of the
                    // X twin translated to `WS_VISIBLE` + client rect —
                    // Win32's `IsWindowVisible` reports "should be
                    // visible", so a probe container's hidden-but-shown
                    // child still proves the toolkit bootstrap ran).
                    // Direct-draw editors paint ON the container and never
                    // create a child, so check real pixels too.
                    let child_live = first_sized_child(self.win)
                        .is_some_and(|(_, w, h, vis)| w >= 8 && h >= 8 && vis);
                    let px = container_painted(self.win);
                    if self.dbg && (it < 3 || it % 10 == 0) {
                        eprintln!("[vstedit] verify {it}: live-child={child_live} px={px}");
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
                    if take_close_request(self.win) {
                        self.teardown(false);
                        return;
                    }
                    // The plugin's editor event loop runs here (FLTK and
                    // friends service input from effEditIdle; JUCE/VSTGUI
                    // service themselves through the pump above).
                    self.handle
                        .dispatch(EFF_EDIT_IDLE, 0, 0, ptr::null_mut(), 0.0);
                    // v36: audioMasterSizeWindow — the plugin asked to
                    // resize its own window. Grow the container to exactly
                    // what it asked for.
                    if let Some((w, h)) = vst2::host::take_size_request(self.handle.as_usize()) {
                        if w >= 8 && h >= 8 {
                            if self.fixed.is_some() {
                                self.fixed = Some((w, h));
                                lock_wm_size(self.win, Some((w, h)));
                            }
                            set_client_size(self.win, w, h);
                            self.shared.width.store(w, Ordering::SeqCst);
                            self.shared.height.store(h, Ordering::SeqCst);
                        }
                    }
                    // The user dragged the frame (R1b parity with the X
                    // twin): route it through the plugin.
                    if let Some((w, h)) = take_resize(self.win) {
                        if let Some((fw, fh)) = self.fixed {
                            // v36: pinned editor — any slipped resize
                            // snaps straight back; never routed.
                            set_client_size(self.win, fw, fh);
                            let _ = (w, h);
                        } else if (w, h) != *last_container && w >= 8 && h >= 8 {
                            *last_container = (w, h);
                            let can = {
                                let _loader = crate::vsthost::vst_loader_lock();
                                self.handle.dispatch(EFF_EDIT_CAN_SIZE, 0, 0, ptr::null_mut(), 0.0)
                                    != 0
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
                                // Stubborn child-window toolkits: resize
                                // the child; Windows itself sends it the
                                // WM_SIZE its layout code listens for.
                                if let Some((k, _, _, _)) = first_sized_child(self.win) {
                                    let _ = SetWindowPos(
                                        k,
                                        None,
                                        0,
                                        0,
                                        w as i32,
                                        h as i32,
                                        SWP_NOZORDER | SWP_NOACTIVATE,
                                    );
                                }
                            }
                        }
                    }
                    // Window-system housekeeping on a timer (v16 parity).
                    if self.house.elapsed() >= HOUSEKEEPING {
                        self.house = Instant::now();
                        // v22: pull the plugin's live parameter values
                        // into the cell mirror.
                        for k in 0..self.ncell {
                            let v = self.handle.get_param(k as i32);
                            if v.is_finite() {
                                self.cells.set(k, v);
                            }
                        }
                        let kids = count_children(self.win);
                        self.shared.children.store(kids, Ordering::SeqCst);
                        // Dead-editor watchdog.
                        let alive = first_sized_child(self.win)
                            .is_some_and(|(_, w, h, vis)| w >= 8 && h >= 8 && vis)
                            || container_painted(self.win);
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
                                set_client_size(self.win, w, h);
                                // v36: re-pin a pinned editor whose PLUGIN
                                // changed the size (preset load, patch).
                                if self.fixed.is_some() {
                                    self.fixed = Some((w, h));
                                    lock_wm_size(self.win, Some((w, h)));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Name — re-asserted once the editor has proven itself, mirroring the
    /// X twin's decorate beat (there is no WM handshake to wait for on
    /// Win32, so this is purely cosmetic symmetry).
    unsafe fn decorate(&mut self) {
        let t = utf16(&format!("slooper \u{2014} {}", self.title));
        let _ = SetWindowTextW(self.win, PCWSTR(t.as_ptr()));
    }

    /// `effEditClose`, destroy the container, mark retired. `refused`
    /// reports the death as an open failure so the UI drops to (or keeps)
    /// the slider panel.
    fn teardown(&mut self, refused: bool) {
        unsafe {
            // v22: final readback so the last gesture inside the plugin's
            // own GUI (within the last housekeeping tick) is persisted.
            // Skipped on refused teardowns — a watchdog-killed instance may
            // not survive parameter calls.
            if !refused {
                for k in 0..self.ncell {
                    let v = self.handle.get_param(k as i32);
                    if v.is_finite() {
                        self.cells.set(k, v);
                    }
                }
            }
            if self.shared.hwnd.load(Ordering::SeqCst) != 0 {
                EDITORS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
                {
                    let _loader = crate::vsthost::vst_loader_lock();
                    self.handle
                        .dispatch(EFF_EDIT_CLOSE, 0, 0, ptr::null_mut(), 0.0);
                }
                if IsWindow(Some(self.win)).as_bool() {
                    let _ = DestroyWindow(self.win);
                }
                forget_window(self.win);
            }
            self.shared.open_failed.store(refused, Ordering::SeqCst);
            self.shared.closed.store(true, Ordering::SeqCst);
            self.phase = Phase::Done;
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Every retire path runs teardown() before removal from REG; this
        // is the belt for any path that forgets.
        if !matches!(self.phase, Phase::Done) {
            self.teardown(true);
        }
    }
}

// ------------------------------------------------------------ VST3 editors
//
// Same shadow-instance architecture as the X twin (see its module notes):
// the sounding instance belongs to the realtime runner; the editor embeds
// a SHADOW instance primed from the shared parameter cells, and every edit
// the view reports is routed back through the same cells the sliders use.
// All plugin-side logic below is OS-independent and mirrors the X twin
// verbatim; only the container calls differ.

struct Editor3 {
    /// The shadow instance, owned by the GUI thread. `Option` so
    /// teardown can retire it under the loader lock exactly once.
    plugin: Option<vst3_host::Plugin>,
    /// v36: `Some(size)` while the frame is pinned.
    fixed: Option<(i32, i32)>,
    /// VST3 parameter ids in `VstParams`-cell order.
    ids: Vec<u32>,
    params: Arc<crate::vsthost::VstParams>,
    /// Load-time sample rate for the teardown state flush.
    sr: u32,
    win: HWND,
    title: String,
    phase: Phase,
    shared: Shared,
    dbg: bool,
    created: Instant,
    house: Instant,
    dead_ticks: u32,
    /// v20: set when the user demonstrably edited through this shadow.
    changed: bool,
    /// Cell-order writability mirror of `ids`.
    writable: Vec<bool>,
    /// v24: last observed shadow-model value per cell (model-sync).
    shadow_last: Vec<f32>,
    /// v24: the parameter ids this shadow has actually seen move.
    edited: std::collections::HashSet<u32>,
    /// Model-sync beat.
    sync: Instant,
}

/// Open a VST3 plugin's own editor window. Mirrors [`open_editor`] from the
/// caller's side — same handle type, same verified-paint rule, same
/// close-button behaviour, same GUI-thread ownership.
#[allow(clippy::too_many_arguments)] // (path, title, sr, seed, state) + (ids, writable, cells) — the editor's full identity, one per slot
pub fn open_vst3_editor(
    path: &str,
    title: &str,
    sample_rate: u32,
    initial: Vec<(u32, f64)>,
    state: Option<Vec<u8>>,
    ids: Vec<u32>,
    writable: Vec<bool>,
    params: Arc<crate::vsthost::VstParams>,
) -> Result<EditorWindow, String> {
    if !x11_available() {
        return Err("no Win32 window station (VST3 editors need a desktop session)".into());
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
            if let Some(e) = unsafe {
                make_editor3(
                    path_j,
                    title,
                    sample_rate,
                    initial,
                    state,
                    ids,
                    writable,
                    params,
                    s2,
                )
            } {
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
        hwnd: shared.hwnd.clone(),
        state: shared.state.clone(),
        params: shared.params.clone(),
    })
}

thread_local! {
    static REG3: std::cell::RefCell<Vec<Editor3>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Test/automation hook: inject a parameter edit into the live VST3
/// shadow editor owning container `hwnd`. Same route as the plugin's own
/// UI. Applied on the shared VST GUI thread.
pub fn inject_shadow_edit(hwnd: u64, cell: usize, v: f32) {
    crate::vsthost::post(move || {
        REG3.with(|reg| {
            for e in reg.borrow_mut().iter_mut() {
                if e.shared.hwnd.load(Ordering::SeqCst) as u64 == hwnd {
                    let _ = e.inject_edit(cell, v);
                }
            }
        });
    })
}

/// Test hook (v24): change the shadow MODEL for cell `cell` and write
/// NOTHING else — exactly what a vendor UI that never reports does to its
/// own instance. The model-sync must notice and carry the edit into the
/// cells on its own.
pub fn mutate_shadow_model(hwnd: u64, cell: usize, v: f32) {
    crate::vsthost::post(move || {
        REG3.with(|reg| {
            for e in reg.borrow_mut().iter_mut() {
                if e.shared.hwnd.load(Ordering::SeqCst) as u64 == hwnd {
                    if let (Some(p), Some(&id)) = (e.plugin.as_mut(), e.ids.get(cell)) {
                        let _ = p.set_parameter(id, v as f64);
                    }
                }
            }
        });
    })
}

/// The host half of VST3 editor→sound sync (identical to the X twin —
/// this is plugin logic, not window logic).
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
/// the shared GUI-thread cadence.
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

/// Load the shadow instance, create the container, attach the editor view
/// to the HWND (`kPlatformTypeHWND`), and hand back the state machine.
/// `None` retires with the failure flags already set on `shared`.
#[allow(clippy::too_many_arguments)] // one bundle per editor: paths, state, param rings
unsafe fn make_editor3(
    path: String,
    title: String,
    sample_rate: u32,
    initial: Vec<(u32, f64)>,
    state: Option<Vec<u8>>,
    ids: Vec<u32>,
    writable: Vec<bool>,
    params: Arc<crate::vsthost::VstParams>,
    shared: Shared,
) -> Option<Editor3> {
    let dbg = std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some();
    // Shadow instantiation runs under the process-wide loader lock, on
    // the GUI thread — the same discipline as every other plugin load.
    let mut plugin = {
        let _loader = crate::vsthost::vst_loader_lock();
        match vst3_host::simple::load_plugin_with_settings(
            &path,
            sample_rate as f64,
            crate::vsthost::VST_BLOCK,
        ) {
            Ok(p) => p,
            Err(e) => {
                if dbg {
                    eprintln!("[vstedit3] {title}: shadow load failed: {e}");
                }
                shared.open_failed.store(true, Ordering::SeqCst);
                shared.closed.store(true, Ordering::SeqCst);
                return None;
            }
        }
    };
    // v20: restore the live instance's state chunk FIRST when it has one,
    // then layer the cell mirror on top. A refused chunk degrades to
    // cell-only priming.
    if let Some(bytes) = &state {
        if let Err(e) = plugin.load_state(bytes) {
            if dbg {
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
    shared.width.store(w0, Ordering::SeqCst);
    shared.height.store(h0, Ordering::SeqCst);
    let hinst = match ensure_class() {
        Ok(h) => h,
        Err(_) => {
            shared.open_failed.store(true, Ordering::SeqCst);
            shared.closed.store(true, Ordering::SeqCst);
            return None;
        }
    };
    let tname = utf16(&format!("slooper \u{2014} {title}"));
    let mut style = frame_style();
    if !probe_mode() {
        style |= WS_VISIBLE;
    }
    let win = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name(),
            PCWSTR(tname.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w0,
            h0,
            None,
            None,
            Some(hinst),
            None,
        )
    } {
        Ok(w) => w,
        Err(_) => {
            shared.open_failed.store(true, Ordering::SeqCst);
            shared.closed.store(true, Ordering::SeqCst);
            return None;
        }
    };
    shared.hwnd.store(win.0 as usize, Ordering::SeqCst);
    // The view's real rect becomes the client size (the X twin maps the
    // container at (w0, h0) first; the Win32 window was created with outer
    // ~(w0, h0) — normalise the client now).
    set_client_size(win, w0, h0);
    {
        let _loader = crate::vsthost::vst_loader_lock();
        // SAFETY: `win` is a live HWND that outlives the attach —
        // teardown detaches (close_editor) before destroying it.
        if let Err(e) =
            plugin.open_editor(unsafe { vst3_host::WindowHandle::from_hwnd(win.0) })
        {
            if std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some() {
                eprintln!("[vstedit3] {title}: open_editor refused: {e}");
            }
            let _ = DestroyWindow(win);
            forget_window(win);
            shared.open_failed.store(true, Ordering::SeqCst);
            shared.closed.store(true, Ordering::SeqCst);
            return None;
        }
    }
    let dbg = std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some();
    if dbg {
        eprintln!("[vstedit3] {title}: view attached to {:?} ({w0}x{h0})", win.0);
    }
    EDITORS_ACTIVE.fetch_add(1, Ordering::Relaxed);
    Some(Editor3 {
        plugin: Some(plugin),
        fixed: None,
        ids,
        params,
        sr: sample_rate,
        win,
        title,
        phase: Phase::Verify { it: 0 },
        shared,
        dbg,
        created: Instant::now(),
        house: Instant::now(),
        dead_ticks: 0,
        changed: false,
        writable,
        shadow_last: Vec::new(),
        edited: std::collections::HashSet::new(),
        sync: Instant::now(),
    })
}

impl Editor3 {
    /// One pump: `service_run_loop`, editor→host parameter drain, size
    /// tracking. (Close requests were consumed into the tables by the
    /// shared message pump.)
    fn step(&mut self) {
        unsafe {
            let Some(plugin) = self.plugin.as_mut() else {
                self.teardown(true);
                return;
            };
            match self.phase {
                Phase::Done => {}
                Phase::Verify { it } => {
                    if take_close_request(self.win) {
                        self.teardown(false);
                        return;
                    }
                    plugin.service_run_loop();
                    {
                        let edits = plugin.get_parameter_changes();
                        if !edits.is_empty() {
                            self.edited.extend(edits.iter().map(|(id, _)| *id));
                            route_param_edits3(edits, &self.ids, &self.params);
                            self.changed = true;
                        }
                    }
                    // The VST3 view creates a real child window; same
                    // live-child / painted-pixel verdict as VST2.
                    let child_live = first_sized_child(self.win)
                        .is_some_and(|(_, w, h, vis)| w >= 8 && h >= 8 && vis);
                    let px = container_painted(self.win);
                    if self.dbg && (it < 3 || it % 10 == 0) {
                        eprintln!("[vstedit3] verify {it}: live-child={child_live} px={px}");
                    }
                    if child_live || px {
                        // Views sometimes only commit their real rect once
                        // mapped: take the attached size before the borrow
                        // of the plugin is dropped for decorate/resize.
                        let attached = match plugin.attached_editor_size() {
                            Ok(Some((w, h))) if w > 0 && h > 0 => Some((w, h)),
                            _ => None,
                        };
                        // v36: query the view's resize stance BEFORE the
                        // `self.decorate()` borrow.
                        let can_resize = plugin.attached_editor_can_resize();
                        self.shared.painted.store(true, Ordering::SeqCst);
                        self.decorate();
                        // v36: pin the frame when the view itself says it
                        // cannot resize. `None`/unknown = conservative.
                        self.fixed = match (can_resize, attached) {
                            (Some(false), Some((w, h))) => Some((w, h)),
                            _ => None,
                        };
                        if let Some(s) = self.fixed {
                            lock_wm_size(self.win, Some(s));
                        }
                        self.phase = match attached {
                            Some((w, h)) => {
                                self.shared.width.store(w, Ordering::SeqCst);
                                self.shared.height.store(h, Ordering::SeqCst);
                                set_client_size(self.win, w, h);
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
                    if take_close_request(self.win) {
                        self.teardown(false);
                        return;
                    }
                    plugin.service_run_loop();
                    {
                        let edits = plugin.get_parameter_changes();
                        if !edits.is_empty() {
                            self.edited.extend(edits.iter().map(|(id, _)| *id));
                            route_param_edits3(edits, &self.ids, &self.params);
                            self.changed = true;
                        }
                    }
                    // v24 model-sync (see the X twin for the full why):
                    // vendor UIs that mutate their own instance without
                    // reporting still change the SOUND live.
                    if self.sync.elapsed() >= MODEL_SYNC {
                        self.sync = Instant::now();
                        if let Ok(model) = plugin.get_parameters() {
                            let mut touched = false;
                            if self.shadow_last.len() != model.len() {
                                // first beat (or the plugin renumbered):
                                // baseline without pushing.
                                self.shadow_last = model.iter().map(|x| x.value as f32).collect();
                            } else {
                                for (k, pm) in model.iter().enumerate() {
                                    let v = pm.value as f32;
                                    if (self.shadow_last[k] - v).abs() <= 1.0 / 8192.0 {
                                        continue;
                                    }
                                    self.shadow_last[k] = v;
                                    if self.writable.get(k).copied().unwrap_or(false) {
                                        if let Some(&id) = self.ids.get(k) {
                                            self.edited.insert(id);
                                            self.params.set(k, v);
                                            touched = true;
                                        }
                                    }
                                }
                            }
                            if touched {
                                self.changed = true;
                            }
                        }
                    }
                    // The plugin may request a size change.
                    if let Some((w, h)) = plugin.take_editor_resize_request() {
                        if w > 0 && h > 0 {
                            set_client_size(self.win, w, h);
                            // v36: a pinned view whose PLUGIN chose the
                            // new size re-pins to it.
                            if self.fixed.is_some() {
                                self.fixed = Some((w, h));
                                lock_wm_size(self.win, Some((w, h)));
                            }
                        }
                    }
                    // The user resized our frame: ask the view to relayout
                    // into it (R1b parity).
                    if let Some((w, h)) = take_resize(self.win) {
                        if let Some((fw, fh)) = self.fixed {
                            // v36: pinned view — snap a slipped resize back.
                            let _ = (w, h);
                            set_client_size(self.win, fw, fh);
                        } else if (w, h) != *last_container && w >= 8 && h >= 8 {
                            *last_container = (w, h);
                            let _ = plugin.resize_editor(w as i32, h as i32);
                        }
                    }
                    // Housekeeping on a timer (v16, VST2 parity).
                    if self.house.elapsed() >= HOUSEKEEPING {
                        self.house = Instant::now();
                        // v22: real-time persistence — chunk-only while
                        // open (the full why is on the X twin).
                        if self.changed {
                            if let Ok(bytes) = plugin.save_state() {
                                if let Ok(mut cell) = self.shared.state.lock() {
                                    *cell = Some(bytes);
                                }
                            }
                        }
                        // v20: restartComponent announcements mark changes
                        // without performEdit broadcasts.
                        if !plugin.take_restart_flags().is_empty() {
                            self.changed = true;
                        }
                        let kids = count_children(self.win);
                        self.shared.children.store(kids, Ordering::SeqCst);
                        let alive = first_sized_child(self.win)
                            .is_some_and(|(_, w, h, vis)| w >= 8 && h >= 8 && vis)
                            || container_painted(self.win);
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
                                    set_client_size(self.win, w, h);
                                    // v36: re-pin a fixed view that
                                    // changed its own size (preset etc).
                                    if self.fixed.is_some() {
                                        self.fixed = Some((w, h));
                                        lock_wm_size(self.win, Some((w, h)));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Name — only after the view proved itself (mirrors the X twin's
    /// decorate beat; no Win32 handshake depends on it).
    unsafe fn decorate(&mut self) {
        let t = utf16(&format!("slooper \u{2014} {}", self.title));
        let _ = SetWindowTextW(self.win, PCWSTR(t.as_ptr()));
    }

    /// v20/v22: serialize a shadow's component state into `shared.state`
    /// — silent-block flush first, then `save_state` (the full PROBE FACT
    /// rationale lives on the X twin; identical reasoning, OS-independent).
    fn harvest3(
        p: &mut vst3_host::Plugin,
        sr: u32,
        dbg: bool,
        title: &str,
        shared: &Shared,
        edited: &std::collections::HashSet<u32>,
    ) {
        if let Err(e) = p.start_processing() {
            if dbg {
                eprintln!("[vstedit3] {title}: state-flush start failed: {e}");
            }
        } else {
            let ch = p.output_channel_count().max(1);
            let mut bufs = vst3_host::audio::AudioBuffers::new(
                ch,
                ch,
                crate::vsthost::VST_BLOCK,
                sr.max(1) as f64,
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
                match p.get_parameters() {
                    Ok(all) => {
                        // v24: ride-along parameter snapshot — ONLY the
                        // params this shadow actually saw move.
                        let psnap: Vec<(u32, f32)> = all
                            .into_iter()
                            .filter(|x| edited.contains(&x.id))
                            .map(|x| (x.id, x.value as f32))
                            .collect();
                        if let (Ok(mut cell), Ok(mut pcell)) =
                            (shared.state.lock(), shared.params.lock())
                        {
                            *cell = Some(bytes);
                            *pcell = Some(psnap);
                        }
                    }
                    Err(_) => {
                        if let Ok(mut cell) = shared.state.lock() {
                            *cell = Some(bytes);
                        }
                    }
                }
            }
            Err(e) => {
                if dbg {
                    eprintln!("[vstedit3] {title}: save_state failed: {e}");
                }
            }
        }
    }

    /// v20/v22: harvest the shadow's chunk (see [`Self::harvest3`]).
    fn harvest(&mut self) {
        if !self.changed {
            return;
        }
        if let Some(p) = self.plugin.as_mut() {
            Self::harvest3(
                p,
                self.sr,
                self.dbg,
                &self.title,
                &self.shared,
                &self.edited,
            );
        }
    }

    /// Backing for [`inject_shadow_edit`].
    fn inject_edit(&mut self, cell: usize, v: f32) -> bool {
        let Some(id) = self.ids.get(cell).copied() else {
            return false;
        };
        let ok = match self.plugin.as_mut() {
            Some(p) => p.set_parameter(id, v as f64).is_ok(),
            None => false,
        };
        if ok {
            self.changed = true;
            self.edited.insert(id);
            if let Some(slot) = self.shadow_last.get_mut(cell) {
                *slot = v;
            }
            self.params.set(cell, v);
        }
        ok
    }

    /// Detach the view, retire the shadow instance under the loader lock,
    /// destroy the container, mark the handle retired.
    fn teardown(&mut self, refused: bool) {
        unsafe {
            // v20: harvest BEFORE anything is torn down.
            self.harvest();
            // View first, plugin second — close_editor must run before
            // the instance tears down.
            let plugin = self.plugin.take();
            if let Some(mut p) = plugin {
                {
                    let _loader = crate::vsthost::vst_loader_lock();
                    let _ = p.close_editor();
                }
                drop(p);
            }
            if self.shared.hwnd.load(Ordering::SeqCst) != 0 {
                EDITORS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
                if IsWindow(Some(self.win)).as_bool() {
                    let _ = DestroyWindow(self.win);
                }
                forget_window(self.win);
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

/// Whether the session can host editor windows at all. The Win32 twin is
/// env-only by design (matching the X twin's rule: never open a probe
/// display / touch a window station from a gate call site). On Windows
/// with a desktop session — which every slooper user has, headless
/// sessions included as far as user32 goes — this is simply true; the
/// name stays so the SHARED call sites (ui.rs / vsthost.rs editor gates,
/// the W8 seam pins) need no OS branch.
pub fn x11_available() -> bool {
    true
}

// ---------------------------------------------------------------- v23 probe
//
// The child half of the editor crash guard (parent half and rationale:
// `vsthost::probe_editor`). Identical flow to the X twin; the Win32
// containers it creates are never shown (probe mode), so the guard is
// invisible exactly like on Linux.

/// Exit-code protocol: 0 = survived (or nothing to test), 2 = plugin did
/// not load in the child (not an editor verdict). Crashes exit as
/// exceptions (the W7 NTSTATUS labelling converts those in the parent).
pub fn probe_child(spec: &str) -> i32 {
    enter_probe_mode();
    // Two env vars on purpose: env strings are NUL-terminated, and paths
    // may legally contain tabs — `SLOOPER_EDITOR_PROBE` carries only the
    // kind token, `SLOOPER_EDITOR_PROBE_PATH` the plugin path.
    let Ok(path) = std::env::var("SLOOPER_EDITOR_PROBE_PATH") else {
        return 0;
    };
    let kind = if spec == "V3" {
        crate::vsthost::VstKind::V3
    } else {
        crate::vsthost::VstKind::V2
    };
    if !x11_available() {
        return 0; // unreachable on Windows; kept for twin symmetry
    }
    let path = std::path::Path::new(&path);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let load = match crate::vsthost::open(path, kind, 48_000, None) {
        Ok(l) => l,
        Err(_) => return 2,
    };
    let entry = load.entry;
    if !entry.has_editor() {
        return 0;
    }
    let mut win = if let Some(h) = entry.vst2_handle() {
        match open_editor(
            &h,
            &path.to_string_lossy(),
            &name,
            entry.params().clone(),
            entry.descs().len(),
        ) {
            Ok(w) => w,
            Err(_) => return 0, // honest refusal: the app path reports it
        }
    } else {
        match entry.open_vst3_editor(48_000) {
            Ok(w) => w,
            Err(_) => return 0,
        }
    };
    // Verify window is 3 s; allow slack for the toolkit's own bootstrap.
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if win.painted() || win.open_failed() || win.is_closed() {
            break;
        }
    }
    // Stay alive briefly PAINTED: crash classes that fire from the idle
    // pump right after the first frame need the pump running.
    std::thread::sleep(std::time::Duration::from_millis(500));
    win.close();
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if win.is_closed() {
            break;
        }
    }
    0
}
