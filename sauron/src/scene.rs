//! The animated Eye of Sauron and its tower. Everything here is a pure function
//! of elapsed milliseconds (and, for the war below, the running-agent count), so
//! nothing has to be ticked or stored between frames and the whole thing is
//! testable off the clock alone.
//!
//! Two ways it draws, chosen by `ui::draw` from the terminal size:
//!
//!   - the compact five-line crown (`scene`), self-contained: the Eye atop a
//!     stub of tower with Frodo/Sam/Gollum crossing the little ground beneath it;
//!   - the whole tower, when there is room -- `crown` caps the header with the
//!     Eye (its foot swapped for a shaft-cap so the stone keeps going), then
//!     `tower_shaft` descends Barad-dûr down the right column of the list region,
//!     and `battle_ground` lands its flared foot on a full-width horizon where a
//!     battle rages. Each *working* agent is one of Sauron's orcs on the field,
//!     so the war swells -- more fighters, arrow volleys, the fallen -- as more
//!     of the swarm runs at once. From time to time Frodo, Sam, and Gollum steal
//!     across this field too, on the same schedule they cross the compact scene.
//!
//! It also shows an engraving of Sindarin in runic script (real Unicode runes,
//! U+16A0, which render out of the box -- true Tengwar lives in the private-use
//! area and needs a font, so this stands in for it) with a faint gloss.
//!
//! grep targets:
//!   fn scene           -- the compact five-line crown (fallback), with walkers
//!   fn crown           -- the header Eye when the whole tower is drawn below it
//!   fn tower_shaft      -- the descending stone shaft (right column of the list)
//!   fn battle_ground    -- the flared foot + the full-width war at its base
//!   fn battle           -- musters both armies and animates the melee by ms
//!   fn eye_tower        -- the Eye sprite for a pose + pupil column
//!   fn walker_sprites  -- Frodo / Sam / Gollum, facing left or right
//!   fn place_walkers   -- stamp the procession onto a ground row (scene + war)
//!   fn runic           -- latin -> Elder Futhark transliteration
//!   const VERSES       -- the engraved Sindarin lines
//!   enum Pose / fn idle_pose / walk timing consts

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::{DIM, FLAME, FLARE, RUNE};

/// Rows the crown occupies in the header (not counting the status line above it).
pub const HEIGHT: u16 = 5;

/// Width of the right-hand column the descending shaft claims. Equal to
/// `EYE_MARGIN`, so the shaft rect's left edge lands exactly under the Eye and
/// the whole tower reads as one piece across the header/list seam.
pub const TOWER_W: u16 = EYE_MARGIN as u16;

/// Rows the war at the tower's foot claims, along the bottom of the list region:
/// the flared foot lands on the top row, then two rows of melee, then the ground.
pub const BASE_H: u16 = 4;

const RING: Color = Color::Rgb(255, 214, 96); // the One Ring's glint
const HOBBIT: Color = Color::Rgb(150, 190, 140); // Frodo & Sam
const GOLLUM_C: Color = Color::Rgb(150, 172, 150); // the pale creeping thing
const GROUND: Color = Color::Rgb(66, 70, 80); // the horizon the walkers cross
const STONE: Color = Color::Rgb(92, 96, 112); // the black tower of Barad-dûr
const STONE_D: Color = Color::Rgb(58, 62, 74); // the shaft's shadowed inner face
const HOT: Color = Color::Rgb(255, 236, 150); // white-hot: flame tips and sparks
const RED: Color = Color::Rgb(200, 54, 20); // deep red: the cool outer edge of the fire
const PUPIL: Color = Color::Rgb(34, 14, 10); // near-black: the cute pupil, the focal point

// The war at the foot of the tower.
const FREE: Color = Color::Rgb(176, 206, 150); // the free peoples: elves, men, hobbits
const ORC_C: Color = Color::Rgb(122, 158, 74); // sauron's orcs, pouring from the gate
const STEEL: Color = Color::Rgb(178, 188, 200); // blades in the line, arrowheads in flight
const BLOOD: Color = Color::Rgb(150, 40, 30); // the fallen, once the field turns grim

// Timeline: one walk per 26s, crossing over ~7.5s, a long calm idle the rest.
const PERIOD: u64 = 26_000;
const WALK_START: u64 = 17_000;
const WALK_END: u64 = 24_500;

// Width of the Eye sprite in cells, and how far its left edge sits from the
// right margin. Every glyph used is unambiguous-width-1 (block, box-drawing,
// runic, latin, ascii), so a char index equals a screen column.
const EYE_W: usize = 13;
const EYE_MARGIN: usize = 15;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pose {
    Center,
    Blink,
    Wide,
}

type Cell = (char, Style);

fn blank_row(w: usize) -> Vec<Cell> {
    vec![(' ', Style::default()); w]
}

/// Overlay `s` at column `x`. Spaces in the sprite are transparent, so sprites
/// can be layered without clobbering what is behind their padding.
fn stamp(row: &mut [Cell], x: i32, s: &str, st: Style) {
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

/// Collapse a styled cell row into spans, merging runs of equal style so a line
/// is a handful of spans rather than one per cell.
fn row_to_line(row: Vec<Cell>) -> Line<'static> {
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

/// The Eye set atop Barad-dûr: five rows, `EYE_W` wide. The Eye burns -- an
/// animated flame crown with white-hot tips and sparks, and a hot -> orange ->
/// deep-red gradient across the fire so it glows like an ember rather than a
/// flat block -- all framed by the stone tower. Pupil column is 0..=6.
fn eye_tower(pose: Pose, pupil: usize, flicker: usize) -> [Vec<Cell>; 5] {
    let bright = pose == Pose::Wide; // when it flares, every band shifts hotter
    let stone = Style::default().fg(STONE);
    let dark = Style::default().fg(PUPIL);
    let lid = Style::default().fg(RUNE);
    let hot = Style::default().fg(HOT);
    let flare = Style::default().fg(if bright { HOT } else { FLARE });
    let flame = Style::default().fg(if bright { FLARE } else { FLAME });
    let red = Style::default().fg(if bright { FLAME } else { RED });

    // The flame crown: three frames of licking fire with sparks flying off the
    // edges. Tips and sparks are white-hot; the body glows.
    let crowns = ["  ▖▄▟███▙▄▗  ", "  ▗▟█▟█▙█▙▖  ", "  ▘▄▟█▙▟█▄▝  "];
    let mut r0 = blank_row(EYE_W);
    for (i, ch) in crowns[flicker % crowns.len()].chars().enumerate() {
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

/// Frodo, Sam, Gollum as two-cell sprites, mid-stride, facing right or left.
fn walker_sprites(leg: usize, right: bool) -> (&'static str, &'static str, &'static str) {
    match (leg % 2, right) {
        (0, true) => ("ó╱", "ô╱", "o╮"),
        (1, true) => ("ó╲", "ô╲", "o╯"),
        (0, false) => ("╲ó", "╲ô", "╭o"),
        _ => ("╱ó", "╱ô", "╰o"),
    }
}

/// The lead hobbit's column at walk progress `prog` (0..1) on a `w`-wide ground,
/// heading `right` or left. The path runs from just off one edge to just off the
/// other (a 16-cell overshoot so the procession fully enters and leaves).
fn walk_base(w: usize, prog: f64, right: bool) -> i32 {
    let span = (w + 16) as f64;
    if right {
        (-12.0 + prog * span).floor() as i32
    } else {
        (w as f64 + 4.0 - prog * span).floor() as i32
    }
}

/// Stamp the procession onto one ground `row`: Frodo at `base` leading, Sam a
/// step back, Gollum skulking further behind, the Ring glinting just ahead of
/// Frodo -- all mirrored when the walk heads left. Drawn last so the walkers pass
/// in front of whatever stone or war stands behind them.
fn place_walkers(row: &mut [Cell], base: i32, right: bool, leg: usize) {
    let (fr, sm, go) = walker_sprites(leg, right);
    let hob = Style::default().fg(HOBBIT);
    let gol = Style::default().fg(GOLLUM_C);
    let ring = Style::default().fg(RING).add_modifier(Modifier::BOLD);
    if right {
        stamp(row, base, fr, hob);
        stamp(row, base - 3, sm, hob);
        stamp(row, base - 9, go, gol);
        stamp(row, base + 2, "*", ring);
    } else {
        stamp(row, base, fr, hob);
        stamp(row, base + 3, sm, hob);
        stamp(row, base + 9, go, gol);
        stamp(row, base - 2, "*", ring);
    }
}

/// The idle Eye: a long, level, watchful stare with only rare, slow motion -- a
/// couple of blinks and one held glance across the whole idle stretch, so it
/// broods rather than darts about. `t` is the phase within `PERIOD`; the walk
/// owns 17s..24.5s, so the events here sit in the calm before it.
fn idle_pose(t: u64) -> (Pose, usize) {
    match t {
        5_000..=5_250 => (Pose::Blink, 3),
        10_500..=11_599 => (Pose::Center, 0), // a slow glance left, held
        11_600..=11_850 => (Pose::Blink, 3),  // and a blink as it returns
        _ => (Pose::Center, 3),               // the level stare (centre of 7)
    }
}

/// Whether a walk is happening at phase `t`, and if so its progress 0..1 and
/// whether it heads rightward. Kept separate so the schedule is unit-testable.
fn walk_at(t: u64, cyc: u64) -> Option<(f64, bool)> {
    if (WALK_START..WALK_END).contains(&t) {
        let prog = (t - WALK_START) as f64 / (WALK_END - WALK_START) as f64;
        Some((prog, cyc.is_multiple_of(2)))
    } else {
        None
    }
}

/// The engraved lines: canonical Sindarin (the West-gate of Moria), shown in
/// runes with a faint English gloss. Real Elvish words, real script glyphs.
const VERSES: [(&str, &str); 3] = [
    ("pedo mellon a minno", "speak, friend, and enter"),
    ("ennyn durin aran moria", "the doors of durin, lord of moria"),
    ("celebrimbor teithant", "celebrimbor drew these signs"),
];

fn verse(ms: u64) -> (&'static str, &'static str) {
    // A slow engraving: each line lingers for half a minute.
    VERSES[((ms / 30_000) % VERSES.len() as u64) as usize]
}

/// Latin -> Elder Futhark. `th` becomes the thorn rune; unknown chars pass
/// through so punctuation and the like survive.
fn runic(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == 't' && chars[i + 1] == 'h' {
            out.push('ᚦ');
            i += 2;
            continue;
        }
        out.push(rune(chars[i]));
        i += 1;
    }
    out
}

fn rune(c: char) -> char {
    match c.to_ascii_lowercase() {
        'a' => 'ᚨ', 'b' => 'ᛒ', 'c' => 'ᚲ', 'd' => 'ᛞ', 'e' => 'ᛖ', 'f' => 'ᚠ',
        'g' => 'ᚷ', 'h' => 'ᚺ', 'i' => 'ᛁ', 'j' => 'ᛃ', 'k' => 'ᚲ', 'l' => 'ᛚ',
        'm' => 'ᛗ', 'n' => 'ᚾ', 'o' => 'ᛟ', 'p' => 'ᛈ', 'q' => 'ᚲ', 'r' => 'ᚱ',
        's' => 'ᛊ', 't' => 'ᛏ', 'u' => 'ᚢ', 'v' => 'ᚹ', 'w' => 'ᚹ', 'x' => 'ᛉ',
        'y' => 'ᛃ', 'z' => 'ᛉ', ' ' => '᛬', other => other,
    }
}

/// Truncate a string to `max` characters (runes are single width, so char count
/// is column count).
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Compose the five scene lines for a terminal `width` at time `ms`.
pub fn scene(width: usize, ms: u64) -> Vec<Line<'static>> {
    let w = width.max(20);
    let mut grid: Vec<Vec<Cell>> = (0..5).map(|_| blank_row(w)).collect();

    // The ground.
    let ground_style = Style::default().fg(GROUND);
    for cell in grid[4].iter_mut() {
        *cell = ('▁', ground_style);
    }

    let t = ms % PERIOD;
    let cyc = ms / PERIOD;
    let leg = ((ms / 180) % 2) as usize;
    let flicker = ((ms / 220) % 3) as usize; // the fire licks; the pupil stays calm
    let eye_left = w.saturating_sub(EYE_MARGIN) as i32;
    let eye_col = eye_left + 6; // the pupil's screen column

    // Resolve the Eye's pose, and remember the procession (if any) so it can be
    // drawn in front of the tower foot rather than behind it.
    let mut walk: Option<(i32, bool)> = None;
    let (pose, pupil) = if let Some((prog, right)) = walk_at(t, cyc) {
        let base = walk_base(w, prog, right);
        walk = Some((base, right));
        // The Eye follows the lead hobbit and flares wide as they pass beneath.
        let frac = (base as f64 / w as f64).clamp(0.0, 1.0);
        let pupil = (frac * 6.0).round() as usize;
        let pose = if (eye_col - base).abs() < 4 {
            Pose::Wide
        } else {
            Pose::Center
        };
        (pose, pupil.min(6))
    } else {
        idle_pose(t)
    };

    // The tower and its Eye occupy all five rows; the foot lands on the ground.
    for (r, row) in eye_tower(pose, pupil, flicker).into_iter().enumerate() {
        for (i, (ch, st)) in row.into_iter().enumerate() {
            if ch == ' ' {
                continue;
            }
            let c = eye_left + i as i32;
            if c >= 0 && (c as usize) < w {
                grid[r][c as usize] = (ch, st);
            }
        }
    }

    // The procession, in the foreground, so the hobbits pass in front of the
    // tower's foot instead of vanishing behind it.
    if let Some((base, right)) = walk {
        place_walkers(&mut grid[4], base, right, leg);
    }

    // The engraving, top-left: runic script on row 1, faint gloss on row 0. Clip
    // both so they never run into the Eye on a narrow terminal.
    let (words, gloss) = verse(ms);
    let room = eye_left.max(2) as usize - 2;
    stamp(&mut grid[1], 1, &clip(&runic(words), room), Style::default().fg(RUNE));
    stamp(&mut grid[0], 1, &clip(gloss, room), Style::default().fg(DIM));

    grid.into_iter().map(row_to_line).collect()
}

// --- The whole tower: crown, shaft, and the war at its foot --------------------

/// A single shaft segment: the two stone walls of Barad-dûr with a shadowed inner
/// face between them. The walls sit at the same columns the Eye's stone frame
/// does (1 and 11), so whatever the row's width, the crown flows into the shaft.
fn shaft_row(width: usize) -> Vec<Cell> {
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

/// The header crown when the whole tower is drawn below: the Eye and its verse,
/// exactly as `scene` builds them, but with the flared foot swapped for a shaft
/// segment so the stone keeps descending into `tower_shaft` instead of stopping.
/// No ground and no walkers here -- those live at the foot now.
pub fn crown(width: usize, ms: u64) -> Vec<Line<'static>> {
    let w = width.max(20);
    let mut grid: Vec<Vec<Cell>> = (0..5).map(|_| blank_row(w)).collect();

    let t = ms % PERIOD;
    let flicker = ((ms / 220) % 3) as usize;
    let (pose, pupil) = idle_pose(t);
    let eye_left = w.saturating_sub(EYE_MARGIN) as i32;

    let mut rows = eye_tower(pose, pupil, flicker);
    rows[4] = shaft_row(EYE_W); // the foot moves to the ground; the tower keeps going

    for (r, row) in rows.into_iter().enumerate() {
        for (i, (ch, st)) in row.into_iter().enumerate() {
            if ch == ' ' {
                continue;
            }
            let c = eye_left + i as i32;
            if c >= 0 && (c as usize) < w {
                grid[r][c as usize] = (ch, st);
            }
        }
    }

    let (words, gloss) = verse(ms);
    let room = eye_left.max(2) as usize - 2;
    stamp(&mut grid[1], 1, &clip(&runic(words), room), Style::default().fg(RUNE));
    stamp(&mut grid[0], 1, &clip(gloss, room), Style::default().fg(DIM));

    grid.into_iter().map(row_to_line).collect()
}

/// The tower shaft: `height` rows of Barad-dûr's stone, `TOWER_W` wide, meant for
/// the right column of the list region directly beneath the crown. Arrow-slit
/// windows glow at fixed rows and pulse with `ms`, so the black spire has a few
/// lit eyes of its own without any of them wandering.
pub fn tower_shaft(height: usize, ms: u64) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(height);
    for r in 0..height {
        let mut row = shaft_row(TOWER_W as usize);
        // A slit every third row, alternating side; each pulses on its own phase.
        if r % 3 == 1 {
            let col = if (r / 3) % 2 == 0 { 4 } else { 8 };
            let lit = (ms / 320 + r as u64) % 5 < 3;
            row[col] = ('▪', Style::default().fg(if lit { FLAME } else { STONE_D }));
        }
        out.push(row_to_line(row));
    }
    out
}

/// The tower's foot and the war around it: `BASE_H` full-width rows for the very
/// bottom of the list region. The flared foot lands on the top row; the ground
/// runs the whole width along the bottom; and between them two armies fight, sized
/// and stirred by `armies` -- the count of agents currently working.
pub fn battle_ground(width: usize, armies: usize, ms: u64) -> Vec<Line<'static>> {
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

    battle(&mut g, eye_left, armies, ms);

    // From time to time -- on the same 26s schedule the compact Eye keeps -- the
    // three of them steal across the field along the ground row, drawn last so
    // they slip in front of the war (and any fallen) rather than into it.
    let t = ms % PERIOD;
    let cyc = ms / PERIOD;
    if let Some((prog, right)) = walk_at(t, cyc) {
        let leg = ((ms / 180) % 2) as usize;
        let base = walk_base(w, prog, right);
        place_walkers(&mut g[n - 1], base, right, leg);
    }

    g.into_iter().map(row_to_line).collect()
}

/// Set one cell if it is on the grid -- the battle's whole vocabulary is single
/// glyphs, so this is all the drawing it needs.
fn put(g: &mut [Vec<Cell>], row: usize, x: i32, ch: char, color: Color) {
    if row < g.len() && x >= 0 && (x as usize) < g[row].len() {
        g[row][x as usize] = (ch, Style::default().fg(color));
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

/// A triangle wave in `[-amp, amp]` over `period`, integer-only so the sway of
/// the battle line stays a pure function of the clock.
fn tri(ms: u64, period: u64, amp: i32) -> i32 {
    if amp == 0 || period == 0 {
        return 0;
    }
    let half = (period / 2) as i32;
    let p = (ms % period) as i32;
    let up = if p < half { p } else { period as i32 - p }; // 0..=half
    up * 2 * amp / half - amp
}

/// Muster and animate the melee. The two lines meet at a front that sways with the
/// tide; how many fighters muster, how hard the line sways, how fast feet move,
/// whether arrows fly and bodies fall -- all climb with `armies`. Zero working
/// agents leaves an uneasy calm: one orc keeps the gate.
fn battle(g: &mut [Vec<Cell>], eye_left: i32, armies: usize, ms: u64) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line_width(l: &Line) -> usize {
        l.spans.iter().map(|s| s.content.chars().count()).sum()
    }
    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn scene_is_five_lines_and_never_overflows_width() {
        for &w in &[24usize, 40, 80, 120] {
            for ms in [0u64, 6_000, 20_000, 21_000, 23_000, 46_000, 999_999] {
                let s = scene(w, ms);
                assert_eq!(s.len(), 5, "w={w} ms={ms}");
                for l in &s {
                    assert!(line_width(l) <= w, "overflow at w={w} ms={ms}: {}", line_text(l));
                }
            }
        }
    }

    #[test]
    fn walkers_appear_only_during_a_walk_window() {
        // Mid-idle: no hobbit on the ground.
        let idle = line_text(&scene(80, 6_000)[4]);
        assert!(!idle.contains('ó'), "hobbit showed up while idle: {idle}");
        // Mid-walk (the window is 17s..24.5s): Frodo, Sam, and the Ring cross.
        let walk = line_text(&scene(80, 20_000)[4]);
        assert!(walk.contains('ó'), "Frodo missing mid-walk: {walk}");
        assert!(walk.contains('ô'), "Sam missing mid-walk: {walk}");
        assert!(walk.contains('*'), "the Ring's glint is missing: {walk}");
    }

    #[test]
    fn walkers_cross_the_battlefield_on_the_walk_schedule() {
        // The ground row is the bottom of the war band.
        let ground = |ms| line_text(&battle_ground(80, 4, ms)[BASE_H as usize - 1]);
        // Mid-idle: the field holds only the war, no procession slips through.
        let idle = ground(6_000);
        assert!(!idle.contains('ó'), "hobbit crossed the war while idle: {idle}");
        // Mid-walk (17s..24.5s): Frodo, Sam, and the Ring steal across the field,
        // whatever the muster.
        for &armies in &[0usize, 4, 12] {
            let walk = line_text(&battle_ground(80, armies, 20_000)[BASE_H as usize - 1]);
            assert!(walk.contains('ó'), "Frodo missing on the field (armies={armies}): {walk}");
            assert!(walk.contains('ô'), "Sam missing on the field (armies={armies}): {walk}");
            assert!(walk.contains('*'), "the Ring's glint is missing (armies={armies}): {walk}");
        }
    }

    #[test]
    fn eye_blinks_on_the_idle_schedule() {
        // The idle blink sits at t≈5000.
        assert_eq!(idle_pose(5_100).0, Pose::Blink);
        // A blink renders the lid rune on the Eye's middle row (row index 2).
        let mid = line_text(&scene(80, 5_100)[2]);
        assert!(mid.contains('━'), "no closed lid on a blink frame: {mid}");
        // ...and the level stare between events is NOT a blink.
        assert_eq!(idle_pose(2_000).0, Pose::Center);
    }

    #[test]
    fn direction_alternates_between_walks() {
        // First walk heads right, the next heads left.
        let prog = (20_000 - WALK_START) as f64 / (WALK_END - WALK_START) as f64;
        assert_eq!(walk_at(20_000, 0), Some((prog, true)));
        assert_eq!(walk_at(20_000, 1).map(|(_, r)| r), Some(false));
        assert_eq!(walk_at(6_000, 0), None);
    }

    #[test]
    fn runic_transliterates_and_keeps_the_thorn_digraph() {
        assert_eq!(runic("a b"), "ᚨ᛬ᛒ");
        // "th" collapses to a single thorn rune rather than two glyphs.
        assert_eq!(runic("th"), "ᚦ");
        assert_eq!(runic("teithant").chars().filter(|&c| c == 'ᚦ').count(), 1);
    }

    #[test]
    fn crown_is_five_lines_and_never_overflows_width() {
        for &w in &[24usize, 40, 80, 120] {
            for ms in [0u64, 5_100, 11_700, 46_000, 999_999] {
                let c = crown(w, ms);
                assert_eq!(c.len(), 5, "w={w} ms={ms}");
                for l in &c {
                    assert!(line_width(l) <= w, "crown overflow at w={w} ms={ms}");
                }
            }
        }
    }

    #[test]
    fn tower_shaft_fills_its_column_with_stone() {
        for &h in &[1usize, 4, 12, 40] {
            let s = tower_shaft(h, 3_000);
            assert_eq!(s.len(), h, "h={h}");
            for l in &s {
                assert_eq!(line_width(l), TOWER_W as usize, "shaft width off at h={h}");
            }
            // Every row carries the two stone walls of the spire.
            assert!(line_text(&s[0]).contains('█'), "shaft has no wall");
        }
    }

    #[test]
    fn battle_ground_lands_the_foot_on_a_full_width_ground() {
        for &w in &[24usize, 40, 80, 120] {
            for &armies in &[0usize, 1, 3, 6, 20] {
                let b = battle_ground(w, armies, 20_000);
                assert_eq!(b.len(), BASE_H as usize, "w={w} armies={armies}");
                for l in &b {
                    assert!(line_width(l) <= w, "base overflow w={w} armies={armies}");
                }
                // The ground runs the full width along the bottom row.
                assert_eq!(line_width(&b[BASE_H as usize - 1]), w, "ground not full width");
                // The tower's foot is present on the top row.
                assert!(line_text(&b[0]).contains('▙'), "no tower foot at w={w}");
            }
        }
    }

    #[test]
    fn the_war_swells_with_the_running_count() {
        // More working agents -> more orcs on the field. Count orc heads at a
        // fixed instant so only the muster size differs.
        let orcs = |armies| -> usize {
            battle_ground(80, armies, 20_000)
                .iter()
                .map(|l| line_text(l).matches('ø').count())
                .sum()
        };
        assert!(orcs(6) > orcs(2), "a bigger swarm should field more orcs");
        assert!(orcs(2) > orcs(0), "an idle gate should be quieter than a fought one");
        // Arrow volleys only fly once the battle is big enough.
        let has_arrows = |armies| {
            battle_ground(80, armies, 20_000)
                .iter()
                .any(|l| line_text(l).contains('»') || line_text(l).contains('«'))
        };
        assert!(!has_arrows(1), "no volleys in a skirmish");
        assert!(has_arrows(6), "a full assault looses arrows");
    }
}
