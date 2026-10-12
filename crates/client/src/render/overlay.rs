//! A 3×5 pixel bitmap font for the viewer's stats overlay.
//!
//! The font and the text are packed into the shader's uniform buffer; the
//! fragment shader draws them, so the spike needs no font or text crates.

/// Characters the overlay can show, in glyph-index order. Index 0 (space) is
/// also used for anything unknown.
const GLYPHS: &[(char, [&str; 5])] = &[
    (' ', ["...", "...", "...", "...", "..."]),
    ('0', ["###", "#.#", "#.#", "#.#", "###"]),
    ('1', [".#.", "##.", ".#.", ".#.", "###"]),
    ('2', ["###", "..#", "###", "#..", "###"]),
    ('3', ["###", "..#", ".##", "..#", "###"]),
    ('4', ["#.#", "#.#", "###", "..#", "..#"]),
    ('5', ["###", "#..", "###", "..#", "###"]),
    ('6', ["###", "#..", "###", "#.#", "###"]),
    ('7', ["###", "..#", "..#", ".#.", ".#."]),
    ('8', ["###", "#.#", "###", "#.#", "###"]),
    ('9', ["###", "#.#", "###", "..#", "###"]),
    ('A', [".#.", "#.#", "###", "#.#", "#.#"]),
    ('B', ["##.", "#.#", "##.", "#.#", "##."]),
    ('C', [".##", "#..", "#..", "#..", ".##"]),
    ('D', ["##.", "#.#", "#.#", "#.#", "##."]),
    ('E', ["###", "#..", "##.", "#..", "###"]),
    ('F', ["###", "#..", "##.", "#..", "#.."]),
    ('G', [".##", "#..", "#.#", "#.#", ".##"]),
    ('H', ["#.#", "#.#", "###", "#.#", "#.#"]),
    ('I', ["###", ".#.", ".#.", ".#.", "###"]),
    ('J', ["..#", "..#", "..#", "#.#", ".#."]),
    ('K', ["#.#", "#.#", "##.", "#.#", "#.#"]),
    ('L', ["#..", "#..", "#..", "#..", "###"]),
    ('M', ["#.#", "###", "###", "#.#", "#.#"]),
    ('N', ["##.", "#.#", "#.#", "#.#", "#.#"]),
    ('O', [".#.", "#.#", "#.#", "#.#", ".#."]),
    ('P', ["##.", "#.#", "##.", "#..", "#.."]),
    ('Q', [".#.", "#.#", "#.#", "##.", ".##"]),
    ('R', ["##.", "#.#", "##.", "#.#", "#.#"]),
    ('S', [".##", "#..", ".#.", "..#", "##."]),
    ('T', ["###", ".#.", ".#.", ".#.", ".#."]),
    ('U', ["#.#", "#.#", "#.#", "#.#", "###"]),
    ('V', ["#.#", "#.#", "#.#", "#.#", ".#."]),
    ('W', ["#.#", "#.#", "###", "###", "#.#"]),
    ('X', ["#.#", "#.#", ".#.", "#.#", "#.#"]),
    ('Y', ["#.#", "#.#", ".#.", ".#.", ".#."]),
    ('Z', ["###", "..#", ".#.", "#..", "###"]),
    ('.', ["...", "...", "...", "...", ".#."]),
    (':', ["...", ".#.", "...", ".#.", "..."]),
    ('-', ["...", "...", "###", "...", "..."]),
    ('/', ["..#", "..#", ".#.", "#..", "#.."]),
    ('%', ["#.#", "..#", ".#.", "#..", "#.#"]),
    ('=', ["...", "###", "...", "###", "..."]),
];

/// Glyph slots in the shader's font table.
pub const FONT_SLOTS: usize = 64;
/// Longest text the overlay shows; longer text is cut.
pub const MAX_CHARS: usize = 64;

/// Packs a glyph: bit `row * 3 + col` is set where the pattern has `#`.
pub fn glyph_bits(rows: &[&str; 5]) -> u32 {
    let mut bits = 0;
    for (r, row) in rows.iter().enumerate() {
        for (c, ch) in row.chars().enumerate() {
            if ch == '#' {
                bits |= 1 << (r * 3 + c);
            }
        }
    }
    bits
}

/// The font table for the shader, indexed by glyph index.
pub fn font_table() -> [u32; FONT_SLOTS] {
    let mut table = [0; FONT_SLOTS];
    for (slot, (_, rows)) in table.iter_mut().zip(GLYPHS) {
        *slot = glyph_bits(rows);
    }
    table
}

/// Glyph index for a character (case-insensitive); unknown characters map to space.
pub fn glyph_index(ch: char) -> u32 {
    let ch = ch.to_ascii_uppercase();
    GLYPHS.iter().position(|(c, _)| *c == ch).unwrap_or(0) as u32
}

/// Converts text to glyph indices, cut to [`MAX_CHARS`].
pub fn encode_text(text: &str) -> Vec<u32> {
    text.chars().take(MAX_CHARS).map(glyph_index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_is_well_formed_and_unique() {
        assert!(GLYPHS.len() <= FONT_SLOTS);
        for (ch, rows) in GLYPHS {
            assert!(
                rows.iter()
                    .all(|r| r.len() == 3 && r.chars().all(|c| c == '#' || c == '.')),
                "glyph {ch:?} is not 3x5"
            );
        }
        let table = font_table();
        for (i, (a, _)) in GLYPHS.iter().enumerate().skip(1) {
            assert_ne!(table[i], 0, "glyph {a:?} is blank");
            for (j, (b, _)) in GLYPHS.iter().enumerate().skip(i + 1) {
                assert_ne!(table[i], table[j], "glyphs {a:?} and {b:?} look the same");
            }
        }
    }

    #[test]
    fn bits_follow_row_major_order() {
        let top_left_only = ["#..", "...", "...", "...", "..."];
        assert_eq!(glyph_bits(&top_left_only), 1);
        let bottom_right_only = ["...", "...", "...", "...", "..#"];
        assert_eq!(glyph_bits(&bottom_right_only), 1 << 14);
    }

    #[test]
    fn text_maps_case_insensitively_and_truncates() {
        assert_eq!(encode_text("a"), encode_text("A"));
        assert_eq!(encode_text("?"), vec![0]);
        assert_eq!(encode_text(&"9".repeat(100)).len(), MAX_CHARS);
        let idx = encode_text("LAT 12.5MS");
        assert_eq!(idx.len(), 10);
        assert_eq!(idx[3], 0);
    }
}
