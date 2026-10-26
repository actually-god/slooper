//! v16 debug 2: does loading the TEMP-COPIED Dragonfly via open_async crash?
use std::time::Duration;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let home = std::env::var("HOME").unwrap();
    let src = std::path::Path::new(&home).join(".vst/DragonflyHallReverb-vst.so");
    let dir = std::env::temp_dir().join(format!("sp-v16probe2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join("dfhall.so");
    std::fs::copy(&src, &dst).unwrap();

    // Same as the pin: scan first (probe instance LEAKS), then open.
    if arg != "noopen" {
        let rx = slooper::vsthost::scan_async(vec![dir.clone()]);
        let entries = rx.recv_timeout(Duration::from_secs(30)).expect("scan");
        println!("scan ok: {}", entries.len());
        if arg == "scanonly" {
            println!("scan-only done (leaving)");
            std::process::exit(0);
        }
    }
    let rx = slooper::vsthost::open_async(&dst, slooper::vsthost::VstKind::V2, 48_000, None);
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(e)) => println!("open ok: {}", e.name()),
        Ok(Err(err)) => println!("open err: {err}"),
        Err(e) => println!("open TIMEOUT: {e:?}"),
    }
    std::thread::sleep(Duration::from_millis(500));
    println!("alive at exit");
}
