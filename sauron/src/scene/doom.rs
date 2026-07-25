//! Orodruin -- Mount Doom -- smoking on the skyline beside the tower.
//!
//! The mountain is the header's second gauge, and it reads the same number the
//! war at the tower's foot does: one degree of heat per working agent. An idle
//! board gets a thin, cold wisp; a full swarm gets a thick plume, a white-hot
//! crater, embers thrown clear of the cone, and lava running down the flanks.
//! The war says the same thing, but the war is at the *bottom* of the screen and
//! only drawn when the terminal is big enough for the whole tower. The mountain
//! is in the header, which is always there and always where you are already
//! looking.
//!
//! The silhouette itself does not change with heat. Rock does not visibly cool,
//! and animating it would put motion on the one part of this panel that should
//! be the still thing everything else moves against.
//!
//! grep targets:
//!   const DOOM_W  -- the mountain's width, which the caller lays out around
//!   fn draw       -- silhouette, crater, plume, embers, runnels
//!   fn crater     -- the glyph and colour of the vent, by heat
//!   fn plume      -- the drifting smoke trail on the row above the peak

use super::paint::{put, stamp, tri, Cell};
use super::{World, ASH, ASH_C, ASH_D, EMBER, HOT, LAVA, SMOKE, SMOKE_D};
use ratatui::style::Style;

/// Columns the mountain occupies. Odd, so the vent sits on a whole column at
/// the exact centre (index 8) instead of straddling two.
pub(super) const DOOM_W: usize = 17;

/// The column of the vent, relative to the mountain's left edge. Left of centre
/// on purpose: a cone with the peak in the middle is an isoceles triangle, and
/// next to a tower of the same height that is what it reads as. Off-centre, with
/// a short steep flank and a long shallow one, it reads as a mountain.
const VENT: i32 = 7;

/// Stamp Orodruin onto the five-row header grid with its left edge at `left`.
/// Rows 1..=4 carry the cone; row 0 carries the plume, which is why the mountain
/// is drawn before the verse is clipped and after nothing at all -- it owns the
/// whole column band it sits in.
pub(super) fn draw(g: &mut [Vec<Cell>], left: i32, ms: u64, world: World) {
    // The rock cools with the board. It is the one part of the mountain that
    // does not move, so shifting its two tones is the only way it can carry the
    // idle state at all -- and an unlit Orodruin behind a lidded Eye is most of
    // what makes an idle header read as idle at a glance.
    let (fg, bg) = if world.repose { (ASH_D, ASH_C) } else { (ASH, ASH_D) };
    let ash = Style::default().fg(fg);
    let shade = Style::default().fg(bg);

    // The cone. Each row is stamped in two pieces, split at the vent, so the far
    // flank falls into shadow and the mountain reads as a solid rather than a
    // flat cut-out -- and because the vent is left of centre, the shadowed side
    // is the long one.
    for (row, l, lit, dark) in [
        (1usize, 5i32, "▗▟", "▙▖"),
        (2, 3, "▗▟███", "██▙▖"),
        (3, 1, "▗▟█████", "█████▙▖"),
        (4, 0, "▄▟██████", "███████▙▄"),
    ] {
        stamp(&mut g[row], left + l, lit, ash);
        stamp(&mut g[row], left + VENT + 1, dark, shade);
    }

    // The vent, in the notch at the top of the cone.
    let (ch, color) = crater(ms, world);
    put(g, 1, left + VENT, ch, color);

    plume(g, left + VENT, ms, world);

    // Lava down the flanks, once the mountain is truly working. Fixed channels,
    // because a runnel that wanders is a river and reads as noise; only whether
    // each is currently running changes with the clock.
    if world.working >= 4 {
        for (i, (row, dx, ch)) in [(2usize, -1i32, '╲'), (3, 2, '╱'), (3, -3, '╲')]
            .into_iter()
            .enumerate()
        {
            if (ms / 700 + i as u64 * 3) % 4 != 0 {
                put(g, row, left + VENT + dx, ch, LAVA);
            }
        }
    }
}

/// The vent glyph and its colour. Banked to a dull seam in repose, glowing at
/// any real muster, and pulsing white-hot once the swarm is large -- so the
/// mountain distinguishes "nothing running" from "nothing running, but something
/// wants you", which the plume length alone would not.
fn crater(ms: u64, world: World) -> (char, ratatui::style::Color) {
    match world {
        World { repose: true, .. } => ('▂', EMBER),
        World { working: 0, .. } => ('▄', LAVA),
        World { working, .. } if working >= 3 && (ms / 260) % 3 == 0 => ('▀', HOT),
        _ => ('▄', LAVA),
    }
}

/// The smoke, drifting off the peak along the row above it. There is only one
/// row of sky in the header, so the plume cannot rise -- it leans, trailing
/// downwind and reversing every nine seconds so the header never settles into a
/// static shape.
///
/// Heat lengthens the trail only as far as the mountain's own base is wide, and
/// then stops: past that the plume would drift into the verse on one side and
/// the Eye on the other, and a gauge that grows into its neighbours is a layout
/// bug wearing a weather effect. Beyond that cap, extra heat shows as *density*
/// -- thin smoke thickening from ░ to ▒ -- and as the embers below.
fn plume(g: &mut [Vec<Cell>], vent: i32, ms: u64, world: World) {
    let puffs = if world.repose {
        2
    } else {
        3 + world.working.min(3)
    };
    let thick = world.working >= 5;
    let dir = if (ms / 9_000) % 2 == 0 { 1 } else { -1 };
    for i in 0..puffs {
        let x = vent + dir * i as i32 + tri(ms + i as u64 * 380, 2_600, 1);
        let (ch, color) = match i {
            0 => ('▓', SMOKE),
            1 | 2 => ('▒', SMOKE),
            _ if thick => ('▒', SMOKE_D),
            _ => ('░', SMOKE_D),
        };
        put(g, 0, x, ch, color);
    }
    // Embers thrown clear of the cone, once it is erupting rather than smoking.
    for j in 0..(world.working / 3).min(3) {
        let x = vent + tri(ms + j as u64 * 610, 1_500, 3);
        put(g, 0, x, '·', HOT);
    }
}

#[cfg(test)]
mod tests {
    use super::super::paint::blank_row;
    use super::*;

    fn grid() -> Vec<Vec<Cell>> {
        (0..5).map(|_| blank_row(40)).collect()
    }
    fn text(g: &[Vec<Cell>], row: usize) -> String {
        g[row].iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn the_cone_widens_by_four_columns_a_row_and_sits_on_its_base() {
        let mut g = grid();
        draw(&mut g, 5, 0, World::default());
        let solid = |row: usize| text(&g, row).trim().chars().count();
        assert_eq!(solid(1), 5, "peak");
        assert_eq!(solid(2), 9);
        assert_eq!(solid(3), 14);
        assert_eq!(solid(4), DOOM_W, "base");
        // The peak is left of centre, so the flanks are not the same length.
        assert_ne!(VENT, DOOM_W as i32 / 2, "a centred vent makes it a triangle");
    }

    #[test]
    fn the_plume_thickens_with_the_working_count() {
        let puffs = |world| {
            let mut g = grid();
            draw(&mut g, 5, 0, world);
            text(&g, 0)
                .chars()
                .filter(|c| ['░', '▒', '▓'].contains(c))
                .count()
        };
        let idle = puffs(World { working: 0, repose: true });
        let quiet = puffs(World { working: 0, repose: false });
        let busy = puffs(World { working: 6, repose: false });
        assert!(idle < quiet, "an idle mountain should barely smoke");
        assert!(quiet < busy, "a working swarm should thicken the plume");
    }

    #[test]
    fn the_crater_is_banked_in_repose_and_white_hot_under_a_swarm() {
        assert_eq!(crater(0, World { working: 0, repose: true }).1, EMBER);
        assert_eq!(crater(0, World { working: 0, repose: false }).1, LAVA);
        // Somewhere in the pulse a big muster shows white-hot.
        let hot = (0..12u64).any(|k| crater(k * 260, World { working: 5, repose: false }).1 == HOT);
        assert!(hot, "a large swarm should pulse the vent white-hot");
    }

    #[test]
    fn runnels_only_run_on_a_hot_mountain() {
        // The vent itself glows lava-coloured at any heat, so look for the
        // runnel *glyphs* rather than the colour -- otherwise this passes on a
        // cold mountain and proves nothing.
        let has_lava = |working| {
            let mut g = grid();
            draw(&mut g, 5, 0, World { working, repose: false });
            (1..5).any(|r| {
                g[r].iter()
                    .any(|(ch, st)| st.fg == Some(LAVA) && ['╱', '╲'].contains(ch))
            })
        };
        assert!(!has_lava(1), "a smoking mountain does not run lava");
        assert!(has_lava(6), "an erupting one does");
    }

    #[test]
    fn the_mountain_never_draws_outside_its_own_columns() {
        // The caller lays the header out around DOOM_W; drifting smoke leaning
        // one column past the peak must not eat into the verse or the Eye.
        for ms in (0..40_000).step_by(313) {
            let mut g = grid();
            draw(&mut g, 10, ms, World { working: 9, repose: false });
            for row in 0..5 {
                for (i, (ch, _)) in g[row].iter().enumerate() {
                    if *ch != ' ' {
                        assert!(
                            (10..10 + DOOM_W).contains(&i),
                            "row {row} col {i} drawn outside the mountain at ms={ms}"
                        );
                    }
                }
            }
        }
    }
}
