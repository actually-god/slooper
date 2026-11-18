# slooper built-in MCP server — architecture design (MCP series spine)

Status: design spine for board cards MCP-00..06 (board `slooper`, ids
mcp-00=t_72a0690e, mcp-01=t_56b24d75, mcp-02=t_377dcec9, mcp-03=t_5cd0d1e5,
mcp-04=t_e0c0ad5e, mcp-05=t_3f374341, mcp-06=t_af0fa70f). Written 2026-11-18.
This document decides; the owning cards build. Every claim marked (spike) was
proven by the throwaway spike described in §9 — no product code was written
for this design.

Goal (user request 2026-11-18): an external agent (Claude Desktop, any MCP
client) can drive the **live running** slooper: all settings/configuration;
subwindow arrangement/sizing/window config; direct subwindow controls (mixer
sliders, plugin selection, asset-rack controls); and all functions of the
native Rust plugins (e.g. pick sampler assets, set velocity/note values).

## Decision list

| # | Question | Decision | Rejected alternatives |
|---|----------|----------|-----------------------|
| D1 | Transport & topology | **Streamable HTTP on 127.0.0.1 inside the GUI process, POST-only with `application/json` responses (no SSE), hand-rolled JSON-RPC 2.0 over `std::net` + `serde_json`** (already product deps), **plus a `slooper --mcp-bridge` stdio↔HTTP shim** for stdio-only clients. Dual protocol shape: `initialize` handshake (2025-03-26…2025-11-25) and stateless `server/discover` (2026-07-28) on one dispatcher. | `rmcp` crate (tokio is a REQUIRED dep — verified from crates.io API); stdio-only server (can't reach a live instance); SSE streams; unix sockets; second-instance server |
| D2 | Security | **Off by default**; loopback-only bind hardcoded; bearer **token** auto-generated into `data_dir()/mcp-endpoint.json` (0600); Host allow-list + Origin check; `mcp_read_only` kill-switch latch. Threat model §3. | first-connection pairing; open 0.0.0.0 option; no-auth localhost |
| D3 | Thread bridge | **Worker thread never touches App/engine.** Bounded `sync_channel(16)` command queue drained inside `App::logic` every frame; `ctx.request_repaint()` wake (production-proven); 5 s timeout → JSON-RPC error; `catch_unwind` firewall; **observed-then-ack** (write tools echo post-change state re-read through the same getters reads use). | published snapshot for reads (dual state = drift class); worker touching state under a mutex; per-tool timeouts |
| D4 | Addressability | **Key strings, never roster indices.** Primary windows get stable keys mirroring `SideWins` fields (`rack`/`binds`/`monitor`/`mixer`); dynamic windows reuse `DynKind::key()`/`from_key()` verbatim (persisted since v35). Control args are flat 0-based ids (`track`, `row`, `pad`, `asset_id`, `key_note`, `param`). Packed sampler words and roster indices never surface. | roster indices (`win_count` slots shift on open/close); free-text titles as addresses |
| D5 | Naming & schema | **dotted `domain.verb`** names exactly as the cards' ACs speak them (`settings.*`, `windows.*`, `mixer.*`, `chain.*`, `assets.*`, `plugin.*`, `sampler.*`); read/write is a **declared per-tool flag** (readOnlyHint = the latch's source of truth), not a name heuristic; units = **native currency of each setter seam** (mixer u8 0..=255, plugin params normalized 0..1, MIDI ints); `ParamMetaV1` exposed via descriptor tools (`x-slooper-unit` + human string via the existing `fmt_param`); **tools-only surface — no resources namespace**; serverInfo.version = `CARGO_PKG_VERSION`. | snake_case `get_*` flat names; name-prefix read detection; float-coerced integer seams; MCP resources for config/project |
| D6 | Tests | Quick-tier pins call `mcp::dispatch(req, &mut App)` headlessly (no sockets, real frame loop via `frame()`); slow-tier `#[ignore = "slow-tier: real socket"]` for full socket round-trips + the bridge child process (`CARGO_BIN_EXE_slooper`, bounded wait — W12e rule); **port 0 + endpoint-discovery file everywhere, never a fixed port**; windows-native full suite (existing BLOCKING step) runs every pin — no new CI step. | fixed test ports; a Linux-only socket suite; vendoring an MCP client crate into prod deps |
| D7 | Parity | `std::net` + `std::thread` only; all paths via `crate::paths` with **chained single-component `.join()`** (mixed-separator trap); endpoint file atomic tmp+rename; stdio bridge frames with `\n` only; accept loop = nonblocking poll + sleep (no self-connect trick, identical on Windows); Windows verdict = windows-native full suite + gnullvm `--all-targets` check. | unix sockets; `/tmp` discovery; SO_REUSE tricks; `std::os::unix` anything |

## 0. Ground rules this design must not break (repo invariants)

- **RT thread is sacred** (skill "Realtime & plugin hard rules"). The MCP
  surface adds ZERO RT-visible state; every engine change goes through the
  existing UI-thread seams: `set_trk_vol/gain/pan/color` (ui.rs, `fn
  set_trk_vol`), `Engine::set_project` / `set_midi_assets` / `set_track_synth`,
  the v51/v52 sampler-map writers, `Registry::create_instance`.
- **No async.** The crate has no tokio/hyper/async-std today; nothing here
  introduces any (D1).
- **One UI thread owns the App.** Same rule as every vNN pass; MCP tools obey
  it structurally (they only run inside the frame-loop drain, §4).
- Config grows only by additive `#[serde(default)]` fields; every `Config
  { .. }` literal (9 sites in src) gains them in the same commit (E0063).
- Asset ids ARE table indices (remap on removal); `MidiAction::ALL` is
  append-only. The MCP surface must therefore hand back ids from read tools
  and re-validate on every write (§5).

## 1. D1 — Transport & topology

### Why Streamable HTTP in-process

The requirement is control of the **live** app. MCP stdio transports *spawn*
the server as a client subprocess — that model cannot attach to the running
GUI; a second `slooper` process is a second engine, second JACK client, second
everything (and JACK would fight it). So the live endpoint must live inside
the GUI process, and the transport must be network-based → **Streamable
HTTP**, which the spec explicitly allows to be *JSON-request/JSON-response
per POST* (no SSE required for a server with no server-initiated messages):

- `POST /mcp` with a JSON-RPC body → `200 application/json` (spec 2025-11-25
  §Sending Messages #5: the server MUST return *either* SSE *or* one JSON
  object — we always take the JSON branch) (spike §1–9).
- `POST` of a notification → `202 Accepted`, empty body (mandated; spike §2).
- `GET /mcp` → `405 Method Not Allowed` (mandated alternative to offering an
  SSE stream; spike §13).
- No `Mcp-Session-Id` is ever assigned. Under 2025-11-25 sessions are a MAY;
  under 2026-07-28 they are **removed entirely** — stateless is the
  forward-compatible shape, and it makes the bridge and CI trivial.

### Protocol versions on one dispatcher

Shipped clients speak the handshake era (`initialize` →
`notifications/initialized`), which is Final as of 2025-11-25; the current
spec revision 2026-07-28 dropped the handshake (per-request `_meta`
versioning, `server/discover`, `resultType` on results, `ttlMs`/`cacheScope`
on list results). One dispatcher serves both shapes:

- answer `initialize` (echo the requested version if supported, else our
  latest handshake-era version `2025-11-25`), and also never *require* it —
  `tools/list`/`tools/call` work cold (2026 stateless shape) (spike §1,§9);
- implement `server/discover` (2026 MUST-have) and keep `ping` (dropped in
  2026, still sent by today's SDKs) (spike §9);
- `tools/list` already emits `ttlMs`/`cacheScope` (2026 `CacheableResult`) —
  older clients ignore unknown fields (spike §3);
- accept-but-ignore `_meta.io.modelcontextprotocol/*` and `Mcp-Method`/
  `Mcp-Name` headers. Never reject on their absence (2025 clients omit them).

### Bridge: `slooper --mcp-bridge` (REQUIRED, not optional)

The spec says clients SHOULD support stdio "whenever possible" — Claude
Desktop and many agent runtimes are stdio-only. The bridge is a mode of the
existing binary (no new artifact to package): reads newline-delimited
JSON-RPC from stdin, forwards each line as one `POST /mcp` to the live
instance, prints the JSON response line (nothing on stdout for 202
notifications — stdio spec forbids non-protocol bytes on stdout), logs to
stderr, exits on stdin EOF. Port + token are read from
`data_dir()/mcp-endpoint.json` so the client's `mcp.json` never embeds the
token. Verified end-to-end in the spike (initialize → 202 → tools/list
through the shim, endpoint-file discovery). Reference `mcp.json`:

```json
{ "mcpServers": { "slooper": { "command": "slooper", "args": ["--mcp-bridge"] } } }
```

### Crate call: hand-rolled over `std::net` (rmcp rejected)

`rmcp` 3.11.0's crates.io dependency list (verified 2026-11-18) has
**`tokio`, `tokio-util`, `futures`, `chrono`, `tracing`, `sha2`, `thiserror`,
`indexmap` as REQUIRED** deps — tokio is not optional. That means async
contagion into a crate that has kept a no-async rule through v0.23, a real
binary-size cost against a ~13 MB fat-LTO release bin, and a two-runtime
problem (an eframe `App` is synchronous; an async handler thread still ends
up bridging to the UI thread exactly the way D3 defines — the async layer
buys nothing on the critical path). The protocol surface we need is
`initialize`/`ping`/`server/discover`/`tools/{list,call}` +
`notifications/*`: a few hundred lines of `serde_json::Value` plumbing. The
2026-07-28 spec churn is the second nail: SDKs chase the revision, a thin
in-house dispatcher chases it in one match arm. (spike §rmcp)

Rejected also: **SSE streams** (resumability/session machinery for a
request/response workload with no server-initiated messages), **unix-domain
sockets** (Windows parity, D7), **a second in-process `slooper` headless
instance** (two engines cannot share one session).

## 2. Module layout & Config growth (MCP-01)

```
src/mcp/mod.rs     — McpServer lifecycle: maybe_start(cfg, ctx), drop/shutdown
src/mcp/http.rs    — minimal HTTP/1.1: request line, headers, Content-Length
                     body; response writer. Connection: close (no keep-alive
                     state); per-conn worker thread, read_timeout 10 s.
src/mcp/rpc.rs     — JSON-RPC 2.0 envelopes over serde_json::Value; dispatch
                     match (initialize|ping|server/discover|tools/*|notif);
                     Host/Origin/token gate; queue push + repaint wake.
src/mcp/tools.rs   — THE tool table: name, description, inputSchema (static
                     json!), annotations, execute(&mut App, &Args).
                     execute runs ONLY on the frame-loop thread.
src/mcp/bridge.rs  — stdio↔HTTP shim (also `--mcp-bridge` in main.rs).
```

New `Config` fields (all `#[serde(default)]`, additive, no VERSION bump —
v58 rule): `mcp_enabled: bool` (**false** — the default), `mcp_port: u16`
(default **47618**), `mcp_read_only: bool` (false). The MCP server starts
**lazily inside `App::logic` on the first frame when `mcp_enabled`** — the
`App` creator closure in main.rs gets no `CreationContext`, and lazy start
also means turning the switch OFF in Settings can stop the thread without
restarting the app. `App::on_exit` (or `Drop`) flips the running flag and
detaches — the process exits either way; tests need the graceful variant.

Settings: a new "Automation (MCP)" section (v54 hover doctrine + v72 echo
doctrine apply): enable checkbox (explains: "lets MCP clients control this
running instance"), port box, read-only latch, token status line + reveal
button, endpoint-file path line. Every action status-echoed.

## 3. D2 — Security model

**Threat model.** The token's job is NOT to defend against the user's own
account (a process running as this user can read the token file, the project
files, and the JACK graph anyway — same trust domain). It defends against:
(a) **drive-by browser scripts** — the exact DNS-rebinding attack the spec's
Security Warning names: a remote page resolves `evil.com → 127.0.0.1` and
POSTs. Defenses: bind 127.0.0.1 only; **Host header allow-list** (`127.0.0.1`,
`localhost`, `[::1]` + any port, else 403); **Origin check** (present Origin
must be `http://127.0.0.1:<port>`/`http://localhost:<port>`/`http://[::1]…`
or `null`, else 403); the **bearer token** the browser cannot guess or set
cross-origin without a preflight we never grant (spike §10–12: 401/403/403
observed). (b) **other local users** — token file 0600 (unix); on Windows it
lives under the user's APPDATA-equivalent (`crate::paths::data_dir()`, which
is per-user ACL'd by profile). No TLS: plaintext on loopback only — TLS would
require trust plumbing for zero extra protection on 127.0.0.1; this is a
documented trade-off.

Gate order per request (fixed, so pins can pin each refusal):
`method != POST → 405` → `Host → 403` → `Origin → 403` → `token → 401` →
`path → 404` → `parse → -32700`.

Backpressure & blast radius: accept loop caps at 8 live connections; the
command queue is bounded 16 (`-32001 busy` on overflow, retry-friendly);
every write tool is `destructiveHint:false` only where it cannot break
music (e.g. `mixer.set`); destructive ones (delete track/asset,
clear map, reset config) MUST carry `destructiveHint:true` so clients can
gate them. `mcp_read_only=true` refuses every tool whose declared `read`
flag is false with `-32003 access-denied` (the user yanks the lever without
touching the agent's connection). Off-by-default + explicit Settings opt-in + visible status echo
on every applied MCP write (goal-1 parity: the user always SEES what the
agent did).

Rejected: first-connection pairing ceremony (a race another local process
could win, plus UI ceremony the token file makes pointless); a LAN-exposure
option (real attack surface, no use case in this series; can be re-litigated
later with real auth).

## 4. D3 — Thread bridge

```
HTTP worker thread                UI frame loop (App::logic)
─────────────────                 ──────────────────────────
gate → parse → Cmd{id,method,     drain(): pop ≤16 → catch_unwind(
  params, oneshot}                  execute(&mut App, cmd)) → send
try_send → Full → -32001 busy       result on oneshot   ← ONLY place
ctx.request_repaint()               App state is touched
recv_timeout(5s) → None → -32002
```

- **Reads AND writes execute on the UI thread** against live `App` state
  through one command path. A published-snapshot read path was rejected:
  it duplicates state the UI already owns — exactly the dual-source drift
  class the repo has repeatedly fought (mirror-stomp, capture-drift).
  Agents make tens of calls/second at most; a few handlers per frame costs
  nothing.
- **Wake**: the pusher calls `ctx.request_repaint()` — production-proven
  from foreign threads (fxgui bridge widget, async plugin scans) and the
  idle `request_repaint_after(100ms)` heartbeat already bounds worst-case
  latency to 100 ms even if a wake is missed (spike: ≤1 "frame" latency
  with a 3 ms loop).
- **Timeout**: worker blocks on the per-command oneshot ≤5 s → `-32002
  ui-timeout` (a frozen window produces clean protocol errors, never hung
  HTTP clients). A late result is dropped and logged.
- **Panic firewall**: every `execute` body runs inside `catch_unwind` →
  `-32000` (v23 editor-guard doctrine: an MCP bug must never take the DAW
  down; spike §7–8: panic → error → next call OK).
- **Observed-then-ack**: write handlers apply through the real setter seam,
  then re-read the state through the *same getter the read tools use* and
  return that fragment in `structuredContent` (spike §5–6: set 0.25 → ack
  carries the full post-change table → later read shows 0.25). This makes
  "did it really happen?" un-fakeable and catches write-to-a-copy bugs —
  the exact failure class the engine-atomic mirror rule warns about.
- **No RT coupling**: engine atomics apply on the next RT block; acks
  therefore attest **UI-thread truth**. Tools that need RT-visibility echo
  a `pending_engine_blocks` note only if a later card proves it necessary
  (default: no).
- **Palette relation**: the command palette's `Act` enum stays a UI-list
  convenience; MCP tools call the *extracted impl functions*, not the
  palette. Doctrine (v59): shared behavior = shared function, entry points
  cannot drift.

## 5. D4 — Addressability

**Windows.** Two namespaces, one flat string key per window, `from_key`
parsers, never indices:

- primary (the four `SideWins` members, `win_title` indices 0–3): new
  `PrimKind` enum beside `win_title` with `key()/from_key()`: `"rack"`
  (ASSET RACK), `"binds"` (MIDI BINDINGS), `"monitor"` (MIDI MONITOR),
  `"mixer"` (MIXER). MCP-03 builds this (and the `dyn_reg`-symmetric
  `prim_visible/set`).
- dynamic: **`DynKind::key()` / `from_key()` as-is** — `"drums"`,
  `"master"`, `"roll"`, `"auto"`, `"params"`, `"chain:<t>"`,
  `"synth:<t>"`, `"synths:<t>"`, `"fx:<t>:<r>"`, `"vst:<t>"`,
  `"vstfx:<t>:<r>"`. Already stable, already persisted in config (v35).
  ⚠ Spec note for MCP-03: `DynKind::Params` is a unit kind whose *title*
  disambiguates per-plugin windows — multiple params windows collide on
  `key()`. MCP-03 extends the key (e.g. `params:<track>:<row>`) and MUST
  keep `from_key("params")` parsing (config back-compat), pinned both ways.
- roster indices (the `win_count()` slot numbers the v47 window menu uses)
  are **never exposed** — they shift with open/close, the asset-id hazard.
  Unknown key → `-32602` whose message lists the valid keys (self-documenting).

**Control addresses** (JSON args; all ids are table indices, all 0-based,
every schema description says "0-based"; GUI labels stay 1-based):

| arg | domain | source of truth |
|-----|--------|-----------------|
| `track` | 0..track count | arrangement |
| `row` | FX/chain row on a track | chain table |
| `pad`, `pad_layer` | drum machine | kit table |
| `asset_id` | asset rack | engine asset table — MUST come from `assets.list` in the same session; ids remap on removal (hard rule) |
| `key_note` (0..=127), `layer` | sampler key map | v51 map, v52 layers — the **packed `(asset<<16)\|(vel<<8)\|root` word never surfaces**; tools take/return decomposed fields so the packing can change |
| `param` | plugin/script param index | `ParamMetaV1`/desc index |
| `note`, `step` | piano-roll / MIDI pattern | pattern grid |

Convention that kills an entire bug class: **read tools return the canonical
ids** (`windows.list`, `assets.list`, `mixer.list`, `plugin.describe`…) and
**write tools accept only ids** (names as *selection filters* only where the
underlying UI is name-addressed, e.g. rack folder names).

## 6. D5 — Tool/resource naming, schemas, errors, ParamMetaV1

**Naming: dotted `domain.verb`, versionless.** The series cards' acceptance
criteria already speak these names (MCP-02 `settings.*`, MCP-03
`windows.*`, MCP-04 `mixer.*`/`chain.*`/`assets.*`, MCP-05
`plugin.*`/`sampler.*`) — the spine adopts them verbatim; **no card invents
aliases.** Domain budget: `settings.` `windows.` `mixer.` `chain.`
`assets.` `plugins.` `plugin.` `sampler.` `drum.` + core-level `echo`
(MCP-01 demo). Verbs within a domain: `list`, `get`, `set`, `add`, `remove`,
`open`, `close`, `move`, `resize`, `dock`, `undock`, `save`, `load`, `pick`,
`import`, `describe`. A pin asserts every registered name matches
`^[a-z_]+\.[a-z_.]+$` with a budget verb tail.

**Read/write split is a declared property, not a name heuristic.** Each
tool row in `tools.rs` carries `read: bool` (rendered as `readOnlyHint`),
and `mcp_read_only` refuses every tool whose flag is false — a future tool
cannot sneak a write past the latch by naming. Read-only tools are
additionally pinned to touch nothing (frame-diff pin).

**Units convention — each control speaks the native currency of its setter
seam; the schema states `minimum`/`maximum`/unit in the description.** No
float normalization on top of integer seams (round-trip loss is a bug
magnet): mixer vol/gain/pan = **u8 0..=255** exactly as `set_trk_vol/gain/
pan` take them; plugin/script params = **normalized f32 0.0..=1.0** (the
engine's own currency, `set_param`); sampler velocity and note/root =
**u8 0..=127** MIDI integers; asset/track/row/slot ids = 0-based ints.
MCP-04..05 must not re-pick these.

**Annotations** (all tools, every call): `readOnlyHint`, `destructiveHint`,
`idempotentHint`; `openWorldHint:false` always (localhost, no network fan-out).

**Error taxonomy.** Transport/protocol level (JSON-RPC `error`): `-32700`
parse, `-32601` method/unknown-tool, `-32602` invalid params **including
unknown address keys** (message carries valid-key hints), `-32000` internal
/ caught panic, `-32001` queue busy, `-32002` UI timeout, `-32003`
read-only refusal (in the implementation-defined band the 2026 revision
reserves: -32000..-32019 grandfathered). Domain failures are NOT JSON-RPC
errors — they're a successful result with `isError:true` +
`structuredContent.error ∈ {invalid_params, not_found, out_of_range,
wrong_kind, read_only, busy}` + human text, so the agent sees a fixable
message instead of a transport error.

**Schemas.** `inputSchema` = plain static `json!` tables in `tools.rs`
(spik-verified shape). Param values follow the seam-currency rule above —
plugin/script params in **normalized `0.0..=1.0`** — plus a per-synth/plugin
`plugin.describe` read tool emitting, per param: index, name, min/max,
`x-slooper-unit` (the `SLOOPER_UNIT_*` code), enum labels (where
`ParamMetaV1` declares steps/labels), and a **human string** produced by the
same `fmt_param` formatter the GUI uses (v70) — one formatter, no drift;
the agent can display or convert and the setter accepts both normalized
floats (canonical) and, where meta has units, human numbers with the
explicit unit in the arg name (`volume_db: -6.0`)? → **No.** Decision:
normalized floats only in v1 (one truth, schema-simple); descriptor output
carries enough for the agent to convert. Revisit only if agent round-trip
pins show friction.

**Versions.** `serverInfo.version` = `CARGO_PKG_VERSION`; tool-set is
additive within the series; a removal/renumbering is a breaking change → new
name + deprecation comment in `tools.rs` (we ship fast; clients cache
`tools/list` with our `ttlMs`). **No MCP resources namespace** in v1 — the
config read surface is `settings.list`/`settings.get` (full `Config` serde
reflection — it already
`Serialize`s); project-state reads ride the domain `*.list` tools. The MCP
resources namespace was rejected to avoid a second read path. If a future
client demands resources, this is a pure-add decision for MCP-06+.

## 7. D6 — Test strategy

- **Quick tier** (every iteration, `scripts/test-quick.sh`):
  `vNN_mcp_*` pins in `src/mcp/tests.rs` + `ui::tests` — call
  `mcp::dispatch(&request, &mut headless_app)` directly through the normal
  `frame()` harness (zero sockets, zero threads: the HTTP layer and the
  dispatcher are split exactly so the pins skip the former). Pins must
  drive the real dispatcher, not the tool fns (contract: gate order, error
  taxonomy, read-only latch, observed-ack, panic firewall). Sabotage-verify
  both ways per the usual doctrine. One pin asserts the **tool table
  invariants** as a source-scan: every tool name has the verb budget + all
  four annotations + a non-empty schema (v54/v70 scan-pin family).
- **Slow tier** (`#[ignore = "slow-tier: real socket"]`): full
  socket round-trips against a lazily-started real server — **port 0 +
  endpoint file** read back from the isolated HOME the harness already
  provides; bridge child-process test via `CARGO_BIN_EXE_slooper`, NEVER a
  build.rs-baked path, child wait bounded (W12e rule); a "no SSE" pin
  asserting `GET /mcp → 405`; Host/Origin/401 pins at the socket layer too.
- **CI**: the windows-native *full-suite* step (ci.yml, BLOCKING) runs
  every lib pin incl. `--include-ignored` on a real Windows runner — MCP
  pins ride it with no new CI step; gnullvm `--all-targets` proves
  cross-compile. No fixed port anywhere. Manual smoke recipes (npx
  @modelcontextprotocol/inspector, Claude Desktop config) go in MCP-06 docs.

## 8. D7 — Windows/Linux parity

`std::net` (TcpListener/TcpStream) and `std::thread` are identical on both;
the accept loop is nonblocking-poll + 50 ms sleep (no self-connect wake, no
signal handling, no `SO_REUSE` fiddling). All filesystem paths (endpoint
file, token inside it) go through `crate::paths::data_dir()` built with
**chained single-component `.join()`** — the mixed-separator trap makes
`join("a/b")` byte-distinct from `join("a").join("b")` on Windows, and this
file path is exactly the kind later compared. Endpoint file writes are
tmp+rename (a client may read it any instant). stdio bridge framing is
`\n`-only (MCP stdio forbids `\r` dependence); any test needle comparing
bridge lines trims. The token file lives in the per-user profile dir on
Windows (profile ACL does the 0600 job). No unix-socket, no `/tmp`, no
`std::os::unix`, no env assumptions beyond what `paths.rs` already reads.
Arbiter unchanged: **windows-native full suite green = parity verdict**;
Linux behavior never changes to satisfy a Windows failure (cfg seams only).

## 9. Spike (throwaway, NOT in the repo)

`~/.hermes/profiles/feature-creator/cache/scratch/slooper-mcp-00/spikes/001-mcp-http-transport/`
— one file, std + `serde_json` only, run `./run_evidence.sh` →
`evidence.log`. Verdict VALIDATED (card comment 197 carries the excerpts).
Proven: initialize/initialized-202/tools/list round-trip; tools/call with
observed-then-ack + read-back; panic → -32000 → alive; ping /
server/discover / -32601; 401/403-Host/403-Origin/405-GET; stdio bridge
forwarding incl. silent notification; worker-thread vs UI-thread separation
by logged thread ids; port-0 + discovery-file; rmcp tokio-required dep fact.
Not re-spiked (already production-proven): `egui::Context::request_repaint`
idle wake.

## 10. Series map & forced scope changes

Actual card mapping (verified from card bodies 2026-11-18; ids in the
header — titles/assignees are the board's):

- **MCP-01** (t_56b24d75, feature-creator) — server core: `src/mcp/` + the
  `--mcp-bridge` mode in main.rs + Config fields + Settings section +
  **hot start/stop** (the card requires a live Settings toggle — §2's lazy
  start must therefore be a start/STOP pair, not start-once) + tool registry
  that MCP-02..05 register onto without touching the core + demo `echo`
  tool. *Forced now:* bridge ships with the core (not deferred — stdio-only
  clients are the primary user); endpoint discovery file exists from card 1
  (tests need it, so it is not a later "feature"); token file + gate order
  exactly §3; port-busy keeps booting with a status echo (card AC).
- **MCP-02** (t_377dcec9, feature-creator) — `settings.list/get/set` over
  **serde reflection of `Config`** (card AC: no hand-maintained list; parity
  pin over every serialized field) + requires-restart fields surfaced, not
  half-applied + **status echo of every MCP write** (goal 1). *Decision
  here:* the card's "consider an MCP resource" is declined — §6 tools-only.
- **MCP-03** (t_5cd0d1e5, **ui-designer**) — window registry: `PrimKind`
  keys + the `params` key extension (back-compat pinned, §5) +
  `windows.list/open/close/move/resize/dock/undock/save/restore` on the dock
  table (`SideWins.dock` and `dyn_wins.dock` share the `[docked, edge,
  order]` encoding — reuse `apply_win_layout`'s conversions, no new
  encoding); plugin-editor windows list/move/close minimum per the card.
- **MCP-04** (t_e0c0ad5e, **ui-designer**) — `mixer.list/get/set` (u8 seam
  currency, §6), `chain.list/pick/remove` — **pick is async like the GUI's
  load path: ack immediately with state `loading`, ready-ness observable via
  `chain.list`, never block the MCP call for a probe** — `assets.list/
  import/remove` + assign-equivalents via `crate::paths` validation. All
  writes through the §5 setter-seam table, acks observed (§4).
- **MCP-05** (t_3f374341, **plugin-designer**) — `plugins.list`,
  `plugin.params/describe`, `plugin.param.get/set`, `plugin.preset.*` on the
  shared preset-bar storage (sanitize honored), `sampler.map.get/set` +
  `sampler.layers.add/remove` **decomposed** (§5) with seam-currency MIDI
  ints, drum-kit pads same spirit; MIDI note get/set rebuilds patterns and
  goes via `set_midi_assets` (`MidiAction::ALL` append-only respected);
  `ParamMetaV1` → schema mapping per §6 (`fmt_param` human strings).
- **MCP-06** (t_af0fa70f, **product-services**) — docs/MCP.md derived from
  the live registry + README/USAGE/wiki + verified client configs once the
  surface is live; CHANGELOG coordinated with the REL chain (REL-3 is
  running as of this writing — do not touch).

Cross-card rules this spine forces on ALL of the above: pin names
`vNN_mcp_<behavior>` with each card claiming the next free vNN at dispatch
(v89+ were free at 2026-11-18 recon; v85–v88 and the v101/v102/v191 special
series are taken); quick-tier default; both clippy targets; windows-native
full suite is the merge gate; no new product dependencies (std + serde_json
only); ledger entry per pass in the skill's gui-cron-passes reference.

## 11. Open questions (deliberately deferred, non-blocking)

1. **Undo integration**: MCP writes currently bypass the undo history (most
   UI writes do too, per-action). If agents make broad changes, a
   project-snapshot undo tool may earn its place (v61 template machinery
   could back it). Not in v1.
2. **Multi-instance**: one endpoint file = last-started instance wins.
   Concurrent slooper instances already fight JACK (NO_START doctrine);
   per-PID endpoint files only become interesting if multi-instance becomes
   a real workflow.
3. **Elicitation / MRTR** (2026 pattern for server-initiated prompts):
   agents supply paths/values directly, so no v1 need; the stateless shape
   keeps the door open.
4. **Resources namespace**: see §6 — pure-add later if any client demands it.
