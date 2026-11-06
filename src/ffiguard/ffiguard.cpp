// v42: C++ exception firewall for plugin-library calls.
//
// yabridge (and any C++ plugin) can THROW when its wine bridge dies
// (measured 2026-11-06: `__cxa_throw` from libyabridge-vst2 on the vst2-gui
// thread while an Arturia host was dying → `std::terminate` → whole app
// dead). Rust cannot catch a C++ exception; the catch must live in a C++
// frame, so every guarded plugin call crosses this one-function shim.
//
// UNWINDING NOTE: `fg_try_call` unwinds the exception only as far as its own
// frame; every Rust frame in between must carry unwind tables. The release
// profile therefore compiles with panic = "unwind" (the ffiguard_* pins
// verify the round trip in release; switching back to panic = "abort"
// silently re-breaks it — the pins then abort, loudly).
#include <exception>
#include <stdexcept>

extern "C" {

typedef void (*fg_fn)(void *ctx);

/// Run fn(ctx); translate a C++ exception into an error code.
/// 0 = returned normally, 1 = std::exception (message copied to err),
/// 2 = any other exception. NEVER returns while an exception is live.
int fg_try_call(fg_fn fn, void *ctx, char *err, int errlen) {
  if (errlen > 0)
    err[0] = 0;
  try {
    fn(ctx);
    return 0;
  } catch (const std::exception &e) {
    const char *m = e.what();
    int i = 0;
    // NOTE the split statement: `err[i] = m[i++]` would evaluate the RHS
    // (and its increment) BEFORE indexing the LHS under C++17 sequencing —
    // the message landed one byte late, found by the v42 pins.
    while (m && m[i] && i + 1 < errlen) {
      err[i] = m[i];
      ++i;
    }
    err[i] = 0;
    return 1;
  } catch (...) {
    if (errlen > 0)
      err[0] = 0;
    return 2;
  }
}

// ---- test fixtures: throw THROUGH the Rust thunk into fg_try_call --------
void fg_raise_std(void *) { throw std::runtime_error("ffiguard test throw"); }
void fg_raise_other(void *) { throw 42; }
}
