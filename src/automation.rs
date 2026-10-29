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

/// How the curve travels between two nodes (FL Studio's shape set).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    /// Straight line.
    Linear,
    /// S-curve: slow away from both nodes, fast in the middle.
    Smooth,
    /// Leaves the left node fast, eases into the right node.
    Slow,
    /// Eases out of the left node, rushes into the right node.
    Fast,
}

impl Shape {
    pub const ALL: [Shape; 4] = [Shape::Linear, Shape::Smooth, Shape::Slow, Shape::Fast];
    pub fn label(self) -> &'static str {
        match self {
            Shape::Linear => "linear",
            Shape::Smooth => "smooth",
            Shape::Slow => "slow",
            Shape::Fast => "fast",
        }
    }
    pub fn next(self) -> Shape {
        use Shape::*;
        match self {
            Linear => Smooth,
            Smooth => Slow,
            Slow => Fast,
            Fast => Linear,
        }
    }
}

/// One control point of the curve. `t` is a SAMPLE position inside the
/// asset (0 .. len_samples); `v` is the absolute target value.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Node {
    pub t: i64,
    pub v: f32,
    /// Shape of the segment LEADING INTO this node (from the previous one).
    #[serde(default = "shape_linear")]
    pub s: Shape,
}

fn shape_linear() -> Shape {
    Shape::Linear
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
                    s: Shape::Linear,
                },
                Node {
                    t: len,
                    v,
                    s: Shape::Linear,
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
                s: Shape::Linear,
            });
        }
        if self.nodes.len() == 1 {
            let v = self.nodes[0].v;
            self.nodes.push(Node {
                t: self.len_samples,
                v,
                s: Shape::Linear,
            });
        }
        if self.nodes[0].t != 0 {
            let v = self.nodes[0].v;
            self.nodes.insert(
                0,
                Node {
                    t: 0,
                    v,
                    s: Shape::Linear,
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
        let k = warp(b.s, u);
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

/// Segment warp `warp(shape, u)` mapping the linear u through the shape.
pub fn warp(s: Shape, u: f32) -> f32 {
    let u = u.clamp(0.0, 1.0);
    match s {
        Shape::Linear => u,
        // smoothstep
        Shape::Smooth => u * u * (3.0 - 2.0 * u),
        // ease-out cubic: fast start, slow arrival
        Shape::Slow => 1.0 - (1.0 - u).powi(3),
        // ease-in cubic: slow start, fast arrival
        Shape::Fast => u * u * u,
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
                s: Shape::Linear,
            },
            Node {
                t: 1000,
                v: 1.7,
                s: Shape::Linear,
            },
        ]);
        assert!((a.eval(0) - 0.2).abs() < 1e-6);
        assert!((a.eval(500) - 0.95).abs() < 1e-4, "mid {}", a.eval(500));
        assert!((a.eval(1000) - 1.7).abs() < 1e-6);
        assert!((a.eval(9999) - 1.7).abs() < 1e-6, "clamped past the end");
    }

    #[test]
    fn v11_shapes_warp_endpoints_and_mid() {
        for s in Shape::ALL {
            assert!(warp(s, 0.0).abs() < 1e-6, "{s:?} at 0");
            assert!((warp(s, 1.0) - 1.0).abs() < 1e-6, "{s:?} at 1");
        }
        // smooth is symmetric, slow leads, fast lags
        assert!((warp(Shape::Smooth, 0.5) - 0.5).abs() < 1e-6);
        assert!(warp(Shape::Slow, 0.5) > 0.5);
        assert!(warp(Shape::Fast, 0.5) < 0.5);
    }

    #[test]
    fn v11_node_lookup_hits_exact_and_bisects() {
        let a = auto(vec![
            Node {
                t: 0,
                v: 0.0,
                s: Shape::Linear,
            },
            Node {
                t: 100,
                v: 1.0,
                s: Shape::Linear,
            },
            Node {
                t: 200,
                v: 0.5,
                s: Shape::Linear,
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
                s: Shape::Linear,
            },
            Node {
                t: 1000,
                v: 1.0,
                s: Shape::Linear,
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
            s: Shape::Smooth,
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
                s: Shape::Linear,
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
}
