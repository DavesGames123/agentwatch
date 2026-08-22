//! What a shell command wrote, read off the command itself.
//!
//! `scan.rs` builds the write-set -- the test surface -- from Claude Code's
//! `file-history-delta` records. Those records follow the `Edit` and `Write`
//! tools. They do not follow `Bash`, and an agent told to edit through heredocs,
//! `sed` and `python3 - <<EOF` writes its whole change set through `Bash`. On
//! three measured sessions in `barnes-hut` that is 93, 50 and 50 shell calls
//! against zero deltas: the board showed "stopped -- your move" with an empty
//! file list while `lang/en.toml` and `lang/tutorial/en.toml` sat rewritten on
//! disk. This module is the missing reader.
//!
//! WHY A PARSER AND NOT A FACT
//! ---------------------------
//! Nothing on disk records which files a shell command touched. The log holds
//! the command text and the text of its output, and that is all. So this is a
//! guess, and it is wrong in both directions: a `>` inside a quoted string reads
//! as a redirect, and an editor invoked by name reads as nothing. The caller
//! bounds the damage -- `scan::shell_relative` keeps only a path that is inside
//! the repo AND exists on disk, which discards `/dev/null`, `/tmp` scratch, glob
//! fragments and any literal this file wrongly harvested. A false positive costs
//! one extra row to glance at; a false negative is the failure that produced
//! this module.
//!
//! WHAT IS NOT COVERED
//! -------------------
//! `git apply`, `git checkout` and `git stash` all write files this cannot name,
//! because the paths are in the patch or the index rather than in the argv. An
//! interpreter that builds its target path by concatenation is missed for the
//! same reason. Both stay missed rather than being guessed at from the command
//! name, because "this command probably wrote something somewhere" cannot be
//! turned into a row.
//!
//! `touch` is excluded on purpose, and it was in this file for one measured
//! board before it came out. It changes an mtime, not content, and its common
//! use is `touch build.rs && cargo test` to force a rebuild -- which put
//! `build.rs` on two test lists in `barnes-hut` as a file nobody had edited. A
//! path is worth listing only when something wrote bytes to it.
//!
//! grep targets:
//!   fn writes            -- the whole command -> candidate paths
//!   fn split_heredocs    -- shell text and heredoc bodies, separated
//!   fn tokenize          -- quote-aware words and operators
//!   fn segment_targets   -- one pipeline stage -> what it wrote
//!   fn script_targets    -- an interpreter body -> what it wrote
//!   fn pathlike          -- whether a bare string could name a file

use std::collections::BTreeMap;

/// Interpreters whose argv may carry a script body rather than a file to run.
const INTERPRETERS: [&str; 6] = ["python", "python3", "perl", "ruby", "node", "php"];

/// Every path this command appears to write, in the order they were found and
/// without duplicates.
///
/// The strings come back exactly as they appeared -- relative stays relative.
/// Resolving them against a working directory is the caller's job, because only
/// the caller knows which directory the command ran in.
pub fn writes(cmd: &str) -> Vec<String> {
    let (shell, bodies) = split_heredocs(cmd);
    let mut out: Vec<String> = Vec::new();

    for seg in segments(&tokenize(&shell)) {
        push_all(&mut out, segment_targets(&seg));
    }
    for body in &bodies {
        push_all(&mut out, script_targets(body));
    }
    out
}

fn push_all(out: &mut Vec<String>, more: Vec<String>) {
    for p in more {
        if !out.contains(&p) {
            out.push(p);
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
//  Heredocs
// ───────────────────────────────────────────────────────────────────────────

/// Split the command into the shell text and the body of each heredoc.
///
/// The bodies must come out before tokenizing. A heredoc body is data, not
/// shell: it is full of quotes that do not balance, `>` characters that redirect
/// nothing, and `#` comments. Feeding it to `tokenize` produces noise, and the
/// noise is what a naive version of this file reports as written files.
///
/// One heredoc per line is handled, which is every form seen in a real log.
fn split_heredocs(cmd: &str) -> (String, Vec<String>) {
    let lines: Vec<&str> = cmd.lines().collect();
    let mut shell = String::new();
    let mut bodies: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        shell.push_str(line);
        shell.push('\n');
        i += 1;

        let Some(delim) = heredoc_delimiter(line) else {
            continue;
        };
        let mut body = String::new();
        while i < lines.len() && lines[i].trim() != delim {
            body.push_str(lines[i]);
            body.push('\n');
            i += 1;
        }
        // Step over the closing delimiter. A body that ran to the end of the
        // command without one is kept anyway -- a truncated log is still
        // evidence of what the command was writing.
        i += 1;
        bodies.push(body);
    }

    (shell, bodies)
}

/// The delimiter word of a heredoc opened on this line, if one is.
///
/// Accepts `<<EOF`, `<< EOF`, `<<'EOF'`, `<<"EOF"` and the `<<-` tab-stripping
/// form. Rejects `<<<`, which is a here-string and carries its data inline.
fn heredoc_delimiter(line: &str) -> Option<String> {
    let b = line.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'<' && b[i + 1] == b'<' {
            let mut j = i + 2;
            if j < b.len() && b[j] == b'<' {
                i = j + 1;
                continue;
            }
            if j < b.len() && b[j] == b'-' {
                j += 1;
            }
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            let quote = b.get(j).copied().filter(|c| *c == b'\'' || *c == b'"');
            if quote.is_some() {
                j += 1;
            }
            let start = j;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j > start {
                return Some(line[start..j].to_string());
            }
            i = j.max(i + 2);
            continue;
        }
        i += 1;
    }
    None
}

// ───────────────────────────────────────────────────────────────────────────
//  Tokenizing
// ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// One argument, with its quotes removed.
    Word(String),
    /// `>` or `>>`.
    Redirect(bool),
    /// A pipeline or list separator: `|`, `;`, `&&`, `||`, `&`, newline.
    Break,
}

/// Words and operators, with quotes honoured.
///
/// Quotes are stripped rather than kept, because every consumer below wants the
/// path and not the syntax. `>` inside quotes stays inside the word, which is
/// the one thing a `split_whitespace` version gets wrong.
fn tokenize(s: &str) -> Vec<Tok> {
    let mut toks: Vec<Tok> = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut it = s.chars().peekable();

    macro_rules! flush {
        () => {
            if !word.is_empty() || quoted {
                toks.push(Tok::Word(std::mem::take(&mut word)));
                quoted = false;
            }
        };
    }

    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(n) = it.next() {
                    // A backslash-newline is a line continuation, not a word.
                    if n != '\n' {
                        word.push(n);
                    }
                }
            }
            '\'' => {
                quoted = true;
                for n in it.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    word.push(n);
                }
            }
            '"' => {
                quoted = true;
                while let Some(n) = it.next() {
                    if n == '\\' {
                        if let Some(e) = it.next() {
                            word.push(e);
                        }
                        continue;
                    }
                    if n == '"' {
                        break;
                    }
                    word.push(n);
                }
            }
            '>' => {
                flush!();
                let double = it.peek() == Some(&'>');
                if double {
                    it.next();
                }
                // `>&2` and `2>&1` duplicate a descriptor. No file is named, so
                // the redirect is dropped rather than reported as a write to a
                // file called `&2`.
                if it.peek() == Some(&'&') {
                    it.next();
                    while it.peek().is_some_and(|n| n.is_ascii_digit() || *n == '-') {
                        it.next();
                    }
                    continue;
                }
                toks.push(Tok::Redirect(double));
            }
            '<' => {
                flush!();
                while it.peek() == Some(&'<') {
                    it.next();
                }
            }
            '|' | ';' | '&' | '\n' => {
                flush!();
                while it.peek().is_some_and(|n| *n == c) {
                    it.next();
                }
                toks.push(Tok::Break);
            }
            c if c.is_whitespace() => flush!(),
            c => word.push(c),
        }
    }
    // The tail, spelt out rather than through `flush!`: the macro also clears
    // `quoted`, and clearing it here is a store nothing reads.
    if !word.is_empty() || quoted {
        toks.push(Tok::Word(word));
    }
    toks
}

/// One pipeline stage: the tokens between two breaks.
fn segments(toks: &[Tok]) -> Vec<Vec<Tok>> {
    let mut out: Vec<Vec<Tok>> = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    for t in toks {
        if *t == Tok::Break {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(t.clone());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ───────────────────────────────────────────────────────────────────────────
//  One command
// ───────────────────────────────────────────────────────────────────────────

/// What one pipeline stage wrote: its redirect targets, plus whatever the
/// command it names is known to write.
fn segment_targets(seg: &[Tok]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut argv: Vec<&str> = Vec::new();
    let mut i = 0;

    while i < seg.len() {
        match &seg[i] {
            Tok::Redirect(_) => {
                if let Some(Tok::Word(w)) = seg.get(i + 1) {
                    if pathlike(w) {
                        push_all(&mut out, vec![w.clone()]);
                    }
                    i += 2;
                    continue;
                }
                i += 1;
            }
            Tok::Word(w) => {
                argv.push(w.as_str());
                i += 1;
            }
            Tok::Break => i += 1,
        }
    }

    // Step over an environment prefix (`FOO=bar cmd`) and the wrappers that take
    // a command as their argument. `sudo -u x cmd` is not handled: the flags
    // would have to be known one by one, and no sampled command used one.
    let mut head = 0;
    while let Some(w) = argv.get(head) {
        let env_assignment = w.contains('=') && !w.starts_with('=');
        if env_assignment || matches!(*w, "env" | "sudo" | "command" | "time" | "nohup") {
            head += 1;
            continue;
        }
        break;
    }
    let Some(cmd) = argv.get(head).map(|c| basename(c)) else {
        return out;
    };
    let args = &argv[(head + 1).min(argv.len())..];

    match cmd {
        "tee" => {
            for a in args.iter().filter(|a| !a.starts_with('-')) {
                if pathlike(a) {
                    push_all(&mut out, vec![a.to_string()]);
                }
            }
        }
        // `-i` edits in place. Without it `sed` writes to stdout and only the
        // redirect above counts.
        "sed" | "perl" if args.iter().any(|a| a.starts_with("-i")) => {
            // The first non-empty operand is the script; the rest are files.
            // macOS spells the flag `-i ''`, so the empty argument is dropped
            // before the script is identified, or the script would be.
            let operands: Vec<&&str> = args
                .iter()
                .filter(|a| !a.starts_with('-') && !a.is_empty())
                .collect();
            for a in operands.iter().skip(1) {
                if pathlike(a) {
                    push_all(&mut out, vec![a.to_string()]);
                }
            }
        }
        "cp" | "mv" | "install" | "rsync" | "ln" => {
            if let Some(dst) = args.iter().rev().find(|a| !a.starts_with('-')) {
                if pathlike(dst) {
                    push_all(&mut out, vec![dst.to_string()]);
                }
            }
        }
        // `-c` and `-e` carry the whole program in one argument. Every argument
        // is offered to the script reader rather than only the one after the
        // flag, because the flag's spelling differs per interpreter and a plain
        // filename argument reads as no writes at all.
        c if INTERPRETERS.contains(&c) => {
            for a in args {
                push_all(&mut out, script_targets(a));
            }
        }
        _ => {}
    }
    out
}

fn basename(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

// ───────────────────────────────────────────────────────────────────────────
//  Interpreter bodies
// ───────────────────────────────────────────────────────────────────────────

/// Paths an interpreter script writes.
///
/// Line-scanned, in two passes, and the second pass is the one that matters. A
/// literal-only reader finds nothing in the shape every sampled session used:
///
/// ```text
/// path = 'lang/tutorial/en.toml'
/// src = open(path, encoding='utf-8').read()
/// ...
/// open(path, 'w', encoding='utf-8').write(src)
/// ```
///
/// The path literal and the write are three lines apart. So pass one records
/// every `name = 'pathlike'` assignment, and pass two harvests, from each line
/// that writes, both the literals on that line and any recorded name it
/// mentions. A literal that never reaches a writing line is a file the script
/// only read, and is not reported.
fn script_targets(body: &str) -> Vec<String> {
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    for line in body.lines() {
        if let Some((name, value)) = assignment(line) {
            if pathlike(&value) {
                vars.insert(name, value);
            }
        }
    }

    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        if !writes_a_file(line) {
            continue;
        }
        for lit in literals(line) {
            if pathlike(&lit) {
                push_all(&mut out, vec![lit]);
            }
        }
        for (name, path) in &vars {
            if mentions(line, name) {
                push_all(&mut out, vec![path.clone()]);
            }
        }
    }
    out
}

/// `name = "value"` or `name = 'value'`, with nothing but the literal on the
/// right. A right-hand side that is an expression is not a path.
fn assignment(line: &str) -> Option<(String, String)> {
    let (lhs, rhs) = line.split_once('=')?;
    let name = lhs.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    let lits = literals(rhs);
    let only = lits.first()?;
    // `x = 'a' + 'b'` and `x = f('a')` are expressions, not a path.
    let bare = rhs.trim();
    let quoted = format!("'{}'", only);
    let double = format!("\"{}\"", only);
    (bare == quoted || bare == double).then(|| (name.to_string(), only.clone()))
}

/// Whether this line performs a write.
///
/// `open(` alone is not enough -- a read is spelt the same way -- so the mode
/// argument has to be there too. `.write(` on its own line is kept because the
/// handle it writes through was opened on an earlier line, which this pass has
/// already seen; on its own it harvests nothing, so it costs nothing.
fn writes_a_file(line: &str) -> bool {
    const MARKERS: [&str; 10] = [
        ".write_text(",
        ".writelines(",
        "writeFileSync",
        "os.replace(",
        "os.rename(",
        "shutil.move(",
        "shutil.copy",
        ">:encoding",
        "print(",
        ".write(",
    ];
    let opens_for_writing = line.contains("open(")
        && ["'w", "\"w", "'a'", "\"a\"", "'x'", "'r+'"]
            .iter()
            .any(|m| line.contains(m));
    // Perl's two-argument open puts the mode inside the path string.
    let perl_open = line.contains("open(") && (line.contains("'>") || line.contains("\">"));
    opens_for_writing || perl_open || MARKERS.iter().any(|m| line.contains(m))
}

/// Every single- or double-quoted string on the line, unescaped only enough to
/// compare. Quotes inside the other kind of quote are ignored, which is what
/// keeps `"it's"` from opening a literal.
fn literals(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\'' && c != '"' {
            continue;
        }
        let mut lit = String::new();
        let mut closed = false;
        while let Some(n) = it.next() {
            if n == '\\' {
                if let Some(e) = it.next() {
                    lit.push(e);
                }
                continue;
            }
            if n == c {
                closed = true;
                break;
            }
            lit.push(n);
        }
        if closed && !lit.is_empty() {
            out.push(lit);
        }
    }
    out
}

/// Whether the line uses this name as a name, rather than inside a longer word.
fn mentions(line: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(hit) = line[from..].find(name) {
        let at = from + hit;
        let before = line[..at].chars().next_back();
        let after = line[at + name.len()..].chars().next();
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !word(before) && !word(after) {
            return true;
        }
        from = at + name.len();
    }
    false
}

// ───────────────────────────────────────────────────────────────────────────
//  What can be a path
// ───────────────────────────────────────────────────────────────────────────

/// Whether a bare string could name a file.
///
/// Deliberately shallow: this only has to reject the obvious non-paths, because
/// the caller then checks that the file is inside the repo and exists. The two
/// rules that do the work are "has a separator or a short extension" and "holds
/// no shell metacharacter" -- together they drop `utf-8`, `w`, `s/a/b/`, `{n}`,
/// `module.docking` and every URL, while keeping `lang/en.toml` and `main.rs`.
fn pathlike(s: &str) -> bool {
    if s.is_empty() || s.len() > 200 || s.starts_with('-') {
        return false;
    }
    if s.contains("://") || s.starts_with('~') {
        return false;
    }
    if s.chars().any(|c| {
        c.is_whitespace() || "*?$%{}[]()<>|&;!=,^`\"'".contains(c)
    }) {
        return false;
    }
    if s.contains('/') {
        // A `sed` script is all separators and no name: `s/old/new/g`.
        return !s.starts_with("s/") && !s.starts_with("s|");
    }
    let Some((stem, ext)) = s.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(cmd: &str) -> Vec<String> {
        writes(cmd)
    }

    #[test]
    fn a_redirect_names_its_target() {
        assert_eq!(w("cargo build > build.log"), vec!["build.log"]);
        assert_eq!(w("echo x >> src/notes.md"), vec!["src/notes.md"]);
    }

    #[test]
    fn a_descriptor_duplication_is_not_a_file() {
        assert!(w("cargo build 2>&1 | tail -5").is_empty());
        assert!(w("echo boom >&2").is_empty());
    }

    #[test]
    fn stderr_to_a_real_file_still_counts() {
        assert_eq!(w("cargo build 2> errors.txt"), vec!["errors.txt"]);
    }

    #[test]
    fn a_heredoc_body_is_not_shell() {
        // The body is full of `>` and unbalanced quotes. None of it is a
        // redirect, and the only write is the one the shell performs.
        let got = w("cat > src/ui.rs << 'EOF'\nlet a = if x > y { \"it's\" } else { 2 };\nEOF");
        assert_eq!(got, vec!["src/ui.rs"]);
    }

    #[test]
    fn tee_and_sed_and_cp_name_their_files() {
        assert_eq!(w("echo x | tee -a src/a.rs"), vec!["src/a.rs"]);
        assert_eq!(w("sed -i '' 's/old/new/g' src/b.rs"), vec!["src/b.rs"]);
        assert_eq!(w("sed -i 's/old/new/g' src/b.rs"), vec!["src/b.rs"]);
        assert_eq!(w("cp -r vendor/x.js assets/x.js"), vec!["assets/x.js"]);
    }

    #[test]
    fn sed_without_in_place_writes_nothing() {
        assert!(w("sed 's/old/new/g' src/b.rs").is_empty());
    }

    #[test]
    fn a_python_heredoc_that_names_the_path_on_the_write_line() {
        let cmd = "python3 - <<'PYEOF'\nopen('lang/en.toml', 'w').write(out)\nPYEOF";
        assert_eq!(w(cmd), vec!["lang/en.toml"]);
    }

    #[test]
    fn a_python_heredoc_that_names_the_path_three_lines_earlier() {
        // The shape every sampled session used. A literal-only reader finds
        // nothing here, which is the bug this module exists to close.
        let cmd = "python3 - <<'PYEOF'\n\
                   import re, sys\n\
                   path='lang/tutorial/en.toml'\n\
                   src=open(path,encoding='utf-8').read()\n\
                   src=src.replace(old,new)\n\
                   open(path,'w',encoding='utf-8').write(src)\n\
                   PYEOF";
        assert_eq!(w(cmd), vec!["lang/tutorial/en.toml"]);
    }

    #[test]
    fn a_file_only_read_is_not_reported() {
        let cmd = "python3 - <<'PYEOF'\n\
                   path='lang/en.toml'\n\
                   for l in open(path):\n\
                       print(l)\n\
                   PYEOF";
        // `print(` marks the line as a write -- it is, to stdout -- but the
        // path variable is not mentioned on it, so no file is claimed.
        assert!(w(cmd).is_empty());
    }

    #[test]
    fn prose_inside_a_body_is_not_a_path() {
        // Real content from a translation edit: tuple keys that look like
        // dotted names, and format placeholders.
        let cmd = "python3 - <<'PYEOF'\n\
                   edits = [('module.docking', 'name', 'Docking Bay')]\n\
                   open('lang/en.toml','w').write('{capacity} slots')\n\
                   PYEOF";
        assert_eq!(w(cmd), vec!["lang/en.toml"]);
    }

    #[test]
    fn pathlike_rejects_the_things_that_are_not_paths() {
        assert!(!pathlike("utf-8"));
        assert!(!pathlike("w"));
        assert!(!pathlike("s/old/new/g"));
        assert!(!pathlike("module.docking"));
        assert!(!pathlike("https://example.com/a.rs"));
        assert!(!pathlike("{names}"));
        assert!(!pathlike("-i"));
        assert!(pathlike("lang/en.toml"));
        assert!(pathlike("main.rs"));
        assert!(pathlike("/Users/d/repo/src/a.rs"));
        // A dotfile has no stem, so it is missed. See `pathlike`.
        assert!(!pathlike(".gitignore"));
    }

    #[test]
    fn a_pipeline_reports_every_stage() {
        let got = w("grep -rn foo src | tee hits.txt | sed -i '' 's/a/b/' src/c.rs");
        assert_eq!(got, vec!["hits.txt", "src/c.rs"]);
    }

    #[test]
    fn touch_is_not_a_write() {
        // `touch build.rs && cargo test` is a rebuild trick, not an edit.
        assert!(w("touch build.rs && cargo test --lib").is_empty());
    }

    #[test]
    fn an_env_prefix_does_not_hide_the_command() {
        assert_eq!(w("RUST_LOG=debug tee out.txt"), vec!["out.txt"]);
    }

    #[test]
    fn duplicates_collapse() {
        assert_eq!(w("echo a > x.txt; echo b >> x.txt"), vec!["x.txt"]);
    }

    #[test]
    fn the_delimiter_forms_all_parse() {
        assert_eq!(heredoc_delimiter("cat > a << 'EOF'").as_deref(), Some("EOF"));
        assert_eq!(heredoc_delimiter("cat > a <<EOF").as_deref(), Some("EOF"));
        assert_eq!(heredoc_delimiter("cat > a <<-\"PYEOF\"").as_deref(), Some("PYEOF"));
        assert_eq!(heredoc_delimiter("echo <<< word"), None);
        assert_eq!(heredoc_delimiter("cargo build"), None);
    }
}
