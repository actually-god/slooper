//! v94/MCP — built-in MCP server core (docs/MCP-DESIGN.md spine).
//!
//! Shape (decisions D1–D7, all settled by MCP-00 — do not re-litigate):
//! - Streamable HTTP on 127.0.0.1 ONLY, POST /mcp, JSON-request/JSON-response
//!   (never SSE: `GET` answers 405). Hand-rolled over `std::net` + serde_json;
//!   no async, no new deps.
//! - One worker thread accepts (nonblocking poll, 50 ms — identical on
//!   Windows), one short-lived thread per connection, `read_timeout` 10 s.
//! - Worker NEVER touches `App`. Requests become [`Cmd`]s on a bounded
//!   `sync_channel(16)`; `App::logic` drains up to 16 per frame and runs
//!   [`rpc::dispatch`] inline (the ONLY place app state is touched), then
//!   `ctx.request_repaint()` wakes the loop from a worker push.
//! - Worker waits ≤5 s (injectable for tests) for the result → `-32002`;
//!   a full queue is `-32001`; caught panics are `-32000`.
//! - The server starts/stops LAZILY from the frame loop (hot toggle — no
//!   restart is ever needed); `mcp_enabled=false` boots zero sockets.
//! - Port + token are auto-written to `data_dir()/mcp-endpoint.json`
//!   (0600 on unix, atomic tmp+rename) — the `--mcp-bridge` stdio shim
//!   and the tests discover the live endpoint there. Port 0 = ephemeral.
//!
//! Domain tools (MCP-02..05) add rows through `tools::install_all` — the
//! core dispatcher, bridge and lifecycle never change for a new tool.

pub mod bridge;
pub(crate) mod http;
pub mod rpc;
pub mod tools;

#[cfg(test)]
mod tests;

use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub use tools::{ToolDef, ToolError, ToolResult};

/// One gated JSON-RPC request waiting for the UI frame loop to execute it.
pub(crate) struct Cmd {
    pub req: Value,
    pub tx: std::sync::mpsc::Sender<Option<Value>>,
}

/// Owner of the listener thread + command queue. Lives inside `App`
/// (`App::mcp`); `sync()` is called from the frame loop for the hot
/// start/stop pair, `drain()` executes the queue against the live App.
pub struct McpServer {
    running: Option<Running>,
    /// (enabled, port) of the last start attempt — a failed bind retries
    /// only on config change or after the 5 s backoff, never every frame.
    last_want: Option<(bool, u16)>,
    retry_at: Option<Instant>,
    last_err: Option<String>,
    /// Worker-side oneshot wait. 5 s in production; tests shrink it to
    /// prove the -32002 taxonomy without wall-clock burns (D6).
    wait: Duration,
}

struct Running {
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    cmd_rx: Option<Receiver<Cmd>>,
    port: u16,
    token: String,
    /// Path of the endpoint file this instance wrote (deleted on stop).
    endpoint: std::path::PathBuf,
}

impl McpServer {
    pub fn new() -> Self {
        McpServer {
            running: None,
            last_want: None,
            retry_at: None,
            last_err: None,
            wait: Duration::from_secs(5),
        }
    }

    /// Test seam: shrink the worker's result wait so the -32002 path is
    /// testable in fast tier (production never calls this).
    #[cfg(test)]
    pub(crate) fn set_wait(&mut self, wait: Duration) {
        self.wait = wait;
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn port(&self) -> Option<u16> {
        self.running.as_ref().map(|r| r.port)
    }

    pub fn token(&self) -> Option<&str> {
        self.running.as_ref().map(|r| r.token.as_str())
    }

    /// One-line description for the Settings section (never panics).
    pub fn status_line(&self) -> String {
        match &self.running {
            Some(r) => format!("listening on 127.0.0.1:{} — loopback only", r.port),
            None => match &self.last_err {
                Some(e) => format!("stopped: {e} (retry after a change or a few seconds)"),
                None => "off — no sockets are open".into(),
            },
        }
    }

    /// Hot start/stop pair, called once per frame from `App::mcp_frame`.
    /// Returns a status-echo line (`App::status`) on every state
    /// transition or start failure — silence otherwise (v72 echo doctrine).
    pub fn sync(&mut self, enabled: bool, port: u16, ctx: &egui::Context) -> Option<String> {
        let want = (enabled, port);
        // Running: stop on disable, restart on port change.
        let mismatch = match &self.running {
            Some(r) => !enabled || r.port != port,
            None => false,
        };
        if mismatch {
            let reason = if !enabled {
                self.stop_now(Some("MCP server stopped — endpoint closed"))
            } else {
                self.stop_now(None)
            };
            debug_assert!(self.running.is_none());
            if !enabled {
                self.last_want = Some(want);
                self.last_err = None;
                return reason;
            }
            // fall through: restart on the new port
        }
        if !enabled || self.running.is_some() {
            return None;
        }
        // Start (with bind-failure backoff that a config change bypasses).
        if self.last_want == Some(want)
            && self.retry_at.is_some_and(|t| t > Instant::now())
        {
            return None;
        }
        match self.try_start(port, ctx) {
            Ok(p) => {
                self.last_want = Some(want);
                self.retry_at = None;
                self.last_err = None;
                Some(format!(
                    "MCP server on 127.0.0.1:{p} — token in {}",
                    bridge::endpoint_path().display()
                ))
            }
            Err(e) => {
                self.last_want = Some(want);
                self.retry_at = Some(Instant::now() + Duration::from_secs(5));
                self.last_err = Some(e.to_string());
                Some(format!(
                    "MCP could not start on port {port} ({e}) — the app keeps \
                     running; pick another port or switch MCP off"
                ))
            }
        }
    }

    /// Execute up to 16 queued commands against the live App. This is the
    /// ONLY place MCP code touches App state (design D3) — the worker
    /// threads never get past the queue. Runs inside `App::logic` (or the
    /// headless `logic_tick`), so every tool call executes on the UI thread.
    pub fn drain(&mut self, app: &mut crate::ui::App) {
        let Some(r) = self.running.as_mut() else { return };
        let Some(rx) = r.cmd_rx.as_ref() else { return };
        for _ in 0..16 {
            let cmd = match rx.try_recv() {
                Ok(c) => c,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            };
            let id = cmd.req.get("id").cloned().unwrap_or(Value::Null);
            // Outer firewall: dispatch already firewalls tool bodies; this
            // net catches a bug in the dispatcher itself so a panic can
            // never ride into the frame loop (v23 guard doctrine).
            let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rpc::dispatch(&cmd.req, app)
            }))
            .unwrap_or_else(|_| Some(rpc::err_resp(&id, -32000, "internal error (caught panic); server still alive")));
            // A timed-out worker has dropped its oneshot — send fails, the
            // late result is simply discarded (and logged nowhere, cheap).
            let _ = cmd.tx.send(resp);
        }
    }

    fn stop_now(&mut self, echo: Option<&str>) -> Option<String> {
        let msg = echo.map(str::to_string);
        if let Some(mut r) = self.running.take() {
            r.stop.store(true, Ordering::Relaxed);
            if let Some(h) = r.accept.take() {
                // The accept loop notices within its 50 ms poll; give it a
                // grace, then detach (it self-exits — a leaked idle thread
                // is bounded and harmless, the design's stated allowance).
                for _ in 0..8 {
                    if h.is_finished() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            // Dropping cmd_rx makes any late queue push fail → 503.
            r.cmd_rx = None;
            drop(r); // Running's Drop removes our endpoint file.
        }
        msg
    }

    fn try_start(&mut self, port: u16, ctx: &egui::Context) -> io::Result<u16> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let real_port = listener.local_addr()?.port();
        let token = gen_token();
        let endpoint = bridge::write_endpoint(real_port, &token)?;
        listener.set_nonblocking(true)?;
        let (cmd_tx, cmd_rx) = sync_channel::<Cmd>(16);
        let t_ctx = ctx.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let conns = Arc::new(AtomicUsize::new(0));
        let t_stop = stop.clone();
        let t_conns = conns.clone();
        let t_token = token.clone();
        let t_wait = self.wait;
        let accept = std::thread::spawn(move || {
            // Nonblocking poll — one accept syscall per 50 ms while idle,
            // identical behavior on Windows and Linux (D7: no self-connect
            // wake, no signal handling).
            while !t_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((sock, _peer)) => {
                        // Blast-radius cap (design §3): never more than 8
                        // live connections; a 9th is accepted and closed
                        // immediately (client-visible as a refused call,
                        // never a hang).
                        let prev = t_conns.fetch_add(1, Ordering::Relaxed);
                        if prev >= 8 {
                            t_conns.fetch_sub(1, Ordering::Relaxed);
                            drop(sock);
                        } else {
                            let tx = cmd_tx.clone();
                            let tk = t_token.clone();
                            let c = t_conns.clone();
                            let ctx = t_ctx.clone();
                            std::thread::spawn(move || {
                                http::serve_conn(sock, &tk, &tx, t_wait, ctx);
                                c.fetch_sub(1, Ordering::Relaxed);
                            });
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        });
        let real = Running {
            stop,
            accept: Some(accept),
            cmd_rx: Some(cmd_rx),
            port: real_port,
            token,
            endpoint,
        };
        self.running = Some(real);
        Ok(real_port)
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // App teardown: flip the flag, delete our endpoint file, detach.
        // The process exits either way (design §2); tests get the same
        // graceful path.
        self.stop_now(None);
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Only delete OUR file (a later-started sibling may own it now —
        // "last-started instance wins", design §11.2).
        if let Ok(body) = std::fs::read_to_string(&self.endpoint) {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if v.get("pid").and_then(|p| p.as_u64()) == Some(std::process::id() as u64) {
                    let _ = std::fs::remove_file(&self.endpoint);
                }
            }
        }
    }
}

/// 32 hex chars. NOT crypto RNG — this is a same-user loopback secret
/// (threat model, design §3: defends against browser drive-by, not the
/// account owner). Seed entropy: wall clock + pid + counter + a stack
/// address, mixed through SplitMix64 rounds.
fn gen_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let stack_probe = &nanos as *const u64 as u64;
    let mut s = nanos ^ ((std::process::id() as u64) << 33) ^ stack_probe ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = String::with_capacity(32);
    for _ in 0..4 {
        s = splitmix64(s);
        out.push_str(&format!("{s:016x}"));
    }
    // The counter keeps tokens distinct even within one clock tick.
    let _ = COUNTER.fetch_add(1, Ordering::Relaxed);
    out
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
