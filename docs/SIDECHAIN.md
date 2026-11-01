# Sidechain routing (v32 R5)

## What sidechaining is

Sidechain (dynamic) processing lets *one* signal control a *different*
signal's processor: the classic four-on-the-floor pump is a compressor on
the pad/bus whose detector is driven by the kick, so every kick transient
ducks the pad. The keying signal never appears at the processor's audio
input — it arrives on a separate "sidechain" path that only the detector
sees. VST plugins expose this as an extra input bus (`sidechain` in the
VST3 spec); sequencers expose it as a routing matrix so any track can key
any other track's compressor.

## Design in slooper

Everything runs on the realtime thread with zero allocations and no locks:

* **Send matrix.** Each track owns a 128-bit send mask (`u128`, stored as
  an atomic lo/hi `u64` pair — there is no `AtomicU128`): bit *k* of
  track *t*'s word means "track *k*'s audio keys *t*'s effect chain".
  `Engine::set_sc_send` clears the self-bit on the way in — a track can
  never key itself (that would be ordinary audio feedback through the
  detector).
* **The SC bus.** `Proc` keeps two per-track block buffers: `sc_build`
  accumulates the *current* block's taps, `sc_apply` is what chains
  consume. They swap at the end of every audio block, so a sidechain
  signal is one block late (~2.6 ms at 48 kHz/128) — well below the
  attack time of any real detector, and it buys a strict one-cycle
  feed-forward graph with no loops and no extra latency bookkeeping.
* **The tap.** Inside the sample loop a track sends `dry × amp-ramp`
  (post-trim, **pre-pan**, pre-chain) to every target that lists it.
  Zero cost when no routing exists: sources that nothing keys are
  filtered by one `u128` AND per mixed track; the edge table
  (`e_src`/`e_tgt`, 256 entries) is expanded once per block.
* **Consumers.** `fx_mix` and `run_chain_block` pass the track's
  sidechain row down to scripted slots. Plugins see silence for `sc` —
  VST3 sidechain inputs need extra JACK port plumbing and are future
  work (see below).

## Script ABI v2

A scripted effect opts into the sidechain with a second entry point:

```rust
#[no_mangle]
pub extern "C" fn slooper_effect_info_v2() -> *const EffectInfoV2 { ... }
```

`EffectInfoV2` adds a `sidechain: u32` flag, and the v2 process fn takes
one extra `sc: f32` argument (the summed key for this sample; silence
when nothing is routed). v1 scripts are untouched — the host keeps the
v1 vtable for them and passes no key. The bundled example is
`effects/sidechain_comp.rs`: a feed-forward compressor whose detector
takes `max(|input|, depth × |sc|)`, i.e. key-only ducking at `depth = 1`.

## Mixer integration

* Each mixer strip has an **SC** picker: a popup listing all audible
  tracks with checkmarks (self excluded). The mask is shown as a short
  tag on the strip and on SC-capable chain slots (`"(SC)"`).
* Sends persist per project (`Config::sc_send`, mirrored into the engine
  on boot and on every change; `write_config_deferred`).
* The chain window tags effects loaded through the v2 entry so it is
  visible whether an insert can actually *use* a key.

## Limitations / future work

* **Plugins**: VST3/VST2 sidechain inputs (`BusInfo` sidechain bus, extra
  JACK input ports fed from the same bus) are not wired yet — the SC bus
  currently feeds scripted effects only. The mixer model is identical
  either way, so plugin support is plumbing, not redesign.
* The bus is mono-sum pre-pan (standard for detectors).
* 128 tracks max ⇒ exactly one `u128` per track; no per-target edge
  budget tuning needed at this scale.

## Tests

* `fx.rs`: v2 example compiles and loads, `sidechain` flag set; v1
  scripts still load through the v1 path.
* `e2e_sidechain_ducks_target_track`: real JACK loop — key panned hard
  left, target tone hard right, probe listens to the right bus only (the
  key's own energy can never bias the measurement). Asserts the tone is
  audible unrouted, drops >30 % when the send is armed, and recovers
  after the send is cleared. Sabotage-tested: removing the routing makes
  the pin fail.
