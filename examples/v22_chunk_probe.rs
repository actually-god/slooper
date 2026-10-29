// v22 probe: which effGetChunk/effSetChunk ABI do the plugins on this host
// actually answer? FILE* (VST2.4 spec) vs memory-buffer (Bitwig/Carla/yabridge
// extension). Opens each plugin through slooper's real path and tries both.
//   cargo run --release --example v22_chunk_probe -- <path> [more paths...]
use std::ffi::c_void;
use std::path::Path;

extern "C" {
    fn tmpfile() -> *mut c_void;
    fn fclose(f: *mut c_void) -> i32;
    fn rewind(f: *mut c_void);
    fn ftell(f: *mut c_void) -> i64;
    fn fread(buf: *mut c_void, size: usize, n: usize, f: *mut c_void) -> usize;
    fn fwrite(buf: *const c_void, size: usize, n: usize, f: *mut c_void) -> usize;
}

const EFF_GET_CHUNK: i32 = 23;
const EFF_SET_CHUNK: i32 = 24;

fn get_chunk_file(handle: &slooper::vsthost::Vst2Handle) -> Vec<u8> {
    unsafe {
        let f = tmpfile();
        if f.is_null() {
            return Vec::new();
        }
        let size = handle.dispatch(EFF_GET_CHUNK, 0, 0, f, 0.0);
        if size <= 0 {
            fclose(f);
            return Vec::new();
        }
        rewind(f);
        let n = ftell(f);
        if n <= 0 {
            fclose(f);
            return Vec::new();
        }
        let mut out = vec![0u8; n as usize];
        let got = fread(out.as_mut_ptr() as *mut c_void, 1, n as usize, f);
        fclose(f);
        out.truncate(got);
        out
    }
}

fn set_chunk_file(handle: &slooper::vsthost::Vst2Handle, data: &[u8]) -> bool {
    unsafe {
        let f = tmpfile();
        if f.is_null() {
            return false;
        }
        fwrite(data.as_ptr() as *const c_void, 1, data.len(), f);
        rewind(f);
        let res = handle.dispatch(EFF_SET_CHUNK, 0, 0, f, 0.0);
        fclose(f);
        res != 0
    }
}

fn get_chunk_buffer(handle: &slooper::vsthost::Vst2Handle) -> Vec<u8> {
    // Bitwig/Carla extension: value=1, ptr = &void* -> plugin exposes buffer.
    let size = handle.dispatch(EFF_GET_CHUNK, 0, 1, std::ptr::null_mut(), 0.0);
    if size <= 0 {
        return Vec::new();
    }
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let size = handle.dispatch(EFF_GET_CHUNK, 0, 1, &mut ptr as *mut _ as *mut c_void, 0.0);
    if size <= 0 || ptr.is_null() {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(ptr as *const u8, size as usize).to_vec() }
}

fn set_chunk_buffer(handle: &slooper::vsthost::Vst2Handle, data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    let res = handle.dispatch(
        EFF_SET_CHUNK,
        0,
        data.len() as isize,
        data.as_ptr() as *mut c_void,
        0.0,
    );
    res != 0
}

fn main() {
    let mut args = std::env::args().skip(1);
    let kind = if std::env::var("PROBE_KIND").is_ok_and(|k| k == "V3") {
        slooper::vsthost::VstKind::V3
    } else {
        slooper::vsthost::VstKind::V2
    };
    while let Some(p) = args.next() {
        let path = Path::new(&p);
        // order controlled by env so we can isolate the dangerous call
        let order = std::env::var("PROBE_ORDER").unwrap_or_else(|_| "file,buffer".into());
        match slooper::vsthost::open(path, kind, 48_000, None) {
            Err(e) => println!("{p}: LOADFAIL {e}"),
            Ok(load) => {
                let entry = load.entry;
                let Some(handle) = entry.vst2_handle() else {
                    println!("{p}: no VST2 handle (V3?)");
                    continue;
                };
                for step in order.split(',') {
                    match step {
                        "file" => {
                            let c = get_chunk_file(&handle);
                            println!("{p}: FILE get -> {} bytes", c.len());
                            if !c.is_empty() {
                                println!("{p}: FILE set -> {}", set_chunk_file(&handle, &c));
                                let c2 = get_chunk_file(&handle);
                                println!(
                                    "{p}: FILE get2 -> {} bytes (stable: {})",
                                    c2.len(),
                                    c2 == c
                                );
                            }
                        }
                        "buffer" => {
                            let c = get_chunk_buffer(&handle);
                            println!("{p}: BUFFER get -> {} bytes", c.len());
                            if !c.is_empty() {
                                println!("{p}: BUFFER set -> {}", set_chunk_buffer(&handle, &c));
                            }
                        }
                        _ => {}
                    }
                }
                std::mem::forget(entry);
            }
        }
    }
    println!("probe done");
}
