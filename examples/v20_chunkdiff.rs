// Probe: capture needs a PROCESSED instance? set → process → save → diff,
// then restore into a fresh instance and read the parameter back.
use vst3_host::audio::AudioBuffers;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let pname = std::env::args().nth(2).unwrap();
    let pval: f64 = std::env::args().nth(3).unwrap().parse().unwrap();
    let mut p = vst3_host::simple::load_plugin_with_settings(&path, 48000.0, 512).unwrap();
    let pre = p.get_parameters().unwrap().into_iter().find(|x| x.name == pname).unwrap();
    let a = p.save_state().unwrap();
    p.set_parameter(pre.id, pval).unwrap();
    let b = p.save_state().unwrap();
    p.start_processing().unwrap();
    let in_ch = p.output_channel_count().max(1); // mono in == out for these
    let mut bufs = AudioBuffers::new(in_ch as usize, in_ch as usize, 512, 48000.0);
    for _ in 0..8 {
        // feed deterministic noise into input channel 0
        for (i, s) in bufs.inputs[0].iter_mut().enumerate() {
            *s = (((i as u32 * 2654435761u32) >> 8) as f32 / (1 << 24) as f32) * 2.0 - 1.0;
        }
        let _ = p.process_audio(&mut bufs);
    }
    let c = p.save_state().unwrap();
    let diff = |x: &[u8], y: &[u8]| x != y;
    println!(
        "chunk: default {}B, after-set {}B differ={}, after-proc {}B differ-vs-default={}",
        a.len(), b.len(), diff(&a, &b), c.len(), diff(&a, &c)
    );
    drop(p);
    let mut q = vst3_host::simple::load_plugin_with_settings(&path, 48000.0, 512).unwrap();
    match q.load_state(&c) { Ok(()) => {}, Err(e) => println!("load ERR {e}") }
    let r = q.get_parameters().unwrap().into_iter().find(|x| x.id == pre.id).unwrap();
    println!("fresh restore readback: {} (asked {pval})", r.value);
    // and audio: peak through restored vs default
    let peak = |pl: &mut vst3_host::Plugin| -> f32 {
        pl.start_processing().ok();
        let mut b = AudioBuffers::new(in_ch as usize, in_ch as usize, 512, 48000.0);
        let mut pk = 0f32;
        for _ in 0..24 {
            for (i, s) in b.inputs[0].iter_mut().enumerate() {
                *s = (((i as u32 * 2654435761u32) >> 8) as f32 / (1 << 24) as f32) * 2.0 - 1.0;
            }
            let _ = pl.process_audio(&mut b);
            pk = pk.max(b.outputs[0].iter().fold(0f32, |m, x| m.max(x.abs())));
        }
        pk
    };
    drop(q);
    let mut q2 = vst3_host::simple::load_plugin_with_settings(&path, 48000.0, 512).unwrap();
    q2.load_state(&c).ok();
    let mut d2 = vst3_host::simple::load_plugin_with_settings(&path, 48000.0, 512).unwrap();
    let _ = d2.load_state(&a);
    println!("peak restored {:.4} vs default {:.4}", peak(&mut q2), peak(&mut d2));
}
