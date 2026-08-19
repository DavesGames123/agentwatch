//! Turning "these files changed" into "open this, it needs that first".
//!
//! A row on the board says an agent wrote eight files and wants them looked at.
//! That is the wrong unit for a human: nobody navigates a GUI by file path. The
//! watched repo ships a map from paths to the surfaces that render them, and
//! this module joins the two.
//!
//! WHY THE MAP LIVES IN THE WATCHED REPO
//! ------------------------------------
//! sauron knows nothing about any particular application, and should not. What
//! it knows is to look for `.sauron/panels.toml` under the repo it is already
//! watching. An app that ships one gets routes; an app that does not gets
//! exactly the behaviour it had before this module existed, because every entry
//! point here returns `None` on a missing file rather than inventing a default.
//!
//! It also keeps the map honest. A panel that moves and a map that describes it
//! are in the same tree, so they move in the same commit. A copy of this
//! knowledge held inside sauron would rot silently the first time someone
//! renamed a button, and rot in a repo the person renaming it never opens.
//!
//! READ-ONLY, LIKE EVERYTHING ELSE
//! -------------------------------
//! "sauron writes nothing to the watched repo" is what makes it safe to point
//! at somebody else's checkout. Reading a file the repo chose to publish does
//! not weaken that; writing a cache next to it would, so the cache is in memory
//! and keyed on mtime.
//!
//! WHY A HAND PARSER
//! -----------------
//! The same reason `beacon.rs` hand-writes its wire format: the dependency is
//! not worth the surface. This reads a deliberately small subset of TOML —
//! `[[panel]]` tables, string values, string arrays, `#` comments — and refuses
//! anything else rather than half-understanding it. If the map ever needs
//! nested tables or typed scalars, take the dependency then.
//!
//! grep targets:
//!   fn for_repo      -- cached entry point, mtime-keyed
//!   fn load          -- path -> Map, or None when the repo ships no map
//!   fn route         -- the join: a row's files -> ordered steps
//!   fn parse         -- the TOML subset reader
//!   fn glob_match    -- `**` prefix, `*` within one path segment
//!   const MAX_STEPS  -- how many surfaces a route may name before it truncates

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Where a repo publishes its map, relative to the repo root.
pub const MAP_PATH: &str = ".sauron/panels.toml";

/// Schema version this reader understands. A map declaring anything else is
/// refused whole, on the same reasoning the beacon refuses a version it was not
/// built for: a partly-understood route reads as authoritative while being
/// wrong, which is worse than no route.
pub const SCHEMA_VERSION: u32 = 1;

/// How many surfaces one route may name.
///
/// A change that touches six panels is real, but a six-step route is not a
/// route — it is a list, and a list is what the file column already was. Three
/// is enough to say "mainly here, also check these two". Anything past the cut
/// is *counted*, never silently dropped: see `Route::truncated`.
const MAX_STEPS: usize = 3;

// ───────────────────────────────────────────────────────────────────────────
//  The map
// ───────────────────────────────────────────────────────────────────────────

/// One surface, as the watched repo describes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Panel {
    /// Stable key. Sorting and dedup happen on this, never on `title`.
    pub id: String,
    pub title: String,
    /// What the host's own UI calls this control, when it has one. Empty means
    /// the surface has no addressable button, so a host highlight has nothing
    /// to draw and the prose in `open` carries the whole answer.
    pub anchor: String,
    /// The flag whose truth means "open". Documentation for a human debugging a
    /// stale route; sauron never resolves it.
    pub flag: String,
    /// The route itself, in prose, written to be followed.
    pub open: String,
    /// What must already be true. A route that silently needs a selected
    /// station sends someone to an empty panel, where the missing content reads
    /// as a bug in the change they were sent to look at.
    pub needs: Vec<String>,
    /// `verified` if the map's author traced the route to a call site,
    /// `inferred` if they reasoned it out from structure. Passed through
    /// untouched so the reader can mark a guess as a guess.
    pub confidence: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Map {
    pub panels: Vec<Panel>,
    /// The universe of paths this map claims to describe, as globs. Empty means
    /// "everything", which is the right default for a repo that is all source.
    ///
    /// Stated positively, as an include list, rather than as the set of things
    /// to ignore. The ignore framing loses: a repo carries vendored trees, build
    /// output, an agent's own scratch store, editor droppings — an unbounded and
    /// growing set, and every entry missed from it silently deflates coverage.
    /// What a map covers is small, known, and stable.
    ///
    /// Only `--check` consults this. Routing ignores it deliberately: if an
    /// out-of-scope file lands in a row it must still count as unmapped, because
    /// "the map does not cover this" is the fact worth surfacing.
    pub scope: Vec<String>,
}

impl Map {
    /// Whether `--check` should hold the map responsible for this path.
    pub fn in_scope(&self, path: &str) -> bool {
        self.scope.is_empty() || self.scope.iter().any(|g| glob_match(path, g))
    }
}

/// One surface to open, with the evidence for why it was picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub title: String,
    pub anchor: String,
    pub open: String,
    pub needs: Vec<String>,
    pub confidence: String,
    /// How many of the row's files this surface renders. The ranking key, and
    /// worth showing: "8 files" and "1 file" are different invitations.
    pub hits: usize,
}

impl Step {
    /// Whether the map's author traced this route rather than inferring it.
    pub fn is_verified(&self) -> bool {
        self.confidence == "verified"
    }
}

/// What to do about one row's worth of edits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Route {
    pub steps: Vec<Step>,
    /// Files that matched no panel at all. **Not** cosmetic: this is the whole
    /// staleness signal. A map that has fallen behind the code produces routes
    /// that look complete, and the only outward sign is files quietly matching
    /// nothing. Reporting the count is what makes "the map does not cover this"
    /// distinguishable from "there is nothing to open".
    pub unmapped: usize,
    /// Surfaces past `MAX_STEPS` that were cut. Reported for the same reason:
    /// a truncated list that does not say it was truncated reads as complete.
    pub truncated: usize,
}

impl Route {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

// ───────────────────────────────────────────────────────────────────────────
//  Loading
// ───────────────────────────────────────────────────────────────────────────

/// The map for `repo`, parsed at most once per edit of the file.
///
/// Cached because the board recomputes every TICK and the map changes on the
/// scale of somebody editing it. Keyed on mtime rather than time-to-live, so an
/// edit shows up on the next tick instead of whenever a timer happened to
/// expire — the map is most likely to be edited precisely when someone is
/// staring at a wrong route.
///
///   grep -n "fn for_repo"  src/route.rs
pub fn for_repo(repo: &Path) -> Option<Map> {
    type Cache = HashMap<PathBuf, (Option<SystemTime>, Option<Map>)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let path = repo.join(MAP_PATH);
    let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());

    let mut guard = cache.lock().ok()?;
    if let Some((seen, map)) = guard.get(&path) {
        if *seen == mtime {
            return map.clone();
        }
    }
    let map = load(&path);
    guard.insert(path, (mtime, map.clone()));
    map
}

/// Parse the map at `path`. `None` when the file is absent, unreadable, or
/// declares a schema this reader was not built for.
///
///   grep -n "fn load"  src/route.rs
pub fn load(path: &Path) -> Option<Map> {
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text)
}

// ───────────────────────────────────────────────────────────────────────────
//  The join
// ───────────────────────────────────────────────────────────────────────────

impl Map {
    /// Which surfaces render `files`, best first.
    ///
    /// Ranked by how many of the row's files each surface owns, ties broken by
    /// declaration order in the map — so the file's own ordering is the
    /// author's tiebreak, and identical input always yields identical output.
    /// A file may legitimately belong to several surfaces (a console command
    /// that forces a red alert is reachable from both), so hits are counted per
    /// panel and not partitioned.
    ///
    ///   grep -n "fn route"  src/route.rs
    pub fn route(&self, files: &[String]) -> Route {
        let mut scored: Vec<(usize, usize, &Panel)> = Vec::new();
        for (idx, panel) in self.panels.iter().enumerate() {
            let hits = files
                .iter()
                .filter(|f| panel.files.iter().any(|g| glob_match(f, g)))
                .count();
            if hits > 0 {
                scored.push((hits, idx, panel));
            }
        }
        // Descending hits, ascending declaration index. Explicit rather than
        // sort_by_key on a negated usize, which underflows.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        let unmapped = files
            .iter()
            .filter(|f| {
                !self
                    .panels
                    .iter()
                    .any(|p| p.files.iter().any(|g| glob_match(f, g)))
            })
            .count();

        let truncated = scored.len().saturating_sub(MAX_STEPS);
        let steps = scored
            .into_iter()
            .take(MAX_STEPS)
            .map(|(hits, _, p)| Step {
                id: p.id.clone(),
                title: p.title.clone(),
                anchor: p.anchor.clone(),
                open: p.open.clone(),
                needs: p.needs.clone(),
                confidence: p.confidence.clone(),
                hits,
            })
            .collect();

        Route { steps, unmapped, truncated }
    }
}

/// `**` matches across directory separators; a lone `*` matches within one path
/// segment. Both are anchored at each end — a glob describes a whole path, not
/// a substring of one, so `overlays/*.rs` does not match `src/overlays/x.rs`.
///
/// Deliberately smaller than a real glob library: no `?`, no character classes,
/// no brace expansion. Every one of those makes a map harder to read for a
/// reader that is trying to answer "does this cover my file", which is the only
/// question anyone asks of it.
///
///   grep -n "fn glob_match"  src/route.rs
pub fn glob_match(path: &str, glob: &str) -> bool {
    match glob.split_once("**") {
        // `a/**` and `a/**/b.rs` both mean "under a/". The tail after `**` is
        // accepted as documentation and not matched against: a map that says
        // `src/gui/**` and one that says `src/gui/**/*.rs` mean the same thing
        // to anybody reading it, and treating them differently would be a
        // surprise rather than a feature.
        Some((prefix, _)) => path.starts_with(prefix),
        None => segments_match(path, glob),
    }
}

/// Segment-wise match for globs without `**`: equal segment counts, each
/// segment matched with `*` standing in for any run of non-separator bytes.
fn segments_match(path: &str, glob: &str) -> bool {
    let p: Vec<&str> = path.split('/').collect();
    let g: Vec<&str> = glob.split('/').collect();
    p.len() == g.len() && p.iter().zip(g).all(|(seg, pat)| star_match(seg, pat))
}

/// `*` within one segment. Iterative rather than recursive: a pathological glob
/// in a checked-in file should not be able to blow sauron's stack.
fn star_match(seg: &str, pat: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return seg == pat;
    }
    // First and last parts are anchored; the middle ones may float.
    let Some(first) = parts.first() else { return false };
    let Some(last) = parts.last() else { return false };
    if !seg.starts_with(first) || !seg.ends_with(last) {
        return false;
    }
    // The anchors may overlap on a short segment: `ab` must not match `a*b*a`.
    if seg.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &seg[first.len()..seg.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

// ───────────────────────────────────────────────────────────────────────────
//  The TOML subset
// ───────────────────────────────────────────────────────────────────────────

/// Read the map. `None` on a version this reader does not implement; unknown
/// keys are ignored so a map can carry fields for a newer sauron without
/// breaking an older one.
///
///   grep -n "fn parse"  src/route.rs
pub fn parse(text: &str) -> Option<Map> {
    let mut version: Option<u32> = None;
    let mut scope: Vec<String> = Vec::new();
    let mut panels: Vec<Panel> = Vec::new();
    let mut cur: Option<Panel> = None;

    let mut lines = text.lines().peekable();
    while let Some(raw) = lines.next() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[panel]]" {
            if let Some(p) = cur.take() {
                panels.push(p);
            }
            cur = Some(Panel::default());
            continue;
        }
        // Any other table header ends the current panel rather than folding its
        // keys into it. Nothing in the schema uses one; this is what keeps a
        // future `[meta]` block from silently becoming part of a panel.
        if line.starts_with('[') {
            if let Some(p) = cur.take() {
                panels.push(p);
            }
            continue;
        }

        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let mut value = value.trim().to_string();

        // A `[` that does not close on this line continues until one does.
        // Consuming here rather than pre-joining the file keeps line handling
        // in one place.
        if value.starts_with('[') && !value.contains(']') {
            while let Some(next) = lines.next() {
                let more = strip_comment(next).trim().to_string();
                value.push(' ');
                value.push_str(&more);
                if more.contains(']') {
                    break;
                }
            }
        }

        if cur.is_none() {
            match key {
                "version" => version = value.trim().parse::<u32>().ok(),
                "scope" => scope = string_array(&value),
                _ => {}
            }
            continue;
        }
        let panel = cur.as_mut()?;
        match key {
            "id" => panel.id = unquote(&value),
            "title" => panel.title = unquote(&value),
            "anchor" => panel.anchor = unquote(&value),
            "flag" => panel.flag = unquote(&value),
            "open" => panel.open = unquote(&value),
            "confidence" => panel.confidence = unquote(&value),
            "needs" => panel.needs = string_array(&value),
            "files" => panel.files = string_array(&value),
            _ => {}
        }
    }
    if let Some(p) = cur.take() {
        panels.push(p);
    }

    if version != Some(SCHEMA_VERSION) {
        return None;
    }
    // A panel with no id cannot be referred to, and one with no globs can never
    // match. Dropping them beats carrying entries that can only ever confuse a
    // reader comparing the map against a route.
    panels.retain(|p| !p.id.is_empty() && !p.files.is_empty());
    Some(Map { panels, scope })
}

/// Everything before an unquoted `#`.
///
/// Quote-aware because the routes themselves are prose and may contain a `#`;
/// a naive split would truncate a sentence mid-word and the result would still
/// parse, which is the worst kind of wrong.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_str => i += 1,
            b'"' => in_str = !in_str,
            b'#' if !in_str => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// Strip surrounding quotes and undo the two escapes the schema allows.
fn unquote(v: &str) -> String {
    let v = v.trim();
    let inner = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v);
    inner.replace("\\\"", "\"").replace("\\\\", "\\")
}

/// `["a", "b"]`, possibly spread over several lines by the time it gets here.
/// Reads quoted runs directly rather than splitting on commas, so a comma
/// inside a route string cannot split one element into two.
fn string_array(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut escaped = false;
    for ch in v.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_str => escaped = true,
            '"' => {
                if in_str {
                    out.push(std::mem::take(&mut cur));
                }
                in_str = !in_str;
            }
            _ if in_str => cur.push(ch),
            _ => {}
        }
    }
    out
}

// ───────────────────────────────────────────────────────────────────────────
//  Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const MAP: &str = r#"
version = 1

# a comment with a # inside it
[[panel]]
id         = "research"
title      = "Research"
anchor     = "research"
flag       = "RESEARCH_WINDOW_OPEN"
open       = "Click the research squircle."
needs      = ["a station selected"]
confidence = "inferred"
files = [
  "src/gui/windows/research/**",
]

[[panel]]
id         = "station"
title      = "Colony"
anchor     = "xray"
flag       = "STATION_MANAGEMENT_OPEN"
open       = "Press E."
needs      = []
confidence = "verified"
files = [
  "src/gui/windows/station_management/**",
  "src/gui/overlays/station_*.rs",
]
"#;

    fn map() -> Map {
        parse(MAP).expect("fixture parses")
    }

    #[test]
    fn parses_panels_and_arrays() {
        let m = map();
        assert_eq!(m.panels.len(), 2);
        assert_eq!(m.panels[0].id, "research");
        assert_eq!(m.panels[0].needs, vec!["a station selected"]);
        assert_eq!(m.panels[1].files.len(), 2);
        assert!(m.panels[1].confidence == "verified");
    }

    #[test]
    fn refuses_a_version_it_was_not_built_for() {
        assert!(parse(&MAP.replace("version = 1", "version = 2")).is_none());
        assert!(parse(&MAP.replace("version = 1", "")).is_none());
    }

    #[test]
    fn double_star_matches_across_directories() {
        assert!(glob_match("src/gui/windows/research/render.rs", "src/gui/windows/research/**"));
        assert!(!glob_match("src/gui/windows/resources/mod.rs", "src/gui/windows/research/**"));
    }

    #[test]
    fn single_star_stays_inside_one_segment() {
        assert!(glob_match("src/gui/overlays/station_damage.rs", "src/gui/overlays/station_*.rs"));
        // The star must not eat a separator, or every panel owning `a/*.rs`
        // would claim the whole subtree below it.
        assert!(!glob_match("src/gui/overlays/sub/station_damage.rs", "src/gui/overlays/station_*.rs"));
        // Anchored at both ends.
        assert!(!glob_match("x/src/gui/overlays/station_a.rs", "src/gui/overlays/station_*.rs"));
    }

    #[test]
    fn ranks_by_hit_count() {
        let files: Vec<String> = [
            "src/gui/windows/research/render.rs",
            "src/gui/windows/research/data.rs",
            "src/gui/overlays/station_damage.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let r = map().route(&files);
        assert_eq!(r.steps.len(), 2);
        assert_eq!(r.steps[0].id, "research");
        assert_eq!(r.steps[0].hits, 2);
        assert_eq!(r.steps[1].id, "station");
        assert_eq!(r.unmapped, 0);
    }

    #[test]
    fn counts_unmapped_rather_than_swallowing_it() {
        // The staleness signal. If this ever returns 0 for a file no panel
        // claims, a map that has fallen behind the code becomes invisible.
        let files = vec!["src/combat/hull.rs".to_string()];
        let r = map().route(&files);
        assert!(r.is_empty());
        assert_eq!(r.unmapped, 1);
    }

    #[test]
    fn a_file_may_belong_to_two_surfaces() {
        let mut m = map();
        m.panels[0].files.push("src/shared.rs".to_string());
        m.panels[1].files.push("src/shared.rs".to_string());
        let r = m.route(&["src/shared.rs".to_string()]);
        assert_eq!(r.steps.len(), 2);
        assert_eq!(r.unmapped, 0);
    }

    #[test]
    fn truncation_is_counted_not_hidden() {
        let mut m = map();
        for i in 0..4 {
            m.panels.push(Panel {
                id: format!("p{i}"),
                files: vec!["src/shared.rs".to_string()],
                ..Default::default()
            });
        }
        let r = m.route(&["src/shared.rs".to_string()]);
        assert_eq!(r.steps.len(), MAX_STEPS);
        assert_eq!(r.truncated, 1); // 4 extra panels, 3 shown, 1 over the cap
    }

    #[test]
    fn comments_do_not_truncate_prose() {
        let m = parse(&MAP.replace(
            r#"open       = "Press E.""#,
            r#"open       = "Press E, then check bay #3.""#,
        ))
        .expect("parses");
        assert!(m.panels[1].open.ends_with("bay #3."));
    }

    #[test]
    fn absent_map_is_not_an_error() {
        assert!(load(Path::new("/nonexistent/.sauron/panels.toml")).is_none());
    }
}

// ───────────────────────────────────────────────────────────────────────────
//  The `sauron route` subcommand
// ───────────────────────────────────────────────────────────────────────────
//
//  Reads the live beacon rather than rebuilding a board, for the same reason
//  `panel status` does: the beacon already applied the liveness gate and the
//  ack filtering, so this prints what the pane would be showing and not a
//  second, subtly different answer to the same question.

const USAGE: &str = "\
sauron route -- what to open in order to look at a change

  sauron route [DIR]
      For every row on DIR's board, print the surfaces its edits render into.
      Reads the live beacon, so it shows exactly what the in-app pane shows.

  sauron route --check [DIR]
      Map health. Joins every glob against the repo's tracked files and reports
      coverage, globs that match nothing, and the verified/inferred split.
      This is the staleness check -- run it when a panel moves.

  sauron route --files <PATH>... [DIR]
      Route an arbitrary file list, without a board. For asking `where would
      this land` before making the edit.

The map is <DIR>/.sauron/panels.toml. A repo without one has no routes, and
every command here says so rather than inventing a default.
";

pub fn run(args: &[String]) -> std::io::Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    if let Some(at) = args.iter().position(|a| a == "--files") {
        let files: Vec<String> = args[at + 1..]
            .iter()
            .take_while(|a| !a.starts_with("--"))
            .cloned()
            .collect();
        let dir = dir_from(&args[..at])?;
        return route_files(&dir, &files);
    }
    if args.iter().any(|a| a == "--check") {
        let dir = dir_from(args)?;
        return check(&dir);
    }
    let dir = dir_from(args)?;
    board_routes(&dir)
}

/// The first non-flag argument as a directory, else the git root, else cwd.
/// Same resolution order as `panel`, so the two commands never disagree about
/// which repo they are talking about.
fn dir_from(args: &[String]) -> std::io::Result<PathBuf> {
    let dir = match args.iter().find(|a| !a.starts_with("--")) {
        Some(p) => PathBuf::from(p),
        None => crate::git_root().unwrap_or(std::env::current_dir()?),
    };
    Ok(dir.canonicalize().unwrap_or(dir))
}

/// Load the map, or explain precisely why there is none. The three failure
/// modes are different problems with different fixes, so they get different
/// sentences rather than one shared "no routes".
fn require_map(dir: &Path) -> Option<Map> {
    let path = dir.join(MAP_PATH);
    if !path.exists() {
        println!("{} ships no {MAP_PATH} -- no routes for this repo.", dir.display());
        return None;
    }
    match load(&path) {
        Some(m) => Some(m),
        None => {
            println!(
                "{} is unreadable, or declares a schema other than v{SCHEMA_VERSION}.",
                path.display()
            );
            None
        }
    }
}

/// One route, indented under whatever printed it.
fn print_route(r: &Route, indent: &str) {
    if r.is_empty() {
        println!("{indent}no mapped surface -- {} file(s) match nothing in the map", r.unmapped);
        return;
    }
    for (i, s) in r.steps.iter().enumerate() {
        let mark = if s.is_verified() { "" } else { "  (route inferred, not traced)" };
        println!("{indent}{}. {} -- {} file(s){}", i + 1, s.title, s.hits, mark);
        println!("{indent}   {}", s.open);
        if !s.needs.is_empty() {
            println!("{indent}   needs: {}", s.needs.join("; "));
        }
    }
    if r.unmapped > 0 {
        println!("{indent}({} file(s) matched no panel -- the map may be behind the code)", r.unmapped);
    }
    if r.truncated > 0 {
        println!("{indent}(+{} more surface(s) not shown)", r.truncated);
    }
}

fn route_files(dir: &Path, files: &[String]) -> std::io::Result<()> {
    let Some(map) = require_map(dir) else { return Ok(()) };
    if files.is_empty() {
        println!("no files given");
        return Ok(());
    }
    print_route(&map.route(files), "  ");
    Ok(())
}

fn board_routes(dir: &Path) -> std::io::Result<()> {
    let Some(map) = require_map(dir) else { return Ok(()) };

    let path = crate::beacon::path_for(dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!("no beacon at {} -- no sauron is watching {}.", path.display(), dir.display());
        return Ok(());
    };
    let Some(b) = crate::beacon::parse(&text) else {
        println!("beacon at {} is unreadable or a foreign version.", path.display());
        return Ok(());
    };
    let now = crate::model::now_ms();
    if !b.is_live(now) {
        println!(
            "beacon is {}s old (stale past {}s) -- sauron pid {} is gone.",
            now.saturating_sub(b.written_ms) / 1000,
            crate::beacon::STALE_MS / 1000,
            b.pid
        );
        return Ok(());
    }

    println!("{}  --  {} rows", b.label, b.rows.len());
    if b.rows.is_empty() {
        println!("  board clear -- nothing to test");
    }
    for r in &b.rows {
        // Same suffix rule as `panel::status`: a row with no usage recorded
        // prints no figure, so an absent number never reads as a spend of zero.
        let tokens = if r.tokens > 0 {
            format!("  ·  {}", crate::model::fmt_count(r.tokens))
        } else {
            String::new()
        };
        println!("\n  [{}] {}{}", r.status, r.name, tokens);
        if r.files.is_empty() {
            println!("      no files written -- nothing to look at yet");
            continue;
        }
        print_route(&map.route(&r.files), "      ");
    }
    Ok(())
}

/// Map health against the repo's tracked files.
///
/// Two numbers matter and they fail in opposite directions. Coverage below 100%
/// means files no route will ever mention. A glob matching nothing means an
/// entry describing a file that no longer exists -- which is how a map starts
/// lying, because the panel it points at usually still opens.
fn check(dir: &Path) -> std::io::Result<()> {
    let Some(map) = require_map(dir) else { return Ok(()) };

    // `--others --exclude-standard` alongside `--cached`: a file an agent wrote
    // ten seconds ago is not staged yet, and it is exactly the file most likely
    // to be in the row being routed. Checking only the index would report the
    // globs covering it as dead.
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()?;
    if !out.status.success() {
        println!("`git ls-files` failed in {} -- not a git repo?", dir.display());
        return Ok(());
    }
    let all: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();
    let total = all.len();
    let files: Vec<String> = all.into_iter().filter(|f| map.in_scope(f)).collect();
    let excluded = total - files.len();

    let mut dead: Vec<(&str, &str)> = Vec::new();
    for p in &map.panels {
        for g in &p.files {
            if !files.iter().any(|f| glob_match(f, g)) {
                dead.push((&p.id, g));
            }
        }
    }
    let unmapped: Vec<&String> = files
        .iter()
        .filter(|f| !map.panels.iter().any(|p| p.files.iter().any(|g| glob_match(f, g))))
        .collect();

    let verified = map.panels.iter().filter(|p| p.confidence == "verified").count();
    let mapped = files.len() - unmapped.len();
    let pct = if files.is_empty() { 0.0 } else { 100.0 * mapped as f64 / files.len() as f64 };

    println!("{}", dir.join(MAP_PATH).display());
    println!(
        "  {} panels ({} verified, {} inferred)",
        map.panels.len(),
        verified,
        map.panels.len() - verified
    );
    println!("  {mapped}/{} in-scope files mapped ({pct:.1}%)", files.len());
    if excluded > 0 {
        println!("  {excluded} file(s) outside the map's declared `scope`");
    }

    if dead.is_empty() {
        println!("  no dead globs");
    } else {
        println!("  {} glob(s) match no tracked file:", dead.len());
        for (id, g) in &dead {
            println!("      {id}: {g}");
        }
    }
    if !unmapped.is_empty() {
        println!("  {} unmapped file(s):", unmapped.len());
        for f in unmapped.iter().take(40) {
            println!("      {f}");
        }
        if unmapped.len() > 40 {
            println!("      ... and {} more", unmapped.len() - 40);
        }
    }
    Ok(())
}
