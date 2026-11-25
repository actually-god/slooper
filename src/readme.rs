//! Doc-lock for the AGENT-LOCKED human header at the top of README.md
//! (docs/AGENT-GUIDE.md invariant #11, t_b0616ef0).
//!
//! The fenced block is the maintainer's hand-written voice: no agent may
//! edit, reword, reformat, reorder, move, or delete it. This module turns
//! that invariant into a CI-red pin: the block must start at file offset 0,
//! carry its two fence markers exactly once, and match the golden copy
//! (`src/readme_human_header.golden.md`) byte-for-byte. Sabotage-proven
//! per polarity at v159: content edit -> golden assert; duplicated marker
//! -> count assert; prepended line -> offset assert; swapped markers ->
//! order assert.

#[cfg(test)]
mod tests {
    /// Opening-fence prefix (the shipped line adds a
    /// "(maintainer-written text ... )" suffix; this prefix pins both).
    const BEGIN: &str = "<!-- BEGIN AGENT-LOCKED HUMAN HEADER";
    /// Closing-fence prefix of the END line.
    const END: &str = "<!-- END AGENT-LOCKED HUMAN HEADER";
    /// Byte-exact copy of README.md lines 1-43: BEGIN at offset 0 through
    /// the END-marker line and its newline. Regenerate ONLY with explicit
    /// maintainer sign-off — never to silence a red pin.
    const GOLDEN: &str = include_str!("readme_human_header.golden.md");

    fn occurrences(hay: &str, needle: &str) -> usize {
        hay.matches(needle).count()
    }

    #[test]
    fn v159_readme_human_header_immutable() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(root.join("README.md"))
            .expect("README.md missing at repo root");
        // Windows-runner insurance: .gitattributes pins *.md to eol=lf, but
        // a stray CRLF checkout must not turn the byte-compare spuriously
        // red (and never masks a content edit inside the block).
        let readme = raw.replace("\r\n", "\n");

        // --- structure polarity: markers, order, offset ---
        assert_eq!(
            occurrences(&readme, BEGIN),
            1,
            "README.md must contain exactly one BEGIN AGENT-LOCKED marker"
        );
        assert_eq!(
            occurrences(&readme, END),
            1,
            "README.md must contain exactly one END AGENT-LOCKED marker"
        );
        let b = readme.find(BEGIN).unwrap();
        let e = readme.find(END).unwrap();
        assert!(
            b < e,
            "BEGIN AGENT-LOCKED marker must precede the END marker (got BEGIN at {b}, END at {e})"
        );
        assert_eq!(
            b, 0,
            "the AGENT-LOCKED block must start at README.md offset 0 (found at {b})"
        );

        // --- anti-vacuity: a blanked/corrupted golden must not pass ---
        assert!(
            GOLDEN.len() > 1000,
            "golden copy implausibly small ({} bytes) — regenerate from the shipped block",
            GOLDEN.len()
        );
        assert!(
            GOLDEN.starts_with(BEGIN) && GOLDEN.ends_with(&format!("{END} -->\n")),
            "golden copy must itself start at BEGIN and end at the END-marker line"
        );

        // --- content polarity: block bytes match the golden copy ---
        assert!(
            readme.starts_with(GOLDEN),
            "the AGENT-LOCKED header block drifted from its golden copy \
             (src/readme_human_header.golden.md). AGENT-GUIDE invariant #11: \
             leave the block byte-identical and open an issue instead."
        );
    }
}
