//! The project's name in block letters: a box-drawing font in two heights, so
//! the one thing on screen that says *which repo this board is watching* is
//! legible from across the room. Light rounded strokes, one weight throughout --
//! see [`glyph_block`] for why that beat the heavier alternatives at this size.
//!
//! It exists because the name used to be nine dim cells in the corner of the
//! status line, indistinguishable at a glance from the word "sauron" beside it.
//! With several boards open in several panes -- which is the normal way this
//! tool is used -- "which repo am I looking at" was a question you answered by
//! leaning in and reading, and sometimes by answering it wrong.
//!
//! Two faces, same skeleton and same column widths:
//!
//!   - [`Face::Tall`], five rows, for a header with the room to frame it;
//!   - [`Face::Block`], three rows, which is what a short terminal falls back to.
//!
//! They are the same letters at two heights rather than two different fonts on
//! purpose: the fallback rung has to read as the same sign, or a board that
//! shrinks looks like a different board.
//!
//! Every glyph is a width-1 box-drawing character, the same constraint the rest
//! of the scene works under, so a char index is a screen column and the banner
//! can be laid out with integer arithmetic like any other sprite.
//!
//! grep targets:
//!   enum Face        -- which of the two heights, and how tall each is
//!   fn width         -- columns a name needs, before drawing it
//!   fn render        -- name -> rows of block letters
//!   fn glyph_block   -- the three-row font table
//!   fn glyph_tall    -- the five-row font table

/// Which height the name is set at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Face {
    /// Three rows: the compact header, and the fallback when the tall one does
    /// not fit the terminal's rows.
    Block,
    /// Five rows: the framed header.
    Tall,
}

impl Face {
    /// Rows a name set in this face occupies.
    pub(super) fn rows(self) -> usize {
        match self {
            Face::Block => 3,
            Face::Tall => 5,
        }
    }
}

/// Columns between two letters, widest first. A caller walks these in order and
/// takes the first that fits, so a name that is one column too wide for the
/// airy setting is set tight rather than demoted to small capitals -- which is
/// what happens at 52 columns, the width of a sauron pane in a four-agent
/// workspace, and so the case that matters most.
pub(super) const KERNS: [usize; 2] = [1, 0];

/// Columns `name` needs in `face` with `kern` between letters. Callers ask this
/// *before* rendering so they can pick a layout -- a banner clipped mid-letter
/// reads as a broken terminal, so the decision has to happen while there is
/// still a cheaper form to fall back to.
///
/// Both faces answer the same number for the same name: the tall face is the
/// block face grown upward, never sideways, so a header that gains rows never
/// has to re-plan its columns.
pub(super) fn width(name: &str, kern: usize, face: Face) -> usize {
    let mut w = 0;
    for (i, c) in name.chars().enumerate() {
        if i > 0 {
            w += kern;
        }
        w += glyph(c, face)[0].chars().count();
    }
    w
}

/// Render `name` as `face.rows()` rows of block letters, all of equal width.
pub(super) fn render(name: &str, kern: usize, face: Face) -> Vec<String> {
    let mut out = vec![String::new(); face.rows()];
    for (i, c) in name.chars().enumerate() {
        let g = glyph(c, face);
        for (r, row) in out.iter_mut().enumerate() {
            if i > 0 {
                row.push_str(&" ".repeat(kern));
            }
            row.push_str(g[r]);
        }
    }
    out
}

fn glyph(c: char, face: Face) -> &'static [&'static str] {
    match face {
        Face::Block => glyph_block(c),
        Face::Tall => glyph_tall(c),
    }
}

/// The three-row font. Light box drawing with rounded corners: one stroke weight
/// throughout, so at three rows tall the letterform is carried by its shape
/// rather than by ink. The double-line strokes this replaced put two rules in
/// every cell, which at a glance filled the counters of the round letters and
/// turned a condensed name into hatching.
///
/// The diagonals borrow whatever Unicode has -- `╱` and `╳` come from a
/// different family than the rest and read a shade heavier; there is no light
/// rounded diagonal to use instead.
///
/// Most glyphs are three columns. `M` and `N` are four, and pay for it: at
/// three columns they came out as `╔╦╗`/`╔╗╔`, which differ from each other and
/// from `╔═╗` by one interior cell, and those are the letters that decide
/// whether a name reads as *agentwatch* or *agentwotch*. `I` is one column,
/// which buys most of that back.
///
/// Rows of a glyph are always the same width -- `width` reads row 0 and trusts
/// the rest, and a test pins that.
fn glyph_block(c: char) -> &'static [&'static str] {
    match c.to_ascii_uppercase() {
        'A' => &["╭─╮", "├─┤", "┴ ┴"],
        'B' => &["╭╮ ", "├┴╮", "╰─╯"],
        'C' => &["╭─╮", "│  ", "╰─╯"],
        'D' => &["╭┬╮", " ││", "─┴╯"],
        'E' => &["╭─╮", "│┤ ", "╰─╯"],
        'F' => &["╭─╮", "├┤ ", "┴  "],
        'G' => &["╭─╮", "│ ┬", "╰─╯"],
        'H' => &["┬ ┬", "├─┤", "┴ ┴"],
        'I' => &["┬", "│", "┴"],
        'J' => &[" ┬", " │", "╰╯"],
        'K' => &["┬╭─", "├┴╮", "┴ ┴"],
        'L' => &["┬  ", "│  ", "┴─╯"],
        'M' => &["╭╮╭╮", "│╰╯│", "┴  ┴"],
        'N' => &["╭╮ ┬", "│╰╮│", "┴ ╰┴"],
        'O' => &["╭─╮", "│ │", "╰─╯"],
        'P' => &["╭─╮", "├─╯", "┴  "],
        'Q' => &["╭─╮ ", "│ │ ", "╰─┴╯"],
        'R' => &["┬─╮", "├┬╯", "┴╰─"],
        'S' => &["╭─╮", "╰─╮", "╰─╯"],
        'T' => &["╭┬╮", " │ ", " ┴ "],
        'U' => &["┬ ┬", "│ │", "╰─╯"],
        'V' => &["┬  ┬", "╰╮╭╯", " ╰╯ "],
        'W' => &["┬ ┬ ┬", "│ │ │", "╰─┴─╯"],
        'X' => &["╮ ╭", " ╳ ", "╯ ╰"],
        'Y' => &["┬ ┬", "╰┬╯", " ┴ "],
        'Z' => &["╭─╮", "╭─╯", "╰─╯"],
        '0' => &["╭─╮", "│╱│", "╰─╯"],
        '1' => &["╭┬ ", " │ ", "─┴─"],
        '2' => &["╭─╮", "╭─╯", "╰──"],
        '3' => &["╭─╮", " ─┤", "╰─╯"],
        '4' => &["┬ ┬", "╰─┤", "  ┴"],
        '5' => &["╭──", "╰─╮", "╰─╯"],
        '6' => &["╭─ ", "├─╮", "╰─╯"],
        '7' => &["╭─╮", "  │", "  ┴"],
        '8' => &["╭─╮", "├─┤", "╰─╯"],
        '9' => &["╭─╮", "╰─┤", " ─╯"],
        '-' => &["   ", "───", "   "],
        '_' => &["   ", "   ", "───"],
        '.' => &[" ", " ", "▪"],
        '/' => &["  ╱", " ╱ ", "╱  "],
        '+' => &[" │ ", "─┼─", " │ "],
        ' ' => &["  ", "  ", "  "],
        // Anything else keeps its column rather than vanishing: a name that
        // silently drops a character is a name that identifies the wrong repo.
        _ => &[" ", "▪", " "],
    }
}

/// The five-row font: the same skeleton with the stems drawn out, so a letter is
/// a stroke of pipe with a shape rather than a shape made of corners. Two extra
/// rows is where the legibility actually is at this size -- the three-row face
/// spends every row on a junction, and junctions are what blur together.
///
/// Column widths match [`glyph_block`] letter for letter, including the
/// four-column `M`/`N` and the one-column `I`, so the two faces are
/// interchangeable in a layout that has already been measured.
///
/// `X` is the exception that proves the constraint: three columns cannot hold a
/// crossing over five rows, so it is set at three rows and centred in the cell.
/// It appears in no repo name I have seen, and a slightly small `X` is a better
/// failure than a ragged one.
fn glyph_tall(c: char) -> &'static [&'static str] {
    match c.to_ascii_uppercase() {
        'A' => &["╭─╮", "│ │", "├─┤", "│ │", "┴ ┴"],
        'B' => &["┬─╮", "│ │", "├─┤", "│ │", "┴─╯"],
        'C' => &["╭─╮", "│  ", "│  ", "│  ", "╰─╯"],
        'D' => &["┬─╮", "│ │", "│ │", "│ │", "┴─╯"],
        'E' => &["╭──", "│  ", "├─ ", "│  ", "╰──"],
        'F' => &["╭──", "│  ", "├─ ", "│  ", "┴  "],
        'G' => &["╭─╮", "│  ", "│ ┬", "│ │", "╰─╯"],
        'H' => &["┬ ┬", "│ │", "├─┤", "│ │", "┴ ┴"],
        'I' => &["┬", "│", "│", "│", "┴"],
        'J' => &[" ┬", " │", " │", " │", "╰╯"],
        'K' => &["┬ ╭", "│╭╯", "├┤ ", "│╰╮", "┴ ╰"],
        'L' => &["┬  ", "│  ", "│  ", "│  ", "╰──"],
        'M' => &["╭╮╭╮", "│╰╯│", "│  │", "│  │", "┴  ┴"],
        'N' => &["┬╮ ┬", "│╰╮│", "│ ╰┤", "│  │", "┴  ┴"],
        'O' => &["╭─╮", "│ │", "│ │", "│ │", "╰─╯"],
        'P' => &["┬─╮", "│ │", "├─╯", "│  ", "┴  "],
        'Q' => &["╭─╮ ", "│ │ ", "│ │ ", "│ │ ", "╰─┴╯"],
        'R' => &["┬─╮", "│ │", "├┬╯", "│╰╮", "┴ ╰"],
        'S' => &["╭─╮", "│  ", "╰─╮", "  │", "╰─╯"],
        'T' => &["╭┬╮", " │ ", " │ ", " │ ", " ┴ "],
        'U' => &["┬ ┬", "│ │", "│ │", "│ │", "╰─╯"],
        'V' => &["┬  ┬", "│  │", "│  │", "╰╮╭╯", " ╰╯ "],
        'W' => &["┬ ┬ ┬", "│ │ │", "│ │ │", "│ │ │", "╰─┴─╯"],
        'X' => &["   ", "╮ ╭", " ╳ ", "╯ ╰", "   "],
        'Y' => &["┬ ┬", "│ │", "╰┬╯", " │ ", " ┴ "],
        'Z' => &["╭──", "  ╱", " ╱ ", "╱  ", "╰──"],
        '0' => &["╭─╮", "│ │", "│╱│", "│ │", "╰─╯"],
        '1' => &["╭┬ ", " │ ", " │ ", " │ ", "─┴─"],
        '2' => &["╭─╮", "  │", " ╭╯", "╭╯ ", "╰──"],
        '3' => &["╭─╮", "  │", " ─┤", "  │", "╰─╯"],
        '4' => &["┬ ┬", "│ │", "╰─┤", "  │", "  ┴"],
        '5' => &["╭──", "│  ", "╰─╮", "  │", "╰─╯"],
        '6' => &["╭─╮", "│  ", "├─╮", "│ │", "╰─╯"],
        '7' => &["╭─╮", "  │", "  │", "  │", "  ┴"],
        '8' => &["╭─╮", "│ │", "├─┤", "│ │", "╰─╯"],
        '9' => &["╭─╮", "│ │", "╰─┤", "  │", "╰─╯"],
        '-' => &["   ", "   ", "───", "   ", "   "],
        '_' => &["   ", "   ", "   ", "   ", "───"],
        '.' => &[" ", " ", " ", " ", "▪"],
        '/' => &["  ╱", "  ╱", " ╱ ", "╱  ", "╱  "],
        '+' => &["   ", " │ ", "─┼─", " │ ", "   "],
        ' ' => &["  ", "  ", "  ", "  ", "  "],
        _ => &[" ", " ", "▪", " ", " "],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACES: [Face; 2] = [Face::Block, Face::Tall];
    const ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz0123456789-_./+ ?";

    /// Every glyph must be a rectangle of the face's height. `width` measures
    /// row 0 and the compositor stamps every row at the same column, so a ragged
    /// glyph would shift everything to its right on one row only -- a corruption
    /// that looks like a terminal bug rather than a font bug.
    #[test]
    fn every_glyph_is_a_rectangle_of_single_width_cells() {
        for face in FACES {
            for c in ALPHABET.chars() {
                let g = glyph(c, face);
                assert_eq!(g.len(), face.rows(), "{c:?} is the wrong height in {face:?}");
                let w = g[0].chars().count();
                assert!(w > 0, "{c:?} renders to nothing in {face:?}");
                for (r, row) in g.iter().enumerate() {
                    assert_eq!(row.chars().count(), w, "glyph {c:?} row {r} is ragged in {face:?}");
                }
            }
        }
    }

    #[test]
    fn render_agrees_with_width_and_keeps_its_rows_level() {
        for face in FACES {
            for name in ["agentwatch", "a", "my-repo_2", "WIDE NAME", "x/y.z"] {
                for kern in KERNS {
                    let rows = render(name, kern, face);
                    assert_eq!(rows.len(), face.rows());
                    for r in &rows {
                        assert_eq!(
                            r.chars().count(),
                            width(name, kern, face),
                            "row of {name:?} disagrees with width() at kern {kern} in {face:?}"
                        );
                    }
                }
            }
        }
    }

    /// The two faces must measure the same, or a header that picks its columns
    /// with one face and draws with the other overflows -- and the fallback from
    /// tall to block happens for want of *rows*, with the columns already fixed.
    #[test]
    fn the_two_faces_are_the_same_width() {
        for c in ALPHABET.chars() {
            assert_eq!(
                glyph_block(c)[0].chars().count(),
                glyph_tall(c)[0].chars().count(),
                "{c:?} is a different width in the two faces"
            );
        }
    }

    /// The condensed setting has to actually buy columns, or the fallback ladder
    /// has a rung that does nothing but cost a comparison.
    #[test]
    fn tightening_the_kerning_narrows_the_sign() {
        assert!(KERNS.windows(2).all(|p| p[0] > p[1]), "KERNS must go widest first");
        assert!(width("agentwatch", 0, Face::Tall) < width("agentwatch", 1, Face::Tall));
    }

    /// `M` and `N` buy their legibility with a fourth column, and `I` pays for
    /// it with a single one. If someone narrows `M`/`N` back to three to win the
    /// column back, the two letters collapse toward `╭─╮` again -- which is the
    /// misreading this font exists to stop -- so the cost is pinned deliberately.
    #[test]
    fn the_letters_that_need_room_get_it() {
        for face in FACES {
            assert_eq!(glyph('M', face)[0].chars().count(), 4, "M narrowed back to three");
            assert_eq!(glyph('N', face)[0].chars().count(), 4, "N narrowed back to three");
            assert_eq!(glyph('I', face)[0].chars().count(), 1, "I is the column budget");
        }
    }

    /// No two characters may draw the same sign. A collision is not a cosmetic
    /// fault: two repos whose names differ only in the colliding letter render
    /// as the same sign, and the header is then confidently wrong about which
    /// board you are looking at.
    #[test]
    fn no_two_characters_draw_the_same_sign() {
        use std::collections::HashMap;
        for face in FACES {
            let mut seen: HashMap<&'static [&'static str], char> = HashMap::new();
            for c in "abcdefghijklmnopqrstuvwxyz0123456789-_./+".chars() {
                let g = glyph(c, face);
                if let Some(prev) = seen.insert(g, c) {
                    panic!("{prev:?} and {c:?} draw the same glyph in {face:?}: {g:?}");
                }
            }
        }
    }

    #[test]
    fn case_does_not_change_the_drawing() {
        // The banner is always capitals -- a lowercase repo name must not render
        // as a different (or narrower) sign than the same name shouted.
        for face in FACES {
            assert_eq!(render("Agentwatch", 1, face), render("AGENTWATCH", 1, face));
            assert_eq!(width("sauron", 1, face), width("SAURON", 1, face));
        }
    }
}
