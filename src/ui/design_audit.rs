#![cfg(test)]
//! v176 (ts-028): the design-language tripwire. Acceptance criterion 5 of
//! TS-028: adding a NEW local `Color32::from_<literal>` site, a NEW
//! numeric geometry const, or (by the v77/v170/v176 roster pins) a forked
//! row painter must FAIL CI until the site is migrated to its token home
//! or explicitly frozen in the baseline below with a stated reason.
//!
//! Scope: `CARGO_MANIFEST_DIR/src/**/*.rs` minus `kit.rs` (the palette
//! home itself), minus whole-file test modules (`#![cfg(test)]` files such
//! as this one and coverage_audit.rs), minus `#[cfg(test)]` / `mod tests`
//! regions (brace-counted, mirroring the v173 comment doctrine). Colors
//! count ONLY when the argument list is purely numeric — computed colors
//! (`from_rgba_unmultiplied(c(r), ..)`) are conversions, not literals.
//!
//! The baseline was generated from the post-v176 tree; regenerating it is
//! ONLY legitimate together with a card comment justifying every added
//! row (the audit exists so drift needs a decision, never silence).
//! Frozen-site reasons live as comments on each row.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
}

fn norm(l: &str) -> String {
    l.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_line(l: &str) -> String {
    if l.trim_start().starts_with("//") {
        return String::new();
    }
    let mut out = String::new();
    let mut instr = false;
    let mut esc = false;
    for (i, c) in l.char_indices() {
        if instr {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                instr = false;
            }
            continue;
        }
        match c {
            '"' => {
                instr = true;
                out.push(c);
            }
            '/' => {
                let rest = &l[i + 1..];
                if rest.starts_with('/') {
                    break;
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Exclusion regions mirroring the audit generator byte-for-bit:
/// `#[cfg(test)]`/`#[cfg(all(test...` attrs and `mod tests` (indent <= 4,
/// optional `pub(crate) `) open a region running through the balanced
/// close of the block their next `{` opens.
fn exclusions(lines: &[&str]) -> Vec<bool> {
    let n = lines.len();
    let mut ex = vec![false; n];
    for i in 0..n {
        let s = lines[i].trim_start();
        let is_modt = {
            let indent = lines[i].len() - s.len();
            let head = s
                .strip_prefix("pub(crate) ")
                .or_else(|| s.strip_prefix("pub "))
                .unwrap_or(s);
            indent <= 4 && (head.starts_with("mod tests") && {
                let r = &head["mod tests".len()..];
                !r.starts_with(|c: char| c.is_alphanumeric() || c == '_')
            })
        };
        let is_attr = s.starts_with("#[cfg(test)]") || s.starts_with("#[cfg(all(test");
        if !is_attr && !is_modt {
            continue;
        }
        let mut k = i;
        let mut depth = 0i32;
        let mut started = false;
        while k < n {
            ex[k] = true;
            depth += lines[k].matches('{').count() as i32 - lines[k].matches('}').count() as i32;
            if lines[k].contains('{') {
                started = true;
            }
            if started && depth <= 0 {
                break;
            }
            k += 1;
        }
    }
    ex
}

fn numeric_args(s: &str, from_at: usize) -> Option<usize> {
    // from_at: index of "Color32::from_"; validate the FIRST call's arg list
    let rest = &s[from_at..];
    let open = rest.find('(')?;
    let after = &rest[open + 1..];
    let close = after.find(')')?;
    let args = &after[..close];
    if !args.is_empty()
        && args.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || c == ',' || c == '.' || c == '-')
    {
        Some(from_at)
    } else {
        None
    }
}

fn is_geom_decl(s: &str) -> bool {
    if s.contains("Color32") {
        return false;
    }
    let mut search_from = 0usize;
    while let Some(rel) = s[search_from..].find("const ") {
        let at = search_from + rel;
        search_from = at + 6;
        let ok0 = at == 0 || {
            let prev = s[..at].chars().last().unwrap();
            !(prev.is_alphanumeric() || prev == '_')
        };
        if !ok0 {
            continue;
        }
        let mut j = at + 6;
        while j < s.len() && s[j..].starts_with(char::is_whitespace) {
            j += 1;
        }
        let ns = j;
        while j < s.len()
            && s[j..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            j += 1;
        }
        let name = &s[ns..j];
        if name.len() < 3 || !name.as_bytes()[0].is_ascii_uppercase() {
            continue;
        }
        let mut k = j;
        while k < s.len() && s[k..].starts_with(char::is_whitespace) {
            k += 1;
        }
        if !s[k..].starts_with(':') {
            continue;
        }
        k += 1;
        while k < s.len() && s[k..].starts_with(char::is_whitespace) {
            k += 1;
        }
        let ty = ["f32", "usize", "u32", "i32", "i64", "f64"]
            .iter()
            .find(|t| s[k..].starts_with(**t));
        let Some(t) = ty else { continue };
        k += t.len();
        while k < s.len() && s[k..].starts_with(char::is_whitespace) {
            k += 1;
        }
        if !s[k..].starts_with('=') {
            continue;
        }
        k += 1;
        while k < s.len() && s[k..].starts_with(char::is_whitespace) {
            k += 1;
        }
        if s[k..].starts_with('-') {
            k += 1;
        }
        if s[k..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    false
}

fn collect(is_color: bool) -> BTreeMap<(String, String), usize> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for path in files {
        let rel = path.strip_prefix(&root).unwrap().display().to_string().replace('\\', "/");
        if rel == "kit.rs" {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let head: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(3)
            .collect();
        if head.iter().any(|l| l.trim() == "#![cfg(test)]") {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        let ex = exclusions(&lines);
        for (i, l) in lines.iter().enumerate() {
            if ex[i] {
                continue;
            }
            let s = strip_line(l);
            let hit = if is_color {
                s.find("Color32::from_").and_then(|at| numeric_args(&s, at)).is_some()
            } else {
                is_geom_decl(&s)
            };
            if hit {
                // Key on the STRIPPED line (mirrors the generator, which
                // normalizes comment-free text): a trailing-comment row
                // must not create a phantom NEW+STALE pair.
                *counts.entry((rel.clone(), norm(&s))).or_insert(0) += 1;
            }
        }
    }
    counts
}

fn diff(base: &[(&str, &str, usize)], is_color: bool) -> (Vec<String>, Vec<String>) {
    let live = collect(is_color);
    let want: BTreeMap<(String, String), usize> = base
        .iter()
        .map(|(f, n, c)| ((f.to_string(), n.to_string()), *c))
        .collect();
    let mut new_sites = Vec::new();
    for (k, c) in &live {
        let b = want.get(k).copied().unwrap_or(0);
        if *c > b {
            new_sites.push(format!("{}:{}x  {}", k.0, c - b, k.1));
        }
    }
    let mut stale = Vec::new();
    for (k, c) in &want {
        let l = live.get(k).copied().unwrap_or(0);
        if *c > l {
            stale.push(format!("{}:{}x expected, {} found: {}", k.0, c, l, k.1));
        }
    }
    (new_sites, stale)
}


const COLOR_BASE: &[(&str, &str, usize)] = &[
    ("ui.rs", "const COL_AUTO_CANVAS: Color32 = Color32::from_rgb(16, 18, 24);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_EDGE: Color32 = Color32::from_gray(60);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_GRID: Color32 = Color32::from_gray(35);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_BAR_LINE: Color32 = Color32::from_gray(52);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_BEAT_LINE: Color32 = Color32::from_gray(32);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_BAR_TXT: Color32 = Color32::from_gray(130);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_LINE: Color32 = Color32::from_rgb(255, 120, 120);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_FILL: Color32 = Color32::from_rgba_premultiplied(255, 120, 120, 36);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_SEL: Color32 = Color32::from_rgb(255, 210, 120);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AUTO_NODE_TXT: Color32 = Color32::from_rgb(255, 220, 150);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_PICK_STRIP: Color32 = Color32::from_rgb(28, 30, 38);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_PICK_STRIP_SEL: Color32 = Color32::from_rgb(40, 42, 54);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_PICK_MASTER: Color32 = Color32::from_rgb(44, 38, 30);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_PICK_PANEL: Color32 = Color32::from_rgb(210, 175, 120);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_AMBER_SEL: Color32 = Color32::from_rgb(255, 210, 120);", 1), // v179 TS-024 automation palette home (values frozen; tripwired by v179_auto_curve_palette_home)
    ("ui.rs", "const COL_PAD_DOT: egui::Color32 = egui::Color32::from_gray(130);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_PAD_DOWN: egui::Color32 = egui::Color32::from_gray(74);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_PAD_STEP: egui::Color32 = egui::Color32::from_gray(56);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SE_CANVAS: egui::Color32 = egui::Color32::from_rgb(14, 16, 22);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SE_CANVAS_EDGE: egui::Color32 = egui::Color32::from_gray(55);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SE_GRID_LINE: egui::Color32 = egui::Color32::from_gray(30);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SE_READOUT: egui::Color32 = egui::Color32::from_gray(150);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SE_SRC_TICK: egui::Color32 = egui::Color32::from_gray(120);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SE_SRC_TXT: egui::Color32 = egui::Color32::from_gray(130);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SE_STRIP_BG: egui::Color32 = egui::Color32::from_rgb(22, 25, 32);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SE_TIME_TXT: egui::Color32 = egui::Color32::from_gray(105);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_KEY_BLACK: egui::Color32 = egui::Color32::from_gray(28);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_KEY_EDGE: egui::Color32 = egui::Color32::from_gray(90);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_KEY_LBL: egui::Color32 = egui::Color32::from_gray(60);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_KEY_WHITE: egui::Color32 = egui::Color32::from_gray(210);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_LANE: egui::Color32 = egui::Color32::from_rgb(24, 26, 32);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_LANE_XF: egui::Color32 = egui::Color32::from_rgb(28, 30, 38);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_MISSING: egui::Color32 = egui::Color32::from_gray(90);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_ROOT: egui::Color32 = egui::Color32::from_rgb(255, 190, 60);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_SEL: egui::Color32 = egui::Color32::from_rgb(46, 52, 72);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_SEL_TXT: egui::Color32 = egui::Color32::from_gray(150);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", "const COL_SMAP_ZONE: egui::Color32 = egui::Color32::from_gray(70);", 1), // v178 TS-023 instrument palette home (values frozen by v178_stock_tints_single_home)
    ("ui.rs", ".color(Color32::from_rgb(120, 220, 160))", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", ".color(Color32::from_rgb(240, 110, 90))", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", ".fill(Color32::from_rgb(35, 40, 50))", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", ".fill(Color32::from_rgba_premultiplied(255, 120, 120, 24))", 1), // destructive-red alpha wash; DESTROY-with-alpha adoption is the surface owner's call (audit report)
    ("ui.rs", "2 => Color32::from_rgb(255, 120, 120),", 1), // rack kind data palette single home (v176)
    ("ui.rs", "3 => Color32::from_rgb(190, 150, 255),", 1), // rack kind data palette single home (v176)
    ("ui.rs", "Color32::from_gray(120)", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(120),", 2), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(140)", 1), // single-site disabled-row tint; frozen not tokenized (one site), token candidate noted in audit report
    ("ui.rs", "Color32::from_gray(140),", 4), // drift filed to v174/ts-022 filed (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(150),", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(160)", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(160),", 1), // drift filed to v174/ts-022 filed (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(175),", 1), // drift filed to v174/ts-022 filed (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(195),", 2), // drift filed to v174/ts-022 filed (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(210),", 1), // drift filed to ts-026 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(220),", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(225)", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(225),", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(28)", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(45)", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(66)", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(75)", 2), // drift filed to ts-023 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_gray(95)", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(112, 122, 238),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgb(136, 202, 96),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgb(152, 162, 178),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgb(170, 70, 74)", 1), // clip kind data color (clip_paint_color)
    ("ui.rs", "Color32::from_rgb(176, 112, 234),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgb(200, 200, 205)", 1), // drift filed to ts-026 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(210, 225, 250)", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(225, 120, 120)", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(226, 226, 232),", 1), // drift filed to v174/ts-022 filed (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(226, 96, 122),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgb(228, 206, 84),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgb(230, 104, 198),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgb(230, 230, 170),", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(240, 156, 64),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgb(255, 165, 160)", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(255, 200, 110)", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(255, 210, 210),", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgb(255, 225, 130)", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "const COL_DOCK_BTN: egui::Color32 = egui::Color32::from_rgb(48, 52, 60);", 1), // v182 ts-026: dock-chrome home block (chrome, not data — ts-028 audit)
    ("ui.rs", "Color32::from_rgb(64, 196, 176),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "const COL_DOCK_BTN_HOT: egui::Color32 = egui::Color32::from_rgb(70, 96, 128);", 1), // v182 ts-026: dock-chrome home block
    ("ui.rs", "Color32::from_rgb(86, 178, 234),", 1), // track-hue data palette (kit.rs module doc)
    ("ui.rs", "Color32::from_rgba_premultiplied(10, 10, 14, 160),", 2), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgba_premultiplied(90, 150, 230, 55),", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "Color32::from_rgba_unmultiplied(196, 64, 64, 80),", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "_ => Color32::from_rgb(210, 175, 120),", 1), // rack kind data palette single home (v176)
    ("ui.rs", "btn = btn.fill(Color32::from_rgb(60, 90, 120));", 2), // drift filed to v174/ts-022 filed (t_2076e182 audit)
    ("ui.rs", "const COL_AUTO_CURVE: Color32 = Color32::from_rgb(255, 130, 130);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_AUTO_CURVE_SEL: Color32 = Color32::from_rgb(255, 190, 190);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_BAR_LINE: Color32 = Color32::from_gray(135);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_BEAT_LINE: Color32 = Color32::from_gray(78);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_CLIP: Color32 = Color32::from_rgb(72, 118, 196);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_CLIP_NOTES: Color32 = Color32::from_rgb(120, 205, 255);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_CLIP_NOTES_SEL: Color32 = Color32::from_rgb(180, 235, 255);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_CLIP_SEL: Color32 = Color32::from_rgb(128, 200, 255);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_DEL_HINT: Color32 = Color32::from_rgb(235, 120, 120);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_GHOST: Color32 = Color32::from_rgb(120, 160, 120);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_ORIG_LEN: Color32 = Color32::from_rgb(250, 200, 90);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_PLAYHEAD: Color32 = Color32::from_rgb(255, 95, 86);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_RESIZE_CAP: Color32 = Color32::from_rgb(250, 200, 50);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_NOTE: Color32 = Color32::from_rgb(80, 150, 245);", 1), // roll/bind data-palette single home (v177 ts-022; values pinned by v177_roll_note_tints_single_home)
    ("ui.rs", "const COL_NOTE_SEL: Color32 = Color32::from_rgb(120, 185, 255);", 1), // roll/bind data-palette single home (v177 ts-022)
    ("ui.rs", "const COL_NOTE_CURVE: Color32 = Color32::from_rgb(90, 220, 170);", 1), // roll/bind data-palette single home (v177 ts-022)
    ("ui.rs", "const COL_VEL_BAR: Color32 = Color32::from_rgb(90, 160, 230);", 1), // roll/bind data-palette single home (v177 ts-022)
    ("ui.rs", "const COL_GLIDE_MID: Color32 = Color32::from_rgb(240, 150, 80);", 1), // roll/bind data-palette single home (v177 ts-022)
    ("ui.rs", "const COL_BIND_TRACK: Color32 = Color32::from_rgb(24, 26, 32);", 1), // bind-bar single renderer (v177 ts-022; pinned by v177_binds_bar_single_renderer)
    ("ui.rs", "const COL_BIND_FILL: Color32 = Color32::from_rgb(96, 104, 132);", 1), // bind-bar single renderer (v177 ts-022)
    ("ui.rs", "const COL_BIND_NEEDLE: Color32 = Color32::from_gray(215);", 1), // bind-bar single renderer (v177 ts-022)
    ("ui.rs", "const COL_RESIZE_CAP_HELD: Color32 = Color32::from_rgb(255, 235, 120);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_RULER_BG: Color32 = Color32::from_rgb(34, 34, 40);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_SEP_BAR: Color32 = Color32::from_rgb(56, 56, 64);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_SEP_BAR_HOT: Color32 = Color32::from_rgb(96, 96, 116);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_SE_BEND: egui::Color32 = egui::Color32::from_rgb(255, 200, 90);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_SE_FI: egui::Color32 = egui::Color32::from_rgb(140, 230, 160);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_SE_FO: egui::Color32 = egui::Color32::from_rgb(255, 150, 120);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_SE_PITCH: egui::Color32 = egui::Color32::from_rgb(120, 255, 170);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_SE_WAVE: egui::Color32 = egui::Color32::from_rgb(120, 205, 255);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "const COL_SKIP_FG: Color32 = Color32::from_rgb(205, 210, 226);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_SKIP_FG_HOVER: Color32 = Color32::from_rgb(240, 244, 255);", 1), // arrangement data-palette single home (v175 ts-021; values pinned by v175_arrangement_tints_single_home)
    ("ui.rs", "const COL_SNAP_LINE: Color32 = Color32::from_gray(44);", 1), // named palette home const (kit.rs module doc)
    ("ui.rs", "egui::Color32::from_rgb(110, 190, 120),", 1), // drift filed to ts-023 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Color32::from_rgb(160, 110, 200),", 1), // drift filed to ts-023 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Color32::from_rgb(200, 160, 90),", 1), // drift filed to ts-023 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Color32::from_rgb(220, 120, 90),", 1), // drift filed to ts-023 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Color32::from_rgb(90, 140, 220),", 1), // drift filed to ts-023 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Color32::from_rgb(90, 180, 190),", 1), // drift filed to ts-023 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(1.0, Color32::from_gray(70)),", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(1.0, Color32::from_gray(90)),", 2), // drift filed to ts-026 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(1.0, Color32::from_rgb(255, 150, 150)),", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(1.0f32, Color32::from_gray(10)),", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(1.2, Color32::from_gray(220)),", 1), // drift filed to ts-026 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(1.4f32, Color32::from_rgb(90, 220, 120)),", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(1.6, Color32::from_rgb(120, 190, 255)),", 1), // drift filed to ts-023 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(1.6f32, Color32::from_rgb(230, 180, 90)),", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "egui::Stroke::new(2.0, Color32::from_rgb(255, 214, 120)),", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "let wedge = Color32::from_black_alpha(120);", 1), // drift filed to ts-021 follow-up (t_2076e182 audit)
    ("ui.rs", "p.rect_filled(sq, 2.0, Color32::from_rgb(230, 70, 70));", 1), // drift filed to v174/ts-022 filed (t_2076e182 audit)
    ("ui.rs", "const COL_DOCK_TAB_BG: egui::Color32 = egui::Color32::from_rgb(24, 26, 30);", 1), // v182 ts-026: dock-chrome home block
    ("ui.rs", "painter.rect_filled(bend_lane, 0.0, Color32::from_gray(16));", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "painter.rect_filled(hbar, 0.0, Color32::from_black_alpha(90));", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "painter.rect_filled(rect, 2.0, Color32::from_rgb(22, 22, 26));", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "painter.rect_filled(vbar, 0.0, Color32::from_black_alpha(90));", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
    ("ui.rs", "painter.rect_filled(vel_lane, 0.0, Color32::from_gray(18));", 1), // drift filed to ts-022 follow-up (t_2076e182 audit)
];

const GEOM_BASE: &[(&str, &str, usize)] = &[
    ("ui.rs", "const AUTO_WIN_W: f32 = 685.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const AUTO_WIN_H: f32 = 460.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const AUTO_CANVAS_W: f32 = 596.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const AUTO_CANVAS_H: f32 = 240.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_WIN_W: f32 = 760.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_SCROLL_H: f32 = 440.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_STRIP_W: f32 = 56.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_MASTER_W: f32 = 64.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_CELL_W: f32 = 54.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_FADER_H: f32 = 72.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_BAR_H: f32 = 28.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_LAMP_H: f32 = 18.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_PARAM_W: f32 = 122.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const PICK_PARAM_H: f32 = 30.0;", 1), // v179 TS-024 automation geometry home (values frozen; tripwired by v179_auto_geometry_home)
    ("ui.rs", "const INSTR_KILL_W: f32 = 16.0;", 1), // v178 TS-023 instrument geometry home (values frozen by v178_instr_grid_single_home)
    ("ui.rs", "const INSTR_NAME_W: f32 = 52.0;", 1), // v178 TS-023 instrument geometry home (values frozen by v178_instr_grid_single_home)
    ("ui.rs", "const INSTR_PLAY_W: f32 = 18.0;", 1), // v178 TS-023 instrument geometry home (values frozen by v178_instr_grid_single_home)
    ("ui.rs", "const INSTR_ROW_H: f32 = 18.0;", 1), // v178 TS-023 instrument geometry home (values frozen by v178_instr_grid_single_home)
    ("ui.rs", "const INSTR_STEP_W: f32 = 14.0;", 1), // v178 TS-023 instrument geometry home (values frozen by v178_instr_grid_single_home)
    ("ui.rs", "const SMAP_HDR_W: f32 = 190.0;", 1), // v178 TS-023 instrument geometry home (values frozen by v178_instr_grid_single_home)
    ("ui.rs", "const SMAP_KEY_H: f32 = 22.0;", 1), // v178 TS-023 instrument geometry home (values frozen by v178_instr_grid_single_home)
    ("ui.rs", "const SMAP_LANE_H: f32 = 24.0;", 1), // v178 TS-023 instrument geometry home (values frozen by v178_instr_grid_single_home)
    ("ui.rs", "const SYNTH_WIN_DEF_W: f32 = 490.0;", 1), // v180 card t_bd658af5 synth-window default size home (values frozen by v180_synth_win_default_home)
    ("ui.rs", "const SYNTH_WIN_DEF_H: f32 = 430.0;", 1), // v180 card t_bd658af5 synth-window default size home (values frozen by v180_synth_win_default_home)
    ("ui.rs", "const SAMPLER_WIN_DEF_W: f32 = 1150.0;", 1), // v180 card t_bd658af5 synth-window default size home (values frozen by v180_synth_win_default_home)
    ("ui.rs", "const SAMPLER_WIN_DEF_H: f32 = 520.0;", 1), // v180 card t_bd658af5 synth-window default size home (values frozen by v180_synth_win_default_home)
    ("ui.rs", "const BEND_LANE_H: f32 = 46.0;", 1), // lane geometry home (filed to ts-022 follow-up)
    ("ui.rs", "const BOT_DB: f32 = -60.0;", 1), // lane geometry home (filed to v174/ts-022 filed)
    ("midiin.rs", "const CAP: usize = 1024;", 1), // non-UI file midiin.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const CLR_INVALID: u32 = 0xFFFF_FFFF;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const CONTAINER_BG: u32 = 0x00B4_162A;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vsthost.rs", "const CREATE_NO_WINDOW: u32 = 0x0800_0000;", 1), // non-UI file vsthost.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const DEAD_TICKS_PATIENCE: u32 = 8;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const DEAD_TICKS_PATIENCE: u32 = 8;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("claphost.rs", "const DISCOVERY_MAX_FILES: usize = 512;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "const DISCOVERY_MAX_PRESETS: usize = 1024;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "const DISCOVERY_MAX_PROVIDERS: u32 = 8;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("ui.rs", "const DRUM_MAX_PADS: usize = 16;", 1), // geometry single-home declaration
    ("vstedit/win32.rs", "const EFF_EDIT_CAN_SIZE: i32 = 79;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EFF_EDIT_CAN_SIZE: i32 = 79;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const EFF_EDIT_CLOSE: i32 = 15;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EFF_EDIT_CLOSE: i32 = 15;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const EFF_EDIT_GET_RECT: i32 = 13;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EFF_EDIT_GET_RECT: i32 = 13;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const EFF_EDIT_IDLE: i32 = 19;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EFF_EDIT_IDLE: i32 = 19;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const EFF_EDIT_OPEN: i32 = 14;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EFF_EDIT_OPEN: i32 = 14;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EFF_EDIT_SIZE: i32 = 78;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const EFF_EDIT_SIZE: i32 = 80;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_CLIENT_MESSAGE: i32 = 33;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_CONFIGURE_NOTIFY: i32 = 22;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_DESTROY_NOTIFY: i32 = 17;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_EXPOSE: i32 = 12;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_FOCUS_IN: i32 = 9;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_FOCUS_OUT: i32 = 10;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_KEY_PRESS: i32 = 2;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_KEY_RELEASE: i32 = 3;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const EVENT_REPARENT_NOTIFY: i32 = 21;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("claphost.rs", "const EV_POOL_CAP: usize = 2 * 1024;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("audio/mod.rs", "const GAIN_MAX_LIN: f32 = 1.584_893_2;", 1), // non-UI file audio/mod.rs: ABI/plumbing const
    ("ui.rs", "const GRAB_EDGE_LANE: f32 = 7.0;", 1), // grab-edge tolerance single home (v175 ts-021; lane-vs-note divergence deliberate, documented at the declaration)
    ("ui.rs", "const GRAB_EDGE_NOTE: f32 = 6.0;", 1), // grab-edge tolerance single home (v175 ts-021; aliased as EDGE_PX at roll_inner)
    ("vstedit/win32.rs", "const IDM_CLOSE: usize = 0x5102;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/win32.rs", "const IDM_REDOCK: usize = 0x5101;", 1), // non-UI file vstedit/win32.rs: ABI/plumbing const
    ("vstedit/x11.rs", "const IS_VIEWABLE: i32 = 2;", 1), // non-UI file vstedit/x11.rs: ABI/plumbing const
    ("ui.rs", "const KEYS_W: f32 = 58.0;", 1), // lane geometry home (filed to ts-022 follow-up)
    ("ui.rs", "const LANE_H: f32 = 48.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("ui.rs", "const LANE_MIN_H: f32 = 36.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("meter.rs", "const LUFS_OFFSET: f64 = -0.691;", 1), // non-UI file meter.rs: ABI/plumbing const
    ("audio/mod.rs", "const MAX_AUD_VOICES: usize = 12;", 1), // non-UI file audio/mod.rs: ABI/plumbing const
    ("assets.rs", "const MAX_DEPTH: usize = 4;", 1), // non-UI file assets.rs: ABI/plumbing const
    ("audio/mod.rs", "const MIDI_QUIET_BLOCKS: u32 = 6;", 1), // non-UI file audio/mod.rs: ABI/plumbing const
    ("audio/mod.rs", "const MIDI_SILENCE_FLOOR: f32 = 1e-4;", 1), // non-UI file audio/mod.rs: ABI/plumbing const
    ("ui/mcp_windows.rs", "const MIN_H: f32 = 80.0;", 1), // non-UI file ui/mcp_windows.rs: ABI/plumbing const
    ("ui.rs", "const MIN_SHARE: f32 = 56.0;", 1), // v182 ts-026: hoisted to one home (was declared twice)
    ("ui/mcp_windows.rs", "const MIN_VIS: f32 = 60.0;", 1), // non-UI file ui/mcp_windows.rs: ABI/plumbing const
    ("ui/mcp_windows.rs", "const MIN_W: f32 = 120.0;", 1), // non-UI file ui/mcp_windows.rs: ABI/plumbing const
    ("ui.rs", "const MIX_COL_GAP: f32 = 8.0;", 1), // geometry single-home declaration
    ("ui.rs", "const MIX_COL_W: f32 = 130.0;", 1), // geometry single-home declaration
    ("ui.rs", "const STATUS_FADE_FLOOR: f32 = 0.15;", 1), // v182 ts-026: echo-strip lifetime home (v182_echo_lifetime)
    ("ui.rs", "const STATUS_FADE_START_S: f64 = 6.0;", 1), // v182 ts-026: echo-strip lifetime home
    ("ui.rs", "const STATUS_TTL_S: f64 = 12.0;", 1), // v182 ts-026: echo-strip lifetime home
    ("ui.rs", "const TOPBAR_COMPACT_AVAIL: f32 = 700.0;", 1), // v182 ts-026: topbar fold-tier single home (v182_topbar_narrow_floor)
    ("ui.rs", "const TOPBAR_FOLD_AVAIL: f32 = 1138.0;", 1), // v182 ts-026: topbar fold-tier single home
    ("ui.rs", "const TOPBAR_STATUS_MAX_W: f32 = 300.0;", 1), // v182 ts-026: status-strip width home
    ("ui.rs", "const TOPBAR_STATUS_MIN_W: f32 = 96.0;", 1), // v182 ts-026: status-strip width home
    ("ui.rs", "const TOPBAR_TAIL_COMPACT: f32 = 346.0;", 1), // v182 ts-026: topbar fold-tier single home
    ("ui.rs", "const TOPBAR_TAIL_FOLD: f32 = 600.0;", 1), // v182 ts-026: topbar fold-tier single home
    ("ui.rs", "const TOPBAR_TAIL_FULL: f32 = 850.0;", 1), // v182 ts-026: topbar fold-tier single home
    ("ui.rs", "const TOPBAR_TITLE_H: f32 = 25.75;", 1), // v182 ts-026: topbar title block home (probe-measured)
    ("ui.rs", "const TOPBAR_TITLE_W: f32 = 140.0;", 1), // v182 ts-026: topbar title block home
    ("claphost.rs", "const OUT_RING: usize = 256;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("ui.rs", "const PAN_WHEEL_LINE: f32 = 80.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("vsthost.rs", "const RING_CAP: usize = 1 << 16;", 1), // non-UI file vsthost.rs: ABI/plumbing const
    ("claphost.rs", "const RING_CAP: usize = 64 * 1024;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("ui.rs", "const RULER_HANDLE_HIT: f32 = 8.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("lib.rs", "const SIGTRAP: i32 = 5;", 1), // non-UI file lib.rs: ABI/plumbing const
    ("ui.rs", "const SMAP_BW: f32 = 14.0;", 1), // geometry single-home declaration
    ("ui.rs", "const SMAP_NW: usize = 128;", 1), // geometry single-home declaration
    ("ui.rs", "const SMAP_ROLL_W: f32 = 75.0 * SMAP_WW;", 1), // lane geometry home (filed to ts-023 follow-up)
    ("ui.rs", "const SMAP_WW: f32 = 24.0;", 1), // geometry single-home declaration
    ("ui.rs", "const SNAP_BOTTOM: f32 = 110.0;", 1), // geometry single-home declaration
    ("ui.rs", "const SNAP_EDGE: f32 = 150.0;", 1), // geometry single-home declaration
    ("ui.rs", "const TOPBAR_H: f32 = 44.0;", 1), // geometry single-home declaration
    ("ui.rs", "const TOP_DB: f32 = 6.0;", 1), // lane geometry home (filed to v174/ts-022 filed)
    ("ui.rs", "const TRACK_MIN_H: f32 = 96.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("ui.rs", "const TRACK_SEP: f32 = 3.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("ui.rs", "const TRACK_SEP_HIT: f32 = 7.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("ui.rs", "const VEL_LANE_H: f32 = 30.0;", 1), // lane geometry home (filed to ts-022 follow-up)
    ("ui.rs", "const VEL_PAD: f32 = 2.0;", 1), // lane geometry home (filed to ts-022 follow-up)
    ("ui.rs", "const WIN_BTN_STRIP: f32 = 64.0;", 1), // geometry single-home declaration
    ("ui.rs", "const WIN_DOCK_BTN_W: f32 = 22.0;", 1), // geometry single-home declaration
    ("ui.rs", "const WIN_HEAD_H: f32 = 27.0;", 1), // geometry single-home declaration
    ("ui.rs", "const ZOOM_DEFAULT: f32 = 160.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("ui.rs", "const ZOOM_MAX: f32 = 2400.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("ui.rs", "const ZOOM_MIN: f32 = 3.0;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("ui.rs", "const ZOOM_WHEEL_LINE: f32 = 0.12;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("ui.rs", "const ZOOM_WHEEL_PX: f32 = 0.01;", 1), // lane geometry home (filed to ts-021 follow-up)
    ("midipat.rs", "pub const BEND_FULL_ST: f32 = 12.0;", 1), // non-UI file midipat.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_LOG_DEBUG: u32 = 0;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_LOG_ERROR: u32 = 3;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_LOG_FATAL: u32 = 4;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_LOG_HOST_MISBEHAVING: u32 = 5;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_LOG_INFO: u32 = 1;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_LOG_PLUGIN_MISBEHAVING: u32 = 6;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_LOG_WARNING: u32 = 2;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_NAME_SIZE: usize = 256;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_PARAM_IS_ENUM: u32 = 1 << 4;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_PARAM_IS_HIDDEN: u32 = 1 << 2;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_PARAM_IS_READONLY: u32 = 1 << 3;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_PATH_SIZE: usize = 1024;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT: u32 = 1 << 0;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("claphost.rs", "pub const CLAP_PROCESS_ERROR: i32 = 0;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("crash.rs", "pub const CRASH_KEEP: usize = 10;", 1), // non-UI file crash.rs: ABI/plumbing const
    ("meter.rs", "pub const DISPLAY_POINTS: usize = 1024;", 1), // non-UI file meter.rs: ABI/plumbing const
    ("tune.rs", "pub const EDO_DEFAULT_N: u32 = 12;", 1), // non-UI file tune.rs: ABI/plumbing const
    ("tune.rs", "pub const EDO_MAX: u32 = 128;", 1), // non-UI file tune.rs: ABI/plumbing const
    ("tune.rs", "pub const EDO_MIN: u32 = 1;", 1), // non-UI file tune.rs: ABI/plumbing const
    ("claphost.rs", "pub const EVENT_SLOT: usize = 64;", 1), // non-UI file claphost.rs: ABI/plumbing const
    ("audio/rt_recovery.rs", "pub const FAULT_DEVICE: u32 = 2;", 1), // non-UI file audio/rt_recovery.rs: ABI/plumbing const
    ("audio/rt_recovery.rs", "pub const FAULT_NONE: u32 = 0;", 1), // non-UI file audio/rt_recovery.rs: ABI/plumbing const
    ("audio/rt_recovery.rs", "pub const FAULT_STREAM: u32 = 1;", 1), // non-UI file audio/rt_recovery.rs: ABI/plumbing const
    ("meter.rs", "pub const FFT_WINDOW: usize = 16384;", 1), // non-UI file meter.rs: ABI/plumbing const
    ("audio/mod.rs", "pub const GAIN_MAX_DB: f32 = 12.0;", 1), // non-UI file audio/mod.rs: ABI/plumbing const
    ("audio/rt_recovery.rs", "pub const JACK_REBUILD_BUDGET: u32 = 3;", 1), // non-UI file audio/rt_recovery.rs: ABI/plumbing const
    ("presetbar.rs", "pub const LABEL_COL: f32 = 72.0;", 1), // non-UI file presetbar.rs: ABI/plumbing const
    ("audio/mod.rs", "pub const LOOP_CAP: u32 = 2_880_000;", 1), // non-UI file audio/mod.rs: ABI/plumbing const
    ("synth.rs", "pub const MAP_EXISTS: u32 = 0x8000_0000;", 1), // non-UI file synth.rs: ABI/plumbing const
    ("mcp/http.rs", "pub const MAX_BODY: usize = 1_000_000;", 1), // non-UI file mcp/http.rs: ABI/plumbing const
    ("fx.rs", "pub const MAX_FX_PER_TRACK: usize = 8;", 1), // non-UI file fx.rs: ABI/plumbing const
    ("audio/mod.rs", "pub const MAX_LOOPS: usize = 128;", 1), // non-UI file audio/mod.rs: ABI/plumbing const
    ("synth.rs", "pub const MAX_MAP_SLOTS: usize = 64;", 1), // non-UI file synth.rs: ABI/plumbing const
    ("audio/mod.rs", "pub const MAX_SLOTS: usize = 16;", 1), // non-UI file audio/mod.rs: ABI/plumbing const
    ("midipat.rs", "pub const NB_MAX_CENTS: f32 = 2400.0;", 1), // non-UI file midipat.rs: ABI/plumbing const
    ("midipat.rs", "pub const NB_MAX_PTS: usize = 16;", 1), // non-UI file midipat.rs: ABI/plumbing const
    ("vsthost.rs", "pub const PRESET_KIND_FILE: u32 = 0;", 1), // non-UI file vsthost.rs: ABI/plumbing const
    ("vsthost.rs", "pub const PRESET_KIND_PLUGIN: u32 = 1;", 1), // non-UI file vsthost.rs: ABI/plumbing const
    ("audio/rt_recovery.rs", "pub const RETRY_BUDGET: u32 = 2;", 1), // non-UI file audio/rt_recovery.rs: ABI/plumbing const
    ("meter.rs", "pub const SPEC_F_MAX: f32 = 20_000.0;", 1), // non-UI file meter.rs: ABI/plumbing const
    ("meter.rs", "pub const SPEC_F_MIN: f32 = 30.0;", 1), // non-UI file meter.rs: ABI/plumbing const
    ("midiin.rs", "pub const STALL_SKIP: u32 = 256;", 1), // non-UI file midiin.rs: ABI/plumbing const
    ("quantize.rs", "pub const TRIM_FLOOR_DB: f32 = -60.0;", 1), // non-UI file quantize.rs: ABI/plumbing const
    ("project.rs", "pub const VERSION: u32 = 3;", 1), // non-UI file project.rs: ABI/plumbing const
    ("vsthost.rs", "pub const VST_BLOCK: usize = 512;", 1), // non-UI file vsthost.rs: ABI/plumbing const
    ("vsthost.rs", "pub const VST_MAX_PARAMS: usize = 8192;", 1), // non-UI file vsthost.rs: ABI/plumbing const
    ("vsthost.rs", "pub(crate) const MIDI_QUEUE_CAP: usize = 8192;", 1), // non-UI file vsthost.rs: ABI/plumbing const
    ("vsthost.rs", "pub(crate) const VST3_K_IS_PROGRAM_CHANGE: u32 = 1 << 15;", 1), // non-UI file vsthost.rs: ABI/plumbing const
];


#[test]
fn v176_color_literal_tripwire() {
    // TS-028 criterion 5: a NEW local Color32 literal outside the token
    // homes fails here; frozen sites live in COLOR_BASE with reasons.
    // (v177 ts-022 lowered the floor 150→120: the ts-022/ts-026 filed rows
    // it drains are legitimate — the guard guards roster DESTRUCTION, not
    // a count floor; current roster 148.)
    assert!(COLOR_BASE.len() > 120, "baseline roster was emptied");
    let (new_sites, stale) = diff(COLOR_BASE, true);
    assert!(
        new_sites.is_empty() && stale.is_empty(),
        "design-language drift (v176/ts-028): migrate to kit:: tokens / rack_kind_color / the owning palette home, or add the exact line to COLOR_BASE in src/ui/design_audit.rs WITH a justification comment (card comment on t_2076e182 required).\\nNEW SITES:\\n{}\\nSTALE BASELINE ROWS (delete them):\\n{}",
        new_sites.join("\n"),
        stale.join("\n")
    );
}

#[test]
fn v176_geometry_const_tripwire() {
    // TS-028 criterion 5 (geometry half): a NEW numeric layout const
    // outside the single homes fails here.
    assert!(GEOM_BASE.len() > 100, "baseline roster was emptied");
    let (new_sites, stale) = diff(GEOM_BASE, false);
    assert!(
        new_sites.is_empty() && stale.is_empty(),
        "geometry drift (v176/ts-028): a new local const — reuse a single home (LABEL_COL / lane_note_x / smap_note_rect / NOTE_* / strip/track families) or add the exact line to GEOM_BASE with a justification comment (card comment on t_2076e182 required).\\nNEW SITES:\\n{}\\nSTALE BASELINE ROWS (delete them):\\n{}",
        new_sites.join("\n"),
        stale.join("\n")
    );
}