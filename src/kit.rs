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

use egui::{Color32, Response, TextStyle, Ui, Visuals};

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
}
