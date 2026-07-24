//! Rendering.
//!
//! Layout priority follows attention priority: sessions awaiting acknowledgement
//! get a banner, a colour, and full detail; sessions with nothing outstanding
//! collapse to a single count line. An earlier version gave every session three
//! lines regardless of state, so fifteen idle sessions buried the one that
//! needed a human.
//!
//! grep targets:
//!   fn draw            -- top-level layout
//!   fn header          -- repo name and the status tally
//!   fn section_header  -- coloured rule introducing each status group
//!   fn card            -- one session -> multi-line ListItem
//!   fn wrap_prompt     -- first lines of your last ask, wrapped for a card
//!   fn detail          -- selected session's write-set and prompt
//!   fn dim_common      -- shared directory prefix compression for path lists
//!   const AMBER/CYAN   -- the status palette

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::{ago, truncate, Status};
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
const BLUE: Color = Color::Rgb(120, 170, 255); // chrome / repo identity
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
    pub repo: &'a str,
    pub saved: bool,
    pub hidden_stale: usize,
    pub clear_count: usize,
    pub show_clear: bool,
    pub copied: bool,
    /// Milliseconds since launch, the clock the Eye and the ring-verse animate
    /// off. Derived, not stored in App: the whole animation is a pure function
    /// of this, so nothing has to be ticked or remembered between frames.
    pub anim_ms: u64,
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

pub fn draw(f: &mut Frame, v: &View, list_state: &mut ListState, geo: &mut FrameGeometry) {
    // The full five-line Eye earns its keep only when the terminal is tall
    // enough to spare the rows; below that the header collapses to the compact
    // one-line Eye so the list and detail keep their space.
    let full = f.area();
    let tall = full.height >= 24;
    let header_h = if tall { 1 + crate::scene::HEIGHT } else { 2 };
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

    header(f, chunks[0], v, mordor);
    list(f, list_area, v, list_state, geo);
    detail(f, chunks[2], v.rows.get(v.selected), v.now);
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
            Paragraph::new(crate::scene::tower_shaft(shaft.height as usize, v.anim_ms)),
            shaft,
        );
        // The war at the foot, full width along the bottom of the list region. Its
        // size tracks the count of agents currently working -- each is one orc.
        let armies = v.rows.iter().filter(|r| r.status == Status::Working).count();
        let base = Rect {
            x: chunks[1].x,
            y: chunks[1].bottom() - base_h,
            width: full.width,
            height: base_h,
        };
        f.render_widget(
            Paragraph::new(crate::scene::battle_ground(full.width as usize, armies, v.anim_ms)),
            base,
        );
    }
    let _ = chunks;
}

fn header(f: &mut Frame, area: Rect, v: &View, mordor: bool) {
    let awaiting = v.rows.iter().filter(|r| r.status == Status::NeedsTest).count();
    let ack = v.rows.iter().filter(|r| r.status == Status::AwaitingAck).count();
    let working = v.rows.iter().filter(|r| r.status == Status::Working).count();
    let delegated = v.rows.iter().filter(|r| r.status == Status::Delegated).count();

    let mut top = vec![
        Span::styled(
            format!(" {} ", v.repo),
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
        ),
        Span::styled("sauron", Style::default().fg(DIM)),
        Span::raw("   "),
    ];

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
    if area.height > crate::scene::HEIGHT {
        if mordor {
            lines.extend(crate::scene::crown(area.width as usize, v.anim_ms));
        } else {
            lines.extend(crate::scene::scene(area.width as usize, v.anim_ms));
        }
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
        let empty = Paragraph::new("No sessions with repo edits yet.")
            .style(Style::default().fg(DIM));
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
        push(&mut items, card(r, i == v.selected, v.now, width), Some(i));
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

fn card(row: &Row, selected: bool, now: i64, width: usize) -> ListItem<'static> {
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
    // A green "orc" badge marks sauron's own maintenance agents, distinct from
    // the hobbits. It sits ahead of the name so it never gets truncated away.
    let orc_room = if row.is_orc { 6 } else { 0 };
    let name_room = width
        .saturating_sub(age.chars().count() + 12 + orc_room)
        .max(12);

    let mut first_spans = vec![
        marker.clone(),
        Span::styled(format!("{} ", glyph_of(row.status)), Style::default().fg(color)),
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

    // Quote the last thing you told this session, up to three lines, right under
    // its name -- the quickest way to reload what you had in mind for it without
    // switching to its terminal.
    let mut lines = vec![first];
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

fn detail(f: &mut Frame, area: Rect, row: Option<&Row>, now: i64) {
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
                Some(r) => format!("{} — switch to its terminal, or a to dismiss", r.detail()),
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

fn footer(f: &mut Frame, area: Rect, v: &View) {
    let key = |k: &'static str, what: &'static str| {
        vec![
            Span::styled(k, Style::default().fg(BLUE).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {}  ", what), Style::default().fg(DIM)),
        ]
    };
    let mut spans = vec![Span::raw(" ")];
    spans.extend(key("j/k", "move"));
    spans.extend(key("a", "ack/dismiss"));
    spans.extend(key("u", "undo"));
    spans.extend(key("A", "ack all"));
    spans.extend(key("y", "copy continue"));
    spans.extend(key("c", if v.show_clear { "hide clear" } else { "show clear" }));
    spans.extend(key("q", "quit"));

    if v.hidden_stale > 0 {
        spans.push(Span::styled(
            format!("+{} older (o)  ", v.hidden_stale),
            Style::default().fg(DIM),
        ));
    }
    // The copied flash wins the corner when both fire -- it is the action the
    // user just took and wants confirmed.
    if v.copied {
        spans.push(Span::styled(
            "continue command copied",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ));
    } else if v.saved {
        spans.push(Span::styled(
            "saved",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
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
            saved: false,
            hidden_stale: 0,
            clear_count: 0,
            show_clear: false,
            copied: false,
            anim_ms: 0, // clock phase 0 -> Eye centred, verse on its first line
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
