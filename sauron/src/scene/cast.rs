//! Who crosses the ground beneath the tower, and when.
//!
//! There are two schedules, and the difference between them is the whole idea.
//!
//! While the swarm is **working**, the plain belongs to Sauron. One company
//! still steals across it -- Frodo, Sam, and Gollum, once every twenty-six
//! seconds, keeping to the ground -- and that is all. It is a rare, small thing
//! happening at the edge of a war.
//!
//! In **repose** -- nothing running, nothing waiting on you -- the war is over
//! and the field is open. A different company crosses every fourteen seconds,
//! rotating through the whole cast: the Fellowship, Gandalf on Shadowfax,
//! Legolas and Gimli, Treebeard, a Nazgûl on its fell beast, the Eagles, Tom
//! Bombadil, Shelob. Two of them fly, so the empty sky gets used too.
//!
//! Rotating rather than randomising is deliberate: the scene is a pure function
//! of the clock, which is what makes every frame of it reproducible in a test.
//! A random cast would be untestable and, worse, would sometimes show the same
//! party three times running.
//!
//! grep targets:
//!   struct Party / static CAST -- the roster, in rotation order
//!   enum Lane                  -- ground or air, which row the party uses
//!   fn crossing                -- who is on the field at `ms`, and how far along
//!   fn walk_base               -- lead column for a crossing's progress
//!   fn draw_fellowship         -- Frodo, Sam, Gollum, and the Ring
//!   const PERIOD / REPOSE_PERIOD -- the two schedules

use ratatui::style::{Modifier, Style};

use super::paint::{stamp, Cell};
use super::{
    DIM, DWARF, EAGLE, ELF, ENT, GOLLUM_C, HOBBIT, MERRY, RING, SPIDER, WHITE, WRAITH,
};

/// Vigil timeline: one crossing per 26s, over ~7.5s, a long calm the rest.
pub(super) const PERIOD: u64 = 26_000;
const WALK_START: u64 = 17_000;
const WALK_END: u64 = 24_500;

/// Repose timeline: a company every 14s. Long enough that the slowest of them
/// (Treebeard, who takes twelve seconds to cross) still gets the field to
/// himself, short enough that an idle board is never empty for long.
pub(super) const REPOSE_PERIOD: u64 = 14_000;

/// Which row of the ground band a party uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Lane {
    /// The horizon itself -- everyone who walks.
    Ground,
    /// The row above it. Only the fell beast and the Eagles, and only in repose,
    /// which is why no compositor has to reconcile a flyer with an arrow volley.
    Air,
}

/// One company that crosses the field.
pub(super) struct Party {
    /// The handle the tests address a company by. Nothing renders it -- a name
    /// on screen would spoil the joke -- but a schedule test that has to say
    /// "the fourth entry" instead of "treebeard" stops being readable, and stops
    /// catching a reordering of the roster, which is exactly the change most
    /// likely to break `crossing`.
    #[allow(dead_code)]
    pub name: &'static str,
    pub lane: Lane,
    /// How long the crossing takes. A horse is quicker than an Ent, and the
    /// difference in pace is most of what identifies them at two glyphs wide.
    pub cross_ms: u64,
    /// Draw the company with its lead figure at column `base`, heading `right`,
    /// on stride phase `leg`.
    pub draw: fn(&mut [Cell], i32, bool, usize),
}

/// The roster, in rotation order. The Fellowship is first because it is also the
/// only company that crosses during a vigil -- `crossing` reaches for `CAST[0]`
/// by name there, so reordering this list would change what walks past a working
/// swarm, not just the order of the idle parade.
pub(super) static CAST: [Party; 8] = [
    Party { name: "fellowship", lane: Lane::Ground, cross_ms: 7_500, draw: draw_fellowship },
    Party { name: "shadowfax", lane: Lane::Ground, cross_ms: 3_400, draw: draw_shadowfax },
    Party { name: "nazgul", lane: Lane::Air, cross_ms: 3_800, draw: draw_nazgul },
    Party { name: "legolas-gimli", lane: Lane::Ground, cross_ms: 6_500, draw: draw_hunters },
    Party { name: "treebeard", lane: Lane::Ground, cross_ms: 12_000, draw: draw_treebeard },
    Party { name: "eagles", lane: Lane::Air, cross_ms: 5_000, draw: draw_eagles },
    Party { name: "bombadil", lane: Lane::Ground, cross_ms: 8_000, draw: draw_bombadil },
    Party { name: "shelob", lane: Lane::Ground, cross_ms: 5_500, draw: draw_shelob },
];

/// A company on the field: who, how far across (0..1), and which way.
pub(super) struct Crossing {
    pub party: &'static Party,
    pub prog: f64,
    pub right: bool,
}

/// Who is crossing at `ms`, if anyone. Kept apart from the drawing so the
/// schedule -- the part that decides whether an idle board looks alive -- is
/// unit-testable without rendering a single cell.
pub(super) fn crossing(ms: u64, repose: bool) -> Option<Crossing> {
    if repose {
        let cyc = ms / REPOSE_PERIOD;
        let t = ms % REPOSE_PERIOD;
        let party = &CAST[(cyc as usize) % CAST.len()];
        (t < party.cross_ms).then(|| Crossing {
            party,
            prog: t as f64 / party.cross_ms as f64,
            right: cyc.is_multiple_of(2),
        })
    } else {
        let cyc = ms / PERIOD;
        let t = ms % PERIOD;
        (WALK_START..WALK_END).contains(&t).then(|| Crossing {
            party: &CAST[0],
            prog: (t - WALK_START) as f64 / (WALK_END - WALK_START) as f64,
            right: cyc.is_multiple_of(2),
        })
    }
}

/// The lead figure's column at crossing progress `prog` on a `w`-wide ground,
/// heading `right` or left.
///
/// The path starts and ends off the field by more than the longest company is
/// long -- Frodo to Gollum is eleven cells, plus the Ring two ahead -- so a
/// crossing begins with an empty field and ends with one. A tighter overshoot
/// popped Gollum out of existence a step short of the edge.
pub(super) fn walk_base(w: usize, prog: f64, right: bool) -> i32 {
    let span = (w + 26) as f64;
    if right {
        (-12.0 + prog * span).floor() as i32
    } else {
        (w as f64 + 12.0 - prog * span).floor() as i32
    }
}

/// The stride phase for `party` at `ms`.
///
/// Derived from how fast the company crosses rather than stored per party: a
/// gallop and an Ent's plod would otherwise share a footfall rate, and at two
/// glyphs wide the pace of the feet is most of what tells them apart.
pub(super) fn stride(party: &Party, ms: u64) -> usize {
    ((ms / (60 + party.cross_ms / 40)) % 2) as usize
}

/// Column `k` cells behind the lead figure, whichever way it is heading.
fn behind(base: i32, right: bool, k: i32) -> i32 {
    if right {
        base - k
    } else {
        base + k
    }
}

// --- the companies ------------------------------------------------------------
//
// Every sprite is one row and a handful of single-width glyphs, authored twice:
// once heading right and once mirrored. Two cells is enough to read as a figure
// when the stride alternates, and the palette does the rest of the identifying.

/// Frodo leading, Sam a step back, Gollum skulking further behind, and the Ring
/// glinting just ahead of Frodo.
fn draw_fellowship(row: &mut [Cell], base: i32, right: bool, leg: usize) {
    let hob = Style::default().fg(HOBBIT);
    let gol = Style::default().fg(GOLLUM_C);
    let ring = Style::default().fg(RING).add_modifier(Modifier::BOLD);
    let (fr, sm, go) = match (leg % 2, right) {
        (0, true) => ("ó╱", "ô╱", "o╮"),
        (1, true) => ("ó╲", "ô╲", "o╯"),
        (0, false) => ("╲ó", "╲ô", "╭o"),
        _ => ("╱ó", "╱ô", "╰o"),
    };
    stamp(row, base, fr, hob);
    stamp(row, behind(base, right, 3), sm, hob);
    stamp(row, behind(base, right, 9), go, gol);
    stamp(row, behind(base, right, -2), "*", ring);
}

/// Gandalf on Shadowfax: the fastest thing on the ground, white, with the staff
/// throwing a spark ahead of the gallop.
fn draw_shadowfax(row: &mut [Cell], base: i32, right: bool, leg: usize) {
    let white = Style::default().fg(WHITE).add_modifier(Modifier::BOLD);
    let spark = Style::default().fg(RING);
    let body = match (leg % 2, right) {
        (0, true) => "ó╤╱╲",
        (1, true) => "ó╤╲╱",
        (0, false) => "╲╱╤ó",
        _ => "╱╲╤ó",
    };
    stamp(row, if right { base } else { base - 3 }, body, white);
    // Two cells clear of the horse's leading edge -- the sprite is four wide, so
    // the usual one-ahead offset put the staff-spark on Shadowfax's own flank.
    stamp(row, if right { base + 5 } else { base - 5 }, "·", spark);
}

/// Legolas with his bow, Gimli three steps behind with the axe.
fn draw_hunters(row: &mut [Cell], base: i32, right: bool, leg: usize) {
    let elf = Style::default().fg(ELF);
    let dwarf = Style::default().fg(DWARF);
    let (le, gi) = match (leg % 2, right) {
        (0, true) => ("ó)", "°╕"),
        (1, true) => ("ó⌐", "°╘"),
        (0, false) => ("(ó", "╒°"),
        _ => ("¬ó", "╘°"),
    };
    stamp(row, base, le, elf);
    stamp(row, behind(base, right, 3), gi, dwarf);
}

/// Treebeard: a walking crown of branches, and by a wide margin the slowest
/// thing that crosses.
fn draw_treebeard(row: &mut [Cell], base: i32, right: bool, leg: usize) {
    let ent = Style::default().fg(ENT);
    let body = match (leg % 2, right) {
        (0, true) => "Ψ╱",
        (1, true) => "Ψ╲",
        (0, false) => "╲Ψ",
        _ => "╱Ψ",
    };
    stamp(row, base, body, ent);
}

/// A Nazgûl on its fell beast, wings beating, the only thing in the sky that is
/// Sauron's -- and it crosses only when Sauron is asleep, which is the joke.
fn draw_nazgul(row: &mut [Cell], base: i32, _right: bool, leg: usize) {
    let wraith = Style::default().fg(WRAITH).add_modifier(Modifier::BOLD);
    // The beast is symmetric, so the sprite does not mirror -- only the wingbeat
    // changes, which at this size reads as flight either way.
    stamp(row, base, if leg % 2 == 0 { "╱▄╲" } else { "╲▀╱" }, wraith);
}

/// Gwaihir and two of his kin, strung out in a line.
fn draw_eagles(row: &mut [Cell], base: i32, right: bool, leg: usize) {
    let eagle = Style::default().fg(EAGLE);
    for i in 0..3i32 {
        let wing = if (leg + i as usize) % 2 == 0 { "╱╲" } else { "╲╱" };
        stamp(row, behind(base, right, i * 4), wing, eagle);
    }
}

/// Tom Bombadil, in no hurry, singing as he goes.
fn draw_bombadil(row: &mut [Cell], base: i32, right: bool, leg: usize) {
    let merry = Style::default().fg(MERRY).add_modifier(Modifier::BOLD);
    let song = Style::default().fg(DIM);
    let body = match (leg % 2, right) {
        (0, true) => "ô╱",
        (1, true) => "ô╲",
        (0, false) => "╲ô",
        _ => "╱ô",
    };
    stamp(row, base, body, merry);
    stamp(row, behind(base, right, 3), "~", song);
}

/// Shelob, skittering low across the ground.
fn draw_shelob(row: &mut [Cell], base: i32, _right: bool, leg: usize) {
    let spider = Style::default().fg(SPIDER);
    stamp(row, base, if leg % 2 == 0 { "╳o╳" } else { "╱o╲" }, spider);
}

#[cfg(test)]
mod tests {
    use super::super::paint::blank_row;
    use super::*;

    fn crossed(ms: u64, repose: bool) -> Option<&'static str> {
        crossing(ms, repose).map(|c| c.party.name)
    }

    #[test]
    fn a_vigil_field_only_ever_sees_the_fellowship() {
        // Across four full vigil periods, nobody but the Fellowship crosses, and
        // only inside the walk window.
        for ms in (0..4 * PERIOD).step_by(97) {
            match crossed(ms, false) {
                None => {}
                Some(name) => assert_eq!(name, "fellowship", "at ms={ms}"),
            }
        }
        assert_eq!(crossed(6_000, false), None, "mid-idle the field is empty");
        assert_eq!(crossed(20_000, false), Some("fellowship"));
    }

    #[test]
    fn repose_rotates_the_whole_cast_and_alternates_direction() {
        // One company per cycle, in roster order, so the parade never repeats a
        // party back to back.
        for (i, p) in CAST.iter().enumerate() {
            let mid = i as u64 * REPOSE_PERIOD + 100;
            assert_eq!(crossed(mid, true), Some(p.name), "cycle {i}");
        }
        // ...and it wraps.
        assert_eq!(crossed(CAST.len() as u64 * REPOSE_PERIOD + 100, true), Some(CAST[0].name));
        // Direction flips every crossing.
        assert!(crossing(100, true).unwrap().right);
        assert!(!crossing(REPOSE_PERIOD + 100, true).unwrap().right);
    }

    #[test]
    fn every_company_gets_the_field_to_itself_and_then_leaves_it_empty() {
        for (i, p) in CAST.iter().enumerate() {
            let start = i as u64 * REPOSE_PERIOD;
            assert!(p.cross_ms < REPOSE_PERIOD, "{} outlasts its slot", p.name);
            // Present at the start of its slot, gone by the end of it.
            assert_eq!(crossed(start + 10, true), Some(p.name));
            assert_eq!(crossed(start + REPOSE_PERIOD - 10, true), None, "{}", p.name);
        }
    }

    #[test]
    fn an_idle_board_is_never_quiet_for_long() {
        // The longest gap between companies, sampled across the whole rotation.
        let mut gap = 0u64;
        let mut worst = 0u64;
        for ms in (0..CAST.len() as u64 * REPOSE_PERIOD).step_by(100) {
            if crossing(ms, true).is_some() {
                gap = 0;
            } else {
                gap += 100;
                worst = worst.max(gap);
            }
        }
        assert!(worst <= 11_000, "an idle field sat empty for {worst}ms");
    }

    #[test]
    fn every_company_draws_something_in_both_directions() {
        for p in CAST.iter() {
            for right in [true, false] {
                for leg in 0..2 {
                    let mut row = blank_row(40);
                    (p.draw)(&mut row, 18, right, leg);
                    let drawn = row.iter().filter(|(c, _)| *c != ' ').count();
                    assert!(drawn >= 2, "{} drew {drawn} cells (right={right})", p.name);
                }
            }
        }
    }

    #[test]
    fn a_company_fully_enters_and_fully_leaves_the_field() {
        // At either end of the crossing the whole company is off the field; in
        // the middle it is on it. Checked on the widest sprite (the Fellowship,
        // eleven cells from Gollum to the Ring).
        let cells = |prog: f64, right: bool| {
            let mut row = blank_row(60);
            draw_fellowship(&mut row, walk_base(60, prog, right), right, 0);
            row.iter().filter(|(c, _)| *c != ' ').count()
        };
        for right in [true, false] {
            assert_eq!(cells(0.0, right), 0, "company visible before it sets out");
            assert!(cells(0.5, right) >= 6, "company missing mid-crossing");
            assert_eq!(cells(1.0, right), 0, "company still on the field at the end");
        }
    }

    #[test]
    fn two_companies_fly_so_the_sky_is_not_wasted() {
        let air: Vec<_> = CAST.iter().filter(|p| p.lane == Lane::Air).map(|p| p.name).collect();
        assert_eq!(air, vec!["nazgul", "eagles"]);
    }
}
