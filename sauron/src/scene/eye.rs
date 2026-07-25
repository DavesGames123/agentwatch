//! The Eye itself, its poses, and the stone it is set in.
//!
//! The Eye has two registers, and which one it is in is the fastest-reading
//! signal on the screen. In **vigil** it burns: a flickering flame crown, a
//! hot-to-deep-red gradient across the fire, and a dark pupil that tracks
//! whatever crosses beneath it. In **repose** -- nothing running, nothing
//! wanting a human -- it *banks*: the crown gutters down to embers, the colours
//! drop to the cool end, and the lid closes over the fire for most of the cycle,
//! opening only now and then for a slow look around.
//!
//! Banking is a state change rather than a dimmer. An Eye that merely got
//! quieter would still read as watching, which is exactly the claim an idle
//! board should not be making.
//!
//! grep targets:
//!   enum Pose        -- Center / Blink / Wide / Lidded
//!   fn eye_tower     -- the five-row sprite for a pose, pupil, and register
//!   fn vigil_pose    -- the burning Eye's slow blink-and-glance schedule
//!   fn repose_pose   -- the banked Eye: lidded, stirring rarely
//!   fn shaft_row     -- one row of Barad-dûr's stone, walls and shadowed face
//!   fn slit          -- an arrow-slit window, lit in vigil and dark in repose

use ratatui::style::Style;

use super::paint::{blank_row, Cell};
use super::{EMBER, EYE_W, FLAME, FLARE, HOT, PUPIL, RED, RUNE, STONE, STONE_D};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Pose {
    Center,
    Blink,
    Wide,
    /// Banked: the lid rests over the fire and only a seam of ember shows. The
    /// idle register's resting state, and the one the board is in most of the
    /// time when nobody is working.
    Lidded,
}

/// The Eye set atop Barad-dûr: five rows, `EYE_W` wide. In vigil the Eye burns
/// -- an animated flame crown with white-hot tips and sparks, and a hot ->
/// orange -> deep-red gradient across the fire so it glows like an ember rather
/// than a flat block -- all framed by the stone tower. When `banked`, every band
/// drops one step toward the cool end and the crown stops licking. Pupil column
/// is 0..=6.
pub(super) fn eye_tower(pose: Pose, pupil: usize, flicker: usize, banked: bool) -> [Vec<Cell>; 5] {
    let bright = pose == Pose::Wide; // when it flares, every band shifts hotter
    let stone = Style::default().fg(STONE);
    let dark = Style::default().fg(PUPIL);
    let lid = Style::default().fg(RUNE);
    let hot = Style::default().fg(if banked { FLAME } else { HOT });
    let flare = Style::default().fg(match (banked, bright) {
        (true, _) => RED,
        (false, true) => HOT,
        (false, false) => FLARE,
    });
    let flame = Style::default().fg(match (banked, bright) {
        (true, _) => EMBER,
        (false, true) => FLARE,
        (false, false) => FLAME,
    });
    let red = Style::default().fg(match (banked, bright) {
        (true, _) => EMBER,
        (false, true) => FLAME,
        (false, false) => RED,
    });

    // The flame crown: three frames of licking fire with sparks flying off the
    // edges. Tips and sparks are white-hot; the body glows. Banked, the fire has
    // burned down to a low even bed of coals that does not lick at all -- one
    // frame, so the crown holds still while the vigil crown flickers.
    let crowns = ["  ▖▄▟███▙▄▗  ", "  ▗▟█▟█▙█▙▖  ", "  ▘▄▟█▙▟█▄▝  "];
    let banked_crown = "   ▁▂▄▄▄▂▁   ";
    let crown = if banked {
        banked_crown
    } else {
        crowns[flicker % crowns.len()]
    };
    let mut r0 = blank_row(EYE_W);
    for (i, ch) in crown.chars().enumerate() {
        if ch == ' ' {
            continue;
        }
        // Sparks (quadrant dots) and flame tips (▄▀) are hottest.
        let st = match ch {
            '▖' | '▗' | '▘' | '▝' | '▄' | '▀' => hot,
            _ => flare,
        };
        r0[i] = (ch, st);
    }

    // Eye upper/lower: red-hot outer corners fading to orange across the middle.
    let mut r1 = blank_row(EYE_W);
    r1[1] = ('█', stone);
    r1[2] = ('▟', red);
    r1[3..10].fill(('█', flame));
    r1[10] = ('▙', red);
    r1[11] = ('█', stone);
    let mut r3 = blank_row(EYE_W);
    r3[1] = ('█', stone);
    r3[2] = ('▜', red);
    r3[3..10].fill(('█', flame));
    r3[10] = ('▛', red);
    r3[11] = ('█', stone);

    // The Eye's middle. The fire glows brightest toward the rim and stays calm
    // in the middle, so the dark pupil -- the focal point -- is never washed out
    // by a hot core sitting right where it lives.
    let mut r2 = blank_row(EYE_W);
    r2[1] = ('█', stone);
    r2[11] = ('█', stone);
    r2[2] = ('▐', red);
    r2[10] = ('▌', red);
    match pose {
        Pose::Blink => r2[3..10].fill(('━', lid)),
        // Lidded is not a blink held longer: the lid is heavy (a half block
        // rather than a hairline) and a seam of banked fire shows beneath it, so
        // a paused frame still reads as sleeping rather than as mid-blink.
        Pose::Lidded => r2[3..10].fill(('▄', Style::default().fg(EMBER))),
        _ => {
            for i in 0..7usize {
                let col = 3 + i;
                let heat = match (col as i32 - 6).abs() {
                    0 | 1 => flame, // calm orange hugging the pupil
                    _ => flare,     // a gentle glow toward the rim; the white-hot
                                    // lives up in the crown, not next to the pupil
                };
                let is_pupil = if pose == Pose::Wide {
                    (2..=4).contains(&i)
                } else {
                    i == pupil.min(6)
                };
                r2[col] = ('█', if is_pupil { dark } else { heat });
            }
        }
    }

    // The tower foot, flaring out where it meets the ground.
    let mut r4 = blank_row(EYE_W);
    for (i, ch) in "▟███████████▙".chars().enumerate() {
        r4[i] = (ch, stone);
    }

    [r0, r1, r2, r3, r4]
}

/// The burning Eye: a long, level, watchful stare with only rare, slow motion --
/// a couple of blinks and one held glance across the whole idle stretch, so it
/// broods rather than darts about. `t` is the phase within `super::PERIOD`; the
/// walk owns 17s..24.5s, so the events here sit in the calm before it.
pub(super) fn vigil_pose(t: u64) -> (Pose, usize) {
    match t {
        5_000..=5_250 => (Pose::Blink, 3),
        10_500..=11_599 => (Pose::Center, 0), // a slow glance left, held
        11_600..=11_850 => (Pose::Blink, 3),  // and a blink as it returns
        _ => (Pose::Center, 3),               // the level stare (centre of 7)
    }
}

/// The banked Eye: lidded almost the whole cycle, cracking open once for a slow
/// look at the empty plain before settling again. `t` is the phase within
/// `super::REPOSE_PERIOD`, and the opening deliberately does not coincide with a
/// crossing -- the Eye is not watching the Fellowship go by, it is dozing
/// through it.
pub(super) fn repose_pose(t: u64) -> (Pose, usize) {
    match t {
        12_400..12_650 => (Pose::Blink, 3),  // the lid parts
        12_650..13_500 => (Pose::Center, 3), // a slow, brief look out
        13_500..13_750 => (Pose::Blink, 3),  // and closes again
        _ => (Pose::Lidded, 3),
    }
}

/// A single shaft segment: the two stone walls of Barad-dûr with a shadowed inner
/// face between them. The walls sit at the same columns the Eye's stone frame
/// does (1 and 11), so whatever the row's width, the crown flows into the shaft.
pub(super) fn shaft_row(width: usize) -> Vec<Cell> {
    let stone = Style::default().fg(STONE);
    let inner = Style::default().fg(STONE_D);
    let mut r = blank_row(width);
    r[1] = ('█', stone);
    r[11] = ('█', stone);
    for c in r.iter_mut().take(11).skip(2) {
        *c = ('▓', inner);
    }
    r
}

/// The arrow-slit window on shaft row `r`, if that row has one: its column and
/// its style. A slit every third row, alternating side, each pulsing on its own
/// phase -- so the black spire has a few lit eyes of its own without any of them
/// wandering. In repose every slit is dark: the garrison has stood down, and a
/// tower with lights still burning in it would undercut the lidded Eye above.
pub(super) fn slit(r: usize, ms: u64, banked: bool) -> Option<(usize, Style)> {
    if r % 3 != 1 {
        return None;
    }
    let col = if (r / 3) % 2 == 0 { 4 } else { 8 };
    let lit = !banked && (ms / 320 + r as u64) % 5 < 3;
    Some((col, Style::default().fg(if lit { FLAME } else { STONE_D })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(r: &[Cell]) -> String {
        r.iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn the_banked_eye_is_lidded_almost_the_whole_cycle() {
        // A handful of samples across the repose cycle: only the brief stir near
        // the end is open, and everything else is shut.
        assert_eq!(repose_pose(0).0, Pose::Lidded);
        assert_eq!(repose_pose(6_000).0, Pose::Lidded);
        assert_eq!(repose_pose(12_000).0, Pose::Lidded);
        assert_eq!(repose_pose(13_000).0, Pose::Center);
        assert_eq!(repose_pose(12_500).0, Pose::Blink);
    }

    #[test]
    fn a_lidded_eye_shows_no_pupil_and_a_banked_crown_does_not_flicker() {
        let lidded = eye_tower(Pose::Lidded, 3, 0, true);
        let mid = row_text(&lidded[2]);
        assert!(mid.contains('▄'), "the heavy lid is missing: {mid}");
        assert!(!mid.contains("███"), "fire still showing under the lid: {mid}");
        // The banked crown is one frame: it holds still while the flicker moves.
        for f in 0..3 {
            assert_eq!(
                row_text(&eye_tower(Pose::Lidded, 3, f, true)[0]),
                row_text(&eye_tower(Pose::Lidded, 3, 0, true)[0]),
                "a banked crown must not lick"
            );
        }
        // ...whereas the burning one does.
        assert_ne!(
            row_text(&eye_tower(Pose::Center, 3, 0, false)[0]),
            row_text(&eye_tower(Pose::Center, 3, 1, false)[0]),
        );
    }

    #[test]
    fn the_pupil_tracks_its_column_when_the_eye_is_open() {
        // The pupil is the one dark cell in the Eye's middle row; moving the
        // requested column moves it.
        let col_of = |p: usize| {
            let rows = eye_tower(Pose::Center, p, 0, false);
            rows[2]
                .iter()
                .position(|(_, st)| st.fg == Some(PUPIL))
                .expect("an open eye has a pupil")
        };
        assert!(col_of(0) < col_of(3));
        assert!(col_of(3) < col_of(6));
    }

    #[test]
    fn slits_light_in_vigil_and_stay_dark_in_repose() {
        // Row 1 carries a slit; row 0 does not.
        assert!(slit(0, 0, false).is_none());
        let lit_somewhere = (0..40u64).any(|k| {
            slit(1, k * 320, false).map(|(_, st)| st.fg) == Some(Some(FLAME))
        });
        assert!(lit_somewhere, "a watching tower should show a lit slit");
        let ever_lit_banked = (0..40u64).any(|k| {
            slit(1, k * 320, true).map(|(_, st)| st.fg) == Some(Some(FLAME))
        });
        assert!(!ever_lit_banked, "a sleeping tower must keep its slits dark");
    }
}
