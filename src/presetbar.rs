//! v77 (TS-003): the ONE preset/persistence bar.
//!
//! Five surfaces grew near-identical preset bars independently: hosted
//! plugin params (v34 R2), the sampler key map (v56 R6), track templates
//! (v61), script-synth sliders (v65), and drum kits (v69). Same shape
//! (name box, save, load picker, delete-inside-picker, reset), same
//! sanitizer, same hover/echo discipline — five copies that drifted
//! (v34 predates the sanitizer, v56 skipped delete, v61 built a row-list
//! instead of a picker). This module is the single renderer (the v74
//! `fxgui::param_row` pattern applied to persistence chrome): a surface
//! supplies a [`PresetBarSpec`] (all copy — labels, hints, hover text —
//! stays with the site so echoes and pins are unchanged) plus the cached
//! preset list, and gets back the [`PresetBarAction`] the user took. The
//! ACTION is returned, never executed here — each surface runs its own
//! deferred-action pattern (status writes mid-row desync popups; see the
//! v34 `do_save` shape).
//!
//! [`PresetStore`] parameterizes STORAGE by directory + file type, so the
//! five list/save/load/delete twins share one implementation of the
//! traversal-safe path math. [`sanitize`] is the v56 sanitizer moved to
//! its single home: keep `[A-Za-z0-9-_]`, spaces→'-', collapse runs,
//! trim, cap 48 — `"../evil"` collapses to `"evil"` (no traversal by
//! construction).

use std::path::{Path, PathBuf};

/// v82 (TS-006): the ONE label-column width for the preset/param bar
/// rows. The PARAMETERS window stacks "filter", "user preset" and
/// "program" rows; with natural-width labels the controls start at
/// three different x and the readouts never line up. One shared column
/// makes every row's control start at the same x (goal 4). `0.0` in a
/// spec keeps the natural width (the four older bar sites unchanged).
pub const LABEL_COL: f32 = 72.0;

/// Draw one bar-row label in the shared column (width `LABEL_COL`; `0.0`
/// = natural width, unchanged pre-v82 behavior). `add_sized` — never
/// `Ui::put`, which justifies and collapses narrow rects.
pub fn bar_label(ui: &mut egui::Ui, text: &str, width: f32, compact: bool) {
    let rich = if compact {
        egui::RichText::new(text).weak().small()
    } else {
        egui::RichText::new(text)
    };
    if width > 0.0 {
        ui.add_sized(
            [width, 0.0],
            egui::Label::new(rich).selectable(false),
        );
    } else if compact {
        ui.label(egui::RichText::new(text).weak().small());
    } else {
        ui.label(text);
    }
}

/// One row inside a picker popup (v82): text plus its hover text. The
/// shared bar's Load picker and `picker_row` render these through the
/// SAME private row painter, so popup rows are one chrome everywhere.
pub struct PickerItem {
    pub display: String,
    pub hover: String,
    /// v87 (TS-013) clip identity: when set, a row whose text runs past the
    /// popup's width recovers its full identity on hover (name + on-disk
    /// file in this directory); `None` = no file identity to recover.
    pub clip_dir: Option<PathBuf>,
}

/// Everything a shared picker row says about itself (copy stays at the
/// site, like `PresetBarSpec`).
pub struct PickerRowSpec<'a> {
    /// Leading column label; `None` = the caption already names it
    /// (the v79 "factory presets" row has no leading label).
    pub label: Option<&'a str>,
    /// Label-column width (`LABEL_COL`, or 0.0 for natural).
    pub label_width: f32,
    /// Combo caption when the popup is closed.
    pub caption: &'a str,
    /// Combo width in points.
    pub width: f32,
    /// Hover text on the combo itself.
    pub hover: &'a str,
    /// Line painted when `items` is empty; `None` = say nothing.
    pub empty_text: Option<&'a str>,
}

/// The shared picker row (v82, TS-006): `[label @ column] [combo]`, rows
/// painted by the SAME painter the preset bar's Load picker uses. Drawn
/// INLINE — wrap in `ui.horizontal` when the caller needs trailing
/// controls on the row (the v79 row appends its file button). Returns
/// the row index picked this frame, `None` otherwise; `current` is the
/// check-marked row (`None` = no check state).
pub fn picker_row(
    ui: &mut egui::Ui,
    salt: egui::Id,
    spec: &PickerRowSpec<'_>,
    items: &[PickerItem],
    current: Option<usize>,
) -> Option<usize> {
    if let Some(l) = spec.label {
        bar_label(ui, l, spec.label_width, false);
    }
    let caption = egui::RichText::new(spec.caption);
    let pick = egui::ComboBox::from_id_salt(salt)
        .selected_text(caption)
        .width(spec.width)
        .show_ui(ui, |ui| {
            if items.is_empty() {
                if let Some(e) = spec.empty_text {
                    ui.label(egui::RichText::new(e).weak().small());
                }
            }
            picker_rows(ui, items, current)
        });
    pick.response.on_hover_text(spec.hover);
    // show_ui only runs the contents closure while the popup is open,
    // so inner arrives Option-wrapped.
    pick.inner.flatten()
}

/// The ONE popup-row painter: selectable text + hover, shared by the
/// preset bar's Load picker and every `picker_row`.
fn picker_rows(
    ui: &mut egui::Ui,
    items: &[PickerItem],
    current: Option<usize>,
) -> Option<usize> {
    let mut pick: Option<usize> = None;
    for (i, it) in items.iter().enumerate() {
        // The v87 (TS-013) contract lives INSIDE this painter: a row that
        // runs past the popup's fixed width recovers its full identity on
        // hover (name + on-disk file); a fully-visible row keeps the plain
        // action copy, nothing new grows.
        let resp = ui.selectable_label(current == Some(i), &it.display);
        let tip = match &it.clip_dir {
            Some(dir) if crate::kit::row_clipped(&resp, ui) => format!(
                "{}\n{}\n{}",
                it.display,
                dir.join(format!("{}.json", it.display)).display(),
                it.hover
            ),
            _ => it.hover.clone(),
        };
        if resp.on_hover_text(tip).clicked() {
            pick = Some(i);
        }
    }
    pick
}

/// The v56 sanitizer: collapse anything filesystem-hostile.
/// Keep `[A-Za-z0-9-_]`, map spaces and stray chars to '-', collapse
/// runs, trim, cap at 48 chars. `"../evil"` -> `"evil"`.
pub fn sanitize(name: &str) -> String {
    let mut s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c == ' ' {
                '-'
            } else if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').chars().take(48).collect()
}

/// One preset folder (directory + extension), shared by all five
/// surfaces: the list/save/load/delete file math lives here once, so a
/// traversal can never reappear at a future call site.
pub struct PresetStore {
    pub dir: PathBuf,
    pub ext: &'static str,
}

impl PresetStore {
    /// A `.json` preset folder (the shape every surface uses).
    pub fn json(dir: PathBuf) -> Self {
        Self { dir, ext: "json" }
    }
    /// Preset names (file stems) in this folder, sorted; missing folder
    /// reads as empty.
    pub fn list(&self) -> Vec<String> {
        let mut v: Vec<String> = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == self.ext) {
                        p.file_stem().map(|s| s.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                })
                .collect(),
            Err(_) => return Vec::new(),
        };
        v.sort();
        v
    }
    /// The file a (possibly hostile) raw name maps to: sanitized, one
    /// component inside `dir`, `.{ext}` appended.
    pub fn file(&self, raw: &str) -> PathBuf {
        let name = sanitize(raw);
        self.dir.join(format!("{name}.{}", self.ext))
    }
    /// `mkdir -p` + pretty JSON write under the sanitized name. The
    /// caller has already refused an empty sanitized name (its own
    /// echo); returns the sanitized name it wrote for.
    pub fn write_json(
        &self,
        raw: &str,
        rec: &impl serde::Serialize,
    ) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        let name = sanitize(raw);
        let js = serde_json::to_string_pretty(rec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(self.dir.join(format!("{name}.{}", self.ext)), js)?;
        Ok(name)
    }
    /// Read + deserialize the sanitized file. One error-string pipeline
    /// for io AND parse failures (the shapes all five loads echoed).
    pub fn load_json<T: serde::de::DeserializeOwned>(&self, raw: &str) -> Result<T, String> {
        let p = self.file(raw);
        std::fs::read_to_string(&p)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<T>(&s).map_err(|e| e.to_string()))
    }
    /// Remove the sanitized file.
    pub fn delete(&self, raw: &str) -> std::io::Result<PathBuf> {
        let p = self.file(raw);
        std::fs::remove_file(&p).map(|()| p)
    }
    /// The folder path (pins + echoes display it; creation stays lazy).
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Everything one surface's bar says. Copy stays at the site (pins and
/// status echoes quote these strings); behavior is the component's.
pub struct PresetBarSpec<'a> {
    /// Leading label; `None` = the surface's title already names it.
    pub label: Option<&'a str>,
    /// Name-box hint text.
    pub hint: &'a str,
    /// Name-box width in points.
    pub name_width: f32,
    /// Save button text + hover.
    pub save_text: &'a str,
    pub save_hover: &'a str,
    /// Load picker caption + width + optional picker hover.
    pub load_text: &'a str,
    pub load_width: f32,
    pub load_hover: Option<&'a str>,
    /// Hover shown on every preset row inside the picker.
    pub row_hover: &'a str,
    /// v87 (TS-013): the preset folder. A row whose label is clipped by the
    /// popup's fixed width gets its full name + `<dir>/<name>.json` prefixed
    /// to `row_hover`; untruncated rows keep `row_hover` verbatim.
    pub row_dir: Option<&'a std::path::Path>,
    /// Empty-list line inside the picker; `None` = say nothing.
    pub empty_text: Option<&'a str>,
    /// Delete-inside-picker button; `None` = the surface has no delete
    /// (v56: every preset row is rewritten, never removed).
    pub delete_text: Option<&'a str>,
    pub delete_hover: &'a str,
    /// Reset button (defaults/stock kit); `None` = no reset affordance.
    pub reset_text: Option<&'a str>,
    pub reset_hover: &'a str,
    /// Compact = the small weak toolbar style (the v56 sampler bar).
    pub compact: bool,
    /// v82: shared label-column width (`LABEL_COL`) so this bar aligns
    /// with sibling rows in the same window; `0.0` = natural width.
    pub label_width: f32,
}

/// What the user just did. Returned, never executed here.
#[derive(Clone, PartialEq)]
pub enum PresetBarAction {
    Save,
    Load(String),
    Delete,
    Reset,
}

/// Render one bar row: [label] [name box] [save] [load picker] [reset].
/// `name` is bound directly to the caller's field (the box commits live);
/// `presets` is the caller's cached list (it refreshes it after Save /
/// Delete, exactly like the retired per-site copies did).
pub fn render(
    ui: &mut egui::Ui,
    salt: egui::Id,
    spec: &PresetBarSpec<'_>,
    name: &mut String,
    presets: &[String],
) -> Option<PresetBarAction> {
    let mut act: Option<PresetBarAction> = None;
    ui.horizontal(|ui| {
        if let Some(l) = spec.label {
            bar_label(ui, l, spec.label_width, spec.compact);
        }
        ui.add(
            egui::TextEdit::singleline(name)
                .hint_text(spec.hint)
                .desired_width(spec.name_width)
                // The sanitizer caps at 48 — the box caps at the same
                // number so what you can type is what can land (v56).
                .char_limit(48usize),
        );
        let save = if spec.compact {
            ui.add_sized(
                [44.0, 18.0],
                egui::Button::new(egui::RichText::new(spec.save_text).small()),
            )
        } else {
            ui.button(spec.save_text)
        };
        if save.on_hover_text(spec.save_hover).clicked() {
            act = Some(PresetBarAction::Save);
        }
        let caption = if spec.compact {
            egui::RichText::new(spec.load_text).small()
        } else {
            egui::RichText::new(spec.load_text)
        };
        let pick = egui::ComboBox::from_id_salt(salt)
            .selected_text(caption)
            .width(spec.load_width)
            .show_ui(ui, |ui| {
                if presets.is_empty() {
                    if let Some(e) = spec.empty_text {
                        ui.label(egui::RichText::new(e).weak().small());
                    }
                }
                // v82: rows ride the ONE popup-row painter (shared with
                // picker_row) so every picker in the kit hovers alike;
                // the v87 clip-identity hover rides inside it via
                // PickerItem::clip_dir.
                let items: Vec<PickerItem> = presets
                    .iter()
                    .map(|p| PickerItem {
                        display: p.clone(),
                        hover: spec.row_hover.to_string(),
                        clip_dir: spec.row_dir.map(|d| d.to_path_buf()),
                    })
                    .collect();
                if let Some(i) = picker_rows(ui, &items, None) {
                    act = Some(PresetBarAction::Load(items[i].display.clone()));
                }
                if let Some(dt) = spec.delete_text {
                    if !presets.is_empty() {
                        ui.separator();
                        if ui.button(dt).on_hover_text(spec.delete_hover).clicked() {
                            act = Some(PresetBarAction::Delete);
                        }
                    }
                }
            });
        if let Some(h) = spec.load_hover {
            pick.response.on_hover_text(h);
        }
        if let Some(rt) = spec.reset_text {
            if ui.button(rt).on_hover_text(spec.reset_hover).clicked() {
                act = Some(PresetBarAction::Reset);
            }
        }
    });
    act
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v132 (t_09676c12): `sanitize` has no empty fallback ("!!!" → ""), so
    /// the write primitive must not trust call sites to have refused: a
    /// hidden `.json` is unloadable AND unlistable (list() filters by
    /// extension, and `Path::new(".json").extension()` is None), while the
    /// surface echoes "saved" — the orphan-file class the card reported.
    /// RED pre-fix: write_json happily wrote it.
    #[test]
    fn v132_write_json_refuses_name_collapsing_to_empty() {
        let dir = std::env::temp_dir().join(format!("v132pstore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = PresetStore::json(dir.clone());
        // Root-cause documentation: the sanitizer really does collapse
        // punctuation-only names to empty.
        assert_eq!(sanitize("!!!"), "");
        assert_eq!(sanitize("..."), "");
        assert_eq!(sanitize("-"), "");
        for raw in ["!!!", "...", "@#$%"] {
            let err = store
                .write_json(raw, &vec![(1i64, 0.5f64)])
                .expect_err("a name that sanitizes to empty must not be written");
            assert!(err.to_string().contains("name"), "{err}");
        }
        let leftovers: Vec<std::ffi::OsString> = std::fs::read_dir(&dir)
            .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(
            !dir.join(".json").exists(),
            "hidden orphan preset file written: {leftovers:?}"
        );
        // Names that survive sanitizing still round-trip untouched.
        let nm = store
            .write_json("Warm Room", &vec![(1i64, 0.5f64)])
            .expect("normal name writes");
        assert_eq!(nm, "Warm-Room");
        assert_eq!(store.list(), vec!["Warm-Room".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
