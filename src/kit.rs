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

use egui::{Color32, Visuals};

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
