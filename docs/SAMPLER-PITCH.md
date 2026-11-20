# Sampler pitch: in-roll per-note pitch curves (sampler-pitch series)

Status: SPEC (pb-1 deliverable). Consumers: **pb-2-curve-engine** (data
model + engine), **pb-3-inroll-ui** (interaction). Card: t_ee1cea7d.
Author: feature-creator, 2026-11-19.

User report (2026-11-18): the sampler pitch bend "is not intuitive and
might not work as intended (so hard to use I can't tell)". Requested
model: septabee's approach — draw the curve directly on the note IN the
piano roll, no separate glide bar.

---

## 1. What exists today (two systems — the report conflates them)

**S1. Sample-editor render sweep** (`src/sample_edit.rs:84-106`,
applied at `268-272`): per-ASSET linear ramp
`bend_start_cents → bend_end_cents` (±2400), edited as a two-dot line +
DragValues in the sample editor (`src/ui.rs:6599-6758`). Rendered
offline into the asset buffer. **Works** (own pins in `sample_edit.rs`).
Not per-note; bakes into the sample, affects every clip of the asset.

**S2. Piano-roll glide lane** (v52 R6): per-CLIP `BendSeg`
(`src/midipat.rs:60-108`: `(t0,v0)-(t1,v1)` in semitones + curve warp,
absolute clip ticks), edited in a separate lane below the roll
(render `src/ui.rs:11809+`, numeric rows `~11473`, hit tests
`~11929-12210`, Z-drag/Shift+RMB language). Source of truth is
`bend_segs`; `Pattern::bake_bends` samples them into the standard
14-bit `bends` lane (`BEND_FULL_ST = 12.0` st full scale).

## 2. Current-behavior verdict (Scope 2)

**Verdict: PARTIALLY BROKEN — broken exactly in the user's case
(bundled sampler); works only for VST instruments.**

| Stage | State | Evidence |
|---|---|---|
| UI: drag in lane → segment | ✅ works | `v52_roll_glide_lane_drag_creates_baked_glide` GREEN on main 77e7681 (2026-11-19, `test result: ok. 1 passed`) |
| State: segment → baked 14-bit lane | ✅ works | same pin asserts `bends` non-empty; numeric rows + delete pins (v52 family) green in CI |
| Sound on **scripted synths** (bundled sampler/piano/drums/fm/…) | ✅ **FIXED v100** | was ❌ dead: host dropped every bend event (`4 \| 5 => {}` in FIVE arms — offline `render_midi_events` +3500/:3727/:3833/:3928-era sites + three audition renderers). Spike on main 77e7681: bake 0→12 st vs flat → **byte-identical audio**. v100: synth ABI v2 (`SynthInfoV2 {abi, bend}` behind optional `slooper_synth_info_v2`) routes roll-space semitones to every arm; all bundled scripts pitch sounding voices by 2^(st/12). Pins `v100_*` (lib + live-RT e2e). |
| Roll-preview audition | ✅ **FIXED v100** | the kind-5 staging at `src/ui.rs:10384` now lands: audition renderers carry the bend arm; pinned live-RT (`v100_audition_baked_glide_heard_on_scripted_synth_live_rt`). |
| Sound on **VST instruments** | ⚠️ works, with a scale trap | `SynthHandle::Vst` forwards `0xE0` raw bytes (`src/audio/mod.rs:3513-3537`). But our full scale = ±12 st (`midipat.rs:77-80`); most plugins default to ±2 st bend range → a drawn octave glide sounds ~6× narrower unless the user retunes the plugin. This is the second half of "I can't tell if it works". |

**→ For scrum-master:** the scripted-synth drop is a real bug class
("glide lane draws a pretty curve that does nothing on default
instruments") — file a separate bug card (per release comment of
2026-11-19). The pb-2 ABI work subsumes the fix, but the bug should
exist as its own card for the changelog. The VST ±2 st default range
mismatch is a second, independent finding (fix = per-track
"bend range in semitones" sent as RPN0, or scale editor-space to the
track's declared range — NOT in pb-2/3 scope; propose follow-up card).

## 3. Reference: how septabee does it (Scope 1)

Septabee (free DAW by Lost Robot; download page
<https://septabee.nekoweb.org>, binaries at `downloads.septabee.com`,
packaged e.g. <https://github.com/JuxGD/septabee-nix>, review
<https://cdm.link/septabee-is-here/>). `septabee.org` (the user's link)
NXDOMAINs on 2026-11-19 — the community wiki is the live docs authority.
The user's "Septor sampler" name matches no wiki/changelog page
(sampler module is documented simply as "Sampler"); the *interaction*
they describe is the documented Waggleboard (piano roll) bend, verified
below. "Septor" may be a recent in-app module rename the wiki hasn't
caught up with.

Primary sources:
- Waggleboard page: <https://septabee.miraheze.org/wiki/Waggleboard>
- Changelog: <https://septabee.miraheze.org/wiki/Changelog> (entries quoted below)

**Where the curve lives.** ON THE NOTE. B_T7 (2026-09-10):
"The Waggleboard supports pitch bending now! Just hold Z and drag. You
can also hold Alt+Z to adjust the curve of the bend. All of this
automatically updates the frequency note parameter with perfect
sample-accuracy." The bend IS a per-note pitch modulation of the note's
frequency parameter — no channel lane, no separate editor.

**How it is drawn.** Hovering a note: `Z + drag` = start/draw a bend,
dragging defines the pitch-over-time path (keybind table: "Pitch Bend:
Hold Z then drag — allows smoothly shifting a note's pitch over time").
`Alt + Z + drag` = adjust the bend's CURVE (shape), not its endpoints.
Drawing is freehand; while dragging, quantize-modifier is Shift.
Visuals follow: B_T8 "Equave ghosts and note colors now follow note
pitch bending" — a bent note is DRAWN as its curve (trailing trail from
the note), and "Improved caching of pitch-bent note drawing" (later B_T)
confirms per-note curve geometry is painted in-roll.

**Snapping / note length.** The curve is carried by the note: moving or
cloning the note carries the bend ("Fixed a Waggleboard bug where
cloning notes…" + per-note model); the curve is expressed against the
note (frequency parameter is evaluated while the note rings), so it
stretches with the note body — culling and previews account for it
(B_T13 "Fixed pitch-bent notes being culled incorrectly").

**Audition.** B_T13: "Note clips now properly show pitch bends in the
previews" and "Waggleboard note previews now take pitch bending into
account" — the CLIP thumbnail shows the curve and the ROLL PREVIEW
(audition) plays it. B_T13 also fixed "notes with pitch bending
ignoring global detune" → bends COMPOSE with detune (multiplicative).

**Secondary industry reference** (same design family, shipped earlier):
MIDISketch "Pitch Curves" — "Pitch bends are drawn on the note itself,
right in the roll" — <https://eliotonic.com/docs/> (Piano Roll · Pitch
Curves section). Confirms: in-roll on-note curves are becoming the
expected idiom for 808/sampler glides; FL Studio-style per-clip automation lanes are what people are escaping.

## 4. Interaction model for pb-3 (decision)

Keep our Z gesture (already the glide-lane language — and coincidentally
septabee's). The glide lane strip is REPLACED by on-note curves.

- **Draw**: hover/hold a note → `Z + drag` outward from the note body.
  The stroke starts at (note start, 0 ¢) and follows the pointer:
  x maps to time within the note (clamped to note length), y maps to
  cents (note's row = 0 ¢). Release commits; the freehand stroke is
  simplified to ≤16 control points (Ramer–Douglas–Peucker, ε ≈ 4 ¢).
- **Land-on-target preserved**: the v52 "Z-drag from note A, release
  over note B" gesture survives as sugar — on release over B, the curve
  on A ends at `pitch(B) − pitch(A)` cents at A's end. (This makes
  808-style note-to-note slides one gesture.)
- **Edit**: selecting a note reveals its control points as dots on the
  note; drag a dot (Shift = snap t to grid, Ctrl = snap cents to 100).
  `Alt+Z` + drag on the note = whole-curve magnitude warp (mirrors
  septabee's Alt+Z curve adjust). Right-click note = menu: *clear pitch
  curve*, *numeric editor* (replaces the v52 glide numeric rows; reuse
  that widget). Shift+RMB on the curve = delete (existing language).
- **Visual**: note body grows a curve trail (cents ↕, note row = 0;
  ±1200 ¢ fills the trail box; auto-scales past ±1200 up to ±2400).
  Trail painted per note, follows the note on move/clone/resize.
- **Audition**: commit (mouse-up after draw/edit) auto-previews the note
  once through the TRACK's synth — this is what makes the feature
  "tellable". Live preview during drag is the pb-3 stretch goal.
- **Retire**: the glide-lane strip renders ONLY when the loaded project
  still has legacy `bend_segs` that couldn't migrate (§ 6, "orphan"
  case); otherwise gone from the UI. Pins asserting the lane (v52
  family) get re-targeted to the in-roll equivalents IN the same commit
  (contract-change rule).

Rejected alternatives (recorded per design protocol):
- (a) *Keep the lane, just fix the script drop* — fixes the bug but not
  the UX complaint; rejected by the user's explicit ask.
- (b) *MPE note-expression (per-voice y) instead of pitch* — wrong
  semantics for sampler slides; enormous ABI/plugin surface.
- (c) *Promote the per-asset sample-editor sweep* — cannot do per-note
  or note-to-note slides (single baked ramp per asset).
- (d) *Host-side output resampling of the synth bus* — phasing/formant
  artifacts, polyphonic ambiguity.

## 5. Data model for pb-2 (decision)

Per-NOTE (septor parity). On `Pattern`:

```rust
/// One in-roll pitch curve, attached to its note.
pub struct NoteBend {
    /// Control points, sorted by t, strictly increasing t.
    /// t = fraction of the NOTE LENGTH [0..=1]; cents = semitone x100
    /// offset from the note's own row, clamped ±2400.
    pub pts: Vec<(f32, f32)>,   // 2..=16 points, UI enforces cap
}
// Pattern gains:
pub note_bends: Vec<NoteBend>,        // table; handles below index it
// Note gains (Copy-safe):
pub bend: Option<u32>,                // index into note_bends
```

- **Remap discipline** (asset-ids-are-indices trap): removing/moving a
  NOTE must remap/close `bend` handles in `note_bends` exactly like
  `remove_asset_at` does for assets — pb-2 pins required.
- **Interpolation**: monotone piecewise cubic Hermite (PCHIP) through
  the points (no overshoot on vibrato strokes); 2 points degrades to
  the old linear sweep exactly; legacy `curve` warp maps to an extra
  midpoint (§ 6). Evaluation is per-SAMPLE (septor parity:
  sample-accuracy) → freq multiplier `2^(cents/1200)` composing
  MULTIPLICATIVELY after the note's base frequency and track/global
  detune (B_T13 lesson: bends must compose with detune).
- **End behavior**: value at t=1 (last point) holds until note-off;
  no automatic return to 0 (808 slides stay put — matches FL/septor
  behavior and the old `bake_seg` lane semantics).
- **Engine delivery (the actual fix for § 2)**: scripted synth **ABI
  v2** must deliver per-voice pitch offset. pb-2 chooses the mechanism
  under RT hard rules (no per-block alloc; publish tables + generation
  bump; `SLOOPER_NULL_AUDIO` observability caveat): candidate shapes =
  (a) host publishes a per-block `bend_cents[voice]` snapshot table the
  script samples per block via a new `synth_api` call, or (b)
  per-sample table view handed to the script's process callback.
  VST path needs NOTHING new (raw 0xE0 already flows) — but VST and
  script content must both derive from `note_bends` (single truth).
- **Derived bakes stay**: `bake_bends` becomes a DERIVED lane (per-note
  curves → unified 14-bit channel lane) for MIDI export and the VST
  instrument feed. Overlapping bent notes: last-onset note wins each
  sample interval (document; single-truth conflicts are display-only
  until pb-3 shows them).
- **Range decision**: editor space stays ±2400 ¢ full scale (matches
  the sample-editor sweep users already see). The VST-receiver range
  mismatch (§ 2) is OUT of pb-2/3 — separate card.

## 6. Migration (decision)

On project load, once, when `bend_segs` non-empty (serde-default the new
fields; no project-format bump beyond adding them):

1. For each `BendSeg`, find its OWNER note: the last note with
   `start ≤ t0` whose body contains t0 (tolerance: 1 tick). Value
   re-normalizes: `pts = sample(v(t))` at t_norm = `(t − note.start) /
   note.len` over `[t0,t1] ∩ [note.start, note.end]`; the legacy warp
   contributes one extra midpoint at the warped position (so 2 pts
   linear or 3 pts warped). Values beyond the note end are clamped to
   the note-end value (per-note model has no empty-time curve —
   documented edge).
2. **Orphans** (no containing note — glides drawn into gaps): keep the
   segment in `bend_segs` untouched; the legacy strip shows it in a
   read-only-ish "legacy" mode and continues to bake it. (Never lose
   sound on load.)
3. First save writes `note_bends`; `bend_segs` keeps round-tripping for
   old-app backward compat until the NEXT release line drops it.
4. pb-3 hides the lane strip whenever no orphan segs exist; the v52
   lane pins migrate to on-note gesture pins in the same commit.

## 7. Acceptance handed down

- pb-2 done when: `note_bends` + interpolation + ABI delivery have
  pins, incl. a render A/B pin (this spike promoted to a REAL pin:
  bend ≠ flat for scripted piano, identical timestamps; sabotage-proof)
  and a remap pin (note remove → handle remap).
- pb-3 done when: vNN_ pins drive draw/edit/land-on-target/audition
  through `frame()`, v52 lane pins re-targeted, boot probe clicks
  through, CI green incl. windows-native full suite.
