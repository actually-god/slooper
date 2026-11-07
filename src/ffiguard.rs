//! v42: the C++ exception firewall around every plugin-library call.
//!
//! WHY: a dying plugin bridge (measured on a dead yabridge host) throws a
//! C++ exception from inside the plugin library on the calling thread. The
//! exception cannot be caught by Rust; the unwinder reaches `std::terminate`
//! and the whole DAW dies with the user's session. The catch has to be a
//! C++ frame — see `src/ffiguard/ffiguard.cpp`, compiled by `build.rs`.
//!
//! THE RULE: anything that crosses into a `dlopen`ed plugin library
//! (VST2/VST3 dispatch, process, editor ops, instantiation, drops) runs
//! inside [`guarded`]. A caught throw means the entry is wedged: callers
//! surface it and stop using that entry (the plugin entry itself is HELD,
//! never dropped on the spot — a throw may have left its internals dead).
//!
//! Windows builds compile the fallback [`guarded`] (direct call, no shim):
//! the guard exists for the wine-bridge/`libstdc++` plugin world, and the
//! gnullvm cross-check gate must not require a target C++ toolchain.

/// True when a real catch shim is linked in (unix).
pub const GUARD: bool = cfg!(not(windows));

#[cfg(not(windows))]
pub fn guarded<R, F: FnOnce() -> R>(f: F) -> Result<R, String> {
    use std::ffi::c_void;
    extern "C" {
        fn fg_try_call(
            fun: extern "C" fn(*mut c_void),
            ctx: *mut c_void,
            err: *mut i8,
            errlen: i32,
        ) -> i32;
    }
    struct Pack<F, R> {
        f: Option<F>,
        out: Option<R>,
    }
    extern "C" fn thunk<F: FnOnce() -> R, R>(p: *mut c_void) {
        // Only ever entered through fg_try_call; a C++ exception is caught
        // out there, so this frame is unwound THROUGH, never returned to.
        let pack = unsafe { &mut *(p as *mut Pack<F, R>) };
        if let Some(f) = pack.f.take() {
            pack.out = Some(f());
        }
    }
    let mut pack = Pack {
        f: Some(f),
        out: None,
    };
    let mut msg = [0i8; 512];
    let r = unsafe {
        fg_try_call(
            thunk::<F, R>,
            &mut pack as *mut _ as *mut c_void,
            msg.as_mut_ptr(),
            msg.len() as i32,
        )
    };
    match r {
        0 => pack
            .out
            .take()
            .ok_or_else(|| "fg_try_call returned OK without running the thunk".into()),
        1 => {
            // The shim wrote an NUL-terminated what() into `msg`.
            let mut bytes: Vec<u8> = msg.iter().map(|b| *b as u8).collect();
            if let Some(z) = bytes.iter().position(|b| *b == 0) {
                bytes.truncate(z);
            }
            let s = String::from_utf8_lossy(&bytes).into_owned();
            if s.is_empty() {
                Err("plugin threw std::exception (no message)".into())
            } else {
                Err(s)
            }
        }
        _ => Err("plugin threw an unknown C++ exception".into()),
    }
}

#[cfg(windows)]
pub fn guarded<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    // Native Windows plugin hosting: no shim, no bridge EH — direct call.
    Ok(f())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    extern "C" {
        fn fg_raise_std(ctx: *mut std::ffi::c_void);
        fn fg_raise_other(ctx: *mut std::ffi::c_void);
    }

    /// The whole crash fix rides on THIS: a C++ exception thrown from a
    /// C-callable frame, crossing a real Rust frame (the thunk), must land
    /// in `Err` with the process alive. If unwinding tables ever disappear
    /// (e.g. someone flips the release profile back to panic = "abort"),
    /// this test ABORTS the test process — loud, not silent.
    #[cfg(not(windows))]
    #[test]
    fn v42_ffiguard_catches_std_exception() {
        let r = guarded(|| unsafe { fg_raise_std(std::ptr::null_mut()) });
        let e = r.expect_err("the throw must surface as Err");
        assert!(
            e.contains("ffiguard test throw"),
            "what() must survive the trip: {e}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn v42_ffiguard_catches_foreign_and_keeps_working() {
        let r = guarded(|| unsafe { fg_raise_other(std::ptr::null_mut()) });
        assert!(r.is_err());
        // The world still turns after a caught throw.
        assert_eq!(guarded(|| 7u32 * 6).unwrap(), 42);
    }

    #[test]
    fn v42_ffiguard_passes_values_through() {
        assert_eq!(guarded(|| String::from("ok")).unwrap(), "ok");
        let v: Vec<u32> = guarded(|| vec![1, 2, 3]).unwrap();
        assert_eq!(v.len(), 3);
        // Err path runs the closure NOT at all is not the contract: a
        // successful call runs it exactly once (out is taken).
        let mut n = 0u32;
        let _ = guarded(|| n += 1);
        assert_eq!(n, 1);
    }
}
