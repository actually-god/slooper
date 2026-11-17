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
            if spec.compact {
                ui.label(egui::RichText::new(l).weak().small());
            } else {
                ui.label(l);
            }
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
            egui::RichText::new(spec.load_text).small().into()
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
                for p in presets {
                    if ui
                        .selectable_label(false, p)
                        .on_hover_text(spec.row_hover)
                        .clicked()
                    {
                        act = Some(PresetBarAction::Load(p.clone()));
                    }
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
