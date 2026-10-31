//! Automation assets: an absolute-value curve bound to a named target.
//!
//! An *automation asset* is the third asset kind next to audio and MIDI.
//! Conceptually it is one curve: nodes at (time, value) with a per-segment
//! curvature, plus a *target id* naming the control the curve drives
//! (`"track.vol"`, `"track.pan"`, `"synth:3"`, `"fx:1:2"`, `"master.tempo"`,
//! …). The asset itself stores NO track number — when a clip references the
//! asset, the curve acts on the track the CLIP sits in, so one saved asset
//! automates track 3 gain in one place and track 1 gain somewhere else
//! (the v0.11 requirement).
//!
//! Values are ABSOLUTE in the target's own unit (linear volume 0..2, pan
//! −1..+1, plugin parameters in the normalized 0..1 domain that the plugin
//! itself publishes through `getParameterDescription` — the documented,
//! host-visible value system), never percentages of a recent range.
//!
//! The evaluation function is allocation-free and f32-only so the realtime
//! thread can call it per block.

use serde::{Deserialize, Serialize};

/// v27: the v11 segment shapes, kept ONLY to load old `.slauto` files.
/// They collapse into the continuous [`Node::c`] curvature on load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LegacyShape {
    Linear,
    Smooth,
    Slow,
    Fast,
}

fn legacy_c(s: LegacyShape) -> f32 {
    match s {
        LegacyShape::Linear => 0.0,
        // The old symmetric S had no exact member of the curvature family;
        // a mild positive curve is the closest feel.
        LegacyShape::Smooth => 0.5,
        // c = ±1 ARE the old cubics exactly (see `warp`).
        LegacyShape::Slow => 1.0,
        LegacyShape::Fast => -1.0,
    }
}

/// One control point of the curve. `t` is a SAMPLE position inside the
/// asset (0 .. len_samples); `v` is the absolute target value.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Node {
    pub t: i64,
    pub v: f32,
    /// v27: CURVATURE of the segment LEADING INTO this node (from the
    /// previous one), one continuous value in −1..=1 like FL Studio's
    /// curvature dial instead of a mode menu:
    /// * `0`  — straight line,
    /// * `+`  — fast away from the previous node, SOFT ARRIVAL here
    ///   (c = +1 is the old `Slow` cubic exactly),
    /// * `−`  — soft away from the previous node, RUSHING ARRIVAL here
    ///   (c = −1 is the old `Fast` cubic exactly).
    #[serde(default, skip_serializing_if = "is_zero_c")]
    pub c: f32,
}

fn is_zero_c(c: &f32) -> bool {
    *c == 0.0
}

/// Wire format: accepts the new `c` and migrates legacy `s` shapes.
#[derive(Deserialize)]
struct NodeWire {
    t: i64,
    v: f32,
    #[serde(default)]
    c: Option<f32>,
    #[serde(default)]
    s: Option<LegacyShape>,
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let w = NodeWire::deserialize(deserializer)?;
        Ok(Node {
            t: w.t,
            v: w.v,
            c: w.c.or(w.s.map(legacy_c)).unwrap_or(0.0).clamp(-1.0, 1.0),
        })
    }
}

/// A saved automation asset: target + curve. Stored as one `.slauto` JSON
/// file in a rack folder, listed in the rack's AUTOMATION section.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Automation {
    /// Target id, e.g. `"track.vol"` (see `describe_target`).
    pub target: String,
    /// Curve duration in samples at the project rate.
    pub len_samples: i64,
    /// Nodes sorted by `t` (invariant maintained by `normalize`).
    pub nodes: Vec<Node>,
}

impl Automation {
    /// Flat asset: constant `v` for `len` samples (start/end nodes).
    pub fn flat(target: impl Into<String>, v: f32, len: i64) -> Automation {
        let len = len.max(1);
        Automation {
            target: target.into(),
            len_samples: len,
            nodes: vec![
                Node {
                    t: 0,
                    v,
                    c: 0.0,
                },
                Node {
                    t: len,
                    v,
                    c: 0.0,
                },
            ],
        }
    }

    /// Sort nodes, clamp times into range and guarantee at least two nodes.
    pub fn normalize(&mut self, min_t: i64, max_v: f32) {
        self.len_samples = self.len_samples.max(1);
        for n in self.nodes.iter_mut() {
            n.t = n.t.clamp(0, self.len_samples);
            if max_v.is_finite() {
                n.v = n.v.clamp(-max_v, max_v);
            }
        }
        self.nodes.sort_by_key(|n| n.t);
        self.nodes.dedup_by(|a, b| a.t == b.t);
        if self.nodes.is_empty() {
            self.nodes.push(Node {
                t: 0,
                v: 0.0,
                c: 0.0,
            });
        }
        if self.nodes.len() == 1 {
            let v = self.nodes[0].v;
            self.nodes.push(Node {
                t: self.len_samples,
                v,
                c: 0.0,
            });
        }
        if self.nodes[0].t != 0 {
            let v = self.nodes[0].v;
            self.nodes.insert(
                0,
                Node {
                    t: 0,
                    v,
                    c: 0.0,
                },
            );
        }
        let _ = (min_t, max_v);
    }

    /// Evaluate the curve at sample offset `t` (clamped to the asset).
    /// O(n) over a handful of nodes — realtime-safe, allocation-free.
    pub fn eval(&self, t: i64) -> f32 {
        let nodes = &self.nodes;
        if nodes.is_empty() {
            return 0.0;
        }
        let t = t.clamp(0, self.len_samples);
        // First node at or after t.
        let hi = match nodes.binary_search_by(|n| n.t.cmp(&t)) {
            Ok(i) => return nodes[i].v,
            Err(i) => i,
        };
        if hi == 0 {
            return nodes[0].v;
        }
        if hi >= nodes.len() {
            return nodes[nodes.len() - 1].v;
        }
        let a = nodes[hi - 1];
        let b = nodes[hi];
        let span = (b.t - a.t).max(1) as f64;
        let u = ((t - a.t) as f64 / span).clamp(0.0, 1.0) as f32;
        let k = warp(b.c, u);
        a.v + (b.v - a.v) * k
    }

    /// Value the target would hold at timeline position `pos` for a clip at
    /// `clip_pos` covering `play_len` source samples from `cut`, looping
    /// the curve with period `len_samples` (R4 semantics: the clip window
    /// CLIPS the asset, never stretches it).
    pub fn eval_at(&self, clip_pos: i64, pos: i64, cut: i64, play_len: i64) -> f32 {
        let rel = pos.saturating_sub(clip_pos).saturating_add(cut);
        let local = if self.len_samples.max(1) == 0 {
            0
        } else {
            rel % self.len_samples.max(1)
        };
        let _ = play_len;
        self.eval(local)
    }

    /// JSON encoding written to `.slauto` files.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| e.to_string())
    }
    pub fn from_json(s: &str) -> Result<Automation, String> {
        serde_json::from_str(s).map_err(|e| e.to_string())
    }
}

/// Segment warp: map the linear `u` through a node's continuous curvature
/// `c` (−1..=1, see [`Node::c`]). Endpoints are exact and the curve is
/// monotonic for every `c`; c = ±1 are the legacy Slow/Fast cubics exactly,
/// c = 0 is the identity.
pub fn warp(c: f32, u: f32) -> f32 {
    let u = u.clamp(0.0, 1.0);
    let c = c.clamp(-1.0, 1.0);
    if c >= 0.0 {
        // ease-out family: slope (1+2c) at u=0, settling into the node.
        1.0 - (1.0 - u).powf(1.0 + 2.0 * c)
    } else {
        // ease-in family: creeping away, rushing into the node.
        u.powf(1.0 - 2.0 * c)
    }
}

/// v26: one-click curve presets for the automation manager, so building a
/// useful curve takes ONE click instead of a node at a time. Times are
/// fractions of the asset length; values are fractions of the TARGET's own
/// range (0 = min, 1 = max), so "fade in" means mute -> +6 dB on volume
/// and left -> right on pan without any unit maths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    FadeIn,
    FadeOut,
    Peak,
    Valley,
    EaseUp,
    EaseDown,
    Saw,
    Square,
    Sine,
}

impl Preset {
    pub const ALL: [Preset; 9] = [
        Preset::FadeIn,
        Preset::FadeOut,
        Preset::Peak,
        Preset::Valley,
        Preset::EaseUp,
        Preset::EaseDown,
        Preset::Saw,
        Preset::Square,
        Preset::Sine,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Preset::FadeIn => "fade in",
            Preset::FadeOut => "fade out",
            Preset::Peak => "peak",
            Preset::Valley => "valley",
            Preset::EaseUp => "ease up",
            Preset::EaseDown => "ease down",
            Preset::Saw => "saw",
            Preset::Square => "square",
            Preset::Sine => "sine",
        }
    }
}

/// Nodes for one preset (first t = 0, last t = len, values inside [lo, hi]).
/// `Saw`/`Square` use 1-sample ramps — they read as jumps on any realistic
/// zoom, and true vertical segments would break the sorted-t invariant.
/// v27: shapes are expressed with the CONTINUOUS curvature `c`, so the
/// results are smoother than the old enum-era presets for the same node
/// count (the sine is the flagship: 9 nodes fit sin to ±0.2 % of range).
pub fn preset_nodes(p: Preset, len: i64, lo: f32, hi: f32) -> Vec<Node> {
    let len = len.max(8);
    let v = |f: f32| lo + f * (hi - lo);
    let n = |u: f32, f: f32, c: f32| Node {
        t: (u * len as f32).round() as i64,
        v: v(f),
        c,
    };
    match p {
        Preset::FadeIn => vec![n(0.0, 0.0, 0.0), n(1.0, 1.0, 0.0)],
        Preset::FadeOut => vec![n(0.0, 1.0, 0.0), n(1.0, 0.0, 0.0)],
        // Hill: rush up into the peak (soft arrival), ease away from it.
        Preset::Peak => vec![
            n(0.0, 0.0, 0.0),
            n(0.5, 1.0, 0.85),
            n(1.0, 0.0, -0.85),
        ],
        Preset::Valley => vec![
            n(0.0, 1.0, 0.0),
            n(0.5, 0.0, 0.85),
            n(1.0, 1.0, -0.85),
        ],
        // Positive curvature = settle softly into the destination value.
        Preset::EaseUp => vec![n(0.0, 0.0, 0.0), n(1.0, 1.0, 0.85)],
        Preset::EaseDown => vec![n(0.0, 1.0, 0.0), n(1.0, 0.0, 0.85)],
        Preset::Saw => vec![
            n(0.0, 0.0, 0.0),
            n((len - 1) as f32 / len as f32, 1.0, 0.0),
            n(1.0, 0.0, 0.0),
        ],
        Preset::Square => vec![
            n(0.0, 0.0, 0.0),
            n(1.0 / len as f32, 1.0, 0.0),
            n(0.5, 1.0, 0.0),
            n(0.5 + 1.0 / len as f32, 0.0, 0.0),
            n(1.0, 0.0, 0.0),
        ],
        Preset::Sine => {
            // Eight 45-degree arcs, each warped by the min-max fitted
            // curvature (±0.06 across the zero crossings, ±0.47 around the
            // extremes): worst error 0.2 % of full range — vs 1.9 % for the
            // old 17-node smoothstep preset — with HALF the nodes to edit.
            const C: [f32; 8] = [0.06, 0.47, -0.47, -0.06, 0.06, 0.47, -0.47, -0.06];
            let mut out = Vec::with_capacity(9);
            for i in 0..8 {
                let u = i as f32 / 8.0;
                out.push(Node {
                    t: (u * len as f32).round() as i64,
                    v: v(0.5 + 0.5 * (std::f32::consts::TAU * u).sin()),
                    c: if i == 0 { 0.0 } else { C[i - 1] },
                });
            }
            out.push(Node {
                t: len,
                v: v(0.5),
                c: C[7],
            });
            out
        }
    }
}

/// Known target ids that address a per-TRACK control (value semantics for
/// the UI; everything else carries its own index).
pub const TARGET_TRACK_VOL: &str = "track.vol";
/// v22: the per-track GAIN TRIM joins the automation domain (the mixer
/// strip enumerates it as a bindable source — evaluating it makes that
/// promise real). Values are LINEAR gain (1.0 = 0 dB; the strip shows dB).
pub const TARGET_TRACK_GAIN: &str = "track.gain";
pub const TARGET_TRACK_PAN: &str = "track.pan";
pub const TARGET_TRACK_MUTE: &str = "track.mute";
pub const TARGET_MASTER_VOL: &str = "master.vol";
pub const TARGET_MASTER_TEMPO: &str = "master.tempo";

/// True for target ids of the form `synth:<idx>` / `fx:<row>:<idx>`.
pub fn is_plugin_target(t: &str) -> bool {
    t.starts_with("synth:") || t.starts_with("fx:")
}

/// Human-readable target name for windows and status lines. Plugin
/// parameter names are resolved by the caller from the live entry; this is
/// the generic fallback.
pub fn describe_target(t: &str) -> String {
    match t {
        TARGET_TRACK_VOL => "track volume".into(),
        TARGET_TRACK_GAIN => "track gain".into(),
        TARGET_TRACK_PAN => "track pan".into(),
        TARGET_TRACK_MUTE => "track mute".into(),
        TARGET_MASTER_VOL => "master volume".into(),
        TARGET_MASTER_TEMPO => "tempo (bpm)".into(),
        _ if t.starts_with("synth:") => {
            format!("synth param {}", &t["synth:".len()..])
        }
        _ if t.starts_with("fx:") => {
            let rest = &t["fx:".len()..];
            match rest.split_once(':') {
                Some((r, i)) => format!("effect row {r} param {i}"),
                None => format!("effect {rest}"),
            }
        }
        other => other.to_string(),
    }
}

/// Value range a target edits, for slider/number clamps in the manager.
/// `(min, max)`; plugin parameters are normalized 0..1 (their documented
/// host-visible domain).
pub fn target_range(t: &str) -> (f32, f32) {
    match t {
        TARGET_TRACK_VOL => (0.0, 2.0),
        // 127 on the gain strip = +10 dB = 3.162 linear; canvas clamp.
        TARGET_TRACK_GAIN => (0.0, 3.162),
        TARGET_TRACK_PAN => (-1.0, 1.0),
        TARGET_TRACK_MUTE => (0.0, 1.0),
        TARGET_MASTER_VOL => (0.0, 2.0),
        TARGET_MASTER_TEMPO => (20.0, 300.0),
        _ => (0.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto(nodes: Vec<Node>) -> Automation {
        let len = nodes.last().map(|n| n.t.max(1)).unwrap_or(1);
        Automation {
            target: "track.vol".into(),
            len_samples: len,
            nodes,
        }
    }

    #[test]
    fn v11_eval_linear_interpolates_absolute_values() {
        // volume ramp 0.2 -> 1.7 over 1000 samples: absolute, not %.
        let a = auto(vec![
            Node {
                t: 0,
                v: 0.2,
                c: 0.0,
            },
            Node {
                t: 1000,
                v: 1.7,
                c: 0.0,
            },
        ]);
        assert!((a.eval(0) - 0.2).abs() < 1e-6);
        assert!((a.eval(500) - 0.95).abs() < 1e-4, "mid {}", a.eval(500));
        assert!((a.eval(1000) - 1.7).abs() < 1e-6);
        assert!((a.eval(9999) - 1.7).abs() < 1e-6, "clamped past the end");
    }

    #[test]
    fn v11_shapes_warp_endpoints_and_mid() {
        // v27: the shape enum became the continuous curvature; the legacy
        // extremes are the exact cubics, 0 is linear, monotone throughout.
        for c in [-1.0, -0.5, 0.0, 0.5, 1.0] {
            assert!(warp(c, 0.0).abs() < 1e-6, "c={c} at 0");
            assert!((warp(c, 1.0) - 1.0).abs() < 1e-6, "c={c} at 1");
            let mut prev = -1.0;
            for i in 0..=100 {
                let u = i as f32 / 100.0;
                let k = warp(c, u);
                assert!(k >= prev - 1e-6, "c={c} monotone at {u}");
                prev = k;
            }
        }
        assert!((warp(0.0, 0.5) - 0.5).abs() < 1e-6, "0 is linear");
        assert!((warp(1.0, 0.5) - 0.875).abs() < 1e-5, "+1 = old Slow cubic");
        assert!((warp(-1.0, 0.5) - 0.125).abs() < 1e-5, "-1 = old Fast cubic");
        assert!(warp(0.6, 0.5) > 0.5, "positive leads");
        assert!(warp(-0.6, 0.5) < 0.5, "negative lags");
    }

    /// v27: legacy `s` shapes in old .slauto files migrate to curvature;
    /// new saves write `c` (and omit it when straight).
    #[test]
    fn v27_legacy_shape_migrates_to_curvature() {
        let legacy = r#"{"target":"track.vol","len_samples":100,
            "nodes":[{"t":0,"v":0.0},{"t":50,"v":1.0,"s":"slow"},{"t":100,"v":0.0,"s":"fast"}]}"#;
        let a = Automation::from_json(legacy).unwrap();
        assert_eq!(a.nodes[1].c, 1.0);
        assert_eq!(a.nodes[2].c, -1.0);
        // old "smooth" becomes the documented mild positive curvature
        let legacy2 = r#"{"target":"track.vol","len_samples":100,
            "nodes":[{"t":0,"v":0.0},{"t":100,"v":1.0,"s":"smooth"}]}"#;
        assert!((Automation::from_json(legacy2).unwrap().nodes[1].c - 0.5).abs() < 1e-6);
        // round-trip keeps c and no longer writes s
        let j = a.to_json().unwrap();
        assert!(j.contains("\"c\": 1.0"), "{j}");
        assert!(!j.contains("\"s\""), "{j}");
        let b = Automation::from_json(&j).unwrap();
        assert_eq!(b.nodes[1].c, 1.0);
    }

    /// v27: the 9-node curvature sine beats the old 17-node preset by a
    /// wide margin — max error < 0.4 % of the target's full range.
    #[test]
    fn v27_sine_preset_fidelity() {
        let len = 96_000i64;
        let nodes = preset_nodes(Preset::Sine, len, 0.0, 2.0);
        assert_eq!(nodes.len(), 9);
        let a = Automation {
            target: "track.vol".into(),
            len_samples: len,
            nodes,
        };
        let mut worst = 0.0f32;
        for i in 0..=2000 {
            let t = (i as f64 / 2000.0 * len as f64) as i64;
            let u = t as f64 / len as f64;
            let want = 1.0 + (2.0 * std::f64::consts::PI * u).sin();
            worst = worst.max((a.eval(t) - want as f32).abs());
        }
        assert!(worst < 0.008, "sine max error {worst} of range 2.0");
    }

    #[test]
    fn v11_node_lookup_hits_exact_and_bisects() {
        let a = auto(vec![
            Node {
                t: 0,
                v: 0.0,
                c: 0.0,
            },
            Node {
                t: 100,
                v: 1.0,
                c: 0.0,
            },
            Node {
                t: 200,
                v: 0.5,
                c: 0.0,
            },
        ]);
        assert_eq!(a.eval(100), 1.0);
        assert!((a.eval(150) - 0.75).abs() < 1e-5);
    }

    #[test]
    fn v11_clip_window_loops_without_stretching() {
        // R4 semantics: a clip window CLIPS the curve; half-length clips
        // still see the ORIGINAL ramp, and the curve repeats per asset len.
        let a = auto(vec![
            Node {
                t: 0,
                v: 0.0,
                c: 0.0,
            },
            Node {
                t: 1000,
                v: 1.0,
                c: 0.0,
            },
        ]);
        assert!((a.eval_at(0, 500, 0, 500) - 0.5).abs() < 1e-5);
        assert!(
            (a.eval_at(0, 1000, 0, 1000) - 0.0).abs() < 1e-5,
            "wraps at the asset len"
        );
        assert!(
            (a.eval_at(10, 610, 100, 400) - 0.7).abs() < 1e-5,
            "cut shifts the window"
        );
    }

    #[test]
    fn v11_json_roundtrip_and_flat() {
        let mut a = Automation::flat("track.pan", -0.25, 2400);
        a.nodes.push(Node {
            t: 1200,
            v: 0.5,
            c: 0.5,
        });
        a.normalize(0, f32::INFINITY);
        let j = a.to_json().unwrap();
        let b = Automation::from_json(&j).unwrap();
        assert_eq!(b.target, "track.pan");
        assert_eq!(b.nodes.len(), 3);
        assert!((b.eval(1200) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn v11_normalize_pins_ends_and_dedups() {
        let mut a = Automation {
            target: "track.vol".into(),
            len_samples: 100,
            nodes: vec![Node {
                t: 50,
                v: 0.5,
                c: 0.0,
            }],
        };
        a.normalize(0, f32::INFINITY);
        // Endpoints pinned; the user node at 50 is kept (it is flat there,
        // but the editor must not silently drop user control points).
        assert_eq!(a.nodes.len(), 3);
        assert_eq!(a.nodes[0].t, 0);
        assert_eq!(a.nodes[0].v, 0.5);
        assert_eq!(a.nodes[1].t, 50);
        assert_eq!(a.nodes.last().unwrap().t, 100);
        assert_eq!(a.nodes.last().unwrap().v, 0.5);
    }

    #[test]
    fn v11_target_helpers() {
        assert_eq!(describe_target("track.vol"), "track volume");
        assert_eq!(describe_target("fx:2:3"), "effect row 2 param 3");
        assert!(is_plugin_target("synth:0"));
        assert!(!is_plugin_target("track.vol"));
        assert_eq!(target_range(TARGET_MASTER_TEMPO), (20.0, 300.0));
        assert_eq!(target_range("synth:5"), (0.0, 1.0));
    }

    /// v26: every preset is a VALID automation curve: ends pinned at
    /// 0/len, strictly increasing times, values inside the target range —
    /// and the representative shapes actually shape.
    #[test]
    fn v26_preset_nodes_shapes() {
        for p in Preset::ALL {
            let nodes = preset_nodes(p, 96_000, 0.0, 2.0);
            assert!(nodes.len() >= 2, "{}: node count", p.label());
            assert_eq!(nodes.first().unwrap().t, 0, "{}: start", p.label());
            assert_eq!(nodes.last().unwrap().t, 96_000, "{}: end", p.label());
            for w in nodes.windows(2) {
                assert!(w[1].t > w[0].t, "{}: times strictly sorted", p.label());
            }
            for n in &nodes {
                assert!(
                    n.v >= -1e-6 && n.v <= 2.0 + 1e-6,
                    "{}: value {} out of range",
                    p.label(),
                    n.v
                );
            }
            let a = Automation {
                target: "track.vol".into(),
                len_samples: 96_000,
                nodes,
            };
            match p {
                Preset::FadeIn => assert!(a.eval(24_000) < a.eval(72_000)),
                Preset::Peak => assert!(a.eval(48_000) > 1.9 && a.eval(0) < 0.1),
                Preset::Valley => assert!(a.eval(48_000) < 0.1 && a.eval(0) > 1.9),
                Preset::Sine => {
                    assert_eq!(a.nodes.len(), 9, "v27 curvature sine");
                    assert!((a.eval(24_000) - 2.0).abs() < 0.01, "sine peak");
                    assert!((a.eval(72_000) - 0.0).abs() < 0.01, "sine trough");
                    assert!((a.eval(0) - 1.0).abs() < 1e-3, "sine starts mid-range");
                }
                Preset::Square => {
                    assert!(a.eval(24_000) > 1.9 && a.eval(72_000) < 0.1);
                }
                _ => {}
            }
        }
    }
}
