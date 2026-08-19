//! The board, as the page needs it.
//!
//! The terminal front end renders a `Row` into coloured cells. The page renders
//! the same `Row` into HTML it lays out itself, so what crosses here is the
//! *data* -- status, timers, file lists, the last thing you said -- and none of
//! the presentation. That split is the reason the web board could be designed as
//! a web page instead of as a screenshot of a terminal.
//!
//! WHAT IS DELIBERATELY SENT AS WELL AS THE RAW VALUE
//! -------------------------------------------------
//! `ago`, `fmt_duration` and `fmt_clock` live in `model.rs` and are the product:
//! they decide that four minutes reads as `4m` and that a task started at 15:42
//! shows on your wall clock and not UTC. Re-deriving those in JavaScript would
//! be a second implementation of a thing already tuned once, and the two would
//! disagree the first time either was touched. So the epoch numbers go over
//! *and* the strings sauron would have drawn, and the page prints the strings.
//!
//! grep targets:
//!   fn board       -- the whole board, one message
//!   fn row         -- one session
//!   fn status_key  -- the stable name the page's CSS keys its colours off
//!   fn group_key   -- which of the board's tables the row is listed under
//!   fn why         -- a YOUR MOVE row's short reason, the contract's wording
//!   fn now_doing   -- a WORKING row's last column
//!   fn tokens_text -- the token total, compacted, or nothing at all
//!   fn esc         -- JSON string escaping, via serde_json

use crate::board::Board;
use crate::model::{ago, fmt_clock, fmt_count, fmt_duration, local_time, now_ms, Status};
use crate::Row;

/// The name the page uses for a status, in CSS and in its own sorting. Stable
/// and lowercase; `Status::label()` is prose for humans and may be reworded.
pub fn status_key(s: Status) -> &'static str {
    match s {
        Status::Errored => "errored",
        Status::Blocked => "blocked",
        Status::AwaitingAck => "ack",
        Status::Working => "working",
        Status::Delegated => "delegated",
        Status::NeedsTest => "needs-test",
        Status::Stalled => "stalled",
        Status::Clear => "clear",
    }
}

/// Which of the board's three tables the row is listed under, as a CSS-safe key.
///
/// The grouping is the contract's, not the page's: `ui.rs` sorts the same
/// statuses into the same tables under the same words. It crosses the wire so
/// the page groups by a decision sauron already made, rather than by a fourth
/// copy of the table written in JavaScript -- which is how the three surfaces
/// came to disagree about what the bands were called in the first place.
///
/// `Clear` is in no table on either surface. It has a key because the `c`
/// toggle reveals those rows and they must have somewhere to land.
pub fn group_key(s: Status) -> &'static str {
    match s {
        Status::Errored | Status::Blocked | Status::AwaitingAck => "your-move",
        Status::NeedsTest => "awaiting-testing",
        Status::Working | Status::Delegated | Status::Stalled => "working",
        Status::Clear => "clear",
    }
}

/// Why a YOUR MOVE row is on the board, in the contract's short form.
///
/// The twin of `ui::why`. The long `detail()` strings travel as well and are
/// what the detail pane prints; this is the one that has to fit a column, so it
/// is the short form or it is nothing.
fn why(r: &Row) -> &'static str {
    match r.status {
        Status::Errored => r
            .error
            .map(|e| e.short())
            .unwrap_or("turn ended on a failure"),
        Status::AwaitingAck => r
            .blocked_reason
            .map(|b| b.short())
            .unwrap_or("stopped — your move"),
        Status::Blocked => r
            .blocked_reason
            .map(|b| b.short())
            .unwrap_or("waiting on you"),
        _ => r.status.tag(),
    }
}

/// A WORKING row's last column: what the agent is on.
///
/// The twin of `ui::now_doing`. No tool name reaches a `Row`, so a working
/// session says the file it wrote last and the two states that are not
/// computing say what they are waiting on. The stalled phrase stays hedged --
/// the log cannot tell a slow command from an unanswered prompt.
fn now_doing(r: &Row) -> String {
    match r.status {
        Status::Delegated => "background agent running — resumes on its own".into(),
        Status::Stalled => "quiet a while — may need approval".into(),
        _ => match r.pending.first() {
            Some(p) => p.rsplit('/').next().unwrap_or(p).to_string(),
            None => "working".into(),
        },
    }
}

/// The token total compacted, or nothing at all.
///
/// A Codex session and a Claude session whose log carries no `usage` both
/// arrive as zero, and "0" in the column would be a measurement rather than the
/// absence of one.
fn tokens_text(n: i64) -> String {
    if n > 0 {
        fmt_count(n)
    } else {
        String::new()
    }
}

fn esc(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn list(items: impl Iterator<Item = String>) -> String {
    let inner: Vec<String> = items.map(|s| esc(&s)).collect();
    format!("[{}]", inner.join(","))
}

/// One session, flattened for the page.
pub fn row(r: &Row, now: i64, local_offset: i64) -> String {
    let mut f: Vec<String> = Vec::new();
    f.push(format!("\"id\":{}", esc(&r.id)));
    f.push(format!("\"short\":{}", esc(&r.id_short)));
    f.push(format!("\"name\":{}", esc(&crate::model::collapse_ws(&r.name))));
    f.push(format!("\"status\":{}", esc(status_key(r.status))));
    f.push(format!("\"statusLabel\":{}", esc(r.status.label())));
    f.push(format!("\"tag\":{}", esc(r.status.tag())));
    // The table this row is listed under, and the two column strings that only
    // that table prints. All three are decided here so the page never has to
    // ask what a status means -- it asks only where to put the row.
    f.push(format!("\"group\":{}", esc(group_key(r.status))));
    f.push(format!("\"why\":{}", esc(why(r))));
    f.push(format!("\"doing\":{}", esc(&now_doing(r))));
    f.push(format!("\"orc\":{}", r.is_orc));
    f.push(format!("\"edits\":{}", r.total_edits));
    // The raw total and the compact form sauron would have drawn, travelling
    // together for the reason `lastActivity` travels with `ago`: the page prints
    // the string, and anything that sorts or compares has the number. Zero is
    // the absence of a measurement, so its text is empty and the column renders
    // blank rather than "0".
    f.push(format!("\"tokens\":{}", r.tokens));
    f.push(format!("\"tokensText\":{}", esc(&tokens_text(r.tokens))));
    f.push(format!("\"lastActivity\":{}", r.last_activity));
    f.push(format!("\"ago\":{}", esc(&ago(r.last_activity, now))));
    f.push(format!("\"turnStarted\":{}", r.turn_started));
    f.push(format!("\"pending\":{}", list(r.pending.iter().cloned())));
    f.push(format!("\"continueCmd\":{}", esc(&r.continue_cmd)));
    // Who this session is, and the colour that says so. Straight from
    // `servant.rs`, so a tab in the browser, a card on the board and a pane in
    // iTerm all agree without anyone coordinating -- the colour is a pure
    // function of the id all three already hold.
    f.push(format!("\"servant\":{}", esc(crate::servant::name_for(&r.id))));
    let (cr, cg, cb) = crate::servant::color_for(&r.id);
    f.push(format!("\"color\":[{cr},{cg},{cb}]"));

    // The elapsed timer and the start time only mean anything once a turn has
    // been seen to begin; a zero here is "unknown", not "midnight 1970".
    //
    // The clock runs to `now` only while the row is genuinely working, and to
    // its last activity once it is not -- the rule `ui::elapsed` has always
    // used. Measuring a settled session to `now` reported the age of the board
    // rather than the length of the task, and the figure climbed for as long as
    // the tab stayed open.
    if r.turn_started > 0 {
        let end = if r.status == Status::Working {
            now
        } else {
            r.last_activity
        };
        f.push(format!(
            "\"elapsed\":{}",
            esc(&fmt_duration(end.saturating_sub(r.turn_started)))
        ));
        f.push(format!(
            "\"startedAt\":{}",
            esc(&fmt_clock(local_time(r.turn_started, local_offset)))
        ));
    }
    if let Some(b) = &r.branch {
        f.push(format!("\"branch\":{}", esc(b)));
    }
    if let Some(p) = &r.last_prompt {
        f.push(format!("\"prompt\":{}", esc(&crate::model::collapse_ws(p))));
    }
    if let Some(reason) = r.blocked_reason {
        f.push(format!("\"blocked\":{}", esc(reason.detail())));
    }
    if let Some(e) = r.error {
        f.push(format!("\"error\":{}", esc(e.detail())));
    }
    // The per-file preview: the most recent lines written to each pending path.
    // Sent only for pending files -- the acked ones are not what anyone is about
    // to look at, and the payload is per-tick.
    let previews: Vec<String> = r
        .pending
        .iter()
        .filter_map(|p| {
            r.previews
                .get(p)
                .map(|lines| format!("{}:{}", esc(p), list(lines.iter().cloned())))
        })
        .collect();
    if !previews.is_empty() {
        f.push(format!("\"previews\":{{{}}}", previews.join(",")));
    }

    format!("{{{}}}", f.join(","))
}

/// The whole board in one message: the tallies the header shows, and every row.
pub fn board(b: &Board, local_offset: i64) -> String {
    let now = now_ms();
    let count = |s: Status| b.rows.iter().filter(|r| r.status == s).count();
    let rows: Vec<String> = b.rows.iter().map(|r| row(r, now, local_offset)).collect();

    format!(
        "{{\"t\":\"board\",\"repo\":{},\"path\":{},\"now\":{},\
         \"counts\":{{\"errored\":{},\"blocked\":{},\"ack\":{},\"needsTest\":{},\
         \"stalled\":{},\"working\":{},\"delegated\":{},\"clear\":{}}},\
         \"hiddenStale\":{},\"showClear\":{},\"rows\":[{}]}}",
        esc(&b.repo_label),
        esc(&crate::model::tilde(b.repo_root())),
        now,
        count(Status::Errored),
        count(Status::Blocked),
        count(Status::AwaitingAck),
        count(Status::NeedsTest),
        count(Status::Stalled),
        count(Status::Working),
        count(Status::Delegated),
        b.clear_count,
        b.hidden_stale,
        b.show_clear,
        rows.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Status; 8] = [
        Status::Errored,
        Status::Blocked,
        Status::AwaitingAck,
        Status::NeedsTest,
        Status::Stalled,
        Status::Working,
        Status::Delegated,
        Status::Clear,
    ];

    /// A row with everything the page reads set to something harmless, so a test
    /// can change the one field it is about.
    fn fixture(status: Status) -> Row {
        Row {
            id: "abc".into(),
            id_short: "abc".into(),
            name: "a task".into(),
            branch: None,
            last_activity: 1_100_000,
            turn_started: 1_000_000,
            status,
            blocked_reason: None,
            error: None,
            pending: Vec::new(),
            total_edits: 0,
            tokens: 0,
            last_prompt: None,
            is_orc: false,
            continue_cmd: "claude --resume abc".into(),
            edits: Default::default(),
            previews: Default::default(),
        }
    }

    #[test]
    fn status_keys_are_stable_and_css_safe() {
        // The page keys its colours off these. A space or a capital here is a
        // silently unstyled row, not an error anyone would see.
        for s in ALL {
            for k in [status_key(s), group_key(s)] {
                assert!(
                    k.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                    "{k} is not a usable class name"
                );
            }
        }
    }

    #[test]
    fn every_status_lands_in_the_table_the_contract_gives_it() {
        // The grouping is the whole point of sending `group`: three surfaces
        // used to each carry their own copy of this table and had already
        // drifted apart. `Stalled` in particular belongs with the agents that
        // are getting on with it, never in the table that says a human is owed
        // something -- a long build and an unanswered prompt look the same in
        // the log, and there are far more long builds.
        for (s, table) in [
            (Status::Errored, "your-move"),
            (Status::Blocked, "your-move"),
            (Status::AwaitingAck, "your-move"),
            (Status::NeedsTest, "awaiting-testing"),
            (Status::Stalled, "working"),
            (Status::Working, "working"),
            (Status::Delegated, "working"),
            (Status::Clear, "clear"),
        ] {
            assert_eq!(group_key(s), table, "{s:?} is in the wrong table");
        }
    }

    #[test]
    fn a_settled_rows_clock_stops_at_its_last_activity() {
        // The bug this test exists for: the page measured every elapsed time to
        // `now`, so a session that finished at lunchtime was still reporting a
        // longer and longer task all afternoon. Only a working row is still
        // running; everything else took as long as it took.
        let mut r = fixture(Status::Working);
        let at = |r: &Row, now: i64| -> String {
            let v: serde_json::Value = serde_json::from_str(&row(r, now, 0)).expect("valid json");
            v["elapsed"].as_str().unwrap_or_default().to_string()
        };

        assert_eq!(at(&r, 1_160_000), "2m 40s", "a working row runs to now");
        assert_eq!(at(&r, 1_460_000), "7m 40s", "and keeps running");

        r.status = Status::AwaitingAck;
        assert_eq!(at(&r, 1_160_000), "1m 40s", "a settled row took until it stopped");
        assert_eq!(
            at(&r, 99_000_000),
            "1m 40s",
            "and the figure does not climb with the board's age"
        );

        // Delegated and stalled sessions are not computing either, whatever
        // their table says.
        for s in [Status::Delegated, Status::Stalled] {
            r.status = s;
            assert_eq!(at(&r, 99_000_000), "1m 40s", "{s:?} is not a running clock");
        }
    }

    #[test]
    fn the_token_total_goes_over_as_a_number_and_as_the_word_for_it() {
        // Both, for the reason `lastActivity` travels with `ago`: the page
        // prints what sauron would have drawn, and anything that wants to sort
        // has the figure. Zero is "never measured", not "no tokens", so the
        // column renders blank.
        let mut r = fixture(Status::Working);
        r.tokens = 12_450;
        let v: serde_json::Value = serde_json::from_str(&row(&r, 1_100_000, 0)).expect("valid json");
        assert_eq!(v["tokens"], 12_450);
        assert_eq!(v["tokensText"], "12.4k");

        r.tokens = 0;
        let v: serde_json::Value = serde_json::from_str(&row(&r, 1_100_000, 0)).expect("valid json");
        assert_eq!(v["tokens"], 0);
        assert_eq!(v["tokensText"], "", "a row with no token data renders blank");
    }

    #[test]
    fn the_column_strings_are_the_contracts_words() {
        // One vocabulary, decided in Rust. The page prints these; it must never
        // be in a position to invent its own phrasing for a state.
        let mut r = fixture(Status::Errored);
        r.error = Some(crate::model::ErrorKind::Truncated);
        let v: serde_json::Value = serde_json::from_str(&row(&r, 1_100_000, 0)).expect("valid json");
        assert_eq!(v["why"], "cut off (max_tokens)");
        assert_eq!(v["group"], "your-move");

        let mut r = fixture(Status::Stalled);
        let v: serde_json::Value = serde_json::from_str(&row(&r, 1_100_000, 0)).expect("valid json");
        assert_eq!(v["doing"], "quiet a while — may need approval", "stay hedged");
        assert_eq!(v["group"], "working");

        r.status = Status::Working;
        r.pending = vec!["src/web/json.rs".into()];
        let v: serde_json::Value = serde_json::from_str(&row(&r, 1_100_000, 0)).expect("valid json");
        assert_eq!(v["doing"], "json.rs", "a working row says the file it wrote last");
    }

    #[test]
    fn a_row_serialises_as_parseable_json_with_the_formatting_already_done() {
        let mut r = Row {
            id: "abc\"def".into(),
            id_short: "abc".into(),
            name: "fix   the\npanel".into(),
            branch: None,
            last_activity: 1_000_000,
            turn_started: 0,
            status: Status::Working,
            blocked_reason: None,
            error: None,
            pending: vec!["src/ui.rs".into()],
            total_edits: 3,
            tokens: 0,
            last_prompt: None,
            is_orc: false,
            continue_cmd: "claude --resume abc".into(),
            edits: Default::default(),
            previews: Default::default(),
        };
        r.previews.insert("src/ui.rs".into(), vec!["a line".into()]);

        let v: serde_json::Value = serde_json::from_str(&row(&r, 1_240_000, 0)).expect("valid json");
        assert_eq!(v["id"], "abc\"def", "quotes in an id must not break the message");
        assert_eq!(v["name"], "fix the panel", "whitespace collapsed once, in Rust");
        assert_eq!(v["status"], "working");
        assert_eq!(v["ago"], "4m", "the page prints what sauron would have drawn");
        assert_eq!(v["previews"]["src/ui.rs"][0], "a line");
        assert!(v.get("elapsed").is_none(), "no timer without a known turn start");
    }
}
