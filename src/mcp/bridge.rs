//! `slooper --mcp-bridge` — stdio↔HTTP shim (docs/MCP-DESIGN.md D1,
//! REQUIRED with the core per §10: Claude Desktop & many agent runtimes
//! are stdio-only). Reads newline-delimited JSON-RPC on stdin, forwards
//! each line as one `POST /mcp` to the live instance, prints the JSON
//! response line (nothing on stdout for 202 — stdio forbids non-protocol
//! bytes), logs to stderr, exits on stdin EOF.
//!
//! Port + token come from `data_dir()/mcp-endpoint.json` — re-read per
//! line, so an app restart (new token, new port-0 value) is picked up
//! without restarting the client. The client's `mcp.json` never embeds
//! the token:
//!
//! ```json
//! { "mcpServers": { "slooper": { "command": "slooper", "args": ["--mcp-bridge"] } } }
//! ```

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

/// The discovery file (design §3): `<data_dir>/mcp-endpoint.json`.
/// Chained single-component join — the mixed-separator trap (W14) makes
/// this path byte-identical to the one any Windows reader compares.
pub fn endpoint_path() -> PathBuf {
    crate::paths::data_dir().join("mcp-endpoint.json")
}

/// Atomic tmp+rename write (a client may read at any instant). Returns
/// the final path. 0600 on unix; on Windows the per-user profile dir's
/// ACL does the job (design D7).
pub(crate) fn write_endpoint(port: u16, token: &str) -> io::Result<PathBuf> {
    let dir = crate::paths::data_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("mcp-endpoint.json");
    let tmp = dir.join(format!("mcp-endpoint.json.tmp-{}", std::process::id()));
    let body = json!({
        "port": port,
        "token": token,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
        "name": "slooper",
    })
    .to_string();
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(body.as_bytes())?;
        f.flush()?;
    }
    // std::fs::rename replaces an existing target on both OSes.
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// (port, token) from the discovery file; None when absent/malformed.
pub fn read_endpoint() -> Option<(u16, String)> {
    let body = std::fs::read_to_string(endpoint_path()).ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let port = v.get("port")?.as_u64()? as u16;
    let token = v.get("token")?.as_str()?.to_string();
    if port == 0 || token.is_empty() {
        return None;
    }
    Some((port, token))
}

/// One line in → (Some(body) | None = 202 notification | Err) out.
fn forward(port: u16, token: &str, body: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut sock = TcpStream::connect(("127.0.0.1", port))?;
    // The server-side UI wait is 5 s; 10 s bounds the client side so a
    // wedged server can never hang the agent forever.
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    sock.set_write_timeout(Some(Duration::from_secs(5)))?;
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes())?;
    sock.write_all(body)?;
    sock.flush()?;
    let mut br = BufReader::new(sock);
    let mut status = String::new();
    if br.read_line(&mut status).unwrap_or(0) == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no status line"));
    }
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut clen = 0usize;
    loop {
        let mut l = String::new();
        if br.read_line(&mut l).unwrap_or(0) == 0 {
            break;
        }
        if l.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                clen = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if code == 202 || clen == 0 {
        return Ok(None);
    }
    let mut buf = vec![0u8; clen];
    br.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// The `--mcp-bridge` main loop (called from main.rs BEFORE any GUI/JACK
/// init — this process is a pipe, never a second DAW).
pub fn run() -> io::Result<()> {
    if read_endpoint().is_none() {
        eprintln!(
            "[mcp-bridge] no usable endpoint file at {} — start slooper with \
             MCP enabled (Settings ▸ AUTOMATION) first",
            endpoint_path().display()
        );
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((port, token)) = read_endpoint() else {
            eprintln!("[mcp-bridge] endpoint file missing — line dropped (start the app with MCP on)");
            continue;
        };
        match forward(port, &token, line.as_bytes()) {
            Ok(Some(body)) => {
                out.write_all(&body)?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
            // 202 notification: the stdio transport forbids any stdout bytes.
            Ok(None) => {}
            Err(e) => eprintln!("[mcp-bridge] forward failed: {e}"),
        }
    }
    Ok(())
}
