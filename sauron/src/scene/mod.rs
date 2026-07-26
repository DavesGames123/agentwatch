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
//! **The header says which repo it is watching, in letters you can read from
//! across the room.** That is the header's first job -- sauron is normally run
//! several at a time, one pane per repo, and a board that identifies itself in
//! nine dim cells is a board you can act on believing it is a different one.
//! So the project's name goes up in block letters ([`sign`]) with its path, and
//! everything else on the header lays out around it: the mountain yields its
//! columns to the name, and the engraved verse only appears in whatever room is
//! left over.
//!
//! How large depends on the rows the terminal can spare -- [`scene_tall`] frames
//! the name and sets it at five rows, [`scene`] is the same sign at three and no
//! frame. Both are the *same* letters (see [`sign::Face`]); a board that shrinks
//! must not look like a different board.
//!
//! grep targets:
//!   struct World       -- what the agents are doing; every drawing takes one
//!   struct Watching    -- which project the board is on; the header's headline
//!   fn build           -- both headers: rows, frame, and where Mordor sits
//!   fn scene           -- the compact five-line header (no tower below)
//!   fn scene_tall      -- the eight-line header: name framed and set tall
//!   fn crown           -- the header when the whole tower is drawn below it
//!   fn crown_tall      -- the framed header, tower below
//!   const HEIGHT_TALL  -- what the framed header costs, and TALL_MIN_H to afford it
//!   fn tower_shaft     -- the descending stone shaft (right column of the list)
//!   fn battle_ground   -- the flared foot and the full-width war at its base
//!   fn doom_left       -- where the mountain fits, and when it does not
//!   fn place_eye       -- stamp the Eye sprite into a header grid
//!   fn place_watching  -- the project name, its path, and any leftover verse
//!   mod eye / doom / cast / war / runes / sign / paint

mod cast;
mod doom;
mod eye;
mod paint;
mod runes;
mod sign;
mod war;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use crate::model::truncate_left;
use crate::ui::{BLUE, DIM, EMBER, FLAME, FLARE, RUNE};
use cast::Lane;
use doom::DOOM_W;
use eye::Pose;
use paint::{blank_row, clip, row_to_line, stamp, Cell};

/// Rows the crown occupies in the header (not counting the status line above it).
pub const HEIGHT: u16 = 5;

/// Rows the framed header occupies: the frame's lid (carrying the path), five
/// rows of tall letters, the frame's floor, and the ground they stand on.
///
/// Three rows dearer than [`HEIGHT`], which is why `ui::draw` only spends it on
/// a terminal with rows to spare -- the list is what pays, and a board that
/// names itself beautifully while showing four sessions has traded the wrong way.
pub const HEIGHT_TALL: u16 = 8;

/// The terminal height at which the framed header becomes affordable: its nine
/// rows (status line included), the list's minimum six, the detail pane's nine,
/// and the footer, with a row spare so the list is not sitting at its floor.
pub const TALL_MIN_H: u16 = 26;

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
/// Columns kept clear at the left edge before the name starts.
const SIGN_X: i32 = 1;

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

/// Which project this board is watching -- the header's headline.
///
/// Two fields because the name alone is ambiguous exactly where it matters
/// most: two checkouts of the same repo, or two worktrees of it, have the same
/// directory name, and those are precisely the pair of boards a user is most
/// likely to confuse. The name is what you read from across the room; the path
/// is what you check when the answer surprises you.
#[derive(Clone, Copy, Debug, Default)]
pub struct Watching<'a> {
    /// The repo's directory name. Goes up in block letters.
    pub name: &'a str,
    /// Its path, home-shortened, faint above the name.
    pub path: &'a str,
}

/// Where Orodruin sits in a `w`-wide header, or `None` if it does not fit.
///
/// `need` is the columns the project's name has already claimed, and the
/// mountain only gets what is left over. That ordering is the point: the name
/// is the one thing on this header that is information, so a terminal too
/// narrow for both loses the mountain, never a letter of the name.
fn doom_left(eye_left: i32, need: i32) -> Option<i32> {
    let left = eye_left - DOOM_GAP - DOOM_W as i32;
    (left >= need + SIGN_X + 2).then_some(left)
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

/// How the header intends to write the project's name, decided before anything
/// is drawn because the mountain's columns depend on the answer.
enum Sign {
    /// Block letters -- the whole point of the header. Carries the face and the
    /// kerning it was measured at, so the drawing cannot disagree with the width
    /// the mountain was laid out around.
    Block {
        w: usize,
        kern: usize,
        face: sign::Face,
    },
    /// No room for that, so one row of capitals instead. Still the name, still
    /// the brightest thing on the left of the header; just not shoutable.
    Plain(usize),
}

impl Sign {
    /// Columns it will claim, frame included when there is one.
    fn width(&self, framed: bool) -> i32 {
        let bare = match *self {
            Sign::Block { w, .. } | Sign::Plain(w) => w as i32,
        };
        bare + if framed { FRAME_W } else { 0 }
    }
}

/// Columns a frame costs: one rule down each side. There is no padding inside
/// it. A space either side would read better and cost two more columns, which is
/// exactly the margin by which the longest of these boards fits at all -- and a
/// framed name that demotes to small capitals to buy breathing room has spent
/// the legibility it was drawn for on whitespace.
const FRAME_W: i32 = 2;

/// Pick the largest form of `name` that fits the columns left of the Eye: block
/// letters airy, then block letters tight, then small capitals.
///
/// Measured against `eye_left` -- the room available with *no* mountain --
/// rather than against the room beside one, so a name that only fits on a bare
/// header still gets its block letters and the mountain is what goes.
///
/// `face` is not a fallback rung here. Both faces measure the same, so a name
/// that will not fit tall will not fit short either; the choice between them is
/// made from the terminal's *rows*, before this is called.
fn plan_sign(name: &str, eye_left: i32, face: sign::Face, framed: bool) -> Sign {
    let frame = if framed { FRAME_W } else { 0 };
    let room = (eye_left - SIGN_X - 1 - frame).max(1) as usize;
    for kern in sign::KERNS {
        let w = sign::width(name, kern, face);
        if w <= room {
            return Sign::Block { w, kern, face };
        }
    }
    Sign::Plain(name.chars().count().min(room))
}

/// Write the project's name across the left of the header, its path with it, and
/// -- only in whatever room is left over on the top row -- a line of engraving.
///
/// The verse used to own rows 0 and 1 outright. It is flavour, and it was
/// sitting in the only part of the header wide enough to say which repo this
/// is, so it now takes what is left rather than what it wants. It is dropped
/// whole rather than clipped: half an engraving reads as a rendering fault,
/// where no engraving reads as a narrow terminal.
///
/// Framed, the path moves *into* the top rule as its title, which is what buys
/// the extra rows back: a frame costs two rows, and hanging the path off it
/// returns one of them.
fn place_watching(grid: &mut [Vec<Cell>], w: &Watching, plan: &Sign, ms: u64, stop: i32, framed: bool) {
    let room = (stop - SIGN_X).max(1) as usize;
    let dim = Style::default().fg(DIM);
    let style = Style::default().fg(BLUE);

    // The name, and how many columns of frame go round it. Row 0 is the path's
    // (or the frame's lid, which carries the path), so letters start at row 1
    // either way.
    let inner = plan.width(false) as usize;
    let top = 1usize;
    let rows = match *plan {
        Sign::Block { face, .. } => face.rows(),
        Sign::Plain(_) => 1,
    };

    if framed {
        // The top rule, carrying the path as its title -- cut from the *front*,
        // because the tail is the half that distinguishes two checkouts of one
        // repo and "/Users/somebody/co…" distinguishes nothing.
        let span = inner + FRAME_W as usize;
        let mut lid = String::from("╭");
        let title_room = span.saturating_sub(5);
        let path = truncate_left(w.path, title_room);
        if !path.is_empty() && title_room >= 4 {
            lid.push_str("─ ");
            lid.push_str(&path);
            lid.push(' ');
        }
        while lid.chars().count() + 1 < span {
            lid.push('─');
        }
        lid.push('╮');
        stamp(&mut grid[0], SIGN_X, &lid, dim);

        let floor: String = std::iter::once('╰')
            .chain(std::iter::repeat_n('─', span.saturating_sub(2)))
            .chain(std::iter::once('╯'))
            .collect();
        stamp(&mut grid[top + rows], SIGN_X, &floor, dim);
        for r in 0..rows {
            stamp(&mut grid[top + r], SIGN_X, "│", dim);
            stamp(&mut grid[top + r], SIGN_X + 1 + inner as i32, "│", dim);
        }
    } else {
        // Unframed, the path keeps its own row above the name.
        let path = truncate_left(w.path, room);
        stamp(&mut grid[0], SIGN_X, &path, dim);
    }

    // The engraving, right-aligned in whatever the top row has left over.
    let used = if framed { inner + FRAME_W as usize } else { truncate_left(w.path, room).chars().count() };
    let verse = runes::runic(runes::verse(ms).0);
    let vw = verse.chars().count();
    if room >= used + vw + 3 {
        let x = SIGN_X + (room - vw) as i32;
        stamp(&mut grid[0], x, &verse, Style::default().fg(RUNE));
    }

    // The name itself, inside whatever was drawn round it.
    let x = SIGN_X + if framed { 1 } else { 0 };
    match *plan {
        Sign::Block { kern, face, .. } => {
            for (r, row) in sign::render(w.name, kern, face).iter().enumerate() {
                stamp(&mut grid[top + r], x, row, style);
            }
        }
        // Bold, because at one row it is competing with the mountain beside it
        // for the eye and it has to win.
        Sign::Plain(_) => {
            let flat = clip(&w.name.to_uppercase(), inner);
            let r = if framed { top } else { 2 };
            stamp(&mut grid[r], x, &flat, style.add_modifier(Modifier::BOLD));
        }
    }
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

// --- the header ----------------------------------------------------------------

/// Compose the header for a terminal `width` at time `ms`.
///
/// `rows` is [`HEIGHT`] or [`HEIGHT_TALL`]; `ground` says whether this header
/// stands on its own horizon (the compact scene) or hands the stone downward to
/// [`tower_shaft`] (the crown).
///
/// The sky is what grows. Mordor -- Eye, mountain, anyone crossing -- is always
/// the bottom [`HEIGHT`] rows, because those five rows are one drawing that
/// stands on the ground: stretching it would mean stretching the Eye, and the
/// Eye is a face. The extra rows of a tall header go *above* it, which is where
/// the sign is, and a sign hung higher on the same skyline still reads as the
/// same place.
fn build(width: usize, ms: u64, world: World, watching: Watching, rows: usize, ground: bool) -> Vec<Line<'static>> {
    let w = width.max(20);
    let mut grid: Vec<Vec<Cell>> = (0..rows).map(|_| blank_row(w)).collect();
    let eye_left = w.saturating_sub(EYE_MARGIN) as i32;
    let eye_col = eye_left + 6; // the pupil's screen column

    // Where Mordor starts: the last five rows, whatever the header's height.
    let sky = rows - HEIGHT as usize;
    let framed = rows > HEIGHT as usize;
    let face = if framed { sign::Face::Tall } else { sign::Face::Block };

    if ground {
        let ground_style = Style::default().fg(GROUND);
        for cell in grid[rows - 1].iter_mut() {
            *cell = ('▁', ground_style);
        }
    }

    // Orodruin, behind everything: the Eye is clipped over it, the name stops
    // short of it, and anyone crossing walks in front of its foot.
    let plan = plan_sign(watching.name, eye_left, face, framed);
    let doom_x = doom_left(eye_left, plan.width(framed));
    if let Some(x) = doom_x {
        doom::draw(&mut grid[sky..], x, ms, world);
    }

    // Who, if anyone, is crossing -- and what the Eye makes of it. In vigil the
    // Eye follows the lead figure and flares wide as they pass beneath; banked,
    // it sleeps through the whole procession, which is the point of the state.
    // A crown has no ground for anyone to cross, so nobody does.
    let cross = ground.then(|| cast::crossing(ms, world.repose)).flatten();
    let (pose, pupil) = match &cross {
        Some(c) if !world.repose => {
            let base = cast::walk_base(w, c.prog, c.right);
            let frac = (base as f64 / w as f64).clamp(0.0, 1.0);
            let pose = if (eye_col - base).abs() < 4 { Pose::Wide } else { Pose::Center };
            (pose, ((frac * 6.0).round() as usize).min(6))
        }
        _ => resting_pose(ms, world),
    };
    place_eye(&mut grid[sky..], eye_left, pose, pupil, ms, world.repose, ground);

    // The company, in the foreground, so it passes in front of the tower's foot
    // instead of vanishing behind it. A flyer is clipped to the columns left of
    // the Eye: the air lane runs straight through the Eye's face, and a fell
    // beast punching a hole in it looks like a redraw bug, not an overflight.
    if let Some(c) = &cross {
        let base = cast::walk_base(w, c.prog, c.right);
        let leg = cast::stride(c.party, ms);
        match c.party.lane {
            Lane::Ground => (c.party.draw)(&mut grid[rows - 1], base, c.right, leg),
            Lane::Air => {
                let lim = (eye_left.max(0) as usize).min(w);
                (c.party.draw)(&mut grid[rows - 2][..lim], base, c.right, leg);
            }
        }
    }

    place_watching(&mut grid, &watching, &plan, ms, doom_x.unwrap_or(eye_left), framed);
    grid.into_iter().map(row_to_line).collect()
}

/// The compact header: [`HEIGHT`] rows, self-contained, standing on its own
/// ground. The fallback when the terminal cannot spare the rows for [`scene_tall`].
pub fn scene(width: usize, ms: u64, world: World, watching: Watching) -> Vec<Line<'static>> {
    build(width, ms, world, watching, HEIGHT as usize, true)
}

/// The framed header: [`HEIGHT_TALL`] rows, the name set tall inside a rule with
/// its path hung on the top of it.
pub fn scene_tall(width: usize, ms: u64, world: World, watching: Watching) -> Vec<Line<'static>> {
    build(width, ms, world, watching, HEIGHT_TALL as usize, true)
}

// --- the whole tower: crown, shaft, and the war at its foot --------------------

/// The header when the whole tower is drawn below it: the Eye, the mountain, and
/// the verse exactly as [`scene`] builds them, but with the flared foot swapped
/// for a shaft segment so the stone keeps descending into [`tower_shaft`]
/// instead of stopping. No ground and no company here -- those live at the foot.
pub fn crown(width: usize, ms: u64, world: World, watching: Watching) -> Vec<Line<'static>> {
    build(width, ms, world, watching, HEIGHT as usize, false)
}

/// [`crown`] with the framed, tall-set name -- the pairing of [`scene_tall`] for
/// a terminal that is drawing the whole tower.
pub fn crown_tall(width: usize, ms: u64, world: World, watching: Watching) -> Vec<Line<'static>> {
    build(width, ms, world, watching, HEIGHT_TALL as usize, false)
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

    /// The board most of these tests are watching. Shadows `super::scene` /
    /// `super::crown` so a test that does not care which repo is on the header
    /// does not have to say -- the ones that do care call through explicitly.
    const DEMO: Watching = Watching { name: "demo", path: "~/src/demo" };

    fn scene(w: usize, ms: u64, world: World) -> Vec<Line<'static>> {
        super::scene(w, ms, world, DEMO)
    }
    fn crown(w: usize, ms: u64, world: World) -> Vec<Line<'static>> {
        super::crown(w, ms, world, DEMO)
    }

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
    #[ignore = "prints the header for eyeballing: cargo test -- --ignored --nocapture eyeball"]
    fn eyeball_the_boards() {
        for name in ["worldsmith", "barnes-hut", "getajob", "agentwatch"] {
            for w in [52usize, 80] {
                let watching = Watching { name, path: &format!("~/Downloads/{name}") };
                println!("--- {name} at {w} columns, framed ---");
                println!("{}\n", text(&super::scene_tall(w, 0, VIGIL, watching)));
            }
        }
        let watching = Watching { name: "agentwatch", path: "~/Downloads/agentwatch" };
        println!("--- the compact fallback, 52 columns ---");
        println!("{}\n", text(&super::scene(52, 0, VIGIL, watching)));
        println!("--- framed crown (whole tower below), 80 columns ---");
        println!("{}\n", text(&super::crown_tall(80, 0, VIGIL, watching)));
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
    fn the_mountain_shows_when_there_is_room_and_yields_to_the_name_when_not() {
        // Wide: cone and plume both present beside a short name.
        let wide = text(&scene(100, 0, VIGIL));
        assert!(wide.contains('▟'), "no mountain on a wide header");
        assert!(wide.contains('▓') || wide.contains('▒'), "no plume: {wide}");
        // A long name on that same 100 columns takes the mountain's ground: the
        // name is information and the mountain is scenery, so this is the order
        // they lose in.
        let long = Watching { name: "agentwatch-frontend", path: "~/src/x" };
        let crowded = text(&super::scene(100, 0, VIGIL, long));
        // The plume, not the cone: the cone's glyphs are shared with the tower's
        // flared foot, which is drawn either way.
        assert!(
            !crowded.contains('▓') && !crowded.contains('▒') && !crowded.contains('░'),
            "the mountain crowded out the name: {crowded}"
        );
        // ...and the name is all there, not clipped to fit around it.
        assert!(crowded.contains(&sign::render("agentwatch-frontend", 1, sign::Face::Block)[1]));
        // Narrow: the mountain is what goes there too.
        let narrow = text(&scene(40, 0, VIGIL));
        assert!(!narrow.contains('▒') && !narrow.contains('░'), "smoke on a narrow header");
    }

    /// The header's whole job. A board that cannot be told from the board in the
    /// next pane is worse than no board, because it is one you will act on.
    #[test]
    fn the_header_writes_the_project_name_large_and_says_where_it_lives() {
        let w = Watching { name: "agentwatch", path: "~/Downloads/agentwatch" };
        for build in [super::scene, super::crown] {
            let head = text(&build(120, 0, VIGIL, w));
            // Three rows of block letters, each row present in full.
            for row in sign::render("agentwatch", 1, sign::Face::Block) {
                assert!(head.contains(&row), "the name is not in block letters: {head}");
            }
            // And the path, which is the half that disambiguates two checkouts.
            assert!(head.contains("~/Downloads/agentwatch"), "no path: {head}");
        }
    }

    /// A sauron pane in a four-agent workspace is 52 columns. That is the width
    /// this tool is used at most of the time, and it is exactly where the airy
    /// setting stops fitting -- so it is the one width the block letters have to
    /// survive, and the condensed rung of the ladder exists for it.
    #[test]
    fn the_name_stays_in_block_letters_at_a_workspace_pane_width() {
        let w = Watching { name: "agentwatch", path: "~/Downloads/agentwatch" };
        let pane = text(&super::scene(52, 0, VIGIL, w));
        assert!(
            pane.contains(&sign::render("agentwatch", 0, sign::Face::Block)[1]),
            "the name dropped out of block letters at 52 columns: {pane}"
        );
    }

    /// The framed header has to hold its shape everywhere the compact one does:
    /// a header that overflows its rect corrupts the row below it, and the row
    /// below it is the session list.
    #[test]
    fn the_framed_header_is_eight_lines_and_never_overflows_width() {
        let w = Watching { name: "agentwatch", path: "~/Downloads/agentwatch" };
        for &cols in &[24usize, 40, 52, 80, 120] {
            for ms in [0u64, 6_000, 13_000, 23_000, 91_000, 999_999] {
                for world in [VIGIL, PAUSED, REPOSE] {
                    for build in [super::scene_tall, super::crown_tall] {
                        let s = build(cols, ms, world, w);
                        assert_eq!(s.len(), HEIGHT_TALL as usize, "cols={cols} ms={ms} {world:?}");
                        for l in &s {
                            assert!(
                                line_width(l) <= cols,
                                "overflow at cols={cols} ms={ms} {world:?}: {}",
                                line_text(l)
                            );
                        }
                    }
                }
            }
        }
    }

    /// The frame is only worth its two rows if it actually closes: an unclosed
    /// rule reads as a drawing that ran out of room, which is the opposite of the
    /// "this is a sign, and it is deliberate" the frame is there to say.
    #[test]
    fn the_frame_closes_round_the_name_and_hangs_the_path_on_it() {
        let w = Watching { name: "agentwatch", path: "~/Downloads/agentwatch" };
        let lines = super::scene_tall(80, 0, VIGIL, w);
        let rows: Vec<String> = lines.iter().map(line_text).collect();

        // The lid carries the path, so the path costs no row of its own.
        assert!(rows[0].contains("╭─ ~/Downloads/agentwatch "), "no titled lid: {}", rows[0]);
        assert!(rows[0].trim_end().ends_with('╮'), "the lid never closed: {}", rows[0]);
        // Five rows of letters, each fenced on both sides.
        let drawn = sign::render("agentwatch", 1, sign::Face::Tall);
        for (r, letters) in drawn.iter().enumerate() {
            let row = &rows[1 + r];
            assert!(row.contains(letters), "letter row {r} is not in tall letters: {row}");
            assert_eq!(row.chars().nth(1), Some('│'), "row {r} lost its left rule: {row}");
        }
        // And the floor closes it.
        let floor = &rows[1 + sign::Face::Tall.rows()];
        assert!(floor.contains('╰') && floor.contains('╯'), "the frame has no floor: {floor}");
    }

    /// The framed form has to survive the width this tool is actually run at.
    /// The frame costs two columns and the longest of these names has exactly two
    /// to give -- if that stops being true, the name demotes to capitals at the
    /// one width where it matters most, and the frame will have cost more than it
    /// bought.
    #[test]
    fn the_framed_name_still_fits_a_workspace_pane() {
        let w = Watching { name: "agentwatch", path: "~/Downloads/agentwatch" };
        let pane = text(&super::scene_tall(52, 0, VIGIL, w));
        assert!(
            pane.contains(&sign::render("agentwatch", 0, sign::Face::Tall)[2]),
            "the framed name dropped out of block letters at 52 columns: {pane}"
        );
    }

    /// Every terminal wide enough to run this tool names its repo somewhere on
    /// the header -- in capitals when there is no room for the block form, but
    /// never nowhere.
    #[test]
    fn a_narrow_header_shrinks_the_name_rather_than_dropping_it() {
        let w = Watching { name: "agentwatch", path: "~/Downloads/agentwatch" };
        let narrow = text(&super::scene(40, 0, VIGIL, w));
        assert!(!narrow.contains(&sign::render("agentwatch", 0, sign::Face::Block)[0]), "block letters fit at 40?");
        assert!(narrow.contains("AGENTWATCH"), "the name vanished at 40 columns: {narrow}");
        // Small enough that even the capitals have to be cut -- what survives is
        // still the front of the name, not nothing.
        let tiny = text(&super::scene(24, 0, VIGIL, w));
        assert!(tiny.contains("AGEN"), "nothing left of the name at 24 columns: {tiny}");
    }

    #[test]
    fn the_engraving_takes_what_is_left_over_and_is_never_half_drawn() {
        // Wide header, short name and path: room for the verse beside them.
        let roomy = Watching { name: "ab", path: "~/x" };
        let wide = text(&super::scene(120, 0, VIGIL, roomy));
        assert!(
            wide.contains(&runes::runic(runes::VERSES[0].0)),
            "the engraving was cut short instead of dropped: {wide}"
        );
        // A path long enough to fill the row drops the verse whole rather than
        // clipping it -- half an engraving reads as a rendering fault.
        let hog = Watching { name: "ab", path: "~/a/very/long/checkout/path/that/eats/the/row" };
        let tight = text(&super::scene(80, 0, VIGIL, hog));
        for ch in tight.chars() {
            assert!(!('ᚠ'..='ᛰ').contains(&ch), "a stray rune survived: {tight}");
        }
    }

    #[test]
    fn nothing_on_the_left_of_the_header_runs_into_the_mountain() {
        // Name, path and verse are all clipped at the mountain's left edge, not
        // the Eye's -- the mountain is drawn first and everything else must stop
        // short of it.
        let eye_left = 100i32 - EYE_MARGIN as i32;
        let plan = plan_sign(DEMO.name, eye_left, sign::Face::Block, false);
        let doom_x = doom_left(eye_left, plan.width(false)).expect("100 columns fits a mountain");
        for ms in [0u64, 30_000, 60_000] {
            for row in scene(100, ms, VIGIL).iter().take(4) {
                for (i, ch) in line_text(row).chars().enumerate() {
                    if (i as i32) < doom_x || ch == ' ' {
                        continue;
                    }
                    // Past that edge, only the mountain and the Eye draw.
                    assert!(
                        !('ᚠ'..='ᛰ').contains(&ch) && !ch.is_ascii_alphanumeric(),
                        "the left of the header spills into the mountain at column {i}, ms={ms}"
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

