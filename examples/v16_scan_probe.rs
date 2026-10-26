//! v16 debug: does a scan_async job land, or wedge the GUI pump?
use std::time::Duration;

fn main() {
    let dirs = slooper::vsthost::default_vst_dirs();
    println!("scan dirs: {dirs:?}");
    let rx = slooper::vsthost::scan_async(dirs);
    let mut hb0 = 0u64;
    for k in 0..15 {
        std::thread::sleep(Duration::from_millis(1000));
        let hb = slooper::vsthost::gui_heartbeat();
        match rx.try_recv() {
            Ok(v) => {
                println!("t={k}s LANDED {} entries (hb {hb0}->{hb})", v.len());
                for e in v.iter().filter(|e| e.error.is_none()).take(6) {
                    println!("  {:?} {}", e.kind, e.name);
                }
                return;
            }
            Err(e) => println!("t={k}s pending {e:?} hb {hb0}->{hb}"),
        }
        hb0 = hb;
    }
    println!("NEVER LANDED — GUI thread wedged");
    std::process::exit(1);
}
