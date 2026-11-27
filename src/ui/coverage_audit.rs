//! v173 (TS-027): whole-program hover + status-echo invariant harness.
//!
//! The v86 completeness audit was scoped to PLUGIN fn regions via a
//! region list, and v76/v80 own narrow per-surface scans; nothing asserted
//! program-wide coverage. That is exactly how v86's false-green classes
//! happened (ComboBox statements matching neither `.button(` nor `_button(`,
//! popup-item hovers leaking onto the owning statement, a commented-out
//! echo surviving a needle scan). This module scans EVERY production fn of
//! every render file — the file is the roster; new surfaces cannot escape
//! coverage silently.
//!
//! Scans (pure-text, deterministic, quick-tier):
//!   v173_hover_program_scan — every affordance statement owns its hover
//!   v173_echo_program_scan  — every state-change seam echoes (command-fn
//!                             seams + inline `.clicked()` block seams),
//!                             comment-stripped so a commented-out echo REDs
//!   v173_surface_roster_scan — the ts-019 map's render fns exist (tripwire)
//!
//! Accepted gaps live in EXEMPTIONS below; every row carries a reason.
//! `skip:<class>` rows encode the v72–v86 skip doctrines (dialog/window
//! opens, view-state toggles, drag same-view repaint, helper-owned hovers,
//! self-explaining popup labels, plumbing fns). `gap:<card>` rows are OPEN
//! work seeded to the ts-018 surface-pass cards — the owning lane fixes the
//! surface AND deletes the row in the same PR. An exemption matching
//! nothing is STALE and fails the pin: the roster stays honest as surfaces
//! land, and a NEW un-exempted gap (or a grown count) fails the pin too.
//!
//! False-green defenses (each sabotage-proved at landing):
//!   * comment lines and trailing `// ...` are string-aware-stripped before
//!     any needle — a commented-out hover or echo cannot keep a gap green;
//!   * argument-closure bodies `(|ui| { ... })` are cut from the statement
//!     window before the hover test — popup-item hovers do not count for a
//!     ComboBox/menu owner (v86 R2 `.response` rule, generalized to every
//!     closure argument incl. single-expression ones);
//!   * statement windows are depth-aware (string-aware paren/brace balance,
//!     negative carry for triggers inside calls opened earlier) and end at
//!     the expression terminator or the clicked() check — a neighbour's
//!     hover past the click cannot leak in (v80 join rule, kept);
//!   * builder handles (`let btn = egui::Button::new(..);` … later
//!     `btn.on_hover_text(..)`) ride the chain: the window extends while the
//!     var is referenced and stops at foreign hovers/affordances;
//!   * file-local helpers whose body attaches `on_hover` own the contract;
//!     their call sites are exempt, the body region itself is scanned
//!     (arrow_button lineage, generalized);
//!   * `menu_button(` argument-closure contents are exempt by the
//!     v76/v80/v86 doctrine (submenu items are the explanation).
//!
//! Known scope limits (deliberate, recorded on ts-027): Sliders/DragValue
//! are not hover-bearing affordances in the v54..v86 standard; RadioButton
//! is outside the v86 family; echo seams scope to ui.rs (kit/presetbar are
//! render helpers — hover-scanned, not command seams); `raw string` content
//! is not parsed by the comment stripper (none exists in scanned regions —
//! roster pin asserts no block comments; raw-string hovers would only
//! *widen* a window, never fake one).

#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};

// ------------------------------------------------------------------
// string-aware text helpers
// ------------------------------------------------------------------

fn is_id(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Collapse whitespace runs, trim — the exemption-key normalizer.
fn norm(l: &str) -> String {
    l.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip `//` comments string-aware (`\"` escapes honored). A whole-comment
/// line becomes "". No block comments / raw strings in scanned regions
/// (roster pin asserts).
fn strip_line(l: &str) -> String {
    if l.trim_start().starts_with("//") {
        return String::new();
    }
    let mut out = String::with_capacity(l.len());
    let mut instr = false;
    let mut inesc = false;
    let it = l.char_indices();
    for (i, c) in it {
        if instr {
            out.push(c);
            if inesc {
                inesc = false;
            } else if c == '\\' {
                inesc = true;
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
                if l[i + 1..].starts_with('/') {
                    break;
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    let t = out.trim_end();
    if t.is_empty() {
        return String::new();
    }
    t.to_string()
}

/// Byte index just past the matching close of the bracket at `open`.
fn close_of(text: &str, open: usize) -> Option<usize> {
    let ob = text.as_bytes()[open] as char;
    let cb = if ob == '(' { ')' } else { '}' };
    let mut depth = 0usize;
    let mut instr = false;
    let mut inesc = false;
    for (i, c) in text.char_indices() {
        if i < open {
            continue;
        }
        if instr {
            if inesc {
                inesc = false;
            } else if c == '\\' {
                inesc = true;
            } else if c == '"' {
                instr = false;
            }
            continue;
        }
        if c == '"' {
            instr = true;
        } else if c == ob {
            depth += 1;
        } else if c == cb {
            depth -= 1;
            if depth == 0 {
                return Some(i + cb.len_utf8());
            }
        }
    }
    None
}

// ------------------------------------------------------------------
// fn spans (brace balance over comment-stripped lines)
// ------------------------------------------------------------------

struct FnSpan {
    name: String,
    a: usize,
    b: usize,
    hdr: String,
}

fn fn_header_name(l: &str) -> Option<String> {
    let mut rest = l.trim_start();
    loop {
        let before = rest;
        for kw in ["pub(", "default ", "const ", "async ", "unsafe ", "extern "] {
            if rest.starts_with(kw) {
                rest = if kw == "pub(" {
                    let e = rest.find(')')?;
                    rest[e + 1..].trim_start()
                } else {
                    rest[kw.len()..].trim_start()
                };
                break;
            }
        }
        if let Some(r) = rest.strip_prefix("pub ") {
            rest = r.trim_start();
        }
        if rest == before {
            break;
        }
    }
    let after = rest.strip_prefix("fn ")?.trim_start();
    if !after.chars().next().is_some_and(is_id) {
        return None;
    }
    let name: String = after.chars().take_while(|c| is_id(*c)).collect();
    Some(name)
}

fn fn_spans(lines: &[String]) -> Vec<FnSpan> {
    let mut spans: Vec<FnSpan> = Vec::new();
    let mut k = 0usize;
    while k < lines.len() {
        let Some(name) = fn_header_name(&lines[k]) else {
            k += 1;
            continue;
        };
        let mut depth = 0i32;
        let mut seen = false;
        let mut j = k;
        while j < lines.len() {
            for c in lines[j].chars() {
                match c {
                    '{' => {
                        depth += 1;
                        seen = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if seen && depth <= 0 {
                break;
            }
            j += 1;
        }
        spans.push(FnSpan {
            name,
            a: k,
            b: (j + 1).min(lines.len()),
            hdr: norm(&lines[k]),
        });
        k += 1; // also nested fns
    }
    spans
}

fn fn_at(spans: &[FnSpan], idx: usize) -> String {
    let mut best = "?".to_string();
    for s in spans {
        if s.a <= idx && idx < s.b {
            best = s.name.clone(); // last = innermost
        }
    }
    best
}

// ------------------------------------------------------------------
// argument-closure cutting (popup bodies never count for their owner)
// ------------------------------------------------------------------

/// `|args|` closure at byte `i` (reject `||`; no nested pipes/brackets in
/// the param list) -> (start, end) span covering the whole closure.
fn closure_at(s: &str, i: usize) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    if b[i] != b'|' || b.get(i + 1) == Some(&b'|') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() {
        let c = b[j];
        if c == b'|' {
            break;
        }
        if matches!(c, b'{' | b'}' | b'(' | b')') {
            return None;
        }
        j += 1;
    }
    if j >= b.len() {
        return None;
    }
    let mut k = j + 1;
    while k < b.len() && (b[k] == b' ' || b[k] == b'\t') {
        k += 1;
    }
    if b.get(k) == Some(&b'{') {
        let end = close_of(s, k).unwrap_or(s.len());
        return Some((i, end));
    }
    // expression closure: runs until the parent-close ')' or a ';'
    let mut depth = 0i32;
    let mut stop = s.len();
    let mut instr = false;
    let mut inesc = false;
    for (jj, c) in s.char_indices() {
        if jj < k {
            continue;
        }
        if instr {
            if inesc {
                inesc = false;
            } else if c == '\\' {
                inesc = true;
            } else if c == '"' {
                instr = false;
            }
            continue;
        }
        match c {
            '"' => instr = true,
            '(' => depth += 1,
            ';' => {
                stop = jj;
                break;
            }
            ')' => {
                if depth == 0 {
                    stop = jj;
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    Some((i, stop))
}

fn strip_closures(stmt: &str) -> String {
    let mut out = String::with_capacity(stmt.len());
    let mut i = 0usize;
    while i < stmt.len() {
        if let Some((_, end)) = closure_at(stmt, i) {
            out.push(' ');
            i = end;
            continue;
        }
        let c = stmt[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

// ------------------------------------------------------------------
// HOVER scan
// ------------------------------------------------------------------

const TRIGS: [&str; 7] = [
    ".button(",
    "_button(",
    "Button::new(",
    ".checkbox(",
    "selectable_label(",
    "selectable_value(",
    "ComboBox::",
];
const CLK: [&str; 3] = ["clicked()", "clicked_by", "changed()"];

fn has_clk(s: &str) -> bool {
    CLK.iter().any(|k| s.contains(k))
}

fn line_depths(row: &str, pdepth: &mut i32, bdepth: &mut i32, has_semi: &mut bool, saw: &mut bool) {
    let mut instr = false;
    let mut inesc = false;
    for c in row.chars() {
        if instr {
            if inesc {
                inesc = false;
            } else if c == '\\' {
                inesc = true;
            } else if c == '"' {
                instr = false;
            }
            continue;
        }
        match c {
            '"' => instr = true,
            '(' => {
                *pdepth += 1;
                *saw = true;
            }
            ')' => {
                *pdepth -= 1;
                *saw = true;
            }
            '{' => {
                *bdepth += 1;
                *saw = true;
            }
            '}' => *bdepth -= 1,
            ';' => *has_semi = true,
            _ => {}
        }
    }
}

/// (end_line, builder_var) for the statement whose trigger is on `li`.
fn statement_window(lines: &[String], li: usize) -> (usize, Option<String>) {
    let mut pdepth = 0i32;
    let mut bdepth = 0i32;
    let cap = (li + 110).min(lines.len());
    for k in li..cap {
        let row = &lines[k];
        let mut saw = false;
        let mut has_semi = false;
        line_depths(row, &mut pdepth, &mut bdepth, &mut has_semi, &mut saw);
        let has_click = has_clk(row);
        if has_click && pdepth <= 0 && bdepth >= 1 && saw {
            return (k, builder_var(&lines[li..=k].join("\n")));
        }
        if pdepth <= 0 && bdepth <= 0 && has_semi && (k > li || has_click || saw) {
            return (k, builder_var(&lines[li..=k].join("\n")));
        }
    }
    (cap.saturating_sub(1), None)
}

fn builder_var(stmt: &str) -> Option<String> {
    let t = stmt.trim_start();
    let rest = t.strip_prefix("let ")?.trim_start();
    let rest = rest.strip_prefix("mut ").unwrap_or(rest).trim_start();
    let name: String = rest.chars().take_while(|c| is_id(*c)).collect();
    if name.is_empty() {
        return None;
    }
    if !rest[name.len()..].trim_start().starts_with('=') {
        return None;
    }
    Some(name)
}

/// Ride the builder chain to the add-call that attaches its hover.
fn extend_for_builder(lines: &[String], end: usize, var: &Option<String>) -> usize {
    let Some(var) = var.as_deref() else {
        return end;
    };
    let mut end = end;
    let mut started = false;
    let cap = (end + 21).min(lines.len());
    let base = end; // frozen: `end` advances inside the loop
    for (off, row) in lines[base + 1..cap].iter().enumerate() {
        let k = base + 1 + off;
        let s = row.trim();
        if row.contains(var) {
            started = true;
        }
        if started {
            end = k;
            if has_clk(row) {
                break;
            }
        } else {
            if s.is_empty() {
                continue;
            }
            if has_clk(row)
                || row.contains("on_hover")
                || TRIGS.iter().any(|t| row.contains(t))
            {
                break;
            }
            if k.saturating_sub(end) >= 8 {
                break;
            }
        }
    }
    end
}

struct AF<'a> {
    name: &'static str,
    lines: &'a [String],
    spans: &'a [FnSpan],
}

fn audit_files<'a>(files: &'a [(&'static str, Vec<String>, Vec<FnSpan>)]) -> Vec<AF<'a>> {
    files
        .iter()
        .map(|(n, l, s)| AF {
            name: n,
            lines: l,
            spans: s,
        })
        .collect()
}

/// menu_button argument-closure spans over the joined text.
fn menu_button_spans(joined: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = joined[from..].find("menu_button(") {
        let st = from + rel;
        let open = st + "menu_button".len();
        let Some(cend) = close_of(joined, open) else {
            from = st + 5;
            continue;
        };
        let mut k = open;
        while k < cend {
            if let Some((_, bend)) = closure_at(joined, k) {
                spans.push((k, bend));
                k = bend;
                continue;
            }
            k += 1;
        }
        from = cend;
    }
    spans
}

#[derive(Debug)]
struct HoverGap {
    site: String, // "<file>\t<fn>\t<norm line>"
    src: String, // trigger line (dump only)
}

struct HoverAudit {
    triggers: usize,
    gaps: Vec<HoverGap>,
}

fn hover_audit(files: &[AF]) -> HoverAudit {
    let mut triggers = 0usize;
    let mut gaps = Vec::new();
    for f in files {
        let joined = f.lines.join("\n");
        let mut offs = vec![0usize; f.lines.len() + 1];
        for (i, l) in f.lines.iter().enumerate() {
            offs[i + 1] = offs[i] + l.len() + 1;
        }
        let mb = menu_button_spans(&joined);
        let helpers: BTreeSet<String> = f
            .spans
            .iter()
            .filter(|s| f.lines[s.a..s.b].join("\n").contains("on_hover"))
            .map(|s| s.name.clone())
            .collect();
        for (li, row) in f.lines.iter().enumerate() {
            if fn_header_name(row).is_some() {
                continue; // fn definitions are not statements
            }
            let mut registered = false;
            for tok in TRIGS {
                if registered {
                    break;
                }
                let mut pos = 0usize;
                while let Some(rel) = row[pos..].find(tok) {
                    let p = pos + rel;
                    pos = p + tok.len();
                    if tok == "Button::new(" {
                        let pre: char = if p > 0 {
                            row.as_bytes()[p - 1] as char
                        } else {
                            ' '
                        };
                        if pre.is_ascii_alphanumeric() || pre == '_' {
                            continue; // RadioButton::new substring artifact
                        }
                    }
                    let off = offs[li] + p;
                    if mb.iter().any(|(a, b)| *a <= off && off < *b) {
                        continue; // submenu item: the items explain themselves
                    }
                    // identifier owning the trigger
                    let mut left = off;
                    while left > 0 && is_id(joined.as_bytes()[left - 1] as char) {
                        left -= 1;
                    }
                    let body: String = tok
                        .chars()
                        .take_while(|c| is_id(*c) || *c == ':')
                        .collect();
                    let body = body.strip_prefix('.').unwrap_or(&body).to_string();
                    let ident = format!("{}{}", &joined[left..off], body);
                    if ident == "menu_button" || helpers.contains(&ident) {
                        continue; // doctrine / helper owns the hover
                    }
                    let (mut end, mut var) = statement_window(f.lines, li);
                    if var.is_none() {
                        for back in 1..7usize {
                            if li < back {
                                break;
                            }
                            let prev = f.lines[li - back].trim_end();
                            if let Some(v) = builder_var(prev) {
                                var = Some(v);
                                break;
                            }
                            if prev.ends_with(';') {
                                break;
                            }
                        }
                    }
                    end = extend_for_builder(f.lines, end, &var);
                    // Window lines are the depth-aware statement; comment
                    // lines carry no tokens, so hovers in comments never
                    // count (the echo scan's comment strip is sabotage-
                    // proved the same way, polarity B).
                    assert!(
                        end < f.lines.len(),
                        "statement window OOB: file={} li={} end={} len={}",
                        f.name,
                        li,
                        end,
                        f.lines.len()
                    );
                    let stmt = f.lines[li..=end].join("\n");
                    let core = strip_closures(&stmt);
                    triggers += 1;
                    if !core.contains("on_hover") {
                        gaps.push(HoverGap {
                            site: format!("{}\t{}\t{}", f.name, fn_at(f.spans, li), norm(row)),
                            src: row.trim().to_string(),
                        });
                    }
                    registered = true;
                    break;
                }
            }
        }
    }
    HoverAudit { triggers, gaps }
}

// ------------------------------------------------------------------
// ECHO scan (ui.rs production region)
// ------------------------------------------------------------------

const SEAM_PREFIX: [&str; 40] = [
    "do_", "apply", "set_", "add_", "remove_", "del_", "delete_", "clear_", "move_", "copy_",
    "rename_", "save_", "load_", "new_", "start_", "stop_", "toggle_", "reset_", "dock_",
    "undock_", "capture_", "import_", "export_", "commit_", "insert_", "duplicate_", "rescan_",
    "bind_", "unbind_", "arm_", "create_", "build_", "split_", "merge_", "swap_", "open_",
    "close_", "pick_", "assign_", "revert_",
];
const SEAM_SUFFIX: [&str; 11] = [
    "_move", "_rename", "_delete", "_clear", "_save", "_load", "_apply", "_bind", "_commit",
    "_reset", "_toggle",
];
const MUT_OPS: [&str; 18] = [
    ".push(", ".insert(", ".remove(", ".clear(", ".retain(", ".swap(", ".extend(", ".pop(",
    ".sort(", ".set_", ".drain(", ".resize(", ".truncate(", ".reverse(", "+=", "-=", "assign(",
    "splice(",
];

fn is_seam_name(n: &str) -> bool {
    SEAM_PREFIX.iter().any(|p| n.starts_with(p)) || SEAM_SUFFIX.iter().any(|s| n.ends_with(s))
}

fn seam_params(hdr: &str) -> bool {
    let Some(o) = hdr.find('(') else {
        return false;
    };
    let close = hdr.rfind(')').unwrap_or(hdr.len());
    let compact: String = hdr[o..close].chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("&mutself") || compact.contains("&mutApp")
}

/// a live `.status = ` write (but not `==`).
fn has_status_write(s: &str) -> bool {
    let mut from = 0usize;
    while let Some(rel) = s[from..].find(".status") {
        let p = from + rel + ".status".len();
        let tail = s[p..].trim_start();
        if tail.starts_with('=') && !tail.starts_with("==") {
            return true;
        }
        from = p;
    }
    false
}

/// state-mutation signature in a body/blk.
fn has_mut(s: &str) -> bool {
    if MUT_OPS.iter().any(|m| s.contains(m)) {
        return true;
    }
    let b = s.as_bytes();
    for key in ["self.", "app."] {
        let mut from = 0usize;
        while from < s.len() {
            let Some(rel) = s[from..].find(key) else {
                break;
            };
            let mut i = from + rel + key.len();
            if i < b.len() && (b[i] as char).is_ascii_lowercase() {
                while i < b.len() && is_id(b[i] as char) {
                    i += 1;
                }
                let rest = s[i..].trim_start();
                if (rest.starts_with('=') && !rest.starts_with("=="))
                    || rest.starts_with("+=")
                    || rest.starts_with("-=")
                {
                    return true;
                }
            }
            from = from + rel + key.len();
        }
    }
    false
}

/// identifier immediately preceding `(` (string-aware).
fn callees(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut instr = false;
    let mut inesc = false;
    for (i, c) in s.char_indices() {
        if instr {
            if inesc {
                inesc = false;
            } else if c == '\\' {
                inesc = true;
            } else if c == '"' {
                instr = false;
            }
            continue;
        }
        match c {
            '"' => instr = true,
            '(' => {
                let mut j = i;
                while j > 0 && is_id(s.as_bytes()[j - 1] as char) {
                    j -= 1;
                }
                if j < i {
                    out.insert(s[j..i].to_string());
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug)]
struct EchoGap {
    kind: &'static str, // "fn" | "blk"
    site: String,       // "ui.rs\t<fn>\t<key>" — key: fn name | norm mut-line
    src: String,
}

struct EchoAudit {
    fn_checked: usize,
    blk_checked: usize,
    gaps: Vec<EchoGap>,
}

fn echo_audit(lines: &[String], spans: &[FnSpan]) -> EchoAudit {
    let joined = lines.join("\n");
    let mut offs = vec![0usize; lines.len() + 1];
    for (i, l) in lines.iter().enumerate() {
        offs[i + 1] = offs[i] + l.len() + 1;
    }
    let mut body: BTreeMap<&str, String> = BTreeMap::new();
    let mut hdr: BTreeMap<&str, String> = BTreeMap::new();
    for s in spans {
        body.entry(&s.name)
            .or_insert_with(|| lines[s.a..s.b].join("\n"));
        hdr.entry(&s.name).or_insert_with(|| s.hdr.clone());
    }
    fn has_echo(
        name: &str,
        body: &BTreeMap<&str, String>,
        depth: usize,
        seen: &mut BTreeSet<String>,
    ) -> bool {
        if depth > 2 || !seen.insert(name.to_string()) {
            return false;
        }
        let Some(b) = body.get(name) else {
            return false;
        };
        if has_status_write(b) {
            return true;
        }
        callees(b)
            .iter()
            .any(|c| c != name && has_echo(c, body, depth + 1, seen))
    }
    fn echoing(name: &str, body: &BTreeMap<&str, String>) -> bool {
        let mut seen = BTreeSet::new();
        has_echo(name, body, 0, &mut seen)
    }
    let mut gaps = Vec::new();
    // class 1 — command fns
    let mut fn_checked = 0usize;
    let mut fn_gaps: Vec<(&str, String)> = Vec::new();
    for (name, b) in &body {
        if !is_seam_name(name)
            || name.ends_with("to_save")
            || name.ends_with("from_save")
            || name.ends_with("_for_save")
        {
            continue;
        }
        if !seam_params(&hdr[name]) || !has_mut(b) {
            continue;
        }
        fn_checked += 1;
        if !echoing(name, &body) {
            fn_gaps.push((name, hdr[name].clone()));
        }
    }
    for (name, h) in fn_gaps {
        gaps.push(EchoGap {
            kind: "fn",
            site: format!("ui.rs\t{name}\t{name}"),
            src: h,
        });
    }
    // class 2 — inline `.clicked() {` blocks that mutate without echo
    let mut blk_checked = 0usize;
    let mut from = 0usize;
    while let Some(rel) = joined[from..].find(".clicked()") {
        let st = from + rel + ".clicked()".len();
        from = st;
        let tail = &joined[st..];
        let lead = tail.len() - tail.trim_start().len();
        if !tail[lead..].starts_with('{') {
            continue;
        }
        let ob = st + lead;
        let Some(cb) = close_of(&joined, ob) else {
            continue;
        };
        let blk = &joined[ob..cb];
        if !has_mut(blk) || has_status_write(blk) {
            continue;
        }
        // the mutation must reach app state (rooted), not a local builder
        // (`rfd::FileDialog::new().set_title(..)`) or a local String — those
        // are not user-visible state-change seams.
        if !(blk.contains("self.")
            || blk.contains("app.")
            || blk.contains("kit[")
            || blk.contains("roll.pat"))
        {
            continue;
        }
        if callees(blk)
            .iter()
            .any(|c| body.contains_key(c.as_str()) && echoing(c, &body))
        {
            continue;
        }
        // key: first rooted mutation line inside the block
        fn rooted_assign(l: &str) -> bool {
            for key in ["self.", "app.", "kit[", "roll.pat"] {
                let mut from = 0usize;
                while let Some(rel) = l[from..].find(key) {
                    let mut i = from + rel + key.len();
                    let b = l.as_bytes();
                    if key == "kit[" || key == "roll.pat" {
                        // rooted collection/pattern access: mutation token?
                        if MUT_OPS.iter().any(|m| l[i..].contains(m)) {
                            return true;
                        }
                        from = i;
                        continue;
                    }
                    while i < b.len()
                        && (is_id(b[i] as char)
                            || (b[i] == b'.'
                                && i + 1 < b.len()
                                && (is_id(b[i + 1] as char) || b[i + 1] == b'_')))
                    {
                        i += 1;
                    }
                    let rest = l[i..].trim_start();
                    let mut ok = (rest.starts_with('=') && !rest.starts_with("=="))
                        || rest.starts_with("+=")
                        || rest.starts_with("-=");
                    // mutation call: re-attach the method dot (the id-skip
                    // above consumed it — `app.x.clear(` stops after "clear")
                    let seg_start = from + rel + key.len();
                    if let Some(d) = l[seg_start..i].rfind('.') {
                        let cand = &l[seg_start + d..];
                        ok |= MUT_OPS.iter().any(|m| cand.starts_with(m));
                    }
                    if ok {
                        return true;
                    }
                    from = i;
                }
            }
            false
        }
        let mut moff = 0usize;
        let mut off = 0usize;
        for line in blk.split_inclusive('\n') {
            if rooted_assign(line) {
                moff = off;
                break;
            }
            off += line.len();
        }
        let g = ob + moff;
        let ll = offs.partition_point(|o| *o <= g) - 1;
        blk_checked += 1;
        gaps.push(EchoGap {
            kind: "blk",
            site: format!("ui.rs\t{}\t{}", fn_at(spans, ll), norm(&lines[ll])),
            src: norm(&lines[ll]),
        });
    }
    EchoAudit {
        fn_checked,
        blk_checked,
        gaps,
    }
}

// ------------------------------------------------------------------
// exemption table
// ------------------------------------------------------------------

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Ex {
    kind: &'static str, // "hov" | "fn" | "blk"
    site: &'static str, // "<file>\t<fn>\t<key>" exactly as the scans build it
    n: usize,           // expected gap count for this site
    why: &'static str,  // required reason
}

const EXEMPTIONS: &[Ex] = &[
    // Audited off main @ 0d71dd6 (ts-027 landing pass, 2026-11-26). Doctrine
    // classes follow v72/v76/v80/v86 skip rules; `gap:` rows are OPEN work
    // seeded to ts-018 surface cards — the owning lane fixes the surface AND
    // deletes the row in the same PR (the pin goes RED on a stale row).
    Ex { kind: "blk", site: "ui.rs\tauto_inner\tself.auto_pick_open = true;", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\tchain_win_body\tapp.backend.engine.set_sc_send(loop_i, 0);", n: 1, why: "skip:same-view — removal/clear repaints visibly; echo would repeat the widget (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tdocked_head\tapp.side_dirty = true;", n: 1, why: "skip:internal-flag — repaint/pending-save plumbing, not user-visible state" },
    Ex { kind: "blk", site: "ui.rs\tdrum_win_body\tself.drum_commit_note = None;", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\texport_window\tapp.export_path = p.to_string_lossy().into_owned();", n: 1, why: "skip:file-dialog — the picked path shows in the field/panel it follows" },
    Ex { kind: "blk", site: "ui.rs\texport_window\tapp.export_sel = false;", n: 2, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\texport_window\tapp.export_sel = true;", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tgrid_context_menu\tapp.sample_edit_win = Some(key);", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\tmix_column\tapp.fx_window = Some(t);", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\tmix_column\tapp.synth_pick = Some(t);", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\tmonitor_body\tapp.midi_log.clear();", n: 1, why: "skip:same-view — removal/clear repaints visibly; echo would repeat the widget (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\track_body\tapp.proj_confirm = Some((name.clone(), pth.clone()));", n: 1, why: "skip:dialog-prompt — opens a confirm/rename editor that IS the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\track_body\tapp.rack_collapsed.clear();", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\track_body\tapp.rack_collapsed.insert(format!(\"{}\\u{1}{d}\", f.path));", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\track_body\tapp.rack_collapsed.insert(key.clone());", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\track_body\tapp.rack_confirm_del = Some(apdisp.clone());", n: 1, why: "skip:dialog-prompt — opens a confirm/rename editor that IS the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\track_body\tapp.rack_group_by_dir = !app.rack_group_by_dir;", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\track_body\tapp.rack_rename =", n: 1, why: "skip:dialog-prompt — opens a confirm/rename editor that IS the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\track_body\tapp.sample_edit_win = Some(apdisp.clone());", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\track_file_row\tapp.rack_confirm_del = Some(fp_disp.clone());", n: 1, why: "skip:dialog-prompt — opens a confirm/rename editor that IS the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\track_file_row\tapp.rack_rename = Some((fp_disp.clone(), name.to_string(), true));", n: 1, why: "skip:dialog-prompt — opens a confirm/rename editor that IS the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\track_file_row\tapp.sample_edit_win = Some(fp_disp.clone());", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\troll_rmb_menu\troll.pat.bend_segs.remove(i);", n: 1, why: "skip:same-view — removal/clear repaints visibly; echo would repeat the widget (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\troll_rmb_menu\troll.pat.notes.remove(ix);", n: 1, why: "skip:same-view — removal/clear repaints visibly; echo would repeat the widget (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tsave_prompt_window\tapp.show_save_prompt = false;", n: 1, why: "skip:dialog-prompt — opens a confirm/rename editor that IS the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\tsettings_panel\tapp.mcp_token_shown = !shown;", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tsettings_panel\tapp.project_dir = p.to_string_lossy().into_owned();", n: 1, why: "skip:file-dialog — the picked path shows in the field/panel it follows" },
    Ex { kind: "blk", site: "ui.rs\tsettings_panel\tapp.sel_midi.insert(n.clone());", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tsettings_panel\tapp.sett_filter = q.to_string();", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tsettings_panel\tapp.sett_filter.clear();", n: 2, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tsmap_body\tself.smap_sel = Some(i);", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tstrip_fx_row\tapp.fx_window = Some(t);", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\tstrip_synth_row\tapp.params_win = Some(GuiTarget::Synth(t));", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\tstrip_synth_row\tapp.synth_pick = Some(t);", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\ttop_bar\tapp.metro_on = !app.metro_on;", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\ttop_bar\tapp.palette = Some((String::new(), 0));", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\ttop_bar\tapp.show_drums = !app.show_drums;", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\ttop_bar\tapp.show_export = true;", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\ttop_bar\tapp.show_load = true;", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\ttop_bar\tapp.show_master = !app.show_master;", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\ttop_bar\tapp.show_settings = !app.show_settings;", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\ttrack_head_menu\tapp.tpl_track = t;", n: 1, why: "skip:view-state — selection/filter/collapse/toggle state paints itself (v86 R3)" },
    Ex { kind: "blk", site: "ui.rs\tvalue_menu_items\tapp.value_dialog = Some((", n: 1, why: "skip:window-open — opens/closes a window or picker; the surface change is the feedback (v72 open_*)" },
    Ex { kind: "blk", site: "ui.rs\twindow_slot_menu\tapp.dock_just_floated = Some(idx);", n: 1, why: "skip:window-chrome — float bookkeeping for window menus (v76 R3)" },
    Ex { kind: "fn", site: "ui.rs\tarm_key_slot\tarm_key_slot", n: 1, why: "skip:midi-binding — armed indicator shows state (v72 arm skip)" },
    Ex { kind: "fn", site: "ui.rs\tarm_midi_slot\tarm_midi_slot", n: 1, why: "skip:midi-binding — armed indicator shows state (v72 arm skip)" },
    Ex { kind: "fn", site: "ui.rs\tcapture_midi_msg\tcapture_midi_msg", n: 1, why: "skip:same-view — per-message RT capture; the armed indicator and MIDI monitor show it live; echoing each message floods the strip (v86 R3)" },
    Ex { kind: "fn", site: "ui.rs\tdock_all_windows\tdock_all_windows", n: 1, why: "skip:window-chrome — dock/float visibly rearranges windows; echo would be noise (v76 R3)" },
    Ex { kind: "fn", site: "ui.rs\tdock_at\tdock_at", n: 1, why: "skip:window-chrome — dock/float visibly rearranges windows; echo would be noise (v76 R3)" },
    Ex { kind: "fn", site: "ui.rs\tdock_last\tdock_last", n: 1, why: "skip:window-chrome — dock/float visibly rearranges windows; echo would be noise (v76 R3)" },
    Ex { kind: "fn", site: "ui.rs\tdock_tab_into\tdock_tab_into", n: 1, why: "skip:window-chrome — tabbing visibly merges windows (v76 R3)" },
    Ex { kind: "fn", site: "ui.rs\topen_auto_editor\topen_auto_editor", n: 1, why: "skip:window-open — the opened editor IS the feedback (v72 open_*)" },
    Ex { kind: "fn", site: "ui.rs\topen_fx_editor\topen_fx_editor", n: 1, why: "skip:window-open — the opened editor IS the feedback (v72 open_*)" },
    Ex { kind: "fn", site: "ui.rs\tselect_toggle\tselect_toggle", n: 1, why: "skip:view-state — selection toggle paints itself (v86 R3 same-view)" },
    Ex { kind: "fn", site: "ui.rs\tset_group_active\tset_group_active", n: 1, why: "skip:view-state — group tab highlight shows the change (v86 R3)" },
    Ex { kind: "fn", site: "ui.rs\tset_master_midi\tset_master_midi", n: 1, why: "skip:midi-binding — bound indicator shows state (v72 MIDI-bind skip)" },
    Ex { kind: "fn", site: "ui.rs\tset_rack_dir_order\tset_rack_dir_order", n: 1, why: "skip:view-state — sort-order toggle repaints the rack visibly (v86 R3)" },
    Ex { kind: "fn", site: "ui.rs\tset_record_target\tset_record_target", n: 1, why: "skip:self-evident-toggle — record-arm indicator shows the target" },
    Ex { kind: "fn", site: "ui.rs\tset_sc_send_mask\tset_sc_send_mask", n: 1, why: "skip:midi-binding — routed chip visual shows state (v72 MIDI-bind skip)" },
    Ex { kind: "fn", site: "ui.rs\tset_trk_gain\tset_trk_gain", n: 1, why: "skip:same-view — gain control shows the value continuously (v86 R3)" },
    Ex { kind: "fn", site: "ui.rs\tset_trk_pan\tset_trk_pan", n: 1, why: "skip:same-view — pan knob shows the value continuously (v86 R3)" },
    Ex { kind: "fn", site: "ui.rs\tset_trk_vol\tset_trk_vol", n: 1, why: "skip:same-view — fader shows the value continuously; echo would flood (v86 R3)" },
    Ex { kind: "fn", site: "ui.rs\tset_visible\tset_visible", n: 1, why: "skip:view-state — visibility toggle paints itself (v86 R3)" },
    Ex { kind: "fn", site: "ui.rs\tstart_transport_now\tstart_transport_now", n: 1, why: "skip:self-evident-toggle — PLAY indicator shows transport state" },
    Ex { kind: "fn", site: "ui.rs\tstop_previews\tstop_previews", n: 1, why: "skip:internal-cleanup — silent preview stop; no user action to echo" },
    Ex { kind: "fn", site: "ui.rs\tundock_to\tundock_to", n: 1, why: "skip:window-chrome — undock visibly floats the window (v76 R3)" },
    Ex { kind: "hov", site: "kit.rs\tconfirm\tui.checkbox(&mut ask.never_warn, label);", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tauto_inner\t.selectable_value(&mut ed.len_bars, true, \"bars\")", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tauto_inner\tegui::ComboBox::from_id_salt(\"auto_len_unit\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tauto_inner\tegui::ComboBox::from_id_salt(\"auto_snap_sub_cb\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tauto_inner\tegui::ComboBox::from_id_salt(\"auto_target\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tauto_inner\tif ui.checkbox(&mut so, \"snap\").changed() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tauto_inner\tif ui.selectable_value(&mut ed.target, id.clone(), label).changed()", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tauto_inner\tui.selectable_value(&mut ed.len_bars, false, \"sec\");", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tauto_inner\tui.selectable_value(&mut ss, i, format!(\"{l} ({c}/beat)\"));", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tdrum_win_body\t.selectable_value(&mut self.drum.sub, *s, format!(\"{label} ({s}/beat)\"))", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tdrum_win_body\tegui::ComboBox::from_id_salt(\"drum_sub\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tdrum_win_body\tegui::ComboBox::from_id_salt(\"drum_track\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\texport_window\tegui::ComboBox::from_id_salt(\"export_fmt\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\texport_window\tui.selectable_value(&mut app.export_fmt, k, f.label());", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tload_opts_window\tui.checkbox(", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tload_opts_window\tui.checkbox(&mut sel.bindings, \"MIDI bindings\");", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tload_opts_window\tui.checkbox(&mut sel.drum, \"Drum machine + kits\");", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tload_opts_window\tui.checkbox(&mut sel.fx, \"Effects setup (track chains)\");", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tload_opts_window\tui.checkbox(&mut sel.instruments, \"Main instruments (track synths)\");", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tload_opts_window\tui.checkbox(&mut sel.mixer, \"Mixer levels (volume / gain / pan / on)\");", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tload_opts_window\tui.checkbox(&mut sel.tempo, \"Tempo, meter and grid\");", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tport_list\tif ui.checkbox(&mut checked, short).clicked() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\track_body\tegui::Button::new(", n: 1, why: "skip:file-row-label — rack row label shows the file/folder name and count (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\troll_inner\tegui::ComboBox::from_id_salt(\"roll_snap_cb\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\troll_inner\tegui::ComboBox::from_id_salt(\"roll_track_pick\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\troll_inner\tif ui.selectable_value(&mut self.snap_sub, n, s.1).changed() {", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsample_edit_window\tegui::ComboBox::from_id_salt(format!(\"se-curve-{label}\"))", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\t.selectable_label(bound_now.as_deref() == Some(p.name.as_str()), txt)", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\t.selectable_label(bound_now.is_none(), \"(default) \\u{2014} text labels\")", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\t.selectable_value(&mut app.edo_root, k, crate::tune::std_name(k as i32))", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\t.selectable_value(&mut app.light_theme, false, \"dark\")", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\t.selectable_value(&mut app.light_theme, true, \"light\")", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tegui::ComboBox::from_id_salt(\"bind_loop_sel\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tegui::ComboBox::from_id_salt(\"edo_pack_pick\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tegui::ComboBox::from_id_salt(\"edo_root_pick\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tegui::ComboBox::from_id_salt(\"slooper_audio_backend\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tegui::ComboBox::from_id_salt(\"theme_cb\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tif ui.checkbox(&mut app.show_binds, \"MIDI bindings\").changed() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tif ui.checkbox(&mut app.show_midi, \"MIDI monitor\").changed() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tif ui.checkbox(&mut app.show_mixer, \"mixer dock\").changed() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tif ui.checkbox(&mut app.show_rack, \"asset rack\").changed() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tif ui.checkbox(&mut checked, n.as_str()).clicked() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tui.selectable_value(&mut app.bind_loop_sel, n, format!(\"{}\", n + 1));", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsettings_panel\tui.selectable_value(&mut chosen, label.clone(), label);", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsmap_body\tegui::ComboBox::from_id_salt((\"smapcombo\", t, i))", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tsynth_sliders_body\tegui::ComboBox::from_id_salt((\"sampler_asset_pick\", t))", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\ttarget_search_box\tif !q2.is_empty() && ui.small_button(\"×\").clicked() {", n: 1, why: "skip:self-explaining-glyph — standard clear glyph, shown only when non-empty (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\ttop_bar\tif ui.checkbox(&mut app.show_binds, \"MIDI bindings\").changed() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\ttop_bar\tif ui.checkbox(&mut app.show_midi, \"MIDI monitor\").changed() {", n: 1, why: "skip:self-explaining-label — the checkbox/label text IS the explanation (v76 R2)" },
    Ex { kind: "hov", site: "ui.rs\tvalue_binds_body\t.selectable_value(&mut target, id.clone(), label)", n: 1, why: "skip:popup-item — the item label is the option; the owner shows the selection (v86 R2)" },
    Ex { kind: "hov", site: "ui.rs\tvalue_binds_body\tegui::ComboBox::from_id_salt(\"vb_target\")", n: 1, why: "skip:combo-label — the combo shows the current selection; popup items explain themselves (v86 R2)" },
];

/// Split gaps vs the table: unmatched gap sites (NEW) + stale rows +
/// count mismatches. `gaps` = site strings; counts aggregate by site.
fn cross(kind: &'static str, gaps: &[String], ex: &[Ex]) -> (Vec<String>, Vec<String>) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for g in gaps {
        *counts.entry(g.as_str()).or_default() += 1;
    }
    let mut offenders = Vec::new();
    for (site, c) in &counts {
        let m: Vec<&Ex> = ex
            .iter()
            .filter(|e| e.kind == kind && e.site == *site)
            .collect();
        if m.is_empty() {
            offenders.push(format!(
                "NEW GAP {kind} site={site:?} x{c} — fix the surface or add a justified row"
            ));
        } else {
            let n: usize = m.iter().map(|e| e.n).sum();
            if &n != c {
                offenders.push(format!(
                    "COUNT MISMATCH {kind} site={site:?}: table sums {n}, actual {c}"
                ));
            }
        }
    }
    let mut stale = Vec::new();
    for e in ex.iter().filter(|e| e.kind == kind) {
        if !counts.contains_key(&e.site) {
            stale.push(format!(
                "STALE EXEMPTION {kind} site={:?} ({}) — gap gone? delete the row",
                e.site, e.why
            ));
        }
    }
    (offenders, stale)
}

// ------------------------------------------------------------------
// candidate dump (dev-only; gated by env to keep the pin quiet otherwise)
// ------------------------------------------------------------------

fn dump_candidates(files: &[AF], lines: &[String], spans: &[FnSpan]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let ha = hover_audit(files);
    let mut hk: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
    for g in &ha.gaps {
        let e = hk
            .entry(g.site.as_str())
            .or_insert((0usize, g.src.as_str()));
        e.0 += 1;
    }
    for (site, (n, src)) in &hk {
        let _ = writeln!(
            out,
            "Ex {{ kind: \"hov\", site: {site:?}, n: {n}, why: \"TODO\" }}, // {src}"
        );
    }
    let ea = echo_audit(lines, spans);
    let mut fk: BTreeMap<&str, &str> = BTreeMap::new();
    let mut bk: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
    for g in &ea.gaps {
        match g.kind {
            "fn" => {
                fk.insert(g.site.as_str(), g.src.as_str());
            }
            _ => {
                let e = bk
                    .entry(g.site.as_str())
                    .or_insert((0usize, g.src.as_str()));
                e.0 += 1;
            }
        }
    }
    for (site, src) in &fk {
        let _ = writeln!(
            out,
            "Ex {{ kind: \"fn\", site: {site:?}, n: 1, why: \"TODO\" }}, // {src}"
        );
    }
    for (site, (n, src)) in &bk {
        let _ = writeln!(
            out,
            "Ex {{ kind: \"blk\", site: {site:?}, n: {n}, why: \"TODO\" }}, // {src}"
        );
    }
    out
}

// ------------------------------------------------------------------
// pins
// ------------------------------------------------------------------

/// Load the render files' production regions (comment-stripped) + fn spans.
fn load_all() -> Vec<(&'static str, Vec<String>, Vec<FnSpan>)> {
    let mut v = Vec::new();
    for (name, raw) in [
        ("ui.rs", include_str!("../ui.rs")),
        ("presetbar.rs", include_str!("../presetbar.rs")),
        ("kit.rs", include_str!("../kit.rs")),
    ] {
        let prod = match raw.find("\nmod tests") {
            Some(i) => &raw[..i],
            None => raw,
        };
        let lines: Vec<String> = prod.lines().map(strip_line).collect();
        let spans = fn_spans(&lines);
        v.push((name, lines, spans));
    }
    v
}

#[test]
fn v173_hover_program_scan() {
    let files_raw = load_all();
    let files = audit_files(&files_raw);
    let ha = hover_audit(&files);
    // broken-scan guard (v76/v80 `checked >= N` lineage): the whole program
    // carries far more affordances than the floor.
    assert!(
        ha.triggers >= 250,
        "hover scan found only {} affordance statements — broken scan (current main carries ~307)",
        ha.triggers
    );
    let sites: Vec<String> = ha.gaps.iter().map(|g| g.site.clone()).collect();
    let (offenders, stale) = cross("hov", &sites, EXEMPTIONS);
    if std::env::var("SLOOPER_V173_DUMP").is_ok() {
        println!("{}", dump_candidates(&files, &files_raw[0].1, &files_raw[0].2));
    }
    assert!(
        offenders.is_empty() && stale.is_empty(),
        "hover-coverage roster broken ({} new, {} stale; {} gaps total, {} covered):\n{}{}",
        offenders.len(),
        stale.len(),
        ha.gaps.len(),
        ha.triggers - ha.gaps.len(),
        offenders.join("\n"),
        if stale.is_empty() {
            String::new()
        } else {
            format!("\n{}", stale.join("\n"))
        }
    );
}

#[test]
fn v173_echo_program_scan() {
    let files_raw = load_all();
    let (ui_lines, ui_spans) = (&files_raw[0].1, &files_raw[0].2);
    let ea = echo_audit(ui_lines, ui_spans);
    assert!(
        ea.fn_checked >= 60,
        "echo scan classified only {} command fns — broken scan (current main carries ~87)",
        ea.fn_checked
    );
    assert!(
        ea.blk_checked >= 40,
        "echo scan classified only {} clicked-block seams — broken scan (current main carries ~50)",
        ea.blk_checked
    );
    let mut offenders = Vec::new();
    let mut stale = Vec::new();
    for kind in ["fn", "blk"] {
        let k: Vec<String> = ea
            .gaps
            .iter()
            .filter(|g| g.kind == kind)
            .map(|g| g.site.clone())
            .collect();
        let (mut o, mut s) = cross(kind, &k, EXEMPTIONS);
        offenders.append(&mut o);
        stale.append(&mut s);
    }
    if std::env::var("SLOOPER_V173_DUMP").is_ok() {
        let files = audit_files(&files_raw);
        println!("{}", dump_candidates(&files, ui_lines, ui_spans));
    }
    assert!(
        offenders.is_empty() && stale.is_empty(),
        "echo-inventory roster broken ({} new, {} stale; {} seam gaps total):\n{}{}",
        offenders.len(),
        stale.len(),
        ea.gaps.len(),
        offenders.join("\n"),
        if stale.is_empty() {
            String::new()
        } else {
            format!("\n{}", stale.join("\n"))
        }
    );
}

/// The ts-019 surface map (t_39d3c141 comment #925): every named render fn
/// must exist EXACTLY ONCE as a production fn header. A renamed or moved
/// surface goes RED here instead of silently vanishing from coverage.
const ROSTER: [&str; 41] = [
    "mix_column",
    "mixer_body",
    "mix_page",
    "master_out_body",
    "monitor_body",
    "value_binds_body",
    "settings_panel",
    "central_panels",
    "paint_grid",
    "grid_context_menu",
    "grid_toolbar",
    "track_head_menu",
    "track_color_submenu",
    "clip_props_strip",
    "roll_inner",
    "roll_rmb_menu",
    "drum_win_body",
    "smap_body",
    "sample_edit_window",
    "synth_sliders_body",
    "synth_pick_body",
    "auto_inner",
    "auto_pick_window",
    "auto_menu",
    "value_menu_items",
    "value_popups",
    "knob_learn_window",
    "over_side_window",
    "load_window",
    "save_prompt_window",
    "export_window",
    "top_bar",
    "window_slot_menu",
    "docked_head",
    "rack_body",
    "rack_file_row",
    "chain_win_body",
    "params_body",
    "embed_body",
    "arrow_button",
    "skip_button",
];

#[test]
fn v173_surface_roster_scan() {
    let files_raw = load_all();
    let (ui_lines, ui_spans) = (&files_raw[0].1, &files_raw[0].2);
    // comment-stripper scope assumptions: block comments / raw strings must
    // not appear in the scanned ui.rs region (recorded scope limit).
    let joined = ui_lines.join("\n");
    assert!(!joined.contains("/*"), "block comments appeared in ui.rs prod — strip_line scope violated");
    assert!(!joined.contains("r#\""), "raw strings appeared in ui.rs prod — strip_line scope violated");
    let mut missing = Vec::new();
    for name in ROSTER {
        let hits = ui_spans.iter().filter(|s| s.name == name).count();
        if hits != 1 {
            missing.push(format!("{name}: {hits} fn headers (want exactly 1)"));
        }
    }
    assert!(
        missing.is_empty(),
        "ts-019 surface roster drifted (a surface vanished, duplicated, or left the scan region):\n{}",
        missing.join("\n")
    );
}
