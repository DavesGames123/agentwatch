//! Mordor: the Eye of Sauron, Orodruin smoking beside it, the tower between
//! them, and the war at its foot.
//!
//! Everything here is a pure function of elapsed milliseconds and a [`World`] --
//! what the agents are doing right now -- so nothing is ticked or stored between
//! frames and every frame is reproducible in a test off the clock alone.
//!
//! Two ways it draws, chosen by `ui::draw` from the terminal size:
//!
//!   - the compact five-line crown ([`scene`]), self-contained: the Eye atop a
//!     stub of tower, the mountain smoking to its left, and the ground beneath
//!     them both, which is where anyone crossing walks;
//!   - the whole tower, when there is room -- [`crown`] caps the header with the
//!     Eye and the mountain (the Eye's foot swapped for a shaft-cap so the stone
//!     keeps going), then [`tower_shaft`] descends Barad-dûr down the right
//!     column of the list region, and [`battle_ground`] lands its flared foot on
//!     a full-width horizon where the war happens.
//!
//! **Mordor answers to the board.** Each *working* agent is one orc on the field
//! and one degree of heat in the mountain, so the war swells and the plume
//! thickens as more of the swarm runs.
//!
//! And when nothing is running and nothing wants a human -- `World::repose` --
//! the scene changes *state* rather than merely going quiet. The Eye banks and
//! lids, the tower's slit-windows go dark, the war stops entirely, and the free
//! peoples get the plain: a different company crossing every fourteen seconds,
//! two of them airborne. An idle board should be recognisable as idle from
//! across the room without reading a word, and a scene that only got slower
//! would not manage that -- slow still reads as watching.
//!
//! grep targets:
//!   struct World       -- what the agents are doing; every drawing takes one
//!   fn scene           -- the compact five-line header (no tower below)
//!   fn crown           -- the header when the whole tower is drawn below it
//!   fn tower_shaft     -- the descending stone shaft (right column of the list)
//!   fn battle_ground   -- the flared foot and the full-width war at its base
//!   fn doom_left       -- where the mountain fits, and when it does not
//!   fn place_eye       -- stamp the Eye sprite into a header grid
//!   fn place_verse     -- the engraving, clipped to whatever room is left
//!   mod eye / doom / cast / war / runes / paint

mod cast;
mod doom;
mod eye;
mod paint;
mod runes;
mod war;

use ratatui::style::{Color, Style};
use ratatui::text::Line;

use crate::ui::{DIM, EMBER, FLAME, FLARE, RUNE};
use cast::Lane;
use doom::DOOM_W;
use eye::Pose;
use paint::{blank_row, clip, row_to_line, stamp, Cell};

/// Rows the crown occupies in the header (not counting the status line above it).
pub const HEIGHT: u16 = 5;

/// Width of the right-hand column the descending shaft claims. Equal to
/// `EYE_MARGIN`, so the shaft rect's left edge lands exactly under the Eye and
/// the whole tower reads as one piece across the header/list seam.
pub const TOWER_W: u16 = EYE_MARGIN as u16;

/// Rows the war at the tower's foot claims, along the bottom of the list region:
/// the flared foot lands on the top row, then two rows of melee, then the ground.
pub const BASE_H: u16 = 4;

// Width of the Eye sprite in cells, and how far its left edge sits from the
// right margin. Every glyph used is unambiguous-width-1 (block, box-drawing,
// runic, latin, ascii), so a char index equals a screen column.
const EYE_W: usize = 13;
const EYE_MARGIN: usize = 15;

/// Columns between the mountain and the Eye. Enough that they read as two things
/// on a skyline rather than one silhouette.
const DOOM_GAP: i32 = 3;
/// Columns the engraving needs before the mountain is worth drawing at all:
/// enough for the longest line -- "the doors of durin, lord of moria" -- to
/// render whole. In practice that puts the mountain on terminals 70 columns and
/// wider.
///
/// The mountain takes its columns from the verse, so a smaller number here does
/// not buy a mountain on a narrow terminal -- it buys a mountain next to
/// "speak, friend, and", which reads as a bug in the engraving rather than as a
/// deliberately compact header. A test pins this against `runes::VERSES`, so
/// adding a longer line moves the threshold instead of silently truncating one.
const MIN_VERSE: i32 = 33;

/// What the agents are doing, which is the only input the scene has besides the
/// clock.
///
/// Two fields rather than one count because "nothing is running" and "nothing is
/// happening" are different claims, and only the second one licenses putting the
/// Eye to sleep. A board with three sessions waiting on your acknowledgement has
/// nothing running, but something is very much outstanding -- banking the Eye
/// there would have the chrome contradict the amber badge two rows above it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct World {
    /// Agents currently working: the muster on the field, and the heat in the
    /// mountain.
    pub working: usize,
    /// Nothing running, nothing delegated, and nothing waiting on a human -- the
    /// same condition the header states as "all caught up".
    pub repose: bool,
}

// --- palette -------------------------------------------------------------------
//
// Chrome only. None of these may borrow a hue from the status palette in `ui`:
// a colour that means something on a card must not turn up on a mountain.

const RING: Color = Color::Rgb(255, 214, 96); // the One Ring's glint
const HOBBIT: Color = Color::Rgb(150, 190, 140); // Frodo & Sam
const GOLLUM_C: Color = Color::Rgb(150, 172, 150); // the pale creeping thing
const GROUND: Color = Color::Rgb(66, 70, 80); // the horizon the walkers cross
const STONE: Color = Color::Rgb(92, 96, 112); // the black tower of Barad-dûr
const STONE_D: Color = Color::Rgb(58, 62, 74); // the shaft's shadowed inner face
const HOT: Color = Color::Rgb(255, 236, 150); // white-hot: flame tips and sparks
const RED: Color = Color::Rgb(200, 54, 20); // deep red: the cool outer edge of the fire
const PUPIL: Color = Color::Rgb(34, 14, 10); // near-black: the cute pupil, the focal point

// Orodruin.
const ASH: Color = Color::Rgb(78, 70, 72); // the lit face of the cone
const ASH_D: Color = Color::Rgb(52, 47, 50); // the side away from the fire
const ASH_C: Color = Color::Rgb(40, 37, 41); // both, once the mountain has cooled
const LAVA: Color = Color::Rgb(228, 88, 24); // the vent, and what runs from it
const SMOKE: Color = Color::Rgb(104, 96, 100); // the plume, thick off the peak
const SMOKE_D: Color = Color::Rgb(72, 67, 72); // and thin where it has drifted

// The war at the foot of the tower.
const FREE: Color = Color::Rgb(176, 206, 150); // the free peoples: elves, men, hobbits
const ORC_C: Color = Color::Rgb(122, 158, 74); // sauron's orcs, pouring from the gate
const STEEL: Color = Color::Rgb(178, 188, 200); // blades in the line, arrowheads in flight
const BLOOD: Color = Color::Rgb(150, 40, 30); // the fallen, once the field turns grim

// The companies that cross an idle plain.
const WHITE: Color = Color::Rgb(228, 230, 234); // Gandalf and Shadowfax
const ELF: Color = Color::Rgb(150, 205, 190); // Legolas
const DWARF: Color = Color::Rgb(198, 150, 96); // Gimli
const ENT: Color = Color::Rgb(120, 150, 96); // Treebeard
const WRAITH: Color = Color::Rgb(126, 106, 158); // the Nazgûl and its fell beast
const EAGLE: Color = Color::Rgb(198, 172, 120); // Gwaihir and his kin
const MERRY: Color = Color::Rgb(226, 190, 96); // Tom Bombadil's yellow boots
const SPIDER: Color = Color::Rgb(126, 104, 116); // Shelob

// --- layout --------------------------------------------------------------------

/// Where Orodruin sits in a `w`-wide header, or `None` if it does not fit.
///
/// The mountain takes its columns from the engraving, so on a narrow terminal
/// something has to give, and it is the mountain: a verse clipped to five runes
/// reads as a rendering bug, while no mountain at all reads as no mountain.
fn doom_left(eye_left: i32) -> Option<i32> {
    let left = eye_left - DOOM_GAP - DOOM_W as i32;
    (left >= MIN_VERSE + 2).then_some(left)
}

/// Stamp the Eye into a five-row header grid with its left edge at `eye_left`.
/// `foot` picks which bottom row it gets: the flared foot that lands on ground
/// (the compact scene), or a shaft segment so the stone descends into the list
/// region below (the full tower).
fn place_eye(grid: &mut [Vec<Cell>], eye_left: i32, pose: Pose, pupil: usize, ms: u64, banked: bool, foot: bool) {
    let flicker = ((ms / 220) % 3) as usize;
    let mut rows = eye::eye_tower(pose, pupil, flicker, banked);
    if !foot {
        rows[4] = eye::shaft_row(EYE_W);
    }
    for (r, row) in rows.into_iter().enumerate() {
        for (i, (ch, st)) in row.into_iter().enumerate() {
            if ch == ' ' {
                continue;
            }
            let c = eye_left + i as i32;
            if c >= 0 && (c as usize) < grid[r].len() {
                grid[r][c as usize] = (ch, st);
            }
        }
    }
}

/// The engraving, top-left: runic script on row 1, faint gloss on row 0. Clipped
/// at whatever stands to its right -- the mountain when there is one, the Eye
/// when there is not -- so it never runs into either.
fn place_verse(grid: &mut [Vec<Cell>], ms: u64, stop: i32) {
    let room = (stop.max(2) as usize).saturating_sub(2);
    let (words, gloss) = runes::verse(ms);
    stamp(&mut grid[1], 1, &clip(&runes::runic(words), room), Style::default().fg(RUNE));
    stamp(&mut grid[0], 1, &clip(gloss, room), Style::default().fg(DIM));
}

/// The Eye's pose when nothing in particular is crossing beneath it: banked and
/// lidded in repose, the long level stare otherwise.
fn resting_pose(ms: u64, world: World) -> (Pose, usize) {
    if world.repose {
        eye::repose_pose(ms % cast::REPOSE_PERIOD)
    } else {
        eye::vigil_pose(ms % cast::PERIOD)
    }
}

// --- the compact header --------------------------------------------------------

/// Compose the five header lines for a terminal `width` at time `ms`.
pub fn scene(width: usize, ms: u64, world: World) -> Vec<Line<'static>> {
    let w = width.max(20);
    let mut grid: Vec<Vec<Cell>> = (0..5).map(|_| blank_row(w)).collect();
    let eye_left = w.saturating_sub(EYE_MARGIN) as i32;
    let eye_col = eye_left + 6; // the pupil's screen column

    // The ground.
    let ground_style = Style::default().fg(GROUND);
    for cell in grid[4].iter_mut() {
        *cell = ('▁', ground_style);
    }

    // Orodruin, behind everything: the Eye is clipped over it, the verse stops
    // short of it, and anyone crossing walks in front of its foot.
    let doom_x = doom_left(eye_left);
    if let Some(x) = doom_x {
        doom::draw(&mut grid, x, ms, world);
    }

    // Who, if anyone, is crossing -- and what the Eye makes of it. In vigil the
    // Eye follows the lead figure and flares wide as they pass beneath; banked,
    // it sleeps through the whole procession, which is the point of the state.
    let cross = cast::crossing(ms, world.repose);
    let (pose, pupil) = match &cross {
        Some(c) if !world.repose => {
            let base = cast::walk_base(w, c.prog, c.right);
            let frac = (base as f64 / w as f64).clamp(0.0, 1.0);
            let pose = if (eye_col - base).abs() < 4 { Pose::Wide } else { Pose::Center };
            (pose, ((frac * 6.0).round() as usize).min(6))
        }
        _ => resting_pose(ms, world),
    };
    place_eye(&mut grid, eye_left, pose, pupil, ms, world.repose, true);

    // The company, in the foreground, so it passes in front of the tower's foot
    // instead of vanishing behind it. A flyer is clipped to the columns left of
    // the Eye: the air lane runs straight through the Eye's face, and a fell
    // beast punching a hole in it looks like a redraw bug, not an overflight.
    if let Some(c) = &cross {
        let base = cast::walk_base(w, c.prog, c.right);
        let leg = cast::stride(c.party, ms);
        match c.party.lane {
            Lane::Ground => (c.party.draw)(&mut grid[4], base, c.right, leg),
            Lane::Air => {
                let lim = (eye_left.max(0) as usize).min(w);
                (c.party.draw)(&mut grid[3][..lim], base, c.right, leg);
            }
        }
    }

    place_verse(&mut grid, ms, doom_x.unwrap_or(eye_left));
    grid.into_iter().map(row_to_line).collect()
}

// --- the whole tower: crown, shaft, and the war at its foot --------------------

/// The header when the whole tower is drawn below it: the Eye, the mountain, and
/// the verse exactly as [`scene`] builds them, but with the flared foot swapped
/// for a shaft segment so the stone keeps descending into [`tower_shaft`]
/// instead of stopping. No ground and no company here -- those live at the foot.
pub fn crown(width: usize, ms: u64, world: World) -> Vec<Line<'static>> {
    let w = width.max(20);
    let mut grid: Vec<Vec<Cell>> = (0..5).map(|_| blank_row(w)).collect();
    let eye_left = w.saturating_sub(EYE_MARGIN) as i32;

    let doom_x = doom_left(eye_left);
    if let Some(x) = doom_x {
        doom::draw(&mut grid, x, ms, world);
    }

    let (pose, pupil) = resting_pose(ms, world);
    place_eye(&mut grid, eye_left, pose, pupil, ms, world.repose, false);
    place_verse(&mut grid, ms, doom_x.unwrap_or(eye_left));
    grid.into_iter().map(row_to_line).collect()
}

/// The tower shaft: `height` rows of Barad-dûr's stone, `TOWER_W` wide, meant for
/// the right column of the list region directly beneath the crown. Arrow-slit
/// windows glow at fixed rows and pulse with `ms` -- except in repose, when the
/// garrison has stood down and every slit is dark.
pub fn tower_shaft(height: usize, ms: u64, world: World) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(height);
    for r in 0..height {
        let mut row = eye::shaft_row(TOWER_W as usize);
        if let Some((col, st)) = eye::slit(r, ms, world.repose) {
            row[col] = ('▪', st);
        }
        out.push(row_to_line(row));
    }
    out
}

/// The tower's foot and the ground around it: `BASE_H` full-width rows for the
/// very bottom of the list region. The flared foot lands on the top row, the
/// ground runs the whole width along the bottom, and between them either a war
/// rages (sized by `world.working`) or -- in repose -- nothing does, and the
/// plain belongs to whoever is crossing it.
pub fn battle_ground(width: usize, ms: u64, world: World) -> Vec<Line<'static>> {
    let w = width.max(20);
    let n = BASE_H as usize;
    let mut g: Vec<Vec<Cell>> = (0..n).map(|_| blank_row(w)).collect();
    let eye_left = w.saturating_sub(EYE_MARGIN) as i32;

    // The horizon everyone stands on.
    let ground = Style::default().fg(GROUND);
    for cell in g[n - 1].iter_mut() {
        *cell = ('▁', ground);
    }
    // The flared foot of the tower, landing on the ground beneath the shaft.
    stamp(&mut g[0], eye_left, "▟███████████▙", Style::default().fg(STONE));

    // No war in repose -- not even the lone bored orc that keeps the gate when
    // the swarm is merely paused. That orc is the difference between "nothing is
    // running" and "nothing is happening", and it has to be absent for the
    // second one to read.
    if !world.repose {
        war::battle(&mut g, eye_left, world.working, ms);
    }

    // Whoever is crossing, drawn last so they slip in front of the war (and any
    // fallen) rather than into it.
    if let Some(c) = cast::crossing(ms, world.repose) {
        let base = cast::walk_base(w, c.prog, c.right);
        let leg = cast::stride(c.party, ms);
        let row = match c.party.lane {
            Lane::Ground => n - 1,
            Lane::Air => n - 2,
        };
        (c.party.draw)(&mut g[row], base, c.right, leg);
    }

    g.into_iter().map(row_to_line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIGIL: World = World { working: 4, repose: false };
    const PAUSED: World = World { working: 0, repose: false };
    const REPOSE: World = World { working: 0, repose: true };

    fn line_width(l: &Line) -> usize {
        l.spans.iter().map(|s| s.content.chars().count()).sum()
    }
    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }
    fn text(ls: &[Line]) -> String {
        ls.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn the_header_is_five_lines_and_never_overflows_width() {
        for &w in &[24usize, 40, 52, 80, 120] {
            for ms in [0u64, 6_000, 13_000, 20_000, 23_000, 46_000, 91_000, 999_999] {
                for world in [VIGIL, PAUSED, REPOSE] {
                    for build in [scene, crown] {
                        let s = build(w, ms, world);
                        assert_eq!(s.len(), 5, "w={w} ms={ms} {world:?}");
                        for l in &s {
                            assert!(
                                line_width(l) <= w,
                                "overflow at w={w} ms={ms} {world:?}: {}",
                                line_text(l)
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_mountain_shows_when_there_is_room_and_yields_to_the_verse_when_not() {
        // Wide: cone and plume both present.
        let wide = text(&scene(100, 0, VIGIL));
        assert!(wide.contains('▟'), "no mountain on a wide header");
        assert!(wide.contains('▓') || wide.contains('▒'), "no plume: {wide}");
        // Narrow: the mountain is what goes, and the engraving survives.
        assert_eq!(doom_left(20), None);
        let narrow = text(&scene(40, 0, VIGIL));
        assert!(!narrow.contains('▒') && !narrow.contains('░'), "smoke on a narrow header");
        assert!(narrow.contains('ᛗ') || narrow.contains('ᛖ'), "the verse was lost: {narrow}");
    }

    #[test]
    fn the_mountains_threshold_still_clears_the_longest_verse() {
        // MIN_VERSE is the promise that a header showing a mountain also shows a
        // whole engraving. Adding a longer line has to move the threshold, and
        // this is the only thing that will say so.
        for (words, gloss) in runes::VERSES {
            assert!(
                gloss.chars().count() as i32 <= MIN_VERSE,
                "gloss {gloss:?} is longer than MIN_VERSE"
            );
            assert!(
                runes::runic(words).chars().count() as i32 <= MIN_VERSE,
                "runes for {words:?} are longer than MIN_VERSE"
            );
        }
    }

    #[test]
    fn the_verse_never_runs_into_the_mountain() {
        // The engraving is clipped at the mountain's left edge, not the Eye's.
        let eye_left = 100i32 - EYE_MARGIN as i32;
        let doom_x = doom_left(eye_left).expect("100 columns fits a mountain");
        for ms in [0u64, 30_000, 60_000] {
            for row in scene(100, ms, VIGIL).iter().take(2) {
                let cells: Vec<char> = line_text(row).chars().collect();
                for (i, ch) in cells.iter().enumerate() {
                    if *ch != ' ' && (i as i32) < doom_x {
                        continue;
                    }
                    // Past the mountain's left edge nothing of the verse remains:
                    // runes are the only thing the verse draws.
                    assert!(
                        !('ᚠ'..='ᛰ').contains(ch),
                        "a rune at column {i} collides with the mountain at ms={ms}"
                    );
                }
            }
        }
    }

    #[test]
    fn repose_banks_the_eye_and_vigil_does_not() {
        // The banked Eye rests behind a heavy lid; the watching one shows fire.
        let banked = text(&scene(80, 0, REPOSE));
        assert!(banked.contains('▄'), "the lid is missing from a banked eye");
        let watching = text(&scene(80, 0, PAUSED));
        assert!(watching.contains("███"), "the watching eye has no fire in it");
        // ...and the difference survives a whole cycle, not just one frame: the
        // banked eye is shut for most of it.
        let shut = (0..cast::REPOSE_PERIOD)
            .step_by(250)
            .filter(|&ms| eye::repose_pose(ms).0 == eye::Pose::Lidded)
            .count();
        let total = (0..cast::REPOSE_PERIOD).step_by(250).count();
        assert!(shut * 10 >= total * 8, "a banked eye should be shut most of the time");
    }

    #[test]
    fn an_idle_plain_has_no_war_on_it_at_all() {
        // Not even the lone gate-keeper that a merely-paused swarm leaves behind.
        for ms in (0..8 * cast::REPOSE_PERIOD).step_by(311) {
            let field = text(&battle_ground(80, ms, REPOSE));
            assert!(!field.contains('ø'), "an orc on an idle field at ms={ms}");
            assert!(!field.contains('»') && !field.contains('«'), "arrows at ms={ms}");
        }
        // A paused swarm still keeps the gate -- that is a different claim.
        assert!(text(&battle_ground(80, 0, PAUSED)).contains('ø'));
    }

    #[test]
    fn the_whole_cast_walks_an_idle_field_and_only_the_fellowship_walks_a_busy_one() {
        // Sample a full rotation; every company should have shown up.
        let seen: Vec<&str> = cast::CAST
            .iter()
            .filter(|p| {
                (0..8 * cast::REPOSE_PERIOD).step_by(151).any(|ms| {
                    cast::crossing(ms, true).map(|c| c.party.name) == Some(p.name)
                })
            })
            .map(|p| p.name)
            .collect();
        assert_eq!(seen.len(), cast::CAST.len(), "an idle plain skipped a company");

        // Gandalf rides an idle field...
        let rides = (0..8 * cast::REPOSE_PERIOD)
            .step_by(151)
            .any(|ms| text(&battle_ground(80, ms, REPOSE)).contains('╤'));
        assert!(rides, "Shadowfax never crossed the idle plain");
        // ...and never a working one, whatever the muster.
        for ms in (0..4 * cast::PERIOD).step_by(151) {
            assert!(
                !text(&battle_ground(80, ms, VIGIL)).contains('╤'),
                "Gandalf rode through a battle at ms={ms}"
            );
        }
    }

    #[test]
    fn the_fellowship_still_crosses_a_field_at_war() {
        // The one company that keeps its old schedule, whatever the muster.
        for &working in &[0usize, 4, 12] {
            let world = World { working, repose: false };
            let walk = line_text(&battle_ground(80, 20_000, world)[BASE_H as usize - 1]);
            assert!(walk.contains('ó'), "Frodo missing (working={working}): {walk}");
            assert!(walk.contains('ô'), "Sam missing (working={working}): {walk}");
            assert!(walk.contains('*'), "the Ring's glint is missing: {walk}");
        }
        // ...and mid-idle the war has the field to itself.
        let idle = line_text(&battle_ground(80, 6_000, VIGIL)[BASE_H as usize - 1]);
        assert!(!idle.contains('ó'), "a hobbit crossed the war off-schedule: {idle}");
    }

    #[test]
    fn a_flyer_never_draws_over_the_eye() {
        // The air lane runs at the Eye's height in the compact header. Sample the
        // Nazgûl's whole crossing and check the Eye's columns are untouched.
        let w = 80usize;
        let eye_left = (w - EYE_MARGIN) as i32;
        let nazgul_slot = cast::CAST.iter().position(|p| p.name == "nazgul").unwrap() as u64;
        let start = nazgul_slot * cast::REPOSE_PERIOD;
        for ms in (start..start + 4_000).step_by(53) {
            let rows = scene(w, ms, REPOSE);
            let air: Vec<char> = line_text(&rows[3]).chars().collect();
            for (i, ch) in air.iter().enumerate() {
                if (i as i32) >= eye_left && *ch != ' ' {
                    // Only the Eye's own glyphs may live here.
                    assert!(
                        !"╱╲▄▀".contains(*ch) || i as i32 >= eye_left + EYE_W as i32,
                        "a wing at column {i} overwrote the Eye at ms={ms}"
                    );
                }
            }
        }
    }

    #[test]
    fn tower_shaft_fills_its_column_with_stone_and_sleeps_in_repose() {
        for &h in &[1usize, 4, 12, 40] {
            for world in [VIGIL, REPOSE] {
                let s = tower_shaft(h, 3_000, world);
                assert_eq!(s.len(), h, "h={h}");
                for l in &s {
                    assert_eq!(line_width(l), TOWER_W as usize, "shaft width off at h={h}");
                }
                assert!(line_text(&s[0]).contains('█'), "shaft has no wall");
            }
        }
        // A watching tower lights its slits somewhere in the pulse; a sleeping
        // one never does.
        let lit = |world| {
            (0..40u64).any(|k| {
                tower_shaft(6, k * 320, world)
                    .iter()
                    .any(|l| l.spans.iter().any(|s| s.style.fg == Some(FLAME)))
            })
        };
        assert!(lit(VIGIL));
        assert!(!lit(REPOSE));
    }

    #[test]
    fn battle_ground_lands_the_foot_on_a_full_width_ground() {
        for &w in &[24usize, 40, 80, 120] {
            for &working in &[0usize, 1, 3, 6, 20] {
                let world = World { working, repose: false };
                let b = battle_ground(w, 20_000, world);
                assert_eq!(b.len(), BASE_H as usize, "w={w} working={working}");
                for l in &b {
                    assert!(line_width(l) <= w, "base overflow w={w} working={working}");
                }
                assert_eq!(line_width(&b[BASE_H as usize - 1]), w, "ground not full width");
                assert!(line_text(&b[0]).contains('▙'), "no tower foot at w={w}");
            }
        }
    }

    #[test]
    fn the_war_and_the_mountain_both_swell_with_the_running_count() {
        let orcs = |working| -> usize {
            battle_ground(80, 20_000, World { working, repose: false })
                .iter()
                .map(|l| line_text(l).matches('ø').count())
                .sum()
        };
        assert!(orcs(6) > orcs(2), "a bigger swarm should field more orcs");
        assert!(orcs(2) > orcs(0), "an idle gate should be quieter than a fought one");

        // Arrow volleys only fly once the battle is big enough.
        let has_arrows = |working| {
            battle_ground(80, 20_000, World { working, repose: false })
                .iter()
                .any(|l| line_text(l).contains('»') || line_text(l).contains('«'))
        };
        assert!(!has_arrows(1), "no volleys in a skirmish");
        assert!(has_arrows(6), "a full assault looses arrows");

        // And the header's gauge tracks the same number.
        let smoke = |working| {
            text(&scene(100, 0, World { working, repose: false }))
                .chars()
                .filter(|c| ['░', '▒', '▓'].contains(c))
                .count()
        };
        assert!(smoke(6) > smoke(0), "the mountain should smoke harder under a swarm");
    }
}

