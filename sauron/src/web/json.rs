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
//!   fn board      -- the whole board, one message
//!   fn row        -- one session
//!   fn status_key -- the stable name the page's CSS keys its colours off
//!   fn esc        -- JSON string escaping, via serde_json

use crate::board::Board;
use crate::model::{ago, fmt_clock, fmt_duration, local_time, now_ms, Status};
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
    f.push(format!("\"orc\":{}", r.is_orc));
    f.push(format!("\"edits\":{}", r.total_edits));
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
    if r.turn_started > 0 {
        f.push(format!(
            "\"elapsed\":{}",
            esc(&fmt_duration(now.saturating_sub(r.turn_started)))
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
         \"working\":{},\"delegated\":{},\"clear\":{}}},\
         \"hiddenStale\":{},\"showClear\":{},\"rows\":[{}]}}",
        esc(&b.repo_label),
        esc(&crate::model::tilde(b.repo_root())),
        now,
        count(Status::Errored),
        count(Status::Blocked),
        count(Status::AwaitingAck),
        count(Status::NeedsTest),
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

    #[test]
    fn status_keys_are_stable_and_css_safe() {
        // The page keys its colours off these. A space or a capital here is a
        // silently unstyled card, not an error anyone would see.
        for s in [
            Status::Errored,
            Status::Blocked,
            Status::AwaitingAck,
            Status::Working,
            Status::Delegated,
            Status::NeedsTest,
            Status::Clear,
        ] {
            let k = status_key(s);
            assert!(
                k.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{k} is not a usable class name"
            );
        }
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
