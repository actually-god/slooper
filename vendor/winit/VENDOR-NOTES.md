# `vendor/winit` — provenance and patch

Vendored from **winit 0.30.13** (crates-io, Apache-2.0/MIT, `LICENSE` ships
next to this file). One deviation, both instances marked `SLOOPER PATCH`:

* `src/platform_impl/linux/x11/event_processor.rs` — the `FocusIn`/`FocusOut`
  handlers `.expect()` that focusing/unfocusing the X input context succeeds.
  slooper hosts plugin editor windows as top-level X windows, so the window
  manager delivers aggressive focus switches; libX11's local-XIM plumbing can
  report a stale `BadMatch` from those calls (reproduced on KWin/Xwayland by
  closing a plugin GUI and re-opening it — coredump:
  `Failed to unfocus input context: XError { BadMatch, request_code: 73 }`).
  Under `panic = "abort"` that panic killed the entire application. The patch
  logs the error and continues; keyboard input without an IME is unaffected.
* `src/platform_impl/linux/x11/ime/context.rs` — same policy for the
  `XVaCreateNestedList` expects on the IC-creation and `set_spot` event
  paths, and lossy (instead of panicking) UTF-8 handling of IME payloads.

`tests/` and `images/` trimmed from the vendored copy. Re-derive by copying
a fresh `~/.cargo/registry/src/*/winit-0.30.13` and re-applying the three
`SLOOPER PATCH` sites.
