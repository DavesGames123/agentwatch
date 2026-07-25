//! The project's name in block letters: a three-row box-drawing font, so the
//! one thing on screen that says *which repo this board is watching* is legible
//! from across the room.
//!
//! It exists because the name used to be nine dim cells in the corner of the
//! status line, indistinguishable at a glance from the word "sauron" beside it.
//! With several boards open in several panes -- which is the normal way this
//! tool is used -- "which repo am I looking at" was a question you answered by
//! leaning in and reading, and sometimes by answering it wrong.
//!
//! Every glyph is a width-1 box-drawing character, the same constraint the rest
//! of the scene works under, so a char index is a screen column and the banner
//! can be laid out with integer arithmetic like any other sprite.
//!
//! grep targets:
//!   const ROWS   -- how tall a rendered name is
//!   fn width     -- columns a name needs, before drawing it
//!   fn render    -- name -> three rows of block letters
//!   fn glyph     -- the font table itself

/// Rows a rendered name occupies.
pub(super) const ROWS: usize = 3;

/// Columns between two letters, widest first. A caller walks these in order and
/// takes the first that fits, so a name that is one column too wide for the
/// airy setting is set tight rather than demoted to small capitals -- which is
/// what happens at 52 columns, the width of a sauron pane in a four-agent
/// workspace, and so the case that matters most.
pub(super) const KERNS: [usize; 2] = [1, 0];

/// Columns `name` needs at block size with `kern` between letters. Callers ask
/// this *before* rendering so they can pick a layout -- a banner clipped
/// mid-letter reads as a broken terminal, so the decision has to happen while
/// there is still a cheaper form to fall back to.
pub(super) fn width(name: &str, kern: usize) -> usize {
    let mut w = 0;
    for (i, c) in name.chars().enumerate() {
        if i > 0 {
            w += kern;
        }
        w += glyph(c)[0].chars().count();
    }
    w
}

/// Render `name` as [`ROWS`] rows of block letters, all of equal width.
pub(super) fn render(name: &str, kern: usize) -> [String; ROWS] {
    let mut out = [String::new(), String::new(), String::new()];
    for (i, c) in name.chars().enumerate() {
        let g = glyph(c);
        for r in 0..ROWS {
            if i > 0 {
                out[r].push_str(&" ".repeat(kern));
            }
            out[r].push_str(g[r]);
        }
    }
    out
}

/// The font. Double-line box drawing for the strokes, so the letters carry
/// weight at three rows tall; the few diagonals borrow single-line glyphs
/// because there is no double-line diagonal in Unicode.
///
/// Rows of a glyph are always the same width -- `width` reads row 0 and trusts
/// the other two, and a test pins that.
fn glyph(c: char) -> [&'static str; ROWS] {
    match c.to_ascii_uppercase() {
        'A' => ["╔═╗", "╠═╣", "╩ ╩"],
        'B' => ["╔╗ ", "╠╩╗", "╚═╝"],
        'C' => ["╔═╗", "║  ", "╚═╝"],
        'D' => ["╔╦╗", " ║║", "═╩╝"],
        'E' => ["╔═╗", "║╣ ", "╚═╝"],
        'F' => ["╔═╗", "╠╣ ", "╚  "],
        'G' => ["╔═╗", "║ ╦", "╚═╝"],
        'H' => ["╦ ╦", "╠═╣", "╩ ╩"],
        'I' => ["╦", "║", "╩"],
        'J' => [" ╦", " ║", "╚╝"],
        'K' => ["╦╔═", "╠╩╗", "╩ ╩"],
        'L' => ["╦  ", "║  ", "╩═╝"],
        'M' => ["╔╦╗", "║║║", "╩ ╩"],
        'N' => ["╔╗╔", "║║║", "╝╚╝"],
        'O' => ["╔═╗", "║ ║", "╚═╝"],
        'P' => ["╔═╗", "╠═╝", "╩  "],
        'Q' => ["╔═╗ ", "║ ║ ", "╚═╩╝"],
        'R' => ["╦═╗", "╠╦╝", "╩╚═"],
        'S' => ["╔═╗", "╚═╗", "╚═╝"],
        'T' => ["╔╦╗", " ║ ", " ╩ "],
        'U' => ["╦ ╦", "║ ║", "╚═╝"],
        'V' => ["╦  ╦", "╚╗╔╝", " ╚╝ "],
        'W' => ["╦ ╦ ╦", "║ ║ ║", "╚═╩═╝"],
        'X' => ["╗ ╔", " ╳ ", "╝ ╚"],
        'Y' => ["╦ ╦", "╚╦╝", " ╩ "],
        'Z' => ["╔═╗", "╔═╝", "╚═╝"],
        '0' => ["╔═╗", "║╱║", "╚═╝"],
        '1' => ["╔╦ ", " ║ ", "═╩═"],
        '2' => ["╔═╗", "╔═╝", "╚══"],
        '3' => ["╔═╗", " ═╣", "╚═╝"],
        '4' => ["╦ ╦", "╚═╣", "  ╩"],
        '5' => ["╔══", "╚═╗", "╚═╝"],
        '6' => ["╔═ ", "╠═╗", "╚═╝"],
        '7' => ["╔═╗", "  ║", "  ╩"],
        '8' => ["╔═╗", "╠═╣", "╚═╝"],
        '9' => ["╔═╗", "╚═╣", " ═╝"],
        '-' => ["   ", "═══", "   "],
        '_' => ["   ", "   ", "═══"],
        '.' => [" ", " ", "▪"],
        '/' => ["  ╱", " ╱ ", "╱  "],
        '+' => [" ║ ", "═╬═", " ║ "],
        ' ' => ["  ", "  ", "  "],
        // Anything else keeps its column rather than vanishing: a name that
        // silently drops a character is a name that identifies the wrong repo.
        _ => [" ", "▪", " "],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every glyph must be a rectangle. `width` measures row 0 and the
    /// compositor stamps all three at the same column, so a ragged glyph would
    /// shift everything to its right on one row only -- a corruption that looks
    /// like a terminal bug rather than a font bug.
    #[test]
    fn every_glyph_is_a_rectangle_of_single_width_cells() {
        let alphabet: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789-_./+ ?"
            .chars()
            .collect();
        for c in alphabet {
            let g = glyph(c);
            let w = g[0].chars().count();
            assert!(w > 0, "{c:?} renders to nothing");
            for (r, row) in g.iter().enumerate() {
                assert_eq!(row.chars().count(), w, "glyph {c:?} row {r} is ragged");
            }
        }
    }

    #[test]
    fn render_agrees_with_width_and_keeps_its_rows_level() {
        for name in ["agentwatch", "a", "my-repo_2", "WIDE NAME", "x/y.z"] {
            for kern in KERNS {
                let rows = render(name, kern);
                for r in &rows {
                    assert_eq!(
                        r.chars().count(),
                        width(name, kern),
                        "row of {name:?} disagrees with width() at kern {kern}"
                    );
                }
            }
        }
    }

    /// The condensed setting has to actually buy columns, or the fallback ladder
    /// has a rung that does nothing but cost a comparison.
    #[test]
    fn tightening_the_kerning_narrows_the_sign() {
        assert!(KERNS.windows(2).all(|p| p[0] > p[1]), "KERNS must go widest first");
        assert!(width("agentwatch", 0) < width("agentwatch", 1));
    }

    #[test]
    fn case_does_not_change_the_drawing() {
        // The banner is always capitals -- a lowercase repo name must not render
        // as a different (or narrower) sign than the same name shouted.
        assert_eq!(render("Agentwatch", 1), render("AGENTWATCH", 1));
        assert_eq!(width("sauron", 1), width("SAURON", 1));
    }
}
