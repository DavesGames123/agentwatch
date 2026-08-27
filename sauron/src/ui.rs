//! Rendering.
//!
//! The board is three tables, not one list: YOUR MOVE, AWAITING TESTING and
//! WORKING, in that order, each with its own scroll and its own columns. An
//! empty table is omitted entirely, header and all.
//!
//! One line per session, and the line height is fixed. That is the whole point:
//! columns only line up while nothing expands a row, so the per-row detail --
//! the write-set, the last ask, the resume command -- lives in the detail pane
//! under the board, driven by the selected row. The previous board gave the
//! selected session up to twelve lines and every other session four, which read
//! well at five sessions and was a wall at fifteen.
//!
//! Column widths are solved, not hard-coded: the fixed columns are met first,
//! the name takes what is left with a floor of `NAME_MIN`, and a row that will
//! not fit gives up whole columns in the order `columns` names rather than
//! shaving every column to unreadable. A sauron pane in a four-agent workspace
//! is 52 columns wide, so this is a load-bearing case and not a courtesy.
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
//!   fn look            -- glyph, colour, name style and table for one status
//!   enum Group         -- the three tables, and which status goes in which
//!   fn board           -- the tables: sized, scrolled, and laid down the pane
//!   fn share_rows      -- how a short pane is divided between the tables
//!   fn solve           -- fixed-then-flex column widths for one table
//!   fn table_row       -- one session -> one aligned line
//!   fn fit             -- truncation by display width, not character count
//!   fn detail          -- selected session's write-set and prompt
//!   fn wrap_prompt     -- first lines of your last ask, wrapped for the pane
//!   fn dim_common      -- shared directory prefix compression for path lists
//!   const AMBER/CYAN   -- the status palette

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::servant;
use crate::model::{
    ago, collapse_ws, fmt_clock, fmt_count, fmt_duration, local_time, truncate, Status,
};
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
const SAID: Color = Color::Rgb(158, 166, 178); // your last words, quoted back in the detail pane
const FILE: Color = Color::Rgb(214, 220, 228); // a modified file, named clearly
const PREVIEW: Color = Color::Rgb(132, 140, 152); // its most recent lines of text

// The Eye of Sauron and its engraved verse. Kept apart from the status palette
// above -- this is chrome flavour, never a signal, so it must not borrow a hue
// that means something. Shared with the `scene` module (the five-line Eye).
pub(crate) const FLAME: Color = Color::Rgb(255, 122, 24); // the lidless eye, wreathed in fire
pub(crate) const EMBER: Color = Color::Rgb(120, 22, 10); // the slit pupil, a hole in the flame
pub(crate) const FLARE: Color = Color::Rgb(255, 176, 60); // the eye flaring wide
pub(crate) const RUNE: Color = Color::Rgb(150, 70, 40); // the engraved script, faint

/// Which table a session is listed under.
///
/// The grouping is a contract shared with the other boards (the web app, the
/// in-app panel), so it is stated once here rather than re-derived at each
/// drawing site. Rows arrive ranked by `Status::rank`, and the ranks already run
/// in this order, so a table is a filter over a sorted list and never a re-sort.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    /// Errored, Blocked, AwaitingAck -- nothing moves until you act.
    YourMove,
    /// NeedsTest -- writes on disk that nobody has run yet.
    AwaitingTesting,
    /// Working, Delegated, Stalled -- the agent is on it and wants nothing.
    Working,
    /// Idle sessions, drawn only while `c` has asked for them. Not one of the
    /// contract's three tables and absent from every other surface: it exists so
    /// that the `c` toggle still has somewhere to put what it reveals, which
    /// otherwise would be counted in the header and then rendered nowhere.
    Clear,
}

impl Group {
    /// Render order, top to bottom.
    pub const ALL: [Group; 4] = [
        Group::YourMove,
        Group::AwaitingTesting,
        Group::Working,
        Group::Clear,
    ];

    /// Index into the per-table scroll offsets carried on `View`/`FrameGeometry`.
    pub fn index(self) -> usize {
        match self {
            Group::YourMove => 0,
            Group::AwaitingTesting => 1,
            Group::Working => 2,
            Group::Clear => 3,
        }
    }

}

/// Everything one status contributes to the board, in one record.
///
/// Five parallel `match status` tables used to answer this -- colour, glyph,
/// section rule, name style, reason colour -- which is four chances to forget
/// one. `Status::Stalled` arrived through exactly that gap and rendered with
/// another state's wording. One record per status closes it: a state is
/// described once, completely, or it does not compile.
struct Look {
    glyph: &'static str,
    /// The state's hue, on the glyph and everywhere the state is named.
    color: Color,
    /// How the session name is set: white and bold while the row wants a human,
    /// quieter once it does not.
    name: Style,
    /// The table this status is listed under. The flat board draws no section
    /// headers, but the grouping still drives `Tab` (jump to the next status
    /// cluster) and the worst-status colour.
    group: Group,
}

fn look(status: Status) -> Look {
    // A row that wants a human gets its name in white and bold; one that does not
    // gets it a step quieter. The two greys are the ones the cards used before.
    let wants = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let quiet = Style::default().fg(Color::Rgb(200, 210, 220));
    match status {
        Status::Errored => Look {
            glyph: "✖",
            color: MAGENTA,
            name: wants,
            group: Group::YourMove,
        },
        Status::Blocked => Look {
            glyph: "▲",
            color: RED,
            name: wants,
            group: Group::YourMove,
        },
        // A prompt chevron: the agent is idle at the prompt, awaiting your reply.
        Status::AwaitingAck => Look {
            glyph: "❯",
            color: AMBER,
            name: wants,
            group: Group::YourMove,
        },
        Status::NeedsTest => Look {
            glyph: "█",
            color: GOLD,
            name: wants,
            group: Group::AwaitingTesting,
        },
        Status::Working => Look {
            glyph: "◐",
            color: CYAN,
            name: quiet,
            group: Group::Working,
        },
        Status::Delegated => Look {
            glyph: "◇",
            color: INDIGO,
            name: quiet,
            group: Group::Working,
        },
        // Amber and hedged, in WORKING rather than YOUR MOVE. A long build and an
        // unanswered permission prompt are the same shape in the log, and there
        // are far more long builds -- so this row says "may need approval" and
        // sits with the agents that are getting on with it, never in the band
        // that claims something is certainly waiting on you.
        Status::Stalled => Look {
            glyph: "◔",
            color: AMBER,
            name: quiet,
            group: Group::Working,
        },
        Status::Clear => Look {
            glyph: "·",
            color: DIM,
            name: Style::default().fg(DIM),
            group: Group::Clear,
        },
    }
}

/// The status hue, for anything outside this module that has to agree with the
/// board about what a state looks like.
pub fn color_of(status: Status) -> Color {
    look(status).color
}

/// The status glyph, for anything outside this module that has to agree with
/// the board about what a state looks like -- `--once` prints the same marks.
pub fn glyph_of(status: Status) -> &'static str {
    look(status).glyph
}

/// Which table a status is listed under. The event loop needs it to move the
/// cursor from one table to the next without re-deriving the grouping.
pub fn group_of(status: Status) -> Group {
    look(status).group
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
    /// Per table (indexed by `Group::index`), the first row it is showing. Owned
    /// by the event loop and handed back through `FrameGeometry`, because only
    /// the drawing knows how many rows a table got.
    pub scroll: [usize; GROUPS],
    /// Whether the board should scroll to keep the selected row on screen.
    ///
    /// True after anything that moved the cursor, false after a wheel scroll.
    /// The wheel moves a viewport and not the selection, so a board that always
    /// chased the cursor would snap straight back on the next frame and the
    /// wheel would appear dead.
    pub follow: bool,
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

/// Screen geometry of the last-drawn board, so a mouse event can be resolved to
/// the row or the table under it. Filled by `board`, read by the event loop.
///
/// Per table rather than per item: the board scrolls three windows
/// independently, so "which row is at y" cannot be answered by walking one list
/// from one offset any more.
#[derive(Default)]
pub struct FrameGeometry {
    pub list_top: u16,
    pub list_height: u16,
    /// The tables actually drawn, in draw order. Empty tables are not here,
    /// because they are not on the screen.
    pub tables: Vec<TableGeometry>,
    /// Per table (indexed by `Group::index`), the first body row it is showing,
    /// after clamping and after any scroll to reveal the cursor. Handed back so
    /// the event loop can keep it: the drawing is what knows how many rows fit.
    pub scroll: [usize; GROUPS],
}

/// One drawn table: where its body sits on the screen, and which rows are in it.
#[derive(Clone, Debug)]
pub struct TableGeometry {
    pub group: Group,
    /// Screen y of the table's first body row (the header is the row above).
    pub body_top: u16,
    /// Body rows on the screen. Fewer than `rows.len()` means it is clipped.
    pub body_height: u16,
    /// Indices into `View::rows`, in draw order.
    pub rows: Vec<usize>,
    /// The first of `rows` on screen.
    pub offset: usize,
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

pub fn draw(f: &mut Frame, v: &View, geo: &mut FrameGeometry) {
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

    // The board gives up the shaft's column and the war's rows when Mordor is
    // drawn -- the column solver simply gets a narrower pane, which is the case
    // the 52-column drop order was written for.
    let mut list_area = chunks[1];
    if mordor {
        list_area.width = list_area.width.saturating_sub(tower_w);
        list_area.height = list_area.height.saturating_sub(base_h);
    }

    let world = world_of(v.rows);
    header(f, chunks[0], v, mordor, world);
    board(f, list_area, v, geo);
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

// ---- the board: three tables of one-line rows -------------------------------

/// How many tables the scroll offsets are carried for. `Group::index` is the
/// index into them.
pub const GROUPS: usize = 4;
/// Cells of chrome ahead of the first column on every body row: the selection
/// marker, the status glyph, and a space either side of it. Solved columns
/// divide up whatever is left.
#[allow(dead_code)] // solver test fixtures
const PREFIX: usize = 4;
/// The name column's floor. Below this a name is not a name, so every other
/// column drops before the name gives up its last characters.
const NAME_MIN: usize = 12;
/// The name column's comfort width. Above its floor a name is legible; at the
/// floor it is a stub, and a table of stubs is a table you cannot use. Columns
/// marked `soft` are given up to reach this even on a row that would have fitted
/// with them -- see `solve`.
const NAME_WANT: usize = 24;

/// A column of a table: how wide it wants to be, and how readily it is given up.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Col {
    /// A fixed column is met before any flex column gets a cell of slack.
    fixed: bool,
    /// Fixed width, or a flex column's floor.
    min: usize,
    /// A flex column's claim on the slack, relative to the other flex columns.
    share: usize,
    /// Drop order: the largest surviving number goes first when the row will not
    /// fit. Zero never drops.
    drop: u8,
    /// Whether the column may be given up to widen the name past `NAME_WANT`,
    /// on a row that would have fitted with it. This is the difference between
    /// "the row does not fit" and "the row fits and reads badly": a token count
    /// beside a name cut to a stub is two facts where one would have done.
    ///
    /// The reason column is never soft. It is why the row is on the board, and a
    /// YOUR MOVE table without it is a list of names.
    soft: bool,
}

impl Col {
    const fn fixed(w: usize, drop: u8) -> Col {
        Col { fixed: true, min: w, share: 0, drop, soft: false }
    }
    const fn flex(min: usize, share: usize, drop: u8) -> Col {
        Col { fixed: false, min, share, drop, soft: false }
    }
    /// The same column, given up for the name's sake before the row is full.
    const fn soft(mut self) -> Col {
        self.soft = true;
        self
    }
}

/// Which columns a table draws, left to right, and the order it gives them up in.
///
/// The drop order is a property of the column and not of its position: WORKING
/// gives up its token count (third from the left) before "now doing" (fourth),
/// because a number nobody is reading is worth less than the phrase saying what
/// the agent is doing. The name never drops and neither does the glyph.
///
///   drop 3  tokens        -- a magnitude, and the last thing you would miss
///   drop 2  why / top file / now doing
///   drop 1  the time column
///   drop 0  the name
#[allow(dead_code)] // realistic multi-column fixtures for the solve() tests
fn columns(group: Group) -> &'static [Col] {
    static YOUR_MOVE: [Col; 4] = [
        Col::flex(NAME_MIN, 2, 0),
        Col::fixed(20, 2),        // why
        Col::fixed(7, 1),         // waited
        Col::fixed(7, 3).soft(),  // tokens
    ];
    static AWAITING_TESTING: [Col; 5] = [
        Col::flex(NAME_MIN, 2, 0),
        Col::fixed(5, 2),           // files
        Col::flex(8, 1, 2).soft(),  // top file
        Col::fixed(7, 1),           // since
        Col::fixed(7, 3).soft(),    // tokens
    ];
    static WORKING: [Col; 4] = [
        Col::flex(NAME_MIN, 2, 0),
        Col::fixed(8, 1),            // elapsed
        Col::fixed(7, 3).soft(),     // tokens
        Col::flex(10, 1, 2).soft(),  // now doing
    ];
    static CLEAR: [Col; 3] = [
        Col::flex(NAME_MIN, 2, 0),
        Col::fixed(7, 1),        // since
        Col::fixed(7, 3).soft(), // tokens
    ];
    match group {
        Group::YourMove => &YOUR_MOVE,
        Group::AwaitingTesting => &AWAITING_TESTING,
        Group::Working => &WORKING,
        Group::Clear => &CLEAR,
    }
}

/// What a set of surviving columns needs: their minimums, plus one cell of gap
/// between each neighbouring pair.
fn demand(cols: &[Col], keep: &[bool]) -> usize {
    let n = keep.iter().filter(|k| **k).count();
    let mins: usize = cols
        .iter()
        .zip(keep)
        .filter(|(_, k)| **k)
        .map(|(c, _)| c.min)
        .sum();
    mins + n.saturating_sub(1)
}

/// Solve a table's columns to cell widths for a row `width` cells wide.
///
/// Fixed columns are met first; what is left over is split between the flex
/// columns by share, on top of their floors. A row that will not fit gives up
/// whole columns in `columns` order rather than shaving every column by a
/// character each -- so a 52-column pane shows fewer facts, each still legible,
/// instead of five columns of stubs.
///
/// A returned width of 0 means the column was dropped. There is always one entry
/// per column, so the caller can zip the answer against its cells.
fn solve(cols: &[Col], width: usize) -> Vec<usize> {
    let mut keep = vec![true; cols.len()];
    // The most droppable survivor, or None when everything left is load-bearing.
    let worst = |keep: &[bool], soft_only: bool| {
        (0..cols.len())
            .filter(|&i| keep[i] && cols[i].drop > 0 && (!soft_only || cols[i].soft))
            .max_by_key(|&i| (cols[i].drop, i))
    };
    while demand(cols, &keep) > width {
        // The most droppable survivor goes; equal claims break to the right, so
        // the row loses its rightmost column first.
        match worst(&keep, false) {
            Some(i) => keep[i] = false,
            // Everything droppable is gone and the row still overruns: the
            // remaining columns keep their floors and the renderer clips. A pane
            // this narrow has no layout that would have helped.
            None => break,
        }
    }

    // The row fits, but the name may still be a stub. Soft columns are handed to
    // it until it is workable or there are none left -- which is what makes a
    // 52-column pane show a name you can read instead of five columns of
    // fragments. `name_of` re-solves rather than guessing: the slack a dropped
    // column releases is shared with the other flex columns, not given whole.
    while name_of(cols, &keep, width) < NAME_WANT {
        match worst(&keep, true) {
            Some(i) => keep[i] = false,
            None => break,
        }
    }

    let mut out: Vec<usize> = cols
        .iter()
        .zip(&keep)
        .map(|(c, &k)| if k { c.min } else { 0 })
        .collect();

    let slack = width.saturating_sub(demand(cols, &keep));
    let shares: usize = cols
        .iter()
        .zip(&keep)
        .filter(|(c, &k)| k && !c.fixed)
        .map(|(c, _)| c.share)
        .sum();
    if slack > 0 && shares > 0 {
        let mut handed = 0usize;
        let mut first = None;
        for i in 0..cols.len() {
            if !keep[i] || cols[i].fixed {
                continue;
            }
            first.get_or_insert(i);
            let take = slack * cols[i].share / shares;
            out[i] += take;
            handed += take;
        }
        // The rounding remainder goes to the leftmost flex column -- the name,
        // which is where a spare cell is always worth the most.
        if let Some(i) = first {
            out[i] += slack - handed;
        }
    }
    out
}

/// What the name column comes out at with a given set of columns surviving.
fn name_of(cols: &[Col], keep: &[bool], width: usize) -> usize {
    let slack = width.saturating_sub(demand(cols, keep));
    let shares: usize = cols
        .iter()
        .zip(keep)
        .filter(|(c, &k)| k && !c.fixed)
        .map(|(c, _)| c.share)
        .sum();
    // The name is the leftmost flex column, wherever it sits: index 0 in the
    // grouped tables, and index 2 in the flat spreadsheet, where `#` and
    // `status` are fixed columns ahead of it. `solve` hands the rounding
    // remainder to this same first-flex column, so the two agree.
    let Some(ni) = cols.iter().position(|c| !c.fixed) else {
        return 0;
    };
    let name = cols[ni].min;
    if shares == 0 || !keep.get(ni).copied().unwrap_or(false) {
        return name;
    }
    // The rounding remainder goes to the name, so it is the whole slack less
    // what the other flex columns take.
    let others: usize = cols
        .iter()
        .zip(keep)
        .enumerate()
        .filter(|(i, (c, &k))| *i != ni && k && !c.fixed)
        .map(|(_, (c, _))| slack * c.share / shares)
        .sum();
    name + slack - others
}

/// Display width in terminal cells, not in characters.
///
/// Session names carry CJK and emoji, both of which draw two cells wide, and a
/// column padded by character count under one of those stops lining up with the
/// row above it. `Span` measures the way the renderer draws, which is the only
/// measurement that keeps the columns straight.
fn width_of(s: &str) -> usize {
    Span::raw(s).width()
}

/// Truncate to a display width, marking the cut.
///
/// `truncate` counts characters and owns the ellipsis and the trailing-space
/// trim, so the character count is measured here -- by walking cells -- and the
/// cut itself is left to it. The guard on the way out is for the wide character
/// that straddles the boundary: it costs two cells where the walk had one left.
fn fit(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width_of(s) <= max {
        return s.to_string();
    }
    let mut used = 0usize;
    let mut chars = 0usize;
    for c in s.chars() {
        let mut buf = [0u8; 4];
        let w = width_of(c.encode_utf8(&mut buf));
        if used + w > max - 1 {
            break;
        }
        used += w;
        chars += 1;
    }
    let cut = truncate(s, chars + 1);
    if width_of(&cut) <= max {
        cut
    } else {
        truncate(s, chars)
    }
}

/// One solved cell: its text, its style, and which edge it hugs. Counts and
/// clocks are right-aligned -- a column of ragged numbers is a column nobody
/// compares down.
struct Cell {
    text: String,
    style: Style,
    right: bool,
}

impl Cell {
    fn left(text: String, style: Style) -> Cell {
        Cell { text, style, right: false }
    }
    fn right(text: String, style: Style) -> Cell {
        Cell { text, style, right: true }
    }
}

/// A cell padded to its solved width, so the next column starts where the row
/// above it did.
fn cell_span(c: &Cell, w: usize) -> Span<'static> {
    let text = fit(&c.text, w);
    let pad = " ".repeat(w.saturating_sub(width_of(&text)));
    let filled = if c.right {
        format!("{pad}{text}")
    } else {
        format!("{text}{pad}")
    };
    Span::styled(filled, c.style)
}

/// The token total, or nothing at all. A Codex session and a Claude session
/// whose log carries no `usage` both arrive as zero, and "0" in the column
/// would be a measurement rather than the absence of one.
fn tokens_cell(row: &Row) -> String {
    if row.tokens > 0 {
        fmt_count(row.tokens)
    } else {
        String::new()
    }
}

/// A task's elapsed run: to `now` while it is genuinely running, and to its last
/// activity once it is not. A settled session whose timer kept climbing would be
/// reporting the age of the board rather than the length of the task.
fn elapsed(row: &Row, now: i64) -> String {
    if row.turn_started <= 0 {
        return String::new();
    }
    let end = if row.status == Status::Working {
        now
    } else {
        row.last_activity
    };
    fmt_duration(end.saturating_sub(row.turn_started))
}

/// The name cell: the orc badge, the name, and the servant underline.
///
/// The underline is the same colour the pane running this session is tinted,
/// computed from the session id by both sides rather than agreed between them
/// (see `servant`). This is the join between the board and the screen: a row and
/// a terminal are the same servant when the line under the name matches the
/// pane. It is an underline and not the text colour because the text colour is
/// already spoken for -- `Look::name` says whether this row wants a human, and
/// overwriting that would trade the more urgent fact for the less urgent one.
fn push_name(spans: &mut Vec<Span<'static>>, row: &Row, style: Style, w: usize) {
    const BADGE: usize = 5; // " orc "
    let mut room = w;
    // The badge marks one of sauron's own maintenance agents rather than a
    // session you started, which is a thing the name it chose will not say. It
    // is only worth its cells while a readable name still fits beside it.
    if row.is_orc && w >= BADGE + 1 + NAME_MIN {
        spans.push(Span::styled(
            " orc ",
            Style::default().fg(INK).bg(ORC).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        room -= BADGE + 1;
    }
    let (r, g, b) = servant::color_for(&row.id);
    let style = style
        .add_modifier(Modifier::UNDERLINED)
        .underline_color(Color::Rgb(r, g, b));
    let text = fit(&collapse_ws(&row.name), room);
    let pad = room.saturating_sub(width_of(&text));
    spans.push(Span::styled(text, style));
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
}

/// The flat spreadsheet's columns, one set for every row on the board.
///
///   # · status · task · started · running · ~fin · tokens
///
/// Status is a column here, not a section header: the whole board is one sorted
/// list, ranked worst-first, so the groups still cluster without three loud
/// headings between them. `task` is the one flex column and never drops; the
/// rest give way from the least useful in a narrow pane -- the estimate first,
/// then tokens, the two clocks, the status word, the number -- exactly as their
/// `drop` order says. The name floor is `NAME_MIN`, same as the old tables, so
/// the 52-column pane still shows a name you can read.
static FLAT: [Col; 7] = [
    Col::fixed(3, 1),          // #        the row's number
    Col::fixed(10, 2),         // status   the state word, the only coloured cell
    Col::flex(NAME_MIN, 2, 0), // task     the description, never dropped
    Col::fixed(7, 4),          // started  local wall-clock the turn began at
    Col::fixed(7, 3),          // running  how long it has run / took
    Col::fixed(4, 6).soft(),   // ~fin     estimate, working rows only
    Col::fixed(6, 5).soft(),   // tokens   billed total, or blank
];

/// Which column holds the name, for `flat_row` and the solver's floor.
const FLAT_NAME: usize = 2;

/// The selection bar plus its one space; the status word carries the colour, so
/// the glyph the grouped board drew here is gone.
const FLAT_PREFIX: usize = 2;

const FLAT_LABELS: [&str; 7] = ["#", "status", "task", "started", "running", "~fin", "tokens"];

/// Whether a column's cell (and its header label) hugs the right edge -- the
/// number, the two clocks, the estimate and the token count all do, because a
/// column of ragged figures is a column nobody reads down.
fn flat_right(i: usize) -> bool {
    matches!(i, 0 | 3 | 4 | 5 | 6)
}

/// One row's cells, in `FLAT` order. The task cell is left empty: `push_name`
/// draws it, because it carries the orc badge and the servant underline and is
/// not one span.
fn flat_cells(row: &Row, num: usize, now: i64, offset: i64) -> Vec<Cell> {
    let l = look(row.status);
    let dim = Style::default().fg(DIM);
    let running = matches!(
        row.status,
        Status::Working | Status::Delegated | Status::Stalled
    );
    let started = if row.turn_started > 0 {
        fmt_clock(local_time(row.turn_started, offset))
    } else {
        "—".into()
    };
    let run = elapsed(row, now);
    let run = if run.is_empty() { "—".to_string() } else { run };
    // The estimate has no honest source -- the log carries turns, not plans --
    // so a running row gets a bare "~" that reads as "ongoing, no estimate" and
    // a settled one gets a dash. It is a column the board was asked for, kept
    // truthful rather than filled with a guessed clock.
    let fin = if running { "~".to_string() } else { "—".to_string() };
    vec![
        Cell::right(num.to_string(), dim),
        Cell::left(row.status.tag().to_string(), Style::default().fg(l.color)),
        Cell::left(String::new(), Style::default()),
        Cell::right(started, dim),
        Cell::right(
            run,
            if row.status == Status::Working {
                Style::default().fg(CYAN)
            } else {
                dim
            },
        ),
        Cell::right(fin, dim),
        Cell::right(tokens_cell(row), dim),
    ]
}

/// One session, one line, the flat columns solved for this pane's width. `num`
/// is the row's 1-based place on the board -- the `#` column.
fn flat_row(
    row: &Row,
    num: usize,
    selected: bool,
    now: i64,
    offset: i64,
    width: usize,
) -> Line<'static> {
    let l = look(row.status);
    let widths = solve(&FLAT, width.saturating_sub(FLAT_PREFIX));
    let cells = flat_cells(row, num, now, offset);

    let mut spans = vec![
        Span::styled(
            if selected { "▎" } else { " " },
            Style::default().fg(l.color),
        ),
        Span::raw(" "),
    ];
    let mut drawn = 0usize;
    for (i, (c, w)) in cells.iter().zip(widths).enumerate() {
        if w == 0 {
            continue;
        }
        if drawn > 0 {
            spans.push(Span::raw(" "));
        }
        drawn += 1;
        if i == FLAT_NAME {
            push_name(&mut spans, row, l.name, w);
        } else {
            spans.push(cell_span(c, w));
        }
    }
    Line::from(spans)
}

/// The column-label row above the list, solved to the same widths so a label
/// sits over the column it names. The clip count rides on top of this row's
/// right edge (see `board`), painted after the columns so it never has to fight
/// them for width.
fn flat_header(width: usize) -> Line<'static> {
    let widths = solve(&FLAT, width.saturating_sub(FLAT_PREFIX));
    let label_style = Style::default().fg(DIM).add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::raw(" ".repeat(FLAT_PREFIX))];
    let mut drawn = 0usize;
    for (i, (label, w)) in FLAT_LABELS.iter().zip(&widths).enumerate() {
        if *w == 0 {
            continue;
        }
        if drawn > 0 {
            spans.push(Span::raw(" "));
        }
        drawn += 1;
        let c = if flat_right(i) {
            Cell::right(label.to_string(), label_style)
        } else {
            Cell::left(label.to_string(), label_style)
        };
        spans.push(cell_span(&c, *w));
    }
    Line::from(spans)
}


/// Where a table's body window starts.
///
/// `sel` is the row the cursor is on, and is passed only while the cursor is
/// being followed. A wheel scroll deliberately leaves the selection behind, so
/// following it unconditionally would undo that scroll on the very next frame --
/// which is what the old board did, because its wheel moved the selection.
fn scroll_to(offset: usize, len: usize, h: usize, sel: Option<usize>) -> usize {
    let max = len.saturating_sub(h);
    let mut off = offset.min(max);
    if let Some(r) = sel {
        if r < off {
            off = r;
        } else if h > 0 && r >= off + h {
            off = r + 1 - h;
        }
    }
    off.min(max)
}

fn board(f: &mut Frame, area: Rect, v: &View, geo: &mut FrameGeometry) {
    let width = area.width.saturating_sub(1) as usize;

    geo.list_top = area.y;
    geo.list_height = area.height;
    geo.tables.clear();
    geo.scroll = v.scroll;

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

    // Idle sessions carry no action, so they collapse to one line unless asked
    // for. This is the difference between a scannable window and a wall. The line
    // is nailed to the last row of the pane and taken out of the tables' budget:
    // a table that grew into it would push the one line saying how to get those
    // sessions back off the bottom of the screen.
    let collapse = !v.show_clear && v.clear_count > 0 && area.height > 1;
    let avail = (area.height as usize).saturating_sub(collapse as usize);

    // One flat list: a column-label header, then every row in rank order. The
    // header takes the top line of the budget; the rows scroll under it. The
    // cursor is already a flat index into `rows`, so `j`/`k` cross the whole
    // board without any table boundaries to step over.
    let body_h = avail.saturating_sub(1);
    let len = v.rows.len();
    let off = scroll_to(
        v.scroll[0],
        len,
        body_h,
        if v.follow { Some(v.selected) } else { None },
    );
    geo.scroll[0] = off;
    let shown = body_h.min(len.saturating_sub(off));

    let mut lines: Vec<Line> = Vec::with_capacity(shown + 1);
    lines.push(flat_header(width));
    for i in off..off + shown {
        lines.push(flat_row(
            &v.rows[i],
            i + 1,
            i == v.selected,
            v.now,
            v.local_offset,
            width,
        ));
    }
    f.render_widget(Paragraph::new(lines), area);

    // The clip count, painted over the header row's right edge whenever a row is
    // scrolled off either end -- so a row never vanishes without a count saying
    // so. Overlaid as its own widget rather than appended to the header line,
    // which is why it never has to fight the columns for width.
    if shown < len {
        let mark = format!(" {}–{} of {} ", off + 1, off + shown, len);
        let mw = width_of(&mark) as u16;
        if area.width > mw {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(mark, Style::default().fg(DIM)))),
                Rect {
                    x: area.x + area.width - mw,
                    y: area.y,
                    width: mw,
                    height: 1,
                },
            );
        }
    }

    // One geometry record for the whole list, so a click or a wheel roll on the
    // board resolves against `scroll[0]` -- the single offset this list keeps.
    // The slot is `YourMove`'s index (0) only because the wheel handler reads a
    // table's `group.index()` to pick a scroll slot, and this list lives in slot
    // 0; there is one list, so which name it wears does not otherwise matter.
    geo.tables.push(TableGeometry {
        group: Group::YourMove,
        body_top: area.y + 1,
        body_height: shown as u16,
        rows: (0..len).collect(),
        offset: off,
    });

    if collapse {
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("· {} clear", v.clear_count),
                Style::default().fg(DIM),
            ),
            Span::styled("  — press c to show", Style::default().fg(DIM)),
        ]);
        f.render_widget(
            Paragraph::new(line),
            Rect {
                x: area.x,
                y: area.bottom() - 1,
                width: area.width,
                height: 1,
            },
        );
    }
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
            // A single word wider than the pane: hard-split it across lines.
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

/// The selected session in full, in the nine rows under the board.
///
/// This pane is where the per-row detail went when the rows became one line
/// each. It is therefore the only place the write-set, the last ask and the
/// resume command are readable, so it is laid out to a budget rather than
/// written out and left to overflow: the head (what this session is) and the
/// tail (how to get back into it) are always drawn, and the write-set and the
/// ask divide whatever rows are left between them.
///
/// Nothing wraps. A wrapped line costs an unknown number of rows, which is what
/// makes a budget unkeepable -- a long path would have silently pushed the
/// resume command off the bottom. Lines are clipped at the pane's width instead.
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

    let width = area.width as usize;
    // The top border takes the first row of the pane.
    let room = area.height.saturating_sub(1) as usize;
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
        Span::raw("  "),
        Span::styled(
            if row.tokens > 0 {
                format!("{} tokens", fmt_count(row.tokens))
            } else {
                String::new()
            },
            Style::default().fg(DIM),
        ),
    ])];

    // The task clock in full: elapsed run (live for a working agent, settled once
    // stopped) and the local time it began, mirroring the board's elapsed column.
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
                fit(&text, width),
                Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
            ));
        }
        Status::Blocked => {
            let text = match row.blocked_reason {
                Some(r) => format!("{} — switch to its terminal, or a to set aside", r.detail()),
                None => "waiting on you".to_string(),
            };
            lines.push(Line::styled(
                fit(&text, width),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ));
        }
        Status::AwaitingAck => {
            let text = match row.blocked_reason {
                Some(r) => format!("{} — reply in its terminal, or a to acknowledge", r.detail()),
                None => "stopped at the prompt — waiting on your reply".to_string(),
            };
            lines.push(Line::styled(
                fit(&text, width),
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ));
        }
        Status::Working => lines.push(Line::styled(
            fit("agent is mid-turn — files still moving, do not test yet", width),
            Style::default().fg(CYAN),
        )),
        Status::Delegated => lines.push(Line::styled(
            fit("spun up a background agent — waiting on it, not on you; resumes on its own", width),
            Style::default().fg(INDIGO),
        )),
        // Hedged, and it stays hedged: a tool call open on a silent log is a slow
        // command far more often than it is an unanswered prompt, and the log
        // cannot tell the two apart. Saying "needs approval" here would be the
        // pane making a claim the classifier explicitly refused to make.
        Status::Stalled => lines.push(Line::styled(
            fit("quiet a while — may need approval, or a slow command still running", width),
            Style::default().fg(AMBER),
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

    // What is left after the head above and the resume command below. The ask is
    // reserved out of it first: the write-set says what changed, but only the ask
    // says what this session was *for*, which is the harder thing to reconstruct.
    let ask: Vec<String> = row
        .last_prompt
        .as_deref()
        .map(|p| wrap_prompt(p, width.saturating_sub(6), 2))
        .unwrap_or_default();
    let mut budget = room.saturating_sub(lines.len() + 1 + ask.len());

    let mut listed = 0usize;
    for p in row.pending.iter() {
        // Keep a row back for the tally when more than one file would be left
        // out -- "and 6 more" carries more than a sixth file name.
        if budget == 0 || (budget == 1 && row.pending.len() - listed > 1) {
            break;
        }
        lines.push(Line::from(vec![
            Span::styled("  › ", Style::default().fg(color)),
            Span::styled(
                fit(p, width.saturating_sub(4)),
                Style::default().fg(Color::Rgb(210, 216, 224)),
            ),
        ]));
        listed += 1;
        budget -= 1;

        // The most recent text written to the first file, when a row is spare.
        // The board no longer shows any of what an agent actually wrote, and this
        // is the one line that says whether the writing is going anywhere.
        if listed == 1 && budget > 0 && row.pending.len() - listed == 0 {
            if let Some(text) = row.previews.get(p).and_then(|v| v.first()) {
                lines.push(Line::from(vec![
                    Span::styled("    │ ", Style::default().fg(DIM)),
                    Span::styled(
                        fit(text.trim_end(), width.saturating_sub(6)),
                        Style::default().fg(PREVIEW),
                    ),
                ]));
                budget -= 1;
            }
        }
    }
    if listed < row.pending.len() && budget > 0 {
        lines.push(Line::styled(
            format!("    … and {} more", row.pending.len() - listed),
            Style::default().fg(DIM),
        ));
    }

    for (i, l) in ask.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(
                if i == 0 { "ask: " } else { "     " },
                Style::default().fg(DIM),
            ),
            Span::styled(l.clone(), Style::default().fg(Color::Rgb(150, 158, 170))),
        ]));
    }

    // The resume command, always present -- this is what lets a dropped thread
    // be picked back up. y (or a click) copies it. Drawn on the pane's last row
    // whatever else had to give way for it.
    while lines.len() + 1 < room {
        lines.push(Line::raw(""));
    }
    lines.truncate(room.saturating_sub(1));
    lines.push(Line::from(vec![
        Span::styled("continue ", Style::default().fg(DIM)),
        Span::styled("[y / click] ", Style::default().fg(BLUE)),
        Span::styled(
            fit(&row.continue_cmd, width.saturating_sub(21)),
            Style::default().fg(Color::Rgb(190, 200, 210)),
        ),
    ]));

    f.render_widget(Paragraph::new(lines).block(block), area);
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
        // Below the acting keys on purpose: j/k already cross the table
        // boundaries, so this is a shortcut and not the only way over them.
        ("⇥", "next table".into()),
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
#[allow(dead_code)] // path-prefix compressor, kept for the detail pane and its tests
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
                scroll: [0; GROUPS],
                follow: true,
            };
            let mut geo = FrameGeometry::default();
            terminal.draw(|f| draw(f, &view, &mut geo)).unwrap();
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
            scroll: [0; GROUPS],
            follow: true,
        };
        let mut geo = FrameGeometry::default();
        terminal
            .draw(|f| draw(f, &view, &mut geo))
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
            scroll: [0; GROUPS],
            follow: true,
        };
        let mut geo = FrameGeometry::default();
        terminal.draw(|f| draw(f, &view, &mut geo)).unwrap();

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
            scroll: [0; GROUPS],
            follow: true,
            anim_ms: 0, // clock phase 0 -> Eye centred, verse on its first line
            local_offset: 0,
            awaiting_log_dir: None,
        };
        let mut geo = FrameGeometry::default();
        terminal
            .draw(|f| draw(f, &view, &mut geo))
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
            scroll: [0; GROUPS],
            follow: true,
            anim_ms: 0,
            local_offset: 0,
            awaiting_log_dir: Some("/home/u/.claude/projects/-tmp-fresh"),
        };
        let mut geo = FrameGeometry::default();
        terminal
            .draw(|f| draw(f, &view, &mut geo))
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
        // No rows, so no table was drawn and the detail pane has nothing to
        // render -- both of which the board has to survive rather than index.
        assert!(geo.tables.is_empty(), "an empty board drew a table");
        assert!(screen.contains("nothing selected"), "detail pane missing: {screen}");
    }

    // ---- the three tables ---------------------------------------------------

    fn a_row(name: &str, status: Status) -> Row {
        Row {
            id: format!("id-{name}"),
            id_short: "abcd1234".into(),
            name: name.into(),
            branch: Some("main".into()),
            last_activity: 0,
            turn_started: 0,
            status,
            blocked_reason: None,
            error: None,
            pending: Vec::new(),
            total_edits: 0,
            tokens: 0,
            last_prompt: None,
            is_orc: false,
            continue_cmd: "claude --resume abcd1234".into(),
            edits: std::collections::BTreeMap::new(),
            previews: std::collections::BTreeMap::new(),
        }
    }

    fn a_view<'a>(rows: &'a [Row]) -> View<'a> {
        View {
            rows,
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
            scroll: [0; GROUPS],
            follow: true,
            anim_ms: 0,
            local_offset: 0,
            awaiting_log_dir: None,
        }
    }

    /// Draw a whole board and hand back the screen, one string per row.
    fn screen_of(v: &View, w: u16, h: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut geo = FrameGeometry::default();
        terminal.draw(|f| draw(f, v, &mut geo)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// The solver's contract at every width the board is used at: the row never
    /// overruns the pane, the name never falls under its floor, and columns leave
    /// whole rather than each giving up a character.
    #[test]
    fn solved_columns_fit_the_pane_at_every_width() {
        for group in Group::ALL {
            let cols = columns(group);
            for width in [120usize, 80, 60, 52, 44, 30, 20] {
                let w = solve(cols, width);
                let kept: Vec<usize> = w.iter().copied().filter(|&x| x > 0).collect();
                let used: usize = kept.iter().sum::<usize>() + kept.len().saturating_sub(1);
                // The floors of what survives may overrun a hopeless pane; nothing
                // else may.
                if kept.len() > 1 {
                    assert!(
                        used <= width || width < 20,
                        "{group:?} at {width}: used {used}"
                    );
                }
                assert!(w[0] >= NAME_MIN, "{group:?} at {width}: name was {}", w[0]);
                assert_eq!(w.len(), cols.len());
            }
        }
    }

    /// The drop order the contract fixes: tokens first, then the reason column,
    /// then the clock. The name and the glyph are never on the table.
    #[test]
    fn a_narrow_row_drops_its_columns_from_the_right() {
        let cols = columns(Group::YourMove);
        // Wide: everything is drawn, and the slack lands on the name.
        let wide = solve(cols, 120);
        assert!(wide.iter().all(|&w| w > 0), "a wide pane dropped a column");
        assert!(wide[0] > NAME_MIN, "the slack did not reach the name");

        // A 52-column workspace pane: the token count goes, the reason stays --
        // "waiting" is the column that says why the row is on the board at all.
        let pane = solve(cols, 52 - PREFIX - 1);
        assert_eq!(pane[3], 0, "tokens survived a narrow pane: {pane:?}");
        assert!(pane[1] > 0, "the reason column dropped before tokens: {pane:?}");
        assert!(pane[0] >= NAME_MIN);

        // Narrower still: the reason goes before the clock, and last of all the
        // name is alone.
        let tight = solve(cols, 26);
        assert_eq!((tight[1], tight[3]), (0, 0), "{tight:?}");
        let hopeless = solve(cols, 14);
        assert!(hopeless[0] >= NAME_MIN, "{hopeless:?}");
        assert!(hopeless[1..].iter().all(|&w| w == 0), "{hopeless:?}");
    }

    /// What a row gives up is a property of the column, and the same column is
    /// worth different things in different tables. WORKING gives up its token
    /// count -- drawn third of four -- rather than the phrase to the right of it,
    /// and gives that phrase up in turn to widen a starved name. YOUR MOVE never
    /// gives up its reason column at any width: it is why the row is listed.
    #[test]
    fn what_a_row_gives_up_is_a_property_of_the_column() {
        let tight = solve(columns(Group::Working), 35);
        assert_eq!(tight[2], 0, "tokens should be the first to go: {tight:?}");

        let pane = solve(columns(Group::YourMove), 52 - PREFIX - 1);
        assert_eq!(pane[3], 0, "tokens survived a 52-column pane: {pane:?}");
        assert_eq!(pane[1], 20, "the reason column was given up: {pane:?}");
        assert!(pane[2] > 0, "the clock went before the token count: {pane:?}");

        // Room for everything: nothing is given up and the slack is the name's.
        let wide = solve(columns(Group::Working), 120);
        assert!(wide.iter().all(|&x| x > 0), "{wide:?}");
    }

    /// A name at its floor is a stub, so a table hands over its cheapest columns
    /// to widen it even on a row that would have fitted with them. This is what
    /// makes the 52-column pane readable rather than merely correct.
    #[test]
    fn a_starved_name_is_paid_for_out_of_the_soft_columns() {
        // The whole WORKING table fits in 47 cells -- and reads badly, with a
        // 16-cell name. Both soft columns go, and the name gets what they held.
        let cols = columns(Group::Working);
        assert!(demand(cols, &[true; 4]) <= 47, "premise: the row fits");
        let w = solve(cols, 47);
        assert!(w[0] >= NAME_WANT, "the name stayed starved: {w:?}");
        assert_eq!((w[2], w[3]), (0, 0), "{w:?}");
        // The clock is not soft: it survives at the name's expense.
        assert!(w[1] > 0, "the elapsed column was given up: {w:?}");
    }

    /// AWAITING TESTING has two flex columns, and the name takes two thirds of
    /// the slack -- a path is compressed by `dim_common`, a name is not.
    #[test]
    fn two_flex_columns_split_the_slack_two_to_one() {
        let cols = columns(Group::AwaitingTesting);
        // Both widths are past the name's comfort width, so nothing is dropped
        // and the only difference between them is the slack being divided.
        let tight = solve(cols, demand(cols, &[true; 5]) + 18);
        let wide = solve(cols, demand(cols, &[true; 5]) + 48);
        assert!(tight.iter().all(|&x| x > 0), "premise: nothing dropped");
        assert_eq!(wide[0] - tight[0], 20, "name share: {wide:?}");
        assert_eq!(wide[2] - tight[2], 10, "top file share: {wide:?}");
    }

    /// Padding is measured in cells, not characters. A name in a script that
    /// draws two cells per character would otherwise push every column after it
    /// one place right of the row above.
    #[test]
    fn fit_measures_display_width_not_characters() {
        assert_eq!(width_of("日本語"), 6);
        // Six cells of demand into six cells of column: nothing is cut.
        assert_eq!(fit("日本語", 6), "日本語");
        // Five cells: one character has to go, and the ellipsis takes its place
        // without overrunning.
        assert!(width_of(&fit("日本語", 5)) <= 5, "{:?}", fit("日本語", 5));
        assert!(width_of(&fit("日本語", 4)) <= 4);
        assert_eq!(fit("plain ascii", 5), "plai…");
        assert_eq!(fit("", 5), "");
    }

    /// The flat board draws one row per session with its status as a column, and
    /// none of the old three section headers.
    #[test]
    fn a_row_shows_its_status_word_and_no_section_headers() {
        let rows = vec![a_row("untested work", Status::NeedsTest)];
        let v = a_view(&rows);
        let screen = screen_of(&v, 80, 24).join("\n");
        assert!(screen.contains("untested work"), "{screen}");
        // Status is a column word now, not a loud section rule.
        assert!(screen.contains("needs test"), "{screen}");
        assert!(!screen.contains("AWAITING TESTING ("), "a section header survived: {screen}");
        assert!(!screen.contains("YOUR MOVE"), "a section header survived: {screen}");
        assert!(!screen.contains("WORKING ("), "a section header survived: {screen}");
    }

    /// One line per session, and the columns line up down the list: a fixed
    /// column starts at the same cell on every row, whatever the task name's
    /// length, because the task column absorbs the slack and the rest are fixed.
    #[test]
    fn every_row_starts_its_columns_in_the_same_place() {
        let rows = vec![
            a_row("a short name", Status::Blocked),
            a_row("a considerably longer session name than the one above", Status::AwaitingAck),
        ];
        let v = a_view(&rows);
        let lines = screen_of(&v, 100, 30);
        let body: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("a short name") || l.contains("a considerably longer"))
            .collect();
        assert_eq!(body.len(), 2, "expected one line each:\n{}", lines.join("\n"));
        // Both rows are settled with no turn stamp, so the `started` column is an
        // em dash on each. Its position is the start of the fixed columns; if the
        // columns line up, the first dash sits at the same cell on both rows.
        // Measured in cells, not bytes -- the dash is multi-byte.
        let dash = |l: &str| l.chars().position(|c| c == '—');
        assert!(dash(body[0]).is_some(), "no fixed column drawn:\n{body:?}");
        assert_eq!(dash(body[0]), dash(body[1]), "columns did not line up:\n{body:?}");
    }

    /// A board showing some of eleven rows says how many, so a row never scrolls
    /// off the list without a count admitting it.
    #[test]
    fn a_clipped_list_says_how_much_it_is_showing() {
        let rows: Vec<Row> = (0..11)
            .map(|i| a_row(&format!("working session {i}"), Status::Working))
            .collect();
        let v = a_view(&rows);
        let screen = screen_of(&v, 80, 24).join("\n");
        assert!(screen.contains("of 11"), "a clipped list hid the clipping: {screen}");
    }

    /// The list scrolls as one window: it clamps to the rows, follows the cursor
    /// while it is being followed, and holds still when it is not.
    #[test]
    fn the_list_scrolls_and_clamps_to_its_rows() {
        // Never past the last row: a list of 10 showing 4 stops at offset 6.
        assert_eq!(scroll_to(99, 10, 4, None), 6);
        // Shorter than its window: there is nothing to scroll.
        assert_eq!(scroll_to(3, 2, 4, None), 0);
        // Following the cursor down pulls the window along by one row at a time.
        assert_eq!(scroll_to(0, 10, 4, Some(5)), 2);
        // …and up, back to the row itself.
        assert_eq!(scroll_to(5, 10, 4, Some(1)), 1);
        // A cursor already on screen moves nothing.
        assert_eq!(scroll_to(2, 10, 4, Some(3)), 2);
        // Not following: a wheel scroll survives the frame that redraws it, which
        // is the whole reason the flag exists.
        assert_eq!(scroll_to(4, 10, 4, None), 4);
    }

    /// Every status maps to the group that drives Tab and the worst-status
    /// colour, and its tag is the word the flat board prints in the status
    /// column. Rows arrive ranked, so the board draws them in this order without
    /// any re-sort.
    #[test]
    fn every_status_has_a_group_and_a_status_word() {
        for (status, group) in [
            (Status::Errored, Group::YourMove),
            (Status::Blocked, Group::YourMove),
            (Status::AwaitingAck, Group::YourMove),
            (Status::NeedsTest, Group::AwaitingTesting),
            (Status::Stalled, Group::Working),
            (Status::Working, Group::Working),
            (Status::Delegated, Group::Working),
            (Status::Clear, Group::Clear),
        ] {
            assert_eq!(look(status).group, group, "{status:?}");
            assert!(!status.tag().is_empty(), "{status:?} has no status word");
        }
    }

    /// A token total of zero is the absence of a measurement -- a Codex session,
    /// or a Claude log with no `usage` -- and printing "0" would report it as one.
    #[test]
    fn a_row_with_no_token_data_leaves_the_column_blank() {
        let mut row = a_row("counted", Status::Working);
        assert_eq!(tokens_cell(&row), "");
        row.tokens = 12_400;
        assert_eq!(tokens_cell(&row), "12.4k");
    }

    /// The 52-column workspace pane is a design target, not a courtesy: this is
    /// the width sauron runs at beside four agents. The name and the status word
    /// survive, and nothing overruns the pane -- the estimate and token columns
    /// give way first, exactly as the drop order says.
    #[test]
    fn the_board_survives_a_fifty_two_column_pane() {
        let mut blocked = a_row("wire up the token store", Status::Blocked);
        blocked.blocked_reason = Some(crate::model::BlockedReason::Question);
        let mut test = a_row("rebuild the ack store", Status::NeedsTest);
        test.pending = vec!["src/store.rs".into()];
        let rows = vec![blocked, test, a_row("sweep the logs", Status::Working)];
        let v = a_view(&rows);
        let lines = screen_of(&v, 52, 24);
        let screen = lines.join("\n");
        // The name survives, and so does the status word beside it.
        assert!(screen.contains("wire up"), "{screen}");
        assert!(screen.contains("rebuild"), "{screen}");
        assert!(screen.contains("waiting"), "{screen}");
        assert!(screen.contains("needs test"), "{screen}");
        // Nothing overran the pane.
        assert!(lines.iter().all(|l| l.chars().count() == 52));
    }

    /// A board of nothing but idle sessions still draws: no rows, and the
    /// collapsed count on the last line. The early empty state does not cover
    /// this one -- there are idle sessions, they are simply collapsed to a count.
    #[test]
    fn a_board_of_only_clear_sessions_draws_its_count() {
        let mut v = a_view(&[]);
        v.clear_count = 4;
        let lines = screen_of(&v, 80, 24);
        let screen = lines.join("\n");
        assert!(screen.contains("· 4 clear"), "{screen}");
        assert!(screen.contains("press c to show"), "{screen}");
    }

    /// `c` reveals the idle sessions: they join the flat list with a `clear`
    /// status word rather than being counted and drawn nowhere.
    #[test]
    fn showing_the_clear_sessions_adds_them_to_the_list() {
        let rows = vec![
            a_row("still going", Status::Working),
            a_row("nothing outstanding", Status::Clear),
        ];
        let mut v = a_view(&rows);
        v.clear_count = 1;
        v.show_clear = true;
        let screen = screen_of(&v, 100, 30).join("\n");
        assert!(screen.contains("nothing outstanding"), "{screen}");
        assert!(screen.contains("clear"), "{screen}");
        // …and the collapsed count is gone, rather than sitting under the rows it
        // was replaced by.
        assert!(!screen.contains("press c to show"), "{screen}");
    }

    /// Every row carries its own status word, so what kind of attention it wants
    /// reads off the row itself -- the loud detail (the failure, the files) moved
    /// to the detail pane under the selected row.
    #[test]
    fn every_row_carries_its_status_word() {
        let mut errored = a_row("wire up the token store", Status::Errored);
        errored.error = Some(crate::model::ErrorKind::Truncated);
        let needs = a_row("rebuild the ack store", Status::NeedsTest);
        let rows = vec![errored, needs];
        let v = a_view(&rows);
        let screen = screen_of(&v, 100, 30).join("\n");
        // The status words are on the rows.
        assert!(screen.contains("errored"), "{screen}");
        assert!(screen.contains("needs test"), "{screen}");
        // And the selected (errored) row's failure shows in the detail pane below.
        assert!(screen.contains("cut off"), "{screen}");
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
