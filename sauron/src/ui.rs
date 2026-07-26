//! Rendering.
//!
//! Layout priority follows attention priority: sessions awaiting acknowledgement
//! get a banner, a colour, and full detail; sessions with nothing outstanding
//! collapse to a single count line. An earlier version gave every session three
//! lines regardless of state, so fifteen idle sessions buried the one that
//! needed a human.
//!
//! The header answers a question before it reports anything: *which repo is
//! this?* Sauron is run several at a time, one pane per project, so a board
//! that names itself quietly is a board you will act on believing it is a
//! different one. The name goes up in block letters when the terminal can spare
//! the rows (see `scene::Watching`), and in a filled badge when it cannot.
//!
//! grep targets:
//!   fn draw            -- top-level layout
//!   fn header          -- the status tally, and how the repo gets named
//!   fn section_header  -- coloured rule introducing each status group
//!   fn card            -- one session -> multi-line ListItem
//!   fn wrap_prompt     -- first lines of your last ask, wrapped for a card
//!   fn detail          -- selected session's write-set and prompt
//!   fn dim_common      -- shared directory prefix compression for path lists
//!   const AMBER/CYAN   -- the status palette

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::{ago, fmt_clock, fmt_duration, local_time, truncate, Status};
use crate::Row;

/// Status palette. Each state owns one hue and keeps it everywhere it appears --
/// glyph, title, section rule, header tally -- so the colour alone identifies
/// the state without reading the label.
const MAGENTA: Color = Color::Rgb(240, 90, 200); // errored — dead, needs rescue
const RED: Color = Color::Rgb(255, 92, 110); // blocked on your answer
const AMBER: Color = Color::Rgb(255, 176, 66); // awaiting acknowledgement — a glance and a reply
const GOLD: Color = Color::Rgb(226, 202, 96); // awaiting testing — untested writes to run and verify
const CYAN: Color = Color::Rgb(86, 205, 226); // agent still working
const GREEN: Color = Color::Rgb(126, 200, 120); // nothing outstanding
const INDIGO: Color = Color::Rgb(150, 140, 235); // delegated to a background agent
const ORC: Color = Color::Rgb(122, 158, 74); // one of sauron's own maintenance orcs
pub(crate) const BLUE: Color = Color::Rgb(120, 170, 255); // chrome / repo identity
pub(crate) const DIM: Color = Color::Rgb(88, 94, 104);
const INK: Color = Color::Rgb(18, 20, 24); // text on a filled badge
const SAID: Color = Color::Rgb(158, 166, 178); // your last words, quoted back on a card
const FILE: Color = Color::Rgb(214, 220, 228); // a modified file, named clearly
const PREVIEW: Color = Color::Rgb(132, 140, 152); // its most recent lines of text

// The Eye of Sauron and its engraved verse. Kept apart from the status palette
// above -- this is chrome flavour, never a signal, so it must not borrow a hue
// that means something. Shared with the `scene` module (the five-line Eye).
pub(crate) const FLAME: Color = Color::Rgb(255, 122, 24); // the lidless eye, wreathed in fire
pub(crate) const EMBER: Color = Color::Rgb(120, 22, 10); // the slit pupil, a hole in the flame
pub(crate) const FLARE: Color = Color::Rgb(255, 176, 60); // the eye flaring wide
pub(crate) const RUNE: Color = Color::Rgb(150, 70, 40); // the engraved script, faint

pub fn color_of(status: Status) -> Color {
    match status {
        Status::Errored => MAGENTA,
        Status::Blocked => RED,
        Status::AwaitingAck => AMBER,
        Status::NeedsTest => GOLD,
        Status::Working => CYAN,
        Status::Delegated => INDIGO,
        Status::Clear => DIM,
    }
}

fn glyph_of(status: Status) -> &'static str {
    match status {
        Status::Errored => "✖",
        Status::Blocked => "▲",
        // A prompt chevron: the agent is idle at the prompt, awaiting your reply.
        Status::AwaitingAck => "❯",
        Status::NeedsTest => "█",
        Status::Working => "◐",
        Status::Delegated => "◇",
        Status::Clear => "·",
    }
}

pub struct View<'a> {
    pub rows: &'a [Row],
    pub selected: usize,
    pub now: i64,
    /// The watched repo's directory name -- the header's headline, drawn in
    /// block letters when the terminal is tall enough for the full crown.
    pub repo: &'a str,
    /// Its path, home-shortened. Two worktrees of one repo share a name, and
    /// the pair of boards a user is most likely to mix up is exactly that pair,
    /// so the name never appears without the path under it.
    pub repo_path: &'a str,
    pub saved: bool,
    pub hidden_stale: usize,
    pub clear_count: usize,
    pub show_clear: bool,
    pub copied: bool,
    /// Outcome of the last `n` / Enter pane spawn while its banner is up: the
    /// message and whether it succeeded. A failed spawn has to say so -- the new
    /// pane is off in the workspace window, so the footer is the only place the
    /// user finds out it never appeared.
    pub spawned: Option<(&'a str, bool)>,
    /// Milliseconds since launch, the clock the Eye and the ring-verse animate
    /// off. Derived, not stored in App: the whole animation is a pure function
    /// of this, so nothing has to be ticked or remembered between frames.
    pub anim_ms: u64,
    /// The machine's UTC offset in seconds, so a task's start time renders on the
    /// local wall clock. Read once at launch and carried through unchanged.
    pub local_offset: i64,
    /// Set to the log directory only while that directory does not exist yet --
    /// a repo no agent has ever run in. The board is legitimately empty rather
    /// than broken, so the empty state says which path it is waiting on. Goes
    /// back to None the moment the first session appears.
    pub awaiting_log_dir: Option<&'a str>,
    /// The cold-target picker, drawn over the board while it is open.
    pub pick: Option<PickView<'a>>,
}

/// The cold-target picker's contents. Carries the *evidence* behind each rank --
/// line count and churn -- because the list is asking the user to approve a file
/// for automated refactoring, and "trust the ordering" is not an answer.
pub struct PickView<'a> {
    pub cold: &'a [crate::orc::Target],
    pub selected: usize,
    /// Candidates excluded because a live session is editing them.
    pub hot: usize,
    /// Candidates excluded because git reports them dirty.
    pub dirty: usize,
}

/// Screen geometry of the last-drawn list, so a mouse click can be resolved to
/// the row under it. Filled by `list`, read by the event loop.
#[derive(Default)]
pub struct FrameGeometry {
    pub list_top: u16,
    pub list_height: u16,
    /// Per rendered item, in draw order: its height in rows.
    pub item_heights: Vec<u16>,
    /// Per rendered item: the `rows` index it maps to, or None for a section
    /// header or the clear-collapse line.
    pub item_rows: Vec<Option<usize>>,
}

/// What Mordor is told about the board.
///
/// `repose` is deliberately the *same* condition the header states in words as
/// "all caught up", widened by "and nothing is running or delegated either".
/// Deriving it here, once, from the rows is what keeps the two from drifting
/// apart -- a lidded Eye sitting above an amber AWAITING ACK badge would be the
/// chrome calling the badge a liar, and whichever one the user believed, one of
/// them would be wrong.
fn world_of(rows: &[Row]) -> crate::scene::World {
    let count = |s: Status| rows.iter().filter(|r| r.status == s).count();
    let working = count(Status::Working);
    let outstanding = count(Status::Errored)
        + count(Status::Blocked)
        + count(Status::AwaitingAck)
        + count(Status::NeedsTest);
    crate::scene::World {
        working,
        repose: working == 0 && count(Status::Delegated) == 0 && outstanding == 0,
    }
}

pub fn draw(f: &mut Frame, v: &View, list_state: &mut ListState, geo: &mut FrameGeometry) {
    // The full five-line Eye earns its keep only when the terminal is tall
    // enough to spare the rows; below that the header collapses to the compact
    // one-line Eye so the list and detail keep their space. Taller still, the
    // name goes up framed and set at five rows -- three rows dearer, and only
    // out of rows the list would not have missed.
    let full = f.area();
    let tall = full.height >= 24;
    let framed = full.height >= crate::scene::TALL_MIN_H;
    let header_h = match (tall, framed) {
        (_, true) => 1 + crate::scene::HEIGHT_TALL,
        (true, false) => 1 + crate::scene::HEIGHT,
        (false, false) => 2,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Min(6),
            Constraint::Length(9),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Draw the whole tower -- shaft down the right column, the war at its foot --
    // only when the terminal is tall AND wide enough to give both the room, and
    // the list region has rows to spare beneath a short session list. Otherwise
    // the header keeps its self-contained crown and the list keeps every column.
    let tower_w = crate::scene::TOWER_W;
    let base_h = crate::scene::BASE_H;
    let mordor = tall
        && full.width >= 64
        && chunks[1].height >= base_h + 3
        && chunks[1].width > tower_w + 24;

    // The list gives up the shaft's column and the war's rows when Mordor is drawn.
    let mut list_area = chunks[1];
    if mordor {
        list_area.width = list_area.width.saturating_sub(tower_w);
        list_area.height = list_area.height.saturating_sub(base_h);
    }

    let world = world_of(v.rows);
    header(f, chunks[0], v, mordor, world);
    list(f, list_area, v, list_state, geo);
    detail(f, chunks[2], v.rows.get(v.selected), v.now, v.local_offset);
    footer(f, chunks[3], v);

    if mordor {
        // The shaft descends the right column, from the top of the list region to
        // just above the war band; its top row meets the crown's shaft-cap at the
        // header seam, so the stone reads as one continuous spire.
        let shaft = Rect {
            x: full.width - tower_w,
            y: chunks[1].y,
            width: tower_w,
            height: chunks[1].height.saturating_sub(base_h),
        };
        f.render_widget(
            Paragraph::new(crate::scene::tower_shaft(shaft.height as usize, v.anim_ms, world)),
            shaft,
        );
        // The war at the foot, full width along the bottom of the list region. Its
        // size tracks the count of agents currently working -- each is one orc --
        // and in repose there is no war at all, only whoever is crossing.
        let base = Rect {
            x: chunks[1].x,
            y: chunks[1].bottom() - base_h,
            width: full.width,
            height: base_h,
        };
        f.render_widget(
            Paragraph::new(crate::scene::battle_ground(full.width as usize, v.anim_ms, world)),
            base,
        );
    }

    // Last, so it sits over everything: the picker is modal and owns the
    // keyboard while it is up, and the drawing should say so.
    if let Some(pick) = &v.pick {
        orc_picker(f, full, pick);
    }
    let _ = chunks;
}

/// The cold-target picker: which file to loose an orc on, ranked, with the
/// evidence for the ranking and an explicit count of what was ruled out.
///
/// The exclusion line is not decoration. Filtering silently reads as "there was
/// nothing else to choose", which is a different and false claim -- the whole
/// safety property of an orc is that contested files were *deliberately* held
/// back, and that only means something if you can see it happening.
fn orc_picker(f: &mut Frame, full: Rect, pick: &PickView) {
    let rows = pick.cold.len().min(12) as u16;
    let w = full.width.saturating_sub(8).min(76).max(30);
    let h = (rows + 5).min(full.height.saturating_sub(2));
    let area = Rect {
        x: (full.width.saturating_sub(w)) / 2,
        y: (full.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, area);

    let inner_w = area.width.saturating_sub(2) as usize;
    // Keep the selection on screen with a simple window that follows the cursor.
    let visible = rows as usize;
    let top = pick.selected.saturating_sub(visible.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::new();
    for (i, t) in pick.cold.iter().enumerate().skip(top).take(visible) {
        let here = i == pick.selected;
        // Churn is the part a user cannot see for themselves at a glance, so it
        // is spelled out rather than folded into the score.
        let stat = if t.churn > 0 {
            format!("{} loc · {} commits", t.loc, t.churn)
        } else {
            format!("{} loc", t.loc)
        };
        let room = inner_w.saturating_sub(stat.chars().count() + 4);
        let path = truncate(&t.path, room.max(8));
        let pad = inner_w
            .saturating_sub(path.chars().count() + stat.chars().count() + 3)
            .max(1);
        let style = if here {
            Style::default().fg(INK).bg(ORC).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(FILE)
        };
        lines.push(Line::from(vec![
            Span::styled(if here { " ▸ " } else { "   " }, style),
            Span::styled(path, style),
            Span::styled(" ".repeat(pad), style),
            Span::styled(
                stat,
                if here {
                    style
                } else {
                    Style::default().fg(DIM)
                },
            ),
        ]));
    }

    lines.push(Line::from(Span::styled(
        format!(
            "  {} cold · {} hot · {} dirty held back",
            pick.cold.len(),
            pick.hot,
            pick.dirty
        ),
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(vec![
        Span::styled("  ⏎", Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
        Span::styled(" stage orc   ", Style::default().fg(DIM)),
        Span::styled("j/k", Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
        Span::styled(" move   ", Style::default().fg(DIM)),
        Span::styled("esc", Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
        Span::styled(" close", Style::default().fg(DIM)),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ORC))
        .title(Span::styled(
            " loose an orc — decompose first ",
            Style::default().fg(ORC).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn header(f: &mut Frame, area: Rect, v: &View, mordor: bool, world: crate::scene::World) {
    let awaiting = v.rows.iter().filter(|r| r.status == Status::NeedsTest).count();
    let ack = v.rows.iter().filter(|r| r.status == Status::AwaitingAck).count();
    let working = v.rows.iter().filter(|r| r.status == Status::Working).count();
    let delegated = v.rows.iter().filter(|r| r.status == Status::Delegated).count();

    // Whether the crown -- and with it the name in block letters -- is drawn
    // below this line. When it is, repeating the name here in small type would
    // be the same word twice in five rows; when it is not, this line is the
    // only place the repo is named, so it gets a filled badge rather than the
    // dim text it used to carry.
    let signed = area.height > crate::scene::HEIGHT;
    let mut top = Vec::new();
    if signed {
        top.push(Span::styled("sauron", Style::default().fg(DIM)));
    } else {
        top.push(Span::styled(
            format!(" {} ", v.repo.to_uppercase()),
            Style::default().fg(INK).bg(BLUE).add_modifier(Modifier::BOLD),
        ));
        top.push(Span::raw(" "));
        top.push(Span::styled("sauron", Style::default().fg(DIM)));
    }
    top.push(Span::raw("   "));

    // An errored agent is dead until rescued and will not recover on its own, so
    // it sits leftmost of all -- ahead even of the blocked badge.
    let errored = v.rows.iter().filter(|r| r.status == Status::Errored).count();
    if errored > 0 {
        top.push(Span::styled(
            format!(" {} ERRORED ", errored),
            Style::default()
                .fg(INK)
                .bg(MAGENTA)
                .add_modifier(Modifier::BOLD),
        ));
        top.push(Span::raw("  "));
    }

    // A blocked agent is doing nothing until you reply, so it gets the loudest
    // badge and sits leftmost -- ahead even of the awaiting count.
    let blocked = v.rows.iter().filter(|r| r.status == Status::Blocked).count();
    if blocked > 0 {
        top.push(Span::styled(
            format!(" {} WAITING ON YOU ", blocked),
            Style::default()
                .fg(INK)
                .bg(RED)
                .add_modifier(Modifier::BOLD),
        ));
        top.push(Span::raw("  "));
    }

    // Agents stopped at the prompt wanting only a nod/reply -- amber, ahead of
    // untested work: a stopped agent is idling a slot while a testable one is done.
    if ack > 0 {
        top.push(Span::styled(
            format!(" {} AWAITING ACK ", ack),
            Style::default()
                .fg(INK)
                .bg(AMBER)
                .add_modifier(Modifier::BOLD),
        ));
        top.push(Span::raw("  "));
    }

    // The untested-writes count is the whole reason the window is open, so it is
    // a filled badge rather than another line of coloured text.
    if awaiting > 0 {
        top.push(Span::styled(
            format!(" {} AWAITING TEST ", awaiting),
            Style::default()
                .fg(INK)
                .bg(GOLD)
                .add_modifier(Modifier::BOLD),
        ));
    } else if ack == 0 && blocked == 0 && errored == 0 {
        // Only claim this when nothing at all wants a human -- saying "all
        // caught up" beside a stalled, idle, or dead agent would be a lie.
        top.push(Span::styled(
            " all caught up ",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ));
    }

    top.push(Span::raw("  "));
    if working > 0 {
        top.push(Span::styled(
            format!("◐ {} working", working),
            Style::default().fg(CYAN),
        ));
        top.push(Span::raw("  "));
    }
    if delegated > 0 {
        top.push(Span::styled(
            format!("◇ {} delegated", delegated),
            Style::default().fg(INDIGO),
        ));
        top.push(Span::raw("  "));
    }
    top.push(Span::styled(
        format!("· {} clear", v.clear_count),
        Style::default().fg(DIM),
    ));

    // The status line always leads; below it, the Eye. When the whole tower is
    // drawn, the header shows the `crown` (its foot swapped for a shaft-cap so the
    // stone descends into the list region); otherwise the self-contained five-line
    // scene, and when there is no room at all the one-line Eye engraved in a rule.
    let mut lines = vec![Line::from(top)];
    if signed {
        let watching = crate::scene::Watching {
            name: v.repo,
            path: v.repo_path,
        };
        let w = area.width as usize;
        // Which of the two heights the layout above already budgeted for. Read
        // back off the rect rather than recomputed from the terminal, so the
        // drawing cannot claim more rows than the layout handed it. The row over
        // the scene's own height is the status line this sits under.
        let framed = area.height > crate::scene::HEIGHT_TALL;
        lines.extend(match (mordor, framed) {
            (true, true) => crate::scene::crown_tall(w, v.anim_ms, world, watching),
            (true, false) => crate::scene::crown(w, v.anim_ms, world, watching),
            (false, true) => crate::scene::scene_tall(w, v.anim_ms, world, watching),
            (false, false) => crate::scene::scene(w, v.anim_ms, world, watching),
        });
    } else {
        lines.push(engraved_rule(area.width as usize, v.anim_ms));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// One pose of the lidless Eye. It is drawn in a fixed five glyphs -- two lashes
/// around a three-cell iris -- so the pupil can slide left/right without the
/// header reflowing, and a blink or a widening swaps glyphs in place.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Eye {
    Center,
    Left,
    Right,
    Blink,
    Wide,
}

/// The Eye's gaze on a 12-second loop: mostly staring you down, glancing aside
/// now and then, blinking, once flaring wide -- and a double-blink that reads as
/// a wink. A pure function of the clock, so the schedule is testable and no
/// per-frame state has to live anywhere.
fn eye_pose(ms: u64) -> Eye {
    match ms % 12_000 {
        0..=2_399 => Eye::Center,
        2_400..=2_699 => Eye::Blink,
        2_700..=4_699 => Eye::Left,
        4_700..=6_899 => Eye::Center,
        6_900..=7_049 => Eye::Blink, // \
        7_050..=7_199 => Eye::Center, //  > two quick blinks -- a wink
        7_200..=7_349 => Eye::Blink, // /
        7_350..=9_499 => Eye::Right,
        9_500..=11_499 => Eye::Center,
        11_500..=11_799 => Eye::Wide, // wary flare before it settles again
        _ => Eye::Center,
    }
}

/// The Eye as coloured spans: fiery lashes and iris, a dark slit for the pupil.
/// Always exactly five glyphs wide.
fn eye(ms: u64) -> Vec<Span<'static>> {
    let flame = Style::default().fg(FLAME);
    let pupil = Style::default().fg(EMBER);
    let wide = Style::default().fg(FLARE).add_modifier(Modifier::BOLD);
    let (l, r, cells): (&str, &str, [(&str, Style); 3]) = match eye_pose(ms) {
        Eye::Center => ("‹", "›", [("▒", flame), ("▮", pupil), ("▒", flame)]),
        Eye::Left => ("‹", "›", [("▮", pupil), ("▒", flame), ("▒", flame)]),
        Eye::Right => ("‹", "›", [("▒", flame), ("▒", flame), ("▮", pupil)]),
        Eye::Blink => ("‹", "›", [("─", flame), ("─", flame), ("─", flame)]),
        Eye::Wide => ("«", "»", [("▓", wide), ("▮", pupil), ("▓", wide)]),
    };
    let mut spans = Vec::with_capacity(5);
    spans.push(Span::styled(l, flame));
    for (g, st) in cells {
        spans.push(Span::styled(g, st));
    }
    spans.push(Span::styled(r, flame));
    spans
}

/// The One Ring's verse in the Black Speech -- the tongue Sauron set in Elvish
/// letters -- one line at a time, advancing every four seconds. In order it
/// reads: "one Ring to rule them all, one Ring to find them, one Ring to bring
/// them all, and in the darkness bind them."
fn inscription(ms: u64) -> &'static str {
    const LINES: [&str; 4] = [
        "ash nazg durbatulûk",
        "ash nazg gimbatul",
        "ash nazg thrakatulûk",
        "agh burzum-ishi krimpatul",
    ];
    LINES[((ms / 4_000) % 4) as usize]
}

/// The divider under the header, engraved like the Ring in the fire: a line of
/// the inscription with the Eye burning at the right margin. Falls back to a
/// plain rule when the terminal is too narrow for the verse or the Eye.
fn engraved_rule(width: usize, ms: u64) -> Line<'static> {
    const EYE_W: usize = 5;
    let motto = inscription(ms);
    let motto_w = motto.chars().count();

    let mut spans: Vec<Span<'static>> = Vec::new();
    if width >= motto_w + EYE_W + 13 {
        // "── " + motto + " " + fill + " " + eye
        let fill = width - motto_w - EYE_W - 5;
        spans.push(Span::styled("──", Style::default().fg(DIM)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(motto, Style::default().fg(RUNE)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("─".repeat(fill), Style::default().fg(DIM)));
        spans.push(Span::raw(" "));
    } else if width >= EYE_W + 2 {
        spans.push(Span::styled(
            "─".repeat(width - EYE_W - 1),
            Style::default().fg(DIM),
        ));
        spans.push(Span::raw(" "));
    } else {
        // No room even for the Eye -- just rule the full width.
        return Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(DIM),
        ));
    }
    spans.extend(eye(ms));
    Line::from(spans)
}

fn list(f: &mut Frame, area: Rect, v: &View, list_state: &mut ListState, geo: &mut FrameGeometry) {
    let width = area.width.saturating_sub(1) as usize;

    geo.list_top = area.y;
    geo.list_height = area.height;
    geo.item_heights.clear();
    geo.item_rows.clear();

    if v.rows.is_empty() && v.clear_count == 0 {
        // Two distinct empty boards: this repo has logs but nothing outstanding,
        // versus no agent has ever run here. The second one looks identical and
        // is the one people mistake for a bug, so it names the path being
        // watched -- the tool stays up and fills in when a session starts.
        let text = match v.awaiting_log_dir {
            Some(dir) => vec![
                Line::raw("No agent sessions here yet — watching for the first one."),
                Line::raw(""),
                Line::styled(format!("  {dir}"), Style::default().fg(DIM)),
            ],
            None => vec![Line::raw("No sessions with repo edits yet.")],
        };
        // Wrapped, not clipped: the whole point of printing the path is that the
        // user can compare it against the repo they meant, and an encoded project
        // dir is longer than the list pane is wide.
        let empty = Paragraph::new(text)
            .style(Style::default().fg(DIM))
            .wrap(Wrap { trim: false });
        f.render_widget(empty, area);
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_item = 0usize;
    let mut current: Option<Status> = None;

    // Push an item while recording its height and which row (if any) it maps to,
    // so a later mouse click can be resolved to the row under the cursor.
    let mut push = |items: &mut Vec<ListItem<'static>>, item: ListItem<'static>, row: Option<usize>| {
        geo.item_heights.push(item.height() as u16);
        geo.item_rows.push(row);
        items.push(item);
    };

    for (i, r) in v.rows.iter().enumerate() {
        if current != Some(r.status) {
            let n = v.rows.iter().filter(|x| x.status == r.status).count();
            push(&mut items, section_header(r.status, n, width), None);
            current = Some(r.status);
        }
        if i == v.selected {
            selected_item = items.len();
        }
        push(&mut items, card(r, i == v.selected, v.now, v.local_offset, width), Some(i));
    }

    // Idle sessions carry no action, so they collapse to one line unless asked
    // for. This is the difference between a scannable window and a wall.
    if !v.show_clear && v.clear_count > 0 {
        let collapse = ListItem::new(vec![
            Line::raw(""),
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("· {} clear", v.clear_count),
                    Style::default().fg(DIM),
                ),
                Span::styled("  — press c to show", Style::default().fg(DIM)),
            ]),
        ]);
        push(&mut items, collapse, None);
    }

    list_state.select(if v.rows.is_empty() {
        None
    } else {
        Some(selected_item)
    });
    f.render_stateful_widget(List::new(items), area, list_state);
}

/// A coloured rule naming the group, so the eye lands on the boundary between
/// "needs me" and "does not" without reading any labels.
fn section_header(status: Status, count: usize, width: usize) -> ListItem<'static> {
    let (label, color) = match status {
        Status::Errored => ("ERRORED", MAGENTA),
        Status::Blocked => ("WAITING ON YOU", RED),
        Status::AwaitingAck => ("AWAITING ACKNOWLEDGEMENT", AMBER),
        Status::NeedsTest => ("AWAITING TESTING", GOLD),
        Status::Working => ("WORKING", CYAN),
        Status::Delegated => ("RUNNING A BACKGROUND AGENT", INDIGO),
        Status::Clear => ("CLEAR", DIM),
    };
    let text = format!(" {} ({}) ", label, count);
    let fill = width.saturating_sub(text.chars().count() + 1);

    ListItem::new(vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled("─".repeat(fill), Style::default().fg(DIM)),
        ]),
    ])
}

/// The task-timing line: "running 4m 12s  ·  started 3:42 PM" while an agent is
/// mid-turn, "took 4m 12s  ·  started 3:42 PM" once it has stopped. The elapsed
/// span is measured from the turn's start to `now` for a live task (so it ticks
/// each refresh) or to its last activity for a settled one. Returns None when no
/// turn start was ever recorded, so a session reconstructed from a partial log
/// simply shows no clock rather than a bogus one.
fn timing_line(row: &Row, now: i64, offset: i64) -> Option<Line<'static>> {
    if row.turn_started <= 0 {
        return None;
    }
    let running = row.status == Status::Working;
    let end = if running { now } else { row.last_activity };
    let dur = fmt_duration(end.saturating_sub(row.turn_started));
    let (verb, dur_style) = if running {
        (
            "running",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )
    } else {
        ("took", Style::default().fg(SAID))
    };
    let clock = fmt_clock(local_time(row.turn_started, offset));
    Some(Line::from(vec![
        Span::raw("     "),
        Span::styled(format!("{} ", verb), Style::default().fg(DIM)),
        Span::styled(dur, dur_style),
        Span::styled("  ·  started ", Style::default().fg(DIM)),
        Span::styled(clock, Style::default().fg(SAID)),
    ]))
}

fn card(row: &Row, selected: bool, now: i64, offset: i64, width: usize) -> ListItem<'static> {
    let color = color_of(row.status);

    let marker = if selected {
        Span::styled("▎", Style::default().fg(color))
    } else {
        Span::raw(" ")
    };

    let title_style = match row.status {
        Status::Errored => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        Status::Blocked => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        Status::AwaitingAck => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        Status::NeedsTest => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        Status::Working => Style::default().fg(Color::Rgb(200, 210, 220)),
        Status::Delegated => Style::default().fg(Color::Rgb(200, 210, 220)),
        Status::Clear => Style::default().fg(DIM),
    };

    let age = ago(row.last_activity, now);
    // The status word rides on every card, in the state's own colour, so a task's
    // status reads off the card itself -- the section header that names the group
    // scrolls away, the glyph alone means nothing until the palette is learned.
    let tag = row.status.tag();
    // A green "orc" badge marks sauron's own maintenance agents, distinct from
    // the hobbits. It sits ahead of the name so it never gets truncated away.
    let orc_room = if row.is_orc { 6 } else { 0 };
    // Both the tag and the orc badge are protected from truncation -- only the
    // name gives up columns when the card is narrow.
    let name_room = width
        .saturating_sub(age.chars().count() + 12 + orc_room + tag.chars().count() + 1)
        .max(12);

    let mut first_spans = vec![
        marker.clone(),
        Span::styled(format!("{} ", glyph_of(row.status)), Style::default().fg(color)),
        Span::styled(format!("{} ", tag), Style::default().fg(color)),
    ];
    if row.is_orc {
        first_spans.push(Span::styled(
            " orc ",
            Style::default().fg(INK).bg(ORC).add_modifier(Modifier::BOLD),
        ));
        first_spans.push(Span::raw(" "));
    }
    first_spans.push(Span::styled(truncate(&row.name, name_room), title_style));
    first_spans.push(Span::raw("  "));
    first_spans.push(Span::styled(age, Style::default().fg(DIM)));
    let first = Line::from(first_spans);

    // A clear session has nothing to say beyond its name -- one line, no files.
    if row.status == Status::Clear {
        return ListItem::new(vec![first]);
    }

    let detail_color = match row.status {
        Status::Errored => MAGENTA,
        Status::AwaitingAck => AMBER,
        Status::NeedsTest => GOLD,
        Status::Delegated => INDIGO,
        _ => DIM,
    };
    // A dead agent's file count is beside the point -- name the failure.
    let summary = if row.status == Status::Errored {
        row.error
            .map(|e| e.short())
            .unwrap_or("turn ended on a failure")
            .to_string()
    } else if row.status == Status::Blocked {
        // The ask itself is now quoted above the summary, so this line is just
        // the reason it is stalled -- no longer a second copy of the prompt.
        row.blocked_reason
            .map(|r| r.short())
            .unwrap_or("waiting on you")
            .to_string()
    } else if row.status == Status::AwaitingAck {
        // Idle at the prompt with nothing to test -- it wants a reply, not a run.
        row.blocked_reason
            .map(|r| r.short())
            .unwrap_or("stopped — your move")
            .to_string()
    } else if row.status == Status::Delegated {
        "background agent running — resumes on its own".to_string()
    } else if row.pending.is_empty() {
        format!("{} file(s) · all acked", row.total_edits)
    } else {
        format!(
            "{} file(s) · {}",
            row.pending.len(),
            truncate(&dim_common(&row.pending), width.saturating_sub(22))
        )
    };

    // The task clock, right under the name: how long this turn has run (live for a
    // Working agent, settled for one that has stopped) and the local wall-clock time
    // it began. This is the "how long has it been going, and since when" a
    // supervisor asks of every active card, which the age tag alone never answered.
    let mut lines = vec![first];
    if let Some(t) = timing_line(row, now, offset) {
        lines.push(t);
    }

    // Quote the last thing you told this session, up to three lines, right under
    // its name -- the quickest way to reload what you had in mind for it without
    // switching to its terminal.
    if let Some(prompt) = &row.last_prompt {
        for pl in wrap_prompt(prompt, width.saturating_sub(7), 3) {
            lines.push(Line::from(vec![
                Span::raw("     "),
                Span::styled("▌ ", Style::default().fg(DIM)),
                Span::styled(pl, Style::default().fg(SAID)),
            ]));
        }
    }

    // The selected card opens up: every modified file gets its own line, named
    // plainly, with the most recent lines written to it shown underneath -- so
    // re-orienting on the active session never means switching to its terminal.
    // Unselected cards stay a single summary line, or the list stops being
    // scannable, which is the whole thing this tool is for.
    if selected && !row.pending.is_empty() {
        const MAX_FILES: usize = 4;
        const MAX_PREVIEW: usize = 2;
        for path in row.pending.iter().take(MAX_FILES) {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    path.clone(),
                    Style::default().fg(FILE).add_modifier(Modifier::BOLD),
                ),
            ]));
            for pl in row.previews.get(path).into_iter().flatten().take(MAX_PREVIEW) {
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled("│ ", Style::default().fg(DIM)),
                    Span::styled(
                        truncate(pl.trim_end(), width.saturating_sub(7)),
                        Style::default().fg(PREVIEW),
                    ),
                ]));
            }
        }
        if row.pending.len() > MAX_FILES {
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    format!("… and {} more", row.pending.len() - MAX_FILES),
                    Style::default().fg(DIM),
                ),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            marker,
            Span::raw("   "),
            Span::styled(summary, Style::default().fg(detail_color)),
        ]));
    }
    lines.push(Line::raw(""));
    ListItem::new(lines)
}

/// The first `max_lines` display lines of a prompt, wrapped to `width`. Hard
/// line breaks in the message are honoured, blank lines are dropped (they carry
/// nothing for a re-brief), an over-long word is split rather than overflowing,
/// and if the message runs past the cap the last kept line ends in an ellipsis
/// so the truncation is visible rather than silent.
fn wrap_prompt(prompt: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(8);
    let cap = max_lines + 1; // wrap one extra line so overflow is detectable
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();

    'outer: for raw in prompt.lines() {
        let line = crate::model::collapse_ws(raw);
        if line.is_empty() {
            continue;
        }
        for word in line.split(' ') {
            let mut word = word.to_string();
            // A single word wider than the card: hard-split it across lines.
            while word.chars().count() > width {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    if out.len() >= cap {
                        break 'outer;
                    }
                }
                out.push(word.chars().take(width).collect());
                if out.len() >= cap {
                    break 'outer;
                }
                word = word.chars().skip(width).collect();
            }
            let need = if cur.is_empty() {
                word.chars().count()
            } else {
                cur.chars().count() + 1 + word.chars().count()
            };
            if need > width {
                out.push(std::mem::take(&mut cur));
                cur = word;
                if out.len() >= cap {
                    break 'outer;
                }
            } else if cur.is_empty() {
                cur = word;
            } else {
                cur.push(' ');
                cur.push_str(&word);
            }
        }
        // A hard line break in the source ends the current display line.
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            if out.len() >= cap {
                break 'outer;
            }
        }
    }
    if !cur.is_empty() && out.len() < cap {
        out.push(cur);
    }

    let truncated = out.len() > max_lines;
    out.truncate(max_lines);
    if truncated {
        if let Some(last) = out.last_mut() {
            while last.chars().count() > width.saturating_sub(1) {
                last.pop();
            }
            last.push('…');
        }
    }
    out
}

fn detail(f: &mut Frame, area: Rect, row: Option<&Row>, now: i64, offset: i64) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(" detail ", Style::default().fg(DIM)));

    let Some(row) = row else {
        f.render_widget(
            Paragraph::new(Line::styled(
                "nothing selected",
                Style::default().fg(DIM),
            ))
            .block(block),
            area,
        );
        return;
    };

    let color = color_of(row.status);
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{} ", glyph_of(row.status)), Style::default().fg(color)),
        Span::styled(row.id_short.clone(), Style::default().fg(DIM)),
        Span::raw("  "),
        Span::styled(
            row.branch.clone().unwrap_or_else(|| "?".into()),
            Style::default().fg(BLUE),
        ),
        Span::raw("  "),
        Span::styled(
            format!("last activity {} ago", ago(row.last_activity, now)),
            Style::default().fg(DIM),
        ),
    ])];

    // The task clock in full: elapsed run (live for a working agent, settled once
    // stopped) and the local time it began, mirroring the card's timing line.
    if row.turn_started > 0 {
        let running = row.status == Status::Working;
        let end = if running { now } else { row.last_activity };
        let dur = fmt_duration(end.saturating_sub(row.turn_started));
        let clock = fmt_clock(local_time(row.turn_started, offset));
        let verb = if running { "running for" } else { "took" };
        lines.push(Line::from(vec![
            Span::styled(format!("{} ", verb), Style::default().fg(DIM)),
            Span::styled(
                dur,
                if running {
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(SAID)
                },
            ),
            Span::styled("  ·  started ", Style::default().fg(DIM)),
            Span::styled(clock, Style::default().fg(SAID)),
        ]));
    }

    match row.status {
        Status::Errored => {
            let text = match row.error {
                Some(e) => format!("{} — switch to its terminal", e.detail()),
                None => "turn ended on a failure".to_string(),
            };
            lines.push(Line::styled(
                text,
                Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
            ));
        }
        Status::Blocked => {
            let text = match row.blocked_reason {
                Some(r) => format!("{} — switch to its terminal, or a to set aside", r.detail()),
                None => "waiting on you".to_string(),
            };
            lines.push(Line::styled(
                text,
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ));
        }
        Status::AwaitingAck => {
            let text = match row.blocked_reason {
                Some(r) => format!("{} — reply in its terminal, or a to acknowledge", r.detail()),
                None => "stopped at the prompt — waiting on your reply".to_string(),
            };
            lines.push(Line::styled(
                text,
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ));
        }
        Status::Working => lines.push(Line::styled(
            "agent is mid-turn — files still moving, do not test yet",
            Style::default().fg(CYAN),
        )),
        Status::Delegated => lines.push(Line::styled(
            "spun up a background agent — waiting on it, not on you; resumes on its own",
            Style::default().fg(INDIGO),
        )),
        Status::Clear => lines.push(Line::styled(
            "nothing outstanding",
            Style::default().fg(GREEN),
        )),
        Status::NeedsTest => {
            lines.push(Line::styled(
                format!("{} untested write(s) — press a when checked", row.pending.len()),
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
            ));
        }
    }

    // Past a handful the pane stops being scannable, which is the overhead this
    // tool exists to remove.
    for p in row.pending.iter().take(4) {
        lines.push(Line::from(vec![
            Span::styled("  › ", Style::default().fg(color)),
            Span::styled(p.clone(), Style::default().fg(Color::Rgb(210, 216, 224))),
        ]));
    }
    if row.pending.len() > 4 {
        lines.push(Line::styled(
            format!("    … and {} more", row.pending.len() - 4),
            Style::default().fg(DIM),
        ));
    }

    if let Some(prompt) = &row.last_prompt {
        lines.push(Line::from(vec![
            Span::styled("ask: ", Style::default().fg(DIM)),
            Span::styled(
                truncate(prompt, 200),
                Style::default().fg(Color::Rgb(150, 158, 170)),
            ),
        ]));
    }

    // The resume command, always present -- this is what lets a dropped thread
    // be picked back up. y (or a click) copies it.
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("continue ", Style::default().fg(DIM)),
        Span::styled("[y / click] ", Style::default().fg(BLUE)),
        Span::styled(
            row.continue_cmd.clone(),
            Style::default().fg(Color::Rgb(190, 200, 210)),
        ),
    ]));

    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

/// The key hints and, at the right edge, the flash from the last action.
///
/// The line does not wrap, so it is fitted rather than emitted whole: the flash
/// is measured first and its room set aside, then hints are taken in priority
/// order until the width runs out. A sauron pane in a workspace is often 52-70
/// columns, which is narrower than the full hint list -- laying it out
/// unconditionally silently ate whatever came last, and what comes last is the
/// message telling you whether the pane you just asked for actually opened.
fn footer(f: &mut Frame, area: Rect, v: &View) {
    // Right edge first: it is the reason the fit exists, so it never gets cut.
    let flash: Option<(String, Color)> = if let Some((msg, ok)) = v.spawned {
        // The spawn result outranks the rest of the corner: it is the only one
        // that reports on something outside this pane, and the only one that
        // can be bad news.
        Some((msg.to_string(), if ok { GREEN } else { AMBER }))
    } else if v.copied {
        Some(("continue command copied".into(), GREEN))
    } else if v.saved {
        Some(("saved".into(), GREEN))
    } else {
        None
    };

    // Priority order, not display convenience: the keys that act come before the
    // keys that toggle a view, and `q` is last because nobody needs telling.
    let mut hints: Vec<(&str, String)> = vec![
        ("j/k", "move".into()),
        // "ack/defer", not "ack/dismiss": `D` is dismiss now, and two keys
        // labelled with the same word is worse than a slightly stiffer one.
        ("a", "ack/defer".into()),
        ("n", "new pane".into()),
        ("⏎", "open pane".into()),
        ("O", "orc".into()),
        ("y", "copy".into()),
        ("u", "undo".into()),
        ("D", "dismiss".into()),
        ("A", "ack all".into()),
        (
            "c",
            if v.show_clear { "hide clear" } else { "show clear" }.into(),
        ),
        ("q", "quit".into()),
    ];
    if v.hidden_stale > 0 {
        hints.push(("o", format!("+{} older", v.hidden_stale)));
    }

    let budget = area.width as usize;
    let mut used = 1 + flash.as_ref().map_or(0, |(m, _)| m.chars().count());
    let mut spans = vec![Span::raw(" ")];
    for (k, what) in &hints {
        let w = k.chars().count() + 1 + what.chars().count() + 2;
        if used + w > budget {
            continue; // a wide hint drops out; a later narrow one may still fit
        }
        used += w;
        spans.push(Span::styled(
            *k,
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {}  ", what),
            Style::default().fg(DIM),
        ));
    }
    if let Some((msg, colour)) = flash {
        spans.push(Span::styled(
            msg,
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Compress a path list to a shared prefix plus leaf names:
/// `src/gui/letters/{mod.rs, render.rs}`. Long lists are the norm here and the
/// shared prefix carries most of the meaning.
fn dim_common(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    if paths.len() == 1 {
        return paths[0].clone();
    }

    let split: Vec<Vec<&str>> = paths.iter().map(|p| p.split('/').collect()).collect();
    let mut common = 0usize;
    'outer: loop {
        let Some(first) = split[0].get(common) else {
            break;
        };
        // Never consume the final component -- that would leave nothing to list.
        if common + 1 >= split[0].len() {
            break;
        }
        for s in &split {
            if s.get(common) != Some(first) || common + 1 >= s.len() {
                break 'outer;
            }
        }
        common += 1;
    }

    let prefix = split[0][..common].join("/");
    let leaves: Vec<String> = split.iter().map(|s| s[common..].join("/")).collect();

    if prefix.is_empty() {
        leaves.join(", ")
    } else {
        format!("{}/{{{}}}", prefix, leaves.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_shared_directory_prefix() {
        let paths = vec![
            "src/gui/windows/letters/mod.rs".to_string(),
            "src/gui/windows/letters/render.rs".to_string(),
        ];
        assert_eq!(
            dim_common(&paths),
            "src/gui/windows/letters/{mod.rs, render.rs}"
        );
    }

    #[test]
    fn handles_disjoint_and_single_paths() {
        assert_eq!(dim_common(&["a.rs".to_string()]), "a.rs");
        let d = dim_common(&["src/a.rs".into(), "tools/b.rs".into()]);
        assert_eq!(d, "src/a.rs, tools/b.rs");
        assert_eq!(dim_common(&[]), "");
    }

    #[test]
    fn does_not_swallow_the_leaf_when_paths_nest() {
        let d = dim_common(&["src/a.rs".into(), "src/sub/b.rs".into()]);
        assert_eq!(d, "src/{a.rs, sub/b.rs}");
    }

    #[test]
    fn each_status_owns_a_distinct_colour() {
        // The palette is the primary signal; two states sharing a hue would make
        // the colour meaningless.
        assert_ne!(color_of(Status::NeedsTest), color_of(Status::Working));
        assert_ne!(color_of(Status::NeedsTest), color_of(Status::Clear));
        assert_ne!(color_of(Status::Working), color_of(Status::Clear));
        // Errored must not read as Blocked -- the whole point is that a dead
        // agent is a different thing from a polite "waiting on you".
        assert_ne!(color_of(Status::Errored), color_of(Status::Blocked));
        // Awaiting acknowledgement (a reply) and awaiting testing (a run) are the
        // distinction this board exists to draw -- they must not share a hue, nor
        // a glyph, and neither may read as the "stuck on a question" Blocked.
        assert_ne!(color_of(Status::AwaitingAck), color_of(Status::NeedsTest));
        assert_ne!(color_of(Status::AwaitingAck), color_of(Status::Blocked));
        assert_ne!(glyph_of(Status::AwaitingAck), glyph_of(Status::NeedsTest));
    }

    #[test]
    fn eye_timeline_hits_each_pose_and_loops() {
        assert_eq!(eye_pose(0), Eye::Center);
        assert_eq!(eye_pose(2_500), Eye::Blink);
        assert_eq!(eye_pose(3_000), Eye::Left);
        assert_eq!(eye_pose(8_000), Eye::Right);
        assert_eq!(eye_pose(11_600), Eye::Wide);
        // The whole act repeats every 12 seconds -- the animation carries no
        // state, so the same clock phase must always give the same pose.
        assert_eq!(eye_pose(3_000), eye_pose(3_000 + 12_000));
    }

    #[test]
    fn eye_is_always_five_glyphs() {
        // The header reserves exactly five cells; a pose that drew more or fewer
        // would push the divider around every blink.
        for ms in [0u64, 2_500, 3_000, 8_000, 11_600, 999_999] {
            assert_eq!(eye(ms).len(), 5, "pose at {ms}ms was not five glyphs");
        }
    }

    /// The header has three rungs and the terminal's rows pick which. The frame
    /// is the expensive one -- nine rows of header against the compact six --
    /// and it is only correct while the list still has rows to show sessions in.
    /// A short terminal that framed its name would have spent the list on
    /// chrome, which is the trade this ladder exists to refuse.
    #[test]
    fn the_header_frames_the_name_only_on_a_terminal_that_can_spare_the_rows() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let screen = |h: u16| {
            let mut terminal = Terminal::new(TestBackend::new(52, h)).unwrap();
            let view = View {
                rows: &[],
                selected: 0,
                now: 0,
                repo: "demo",
                repo_path: "~/src/demo",
                saved: false,
                hidden_stale: 0,
                clear_count: 0,
                show_clear: false,
                copied: false,
                spawned: None,
                anim_ms: 0,
                local_offset: 0,
                awaiting_log_dir: None,
                pick: None,
            };
            let mut ls = ListState::default();
            let mut geo = FrameGeometry::default();
            terminal.draw(|f| draw(f, &view, &mut ls, &mut geo)).unwrap();
            let buf = terminal.backend().buffer().clone();
            (0..h)
                .map(|y| (0..52u16).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Tall enough: the name is framed, and the path hangs on the frame.
        let framed = screen(crate::scene::TALL_MIN_H);
        assert!(framed.contains("╭─ ~/src/demo"), "no frame on a tall terminal: {framed}");

        // A row short of that: the compact header, which still names the repo in
        // block letters but pays no frame -- the path keeps its own row instead.
        let compact = screen(crate::scene::TALL_MIN_H - 1);
        assert!(!compact.contains("╭─ ~/src/demo"), "framed below the threshold: {compact}");
        assert!(compact.contains("~/src/demo"), "the compact header lost the path: {compact}");

        // Shorter still: no scene at all, and the name falls back to the badge on
        // the status line rather than disappearing.
        let collapsed = screen(20);
        assert!(collapsed.contains("DEMO"), "the name vanished on a short terminal: {collapsed}");
    }

    #[test]
    fn a_narrow_footer_keeps_the_spawn_result_and_the_keys_that_act() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        // 52 columns -- the width of a sauron pane in a four-agent workspace, and
        // narrower than the full hint list.
        let mut terminal = Terminal::new(TestBackend::new(52, 24)).unwrap();
        let view = View {
            rows: &[],
            selected: 0,
            now: 0,
            repo: "demo",
            repo_path: "~/src/demo",
            saved: false,
            hidden_stale: 0,
            clear_count: 0,
            show_clear: false,
            copied: false,
            spawned: Some(("no agent column left of sauron", false)),
            anim_ms: 0,
            local_offset: 0,
            awaiting_log_dir: None,
            pick: None,
        };
        let mut ls = ListState::default();
        let mut geo = FrameGeometry::default();
        terminal
            .draw(|f| draw(f, &view, &mut ls, &mut geo))
            .unwrap();

        let buf = terminal.backend().buffer();
        let mut line = String::new();
        for x in 0..52u16 {
            line.push_str(buf[(x, 23)].symbol());
        }
        // The failure message is why the fit exists -- it survives whole.
        assert!(
            line.contains("no agent column left of sauron"),
            "spawn result was truncated away: {line:?}"
        );
        // And the hints that fit are the ones that do something, not `q quit`.
        assert!(line.contains("j/k"), "footer lost the move hint: {line:?}");
        assert!(!line.contains("quit"), "footer kept a low-priority hint over the flash: {line:?}");
    }

    /// The picker is the whole GUI dispatch surface, so it gets drawn for real
    /// rather than trusted. Two things have to survive: the ranking evidence, and
    /// the count of what was held back -- a picker that silently hides contested
    /// files reads as "there was nothing else", which is a different claim.
    #[test]
    fn the_orc_picker_shows_its_ranking_evidence_and_what_it_held_back() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let cold = vec![
            crate::orc::Target {
                path: "sauron/src/clip/store.rs".into(),
                loc: 1178,
                churn: 4,
                score: 1338,
            },
            crate::orc::Target {
                path: "sauron/src/clip/mod.rs".into(),
                loc: 633,
                churn: 0,
                score: 633,
            },
        ];
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        let view = View {
            rows: &[],
            selected: 0,
            now: 0,
            repo: "demo",
            repo_path: "~/src/demo",
            saved: false,
            hidden_stale: 0,
            clear_count: 0,
            show_clear: false,
            copied: false,
            spawned: None,
            anim_ms: 0,
            local_offset: 0,
            awaiting_log_dir: None,
            pick: Some(PickView {
                cold: &cold,
                selected: 0,
                hot: 3,
                dirty: 9,
            }),
        };
        let mut ls = ListState::default();
        let mut geo = FrameGeometry::default();
        terminal.draw(|f| draw(f, &view, &mut ls, &mut geo)).unwrap();

        let buf = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..30u16 {
            for x in 0..90u16 {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        assert!(screen.contains("store.rs"), "picker lost the target: {screen}");
        // The evidence, not just the ordering.
        assert!(screen.contains("1178 loc"), "picker lost the line count");
        assert!(screen.contains("4 commits"), "picker lost the churn evidence");
        // A quiet file says loc only -- no misleading "0 commits".
        assert!(!screen.contains("0 commits"));
        // …and the exclusions are stated out loud.
        assert!(
            screen.contains("3 hot") && screen.contains("9 dirty"),
            "picker hid what it filtered: {screen}"
        );
        // The charge's priority is on the frame, so the dispatch says what it does.
        assert!(screen.contains("decompose first"));
    }

    #[test]
    fn header_engraves_the_verse_and_burns_the_eye() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let view = View {
            rows: &[],
            selected: 0,
            now: 0,
            repo: "demo",
            repo_path: "~/src/demo",
            saved: false,
            hidden_stale: 0,
            clear_count: 0,
            show_clear: false,
            copied: false,
            spawned: None,
            pick: None,
            anim_ms: 0, // clock phase 0 -> Eye centred, verse on its first line
            local_offset: 0,
            awaiting_log_dir: None,
        };
        let mut ls = ListState::default();
        let mut geo = FrameGeometry::default();
        terminal
            .draw(|f| draw(f, &view, &mut ls, &mut geo))
            .unwrap();

        // Row 1 is the engraved divider: the ring-verse plus the lidless Eye.
        let buf = terminal.backend().buffer();
        let mut rule = String::new();
        for x in 0..80u16 {
            rule.push_str(buf[(x, 1)].symbol());
        }
        assert!(rule.contains("ash nazg durbatulûk"), "verse missing: {rule:?}");
        assert!(
            rule.contains('‹') && rule.contains('▮') && rule.contains('›'),
            "Eye missing: {rule:?}"
        );

        // A collapsed header has no room for the block-letter name, so the
        // status line has to carry it -- and carry it as a filled badge, which
        // is the only thing on that row loud enough to be read at a glance from
        // the next pane over.
        let mut top = String::new();
        for x in 0..80u16 {
            top.push_str(buf[(x, 0)].symbol());
        }
        assert!(top.starts_with(" DEMO "), "the short header buried the repo: {top:?}");
        assert_eq!(
            buf[(1u16, 0u16)].bg,
            BLUE,
            "the repo name is not on a filled badge"
        );
    }

    #[test]
    fn a_repo_with_no_logs_yet_still_renders_and_names_what_it_watches() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        // Opening sauron in a fresh folder: no rows, no log directory. It must
        // draw a board rather than refuse to start, and the empty state has to
        // say which path it is waiting on -- otherwise "watching the wrong repo"
        // and "nothing has run here yet" look identical.
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let view = View {
            rows: &[],
            selected: 0,
            now: 0,
            repo: "fresh",
            repo_path: "~/src/fresh",
            saved: false,
            hidden_stale: 0,
            clear_count: 0,
            show_clear: false,
            copied: false,
            spawned: None,
            pick: None,
            anim_ms: 0,
            local_offset: 0,
            awaiting_log_dir: Some("/home/u/.claude/projects/-tmp-fresh"),
        };
        let mut ls = ListState::default();
        let mut geo = FrameGeometry::default();
        terminal
            .draw(|f| draw(f, &view, &mut ls, &mut geo))
            .unwrap();

        let buf = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..24u16 {
            for x in 0..80u16 {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        assert!(
            screen.contains("watching for the first one"),
            "empty state missing: {screen}"
        );
        assert!(
            screen.contains("/home/u/.claude/projects/-tmp-fresh"),
            "watched path missing: {screen}"
        );
        // Nothing selected, so the detail pane must not have been asked for a row.
        assert_eq!(ls.selected(), None);
    }

    #[test]
    fn each_card_carries_its_status_word() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        // A NeedsTest task should print its tag ("needs test") on the card, so the
        // status reads off the card itself and not only off the section header.
        let row = Row {
            id: "abcd1234".into(),
            id_short: "abcd1234".into(),
            name: "wire up the token store".into(),
            branch: Some("main".into()),
            last_activity: 0,
            turn_started: 0,
            status: Status::NeedsTest,
            blocked_reason: None,
            error: None,
            pending: vec!["src/a.rs".into()],
            total_edits: 1,
            last_prompt: None,
            is_orc: false,
            continue_cmd: "claude --resume abcd1234".into(),
            edits: std::collections::BTreeMap::new(),
            previews: std::collections::BTreeMap::new(),
        };
        let view = View {
            rows: std::slice::from_ref(&row),
            selected: 0,
            now: 0,
            repo: "demo",
            repo_path: "~/src/demo",
            saved: false,
            hidden_stale: 0,
            clear_count: 0,
            show_clear: false,
            copied: false,
            spawned: None,
            pick: None,
            anim_ms: 0,
            local_offset: 0,
            awaiting_log_dir: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut ls = ListState::default();
        let mut geo = FrameGeometry::default();
        terminal.draw(|f| draw(f, &view, &mut ls, &mut geo)).unwrap();

        let buf = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..24u16 {
            for x in 0..80u16 {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        assert!(
            screen.contains("needs test"),
            "card is missing its status word:\n{screen}"
        );
    }

    #[test]
    fn wrap_prompt_wraps_a_long_line_and_keeps_the_first_few() {
        let p = "refactor the auth middleware to use the new token store";
        let out = wrap_prompt(p, 20, 3);
        assert!(out.len() <= 3);
        assert!(out.iter().all(|l| l.chars().count() <= 20), "over width: {out:?}");
        assert!(out[0].starts_with("refactor"));
    }

    #[test]
    fn wrap_prompt_honours_hard_breaks_and_drops_blank_lines() {
        // Blank lines carry nothing for a re-brief, so they are skipped, and the
        // three real lines survive intact -- no ellipsis, since nothing is cut.
        let p = "first line\n\n\nsecond line\nthird line";
        assert_eq!(
            wrap_prompt(p, 40, 3),
            vec!["first line", "second line", "third line"]
        );
    }

    #[test]
    fn wrap_prompt_marks_overflow_visibly() {
        let out = wrap_prompt("l1\nl2\nl3\nl4", 40, 3);
        assert_eq!(out.len(), 3);
        assert!(out[2].ends_with('…'), "overflow must be marked: {:?}", out[2]);
    }

    #[test]
    fn wrap_prompt_of_only_whitespace_is_empty() {
        assert!(wrap_prompt("   \n\t\n  ", 40, 3).is_empty());
    }

    #[test]
    fn inscription_cycles_the_four_ring_lines() {
        assert_eq!(inscription(0), "ash nazg durbatulûk");
        assert_eq!(inscription(4_000), "ash nazg gimbatul");
        assert_eq!(inscription(8_000), "ash nazg thrakatulûk");
        assert_eq!(inscription(12_000), "agh burzum-ishi krimpatul");
        // Four lines at four seconds each -- the verse restarts at 16s.
        assert_eq!(inscription(16_000), inscription(0));
    }
}
