//! The drawing primitives the rest of the scene is built from: a grid of styled
//! cells, transparent sprite stamping, and the collapse back into ratatui spans.
//!
//! Sprites are *stamped*, not blitted -- a space in a sprite leaves whatever is
//! behind it alone. That is what lets the walkers pass in front of the tower's
//! foot and the war instead of punching a hole through them, and it is why every
//! sprite in this module tree can be authored as a plain padded string.
//!
//! Every glyph the scene uses is unambiguous-width-1 (block, box-drawing, runic,
//! greek, latin, ascii), so a char index is a screen column and the whole thing
//! can be laid out with integer arithmetic. Adding a wide or ambiguous-width
//! glyph -- most emoji, most dingbats -- silently shifts everything to its right
//! on the terminals that render it wide.
//!
//! grep targets:
//!   type Cell / fn blank_row -- the grid a scene is composed on
//!   fn stamp                 -- overlay a sprite, spaces transparent
//!   fn put                   -- set a single cell, bounds-checked
//!   fn row_to_line           -- cells -> spans, runs of equal style merged
//!   fn tri                   -- integer triangle wave, for anything that sways
//!   fn clip                  -- truncate to a column count

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

pub(super) type Cell = (char, Style);

pub(super) fn blank_row(w: usize) -> Vec<Cell> {
    vec![(' ', Style::default()); w]
}

/// Overlay `s` at column `x`. Spaces in the sprite are transparent, so sprites
/// can be layered without clobbering what is behind their padding.
///
/// Takes a row *slice*, which is how the compositors clip a crossing: passing
/// `&mut row[..eye_left]` keeps a flying Nazgûl from being drawn over the Eye
/// without any of the sprites knowing the Eye exists.
pub(super) fn stamp(row: &mut [Cell], x: i32, s: &str, st: Style) {
    for (i, ch) in s.chars().enumerate() {
        if ch == ' ' {
            continue;
        }
        let c = x + i as i32;
        if c >= 0 && (c as usize) < row.len() {
            row[c as usize] = (ch, st);
        }
    }
}

/// Set one cell if it is on the grid -- the battle's whole vocabulary is single
/// glyphs, so this is all the drawing it needs.
pub(super) fn put(g: &mut [Vec<Cell>], row: usize, x: i32, ch: char, color: Color) {
    if row < g.len() && x >= 0 && (x as usize) < g[row].len() {
        g[row][x as usize] = (ch, Style::default().fg(color));
    }
}

/// Collapse a styled cell row into spans, merging runs of equal style so a line
/// is a handful of spans rather than one per cell.
pub(super) fn row_to_line(row: Vec<Cell>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut cur_st: Option<Style> = None;
    for (ch, st) in row {
        if cur_st != Some(st) {
            if let Some(s) = cur_st {
                spans.push(Span::styled(std::mem::take(&mut cur), s));
            }
            cur_st = Some(st);
        }
        cur.push(ch);
    }
    if let Some(s) = cur_st {
        spans.push(Span::styled(cur, s));
    }
    Line::from(spans)
}

/// A triangle wave in `[-amp, amp]` over `period`, integer-only so the sway of
/// the battle line -- and the lean of Orodruin's plume -- stays a pure function
/// of the clock.
pub(super) fn tri(ms: u64, period: u64, amp: i32) -> i32 {
    if amp == 0 || period == 0 {
        return 0;
    }
    let half = (period / 2) as i32;
    let p = (ms % period) as i32;
    let up = if p < half { p } else { period as i32 - p }; // 0..=half
    up * 2 * amp / half - amp
}

/// Truncate a string to `max` characters (every glyph here is single width, so
/// char count is column count).
pub(super) fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_leaves_the_background_showing_through_sprite_padding() {
        let mut row = blank_row(8);
        stamp(&mut row, 0, "▁▁▁▁▁▁▁▁", Style::default());
        stamp(&mut row, 2, "a b", Style::default());
        let text: String = row.iter().map(|(c, _)| c).collect();
        // The space in "a b" is transparent: the ground shows through it.
        assert_eq!(text, "▁▁a▁b▁▁▁");
    }

    #[test]
    fn stamp_clips_at_both_edges_of_its_slice() {
        let mut row = blank_row(4);
        stamp(&mut row, -2, "abcd", Style::default());
        stamp(&mut row, 3, "xyz", Style::default());
        let text: String = row.iter().map(|(c, _)| c).collect();
        assert_eq!(text, "cd x");
    }

    #[test]
    fn tri_stays_inside_its_amplitude_and_turns_around() {
        for ms in 0..4_000u64 {
            assert!((-2..=2).contains(&tri(ms, 1_000, 2)), "out of band at {ms}");
        }
        assert_eq!(tri(0, 1_000, 2), -2);
        assert_eq!(tri(500, 1_000, 2), 2);
        assert_eq!(tri(0, 1_000, 0), 0);
        assert_eq!(tri(0, 0, 3), 0);
    }

    #[test]
    fn row_to_line_merges_runs_of_equal_style() {
        let red = Style::default().fg(Color::Red);
        let mut row = blank_row(4);
        row[0] = ('a', red);
        row[1] = ('b', red);
        let line = row_to_line(row);
        // Two styles across the row -> two spans, not four.
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "ab");
    }
}
