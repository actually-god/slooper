//! v54 design tokens — the single source for the app's chrome colors.
//!
//! The UI accent (selection frames, active states, drag highlights, the
//! bridge kit's value readouts) is painted in ~20 places across the main
//! window, the dock chrome, and the plugin bridge. Before v54 those sites
//! hand-wrote two nearly identical blues, `(150,200,255)` and
//! `(150,215,255)`, so a highlight could look subtly different in the
//! piano roll vs the mixer. Rule: never write an accent RGB again —
//! import it from here.
//!
//! Which colors belong here: CHROME (accents, text/edge tokens shared by
//! several widgets/windows). Which do NOT: data palettes — clip/note/grid
//! colors live with their owning painter (`COL_CLIP`, `COL_PLAYHEAD`, the
//! note-state blues in `roll_inner`) because they encode meaning, not
//! theme. The plugin canvas fill (`KIT_BG`) deliberately stays dark in
//! both themes: script canvas colors are dark-authored RGBA floats handed
//! to the host verbatim, so a light canvas would silently hide them
//! (documented for script authors in effect_api).

use egui::{Color32, Key, PointerButton, Rect, Response, TextStyle, Ui, Visuals};

/// The one accent: selection frames, drag highlights, kit value readouts,
/// the knob needle. Light-mode plugin editors borrow this same blue from
/// the egui theme (`Kit::of`), dark mode uses it verbatim.
pub(crate) const ACCENT: Color32 = Color32::from_rgb(150, 200, 255);

/// Custom script-canvas fill — deliberately DARK IN BOTH THEMES (see the
/// module docs). The frame edge around it is `EDGE`.
pub(crate) const KIT_BG: Color32 = Color32::from_rgb(21, 23, 28);
pub(crate) const EDGE: Color32 = Color32::from_gray(58);
/// Bridge caption text (dark theme). Light mode resolves through `Kit`.
pub(crate) const TEXT: Color32 = Color32::from_gray(170);
pub(crate) const MUTED: Color32 = Color32::from_gray(110);
/// v80 (TS-005): the ONE danger/error text token — "this thing is broken
/// or bypassed" labels (not-loaded chain rows, dead plugin bridges).
/// Frozen at the historical chain-window error red; a local `from_rgb`
/// for error text is a drift bug, import this instead.
pub(crate) const DANGER: Color32 = Color32::from_rgb(220, 120, 120);

/// Theme-resolved kit chrome for the bridge widgets. Dark mode returns
/// the historical constants byte-for-byte (pins freeze them); light mode
/// derives from the active egui `Visuals`, so a light-themed host never
/// paints gray-170 captions or dark knob blobs onto white panels.
pub(crate) struct Kit {
    /// Knob face fill, idle / hovered-or-dragged.
    pub knob_fill: Color32,
    pub knob_fill_hover: Color32,
    /// Knob ring stroke, idle / hovered-or-dragged.
    pub ring: Color32,
    pub ring_hover: Color32,
    /// Caption label text.
    pub text: Color32,
    /// Accent role (needle, live-value readout).
    pub accent: Color32,
}

impl Kit {
    pub(crate) fn of(v: &Visuals) -> Kit {
        if v.dark_mode {
            Kit {
                knob_fill: Color32::from_rgb(48, 52, 60),
                knob_fill_hover: Color32::from_rgb(60, 64, 74),
                ring: MUTED,
                ring_hover: TEXT,
                text: TEXT,
                accent: ACCENT,
            }
        } else {
            // Light theme: the knob is a slightly-shaded disc on the
            // panel, the ring reads as its outline, captions use egui's
            // own text colors, and the accent follows egui's link blue so
            // the bridge matches the host window it sits in.
            Kit {
                knob_fill: Color32::from_gray(214),
                knob_fill_hover: Color32::from_gray(198),
                ring: Color32::from_gray(120),
                ring_hover: Color32::from_gray(60),
                text: v.weak_text_color(),
                accent: v.hyperlink_color,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// v87 (TS-013): truncated-row detection — the ONE measurement home.
//
// A row counts as truncated only when its painted label actually loses
// glyphs: it runs past the container's right clip edge (a long unbreakable
// token — the v76 "70-char space-less filename" class), or wrapped rows
// exceed a FIXED row height (the ♪ picker's `add_sized([w, 24])` class).
// Never guess truncation from character counts, and never attach a
// full-identity tooltip to a row that is not truncated (TS-013).
// ---------------------------------------------------------------------------

/// Post-paint right-overflow test: the row painted wider than the enclosing
/// clip allows, so its tail is silently cropped by the container.
pub(crate) fn row_clipped(resp: &Response, ui: &Ui) -> bool {
    resp.rect.max.x > ui.clip_rect().max.x + 0.5
}

/// Pre-paint test for a row that will lay its label out inside
/// `avail_w` (and an optional fixed height `max_h`): is `text` truncated —
/// a laid-out row wider than the space (no break point), or taller than
/// the fixed row (wrapped rows vanish below the clip)? Text that fits, or
/// wraps into fully-visible space, is NOT truncated: no tooltip grows for
/// it (TS-013 negative polarity).
pub(crate) fn label_clipped(ui: &Ui, text: &str, avail_w: f32, style: TextStyle, max_h: Option<f32>) -> bool {
    let fid = style.resolve(ui.style());
    let col = ui.visuals().widgets.noninteractive.fg_stroke.color;
    let (over_w, h) = ui.ctx().fonts_mut(|fb| {
        let j = egui::epaint::text::LayoutJob::simple(text.to_string(), fid, col, avail_w);
        let g = fb.layout_job(j);
        let over = g.rows.iter().any(|pr| pr.row.size.x > avail_w + 0.5);
        (over, g.size().y)
    });
    over_w || max_h.is_some_and(|hmax| h > hmax + 1.0)
}

// ---------------------------------------------------------------------------
// v152 (t_5bd4fc33): the ONE armed-popup dismissal predicate.
//
// Three ad-hoc popup windows (automation value menu, command palette,
// window-slot menu) each hand-rolled the same "armed && click outside the
// rect" dismissal check, and the copies immediately drifted (palette keyed
// no Escape and forgave 6 px; the other two keyed Escape at 4 px). One
// predicate now; the per-surface differences are EXPLICIT parameters, never
// silent divergence.
//
// Contract (byte-identical to the three former copies):
//   - `armed` gates everything: on the frame that OPENED a menu, the press
//     that opened it must not also count as the click-away (all three
//     surfaces keep an `*_armed` field for this one-frame exclusion).
//   - A fresh primary press, secondary press, or (when `escape` is set) the
//     Escape key dismisses — Escape evaluated THIRD to preserve the
//     short-circuit order of the originals.
//   - "Outside" = the pointer hover position (fallback: the rect center, so
//     an Escape with no known hover never dismisses on its own) is not
//     inside rect.expand(pad) — the pad forgives the window border/shadow.
//
// Surfaces that own a DIFFERENT Escape close (the command palette closes
// unconditionally on Escape, modal: the query box owns typing, not gated by
// armed) pass `escape = false` — the key is consumed upstream and a second
// dismissal there would double-fire with different semantics.
// ---------------------------------------------------------------------------

/// `armed && (primary-click | secondary-click | (escape && Escape))` with the
/// hover point OUTSIDE `rect.expand(pad)` — see the section doc for the law.
pub(crate) fn armed_outside_click(ui: &Ui, armed: bool, rect: Rect, pad: f32, escape: bool) -> bool {
    armed
        && ui.ctx().input(|i| {
            (i.pointer.button_clicked(PointerButton::Primary)
                || i.pointer.button_clicked(PointerButton::Secondary)
                || (escape && i.key_pressed(Key::Escape)))
                && !rect
                    .expand(pad)
                    .contains(i.pointer.hover_pos().unwrap_or(rect.center()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v54_kit_token_values() {
        // Frozen on purpose: pins in ui::tests filter painted shapes by
        // these colors, and the dock-ghost pins were retargeted to
        // ACCENT. Changing a value here MUST be a deliberate design
        // decision, never a drive-by edit.
        assert_eq!(ACCENT, Color32::from_rgb(150, 200, 255));
        assert_eq!(KIT_BG, Color32::from_rgb(21, 23, 28));
        assert_eq!(EDGE, Color32::from_gray(58));
        assert_eq!(TEXT, Color32::from_gray(170));
        assert_eq!(MUTED, Color32::from_gray(110));
    }

    #[test]
    fn v80_kit_danger_token_frozen() {
        // v80: error-text token — frozen at the red the chain window
        // hand-wrote before v80 (zero visual drift); local error-reds
        // are pinned-out by v80_chain_chrome_kit_only.
        assert_eq!(DANGER, Color32::from_rgb(220, 120, 120));
    }

    #[test]
    fn v54_kit_dark_freezes_history_light_follows_visuals() {
        let dark = Kit::of(&Visuals::dark());
        // The exact knob colors the bridge painted before v54.
        assert_eq!(dark.knob_fill, Color32::from_rgb(48, 52, 60));
        assert_eq!(dark.knob_fill_hover, Color32::from_rgb(60, 64, 74));
        assert_eq!(dark.ring, Color32::from_gray(110));
        assert_eq!(dark.ring_hover, Color32::from_gray(170));
        assert_eq!(dark.text, Color32::from_gray(170));
        assert_eq!(dark.accent, ACCENT);

        let light = Kit::of(&Visuals::light());
        // Readable on white: caption + ring must be DARK, knob face LIGHT.
        let lum =
            |c: Color32| (c.r() as u32 * 299 + c.g() as u32 * 587 + c.b() as u32 * 114) / 1000;
        assert!(lum(light.text) < 128, "light caption must be dark");
        assert!(lum(light.ring) < 190, "light ring must read on white");
        assert!(lum(light.knob_fill) > 150, "light knob stays light");
        assert!(lum(light.accent) < 190, "light accent keeps contrast");
        // and it must actually be the theme's own colors
        let v = Visuals::light();
        assert_eq!(light.text, v.weak_text_color());
        assert_eq!(light.accent, v.hyperlink_color);
    }

    // ---- v152: the ONE armed-popup dismissal predicate (t_5bd4fc33) ----
    //
    // Driven through a REAL egui Context: events are injected as RawInput,
    // the helper runs inside a live Area's Ui, and the verdict is read back.
    // Semantics are pinned as the UNION of the three former hand-rolled
    // copies (value/win menus: escape=true pad=4; palette: escape=false pad=6).

    /// The rect the dismissal test runs against; `probe` renders one frame
    /// with `events` injected and asks the seam for its verdict.
    fn v152_verdict(
        events: Vec<egui::Event>,
        armed: bool,
        pad: f32,
        escape: bool,
    ) -> bool {
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(120.0, 80.0));
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.events = events;
        // egui 0.36: run_ui takes FnOnce(&mut Context) and returns FullOutput.
        // The FullOutput MUST be consumed — the first run installs the font
        // atlas and dropping an un-cleared TexturesDelta panics (0.36 trap).
        let mut got = false;
        let mut out = ctx.run_ui(raw, |ctx| {
            egui::Area::new(egui::Id::new("v152_probe"))
                .fixed_pos(egui::pos2(10.0, 10.0))
                .show(ctx, |ui| {
                    got = armed_outside_click(ui, armed, rect, pad, escape);
                });
        });
        out.textures_delta.clear();
        got
    }

    fn v152_click(pos: egui::Pos2, btn: egui::PointerButton) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerButton { pos, button: btn, pressed: true, modifiers: Default::default() },
            egui::Event::PointerButton { pos, button: btn, pressed: false, modifiers: Default::default() },
        ]
    }

    fn v152_escape(hover: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(hover),
            egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            },
        ]
    }

    const OUT: egui::Pos2 = egui::pos2(500.0, 400.0);
    const IN: egui::Pos2 = egui::pos2(140.0, 130.0);

    #[test]
    fn v152_kit_dismiss_press_semantics() {
        // A fresh press OUTSIDE (either button) dismisses; inside does not.
        assert!(v152_verdict(v152_click(OUT, egui::PointerButton::Primary), true, 4.0, true));
        assert!(v152_verdict(v152_click(OUT, egui::PointerButton::Secondary), true, 4.0, true));
        assert!(!v152_verdict(v152_click(IN, egui::PointerButton::Primary), true, 4.0, true));
        // No press this frame at all: never dismisses.
        assert!(!v152_verdict(vec![egui::Event::PointerMoved(OUT)], true, 4.0, true));
    }

    #[test]
    fn v152_kit_dismiss_armed_gate() {
        // The opening frame is EXCLUDED: same outside press, unarmed = no dismissal.
        assert!(!v152_verdict(v152_click(OUT, egui::PointerButton::Primary), false, 4.0, true));
        assert!(!v152_verdict(v152_escape(OUT), false, 4.0, true));
    }

    #[test]
    fn v152_kit_dismiss_escape_policy() {
        // escape=true (value/win menus): Escape dismisses when the hover is
        // outside; escape=false (palette shape — its Escape is an
        // unconditional global handler upstream): the key never fires here.
        assert!(v152_verdict(v152_escape(OUT), true, 4.0, true));
        assert!(!v152_verdict(v152_escape(OUT), true, 4.0, false));
        // Quirk frozen from ALL three originals: Escape AND'd with the
        // outside test — hover INSIDE the menu means no dismissal.
        assert!(!v152_verdict(v152_escape(IN), true, 4.0, true));
        // Escape with no known hover falls back to the rect center = inside.
        assert!(!v152_verdict(
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
            true,
            4.0,
            true
        ));
    }

    #[test]
    fn v152_kit_dismiss_pad_knob() {
        // 5 px past the rect edge: the 4 px forgiveness (menus) is crossed
        // and dismisses; the palette's 6 px band still protects it. The pad
        // parameter IS the documented drift — a knob, not an accident.
        let near = egui::pos2(225.0, 130.0); // rect max.x = 220
        assert!(v152_verdict(v152_click(near, egui::PointerButton::Primary), true, 4.0, true));
        assert!(!v152_verdict(v152_click(near, egui::PointerButton::Primary), true, 6.0, true));
    }
}
