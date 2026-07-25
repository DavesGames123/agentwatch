//! The battle at the foot of Barad-dûr, sized by the swarm.
//!
//! Each working agent is one of Sauron's orcs on the field, so everything here
//! scales off that one number: how many fighters muster, how wide the front
//! sways, how fast feet move, whether arrows fly, whether the fallen start
//! piling up. It is the same figure the header's mountain burns on, read at the
//! other end of the screen.
//!
//! Two quiet states, and they are not the same quiet. With nothing running but
//! something still wanting a human, a single orc keeps the gate -- the war is
//! paused, not over. In repose the compositor does not call in here at all: the
//! field is bare, because an empty gate is the only way to say "nothing is
//! happening" that a lone shuffling orc does not contradict.
//!
//! grep targets:
//!   fn battle     -- muster both armies and animate the melee by ms
//!   fn place_free -- an elf, man, or hobbit charging in from the left
//!   fn place_orc  -- an orc pressing out of the gate

use super::paint::{put, tri, Cell};
use super::{BLOOD, FREE, HOT, ORC_C, STEEL};

/// Muster and animate the melee. The two lines meet at a front that sways with
/// the tide; how many fighters muster, how hard the line sways, how fast feet
/// move, whether arrows fly and bodies fall -- all climb with `armies`. Zero
/// working agents leaves an uneasy calm: one orc keeps the gate.
pub(super) fn battle(g: &mut [Vec<Cell>], eye_left: i32, armies: usize, ms: u64) {
    let field_l = 1i32;
    let field_r = eye_left - 2; // stop short of the tower's foot

    if armies == 0 {
        let stride = ((ms / 500) % 2) as usize; // a slow, bored shuffle
        place_orc(g, 2, (field_r - 1).max(field_l), stride);
        return;
    }

    let amp = (armies.min(5) as i32) / 2; // the tide swings wider in a bigger war
    // The lines meet nearer the gate than the far edge: the free host besieges
    // across most of the field, the orcs sally from the tower to meet them.
    let front = field_l + (field_r - field_l) * 3 / 5 + tri(ms, 2200, amp);
    // Feet quicken as the field fills; never so fast the stride blurs.
    let stride_ms = 260u64.saturating_sub(armies.min(6) as u64 * 24).max(90);

    let max_free = ((front - field_l) / 2).max(0) as usize;
    let max_orc = ((field_r - front) / 2).max(0) as usize;
    let n_free = armies.min(max_free);
    let n_orc = armies.min(max_orc);

    // The free peoples charge in from the left toward the front.
    for i in 0..n_free {
        let x = front - 2 - i as i32 * 2;
        if x < field_l {
            break;
        }
        let stride = ((ms / stride_ms + i as u64) % 2) as usize;
        let leap = (ms / (stride_ms * 2) + i as u64 * 3).is_multiple_of(7); // an occasional lunge
        place_free(g, if leap { 1 } else { 2 }, x, stride, i % 3 == 2);
    }
    // Orcs pour from the tower on the right, pressing toward the front.
    for i in 0..n_orc {
        let x = front + 1 + i as i32 * 2;
        if x > field_r {
            break;
        }
        let stride = ((ms / stride_ms + i as u64) % 2) as usize;
        let leap = (ms / (stride_ms * 2) + i as u64 * 5).is_multiple_of(7);
        place_orc(g, if leap { 1 } else { 2 }, x, stride);
    }

    // Where the lines meet, steel on steel -- a spark that jumps and changes shape.
    if n_free > 0 && n_orc > 0 {
        let ch = match (ms / 120) % 3 {
            0 => '*',
            1 => '+',
            _ => '×',
        };
        let row = if (ms / 120).is_multiple_of(2) { 1 } else { 2 };
        put(g, row, front, ch, HOT);
    }

    // Arrow volleys once the war is big enough: free shafts fly right, orc shafts
    // left, arcing along the top rows.
    if armies >= 3 {
        let span = (field_r - field_l).max(1) as u64;
        for j in 0..(1 + armies / 3) {
            let fx = field_l + ((ms / 70 + j as u64 * 13) % span) as i32;
            put(g, if (fx / 6) % 2 == 0 { 0 } else { 1 }, fx, '»', STEEL);
            let ox = field_r - ((ms / 70 + j as u64 * 29) % span) as i32;
            put(g, if (ox / 6) % 2 == 0 { 1 } else { 0 }, ox, '«', STEEL);
        }
    }

    // The fallen, once it is truly grim -- laid on the ground, never over a glyph.
    if armies >= 5 {
        let span = (field_r - field_l).max(1) as u64;
        for j in 0..armies / 2 {
            let x = field_l + ((j as u64 * 37 + 5) % span) as i32;
            if g[3][x as usize].0 == '▁' {
                put(g, 3, x, 'x', BLOOD);
            }
        }
    }
}

/// A free fighter -- elf, man, or (every third one) a hobbit -- charging right:
/// head, then a blade that swings with the stride.
fn place_free(g: &mut [Vec<Cell>], row: usize, x: i32, stride: usize, hobbit: bool) {
    put(g, row, x, if hobbit { 'ó' } else { 'Å' }, FREE);
    put(g, row, x + 1, if stride == 0 { '╱' } else { '╲' }, STEEL);
}

/// An orc pressing left out of the gate: a swung blade, then its head.
fn place_orc(g: &mut [Vec<Cell>], row: usize, x: i32, stride: usize) {
    put(g, row, x, if stride == 0 { '╲' } else { '╱' }, STEEL);
    put(g, row, x + 1, 'ø', ORC_C);
}
