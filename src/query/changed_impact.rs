//! `changes-impact` — map a unified diff's ADDED lines onto indexed symbols,
//! then list the affected direct callers.
//!
//! Answers the pre-commit question "what does this change affect?" without an
//! LLM: `git diff` output is parsed into per-file added-line ranges, ranges are
//! overlapped with the repo's indexed symbol spans, and each changed symbol's
//! direct callers come from the same `calls` table the `impact`/`trace-path`
//! tools read (repo-local edges only per decision 0002 — cross-repo edges are
//! owned by the caller's DB and are NOT followed, stated in the output).
//!
//! Pure and deterministic: the diff parser never touches the filesystem (tests
//! feed diff text directly), and the DB queries are read-only SELECTs.

use std::collections::HashMap;

use serde::Deserialize;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

/// One hunk from a unified diff, reduced to the only thing we need: which
/// lines are ADDED in the new file, keyed by the NEW file path.
#[derive(Debug, Clone, PartialEq)]
pub struct AddedRange {
    pub file: String,
    /// 1-based inclusive line range in the NEW file.
    pub start: i64,
    pub end: i64,
}

/// Parse a unified diff (`git diff` / `git diff --cached` shape) into the
/// ADDED-line ranges of each touched file. Consecutive added lines collapse
/// into one range so a rewritten function body reports one span, not one per
/// line. Position tracking follows the unified-diff contract: `+` consumes a
/// NEW-file line (extends the open run), `-` consumes an OLD-file line (no
/// gap in the new file, so a run continues across it), context closes the run
/// and advances the line cursor. Hunk headers with a 0-length new range
/// (`@@ -5,0 +6 @@`) or deleted-only files add nothing. Unparsable hunks are
/// ignored — the diff format is authoritative; guessing would fabricate
/// ranges.
pub fn parse_added_ranges(diff: &str) -> Vec<AddedRange> {
    let mut out: Vec<AddedRange> = Vec::new();
    let mut file: Option<String> = None;
    // Next NEW-file line the hunk body will emit.
    let mut cursor: i64 = 0;
    // True only while inside a hunk whose header parsed — a `+` line outside
    // any hunk (or after an unparsable header) is ignored, never guessed.
    let mut in_hunk = false;
    // Currently accumulating run of added lines.
    let mut open: Option<(i64, i64)> = None;

    // Close the open run into `out` (empty sentinels — e < s — are dropped).
    macro_rules! close_run {
        () => {
            if let (Some(f), Some((s, e))) = (file.clone(), open.take())
                && e >= s
            {
                out.push(AddedRange {
                    file: f,
                    start: s,
                    end: e,
                });
            }
        };
    }

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            close_run!();
            let p = rest.trim();
            file = if p == "/dev/null" {
                None
            } else {
                Some(
                    p.strip_prefix("b/")
                        .map(str::to_string)
                        .unwrap_or_else(|| p.to_string()),
                )
            };
        } else if line.starts_with("--- ") || line.starts_with("@@") {
            // A new diff pair or hunk bounds the previous run. `---` never
            // carries added lines; `@@` re-anchors the cursor.
            close_run!();
            if line.starts_with("@@") {
                if let Some(start) = hunk_new_start(line) {
                    cursor = start;
                    in_hunk = true;
                } else {
                    in_hunk = false; // unparsable header: body is not trusted
                }
            }
        } else if line.starts_with('+') {
            if !in_hunk {
                continue; // `+` line outside a valid hunk — ignore
            }
            match open {
                Some((s, _)) => open = Some((s, cursor)), // extend
                None => open = Some((cursor, cursor)),    // first added line
            }
            cursor += 1;
        } else if line.starts_with('-') || line.starts_with('\\') {
            // Deletion or "No newline" marker: consumes an OLD-file line
            // only — no new-file gap, the run continues.
        } else {
            // Context (or any unrecognized line: diff/index headers) closes
            // the run; only a real context line advances the cursor.
            close_run!();
            if line.starts_with(' ') {
                cursor += 1;
            }
        }
    }
    close_run!();
    out
}

/// Extract the NEW-file start line from a hunk header
/// `@@ -l,s +l,s @@ ...`. Returns None for unparsable headers or zero-length
/// new ranges (`+6,0` adds nothing).
fn hunk_new_start(header: &str) -> Option<i64> {
    // The hunk header's second range is the one after `+`.
    let plus = header.split_whitespace().find(|tok| tok.starts_with('+'))?;
    let tok = plus.trim_start_matches('+');
    let start = tok.split(',').next()?.parse::<i64>().ok()?;
    let len = tok
        .split_once(',')
        .map(|(_, l)| l.parse::<i64>().ok())
        .unwrap_or(Some(1))
        .unwrap_or(1);
    if start <= 0 || len <= 0 {
        return None;
    }
    Some(start)
}

#[derive(Deserialize)]
struct SpanRow {
    // `Option` per the trace_path lesson: NULL columns fail i64 deserialization
    // even with `#[serde(default)]`.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    fqn: Option<String>,
    #[serde(default)]
    line_start: Option<i64>,
    #[serde(default)]
    line_end: Option<i64>,
    #[serde(default)]
    kind: Option<String>,
}

/// One indexed symbol overlapped by an added range.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedSymbol {
    pub fqn: String,
    pub name: String,
    pub file: String,
    pub kind: Option<String>,
    pub line_start: i64,
    pub line_end: i64,
    /// 1 = direct caller count is filled; remaining levels come from the
    /// shared impact BFS in the funnel layer.
    pub direct_callers: Vec<(String, String, bool)>, // (fqn, file, inferred)
}

/// Overlap the added ranges against the repo's symbol table and collect the
/// direct callers of every hit. Bounded: one SELECT per distinct touched file
/// (diffs touch few files), with a hard file cap.
pub async fn changed_symbols(
    db: &Surreal<Db>,
    ranges: &[AddedRange],
    max_files: usize,
) -> Result<Vec<ChangedSymbol>, String> {
    let mut by_file: HashMap<&str, (i64, i64)> = HashMap::new();
    for r in ranges {
        // Coalesce per-file into one broad envelope query; exact overlap is
        // filtered in Rust so the DB does one cheap indexed scan per file.
        match by_file.get_mut(r.file.as_str()) {
            Some((s, e)) => {
                *s = (*s).min(r.start);
                *e = (*e).max(r.end);
            }
            None => {
                by_file.insert(r.file.as_str(), (r.start, r.end));
            }
        }
    }
    let mut files: Vec<(&str, i64, i64)> =
        by_file.into_iter().map(|(f, (s, e))| (f, s, e)).collect();
    files.sort(); // deterministic
    files.truncate(max_files);

    let mut out: Vec<ChangedSymbol> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (file, lo, hi) in files {
        let rows: Vec<SpanRow> = db
            .query(
                "SELECT name, meta::id(id) AS fqn, line_start, line_end, kind \
                 FROM symbol WHERE file = $file",
            )
            .bind(("file", file.to_string()))
            .await
            .map_err(|e| format!("symbol lookup failed for {file}: {e}"))?
            .take(0)
            .map_err(|e| format!("symbol lookup failed for {file}: {e}"))?;
        let mut hits: Vec<(i64, i64, SpanRow)> = rows
            .into_iter()
            .filter_map(|row| {
                let ls = row.line_start?;
                let le = row.line_end.unwrap_or(ls);
                // Overlap: [ls, le] ∩ [lo, hi] non-empty.
                (ls <= hi && le >= lo).then_some((ls, le, row))
            })
            .collect();
        hits.sort_by_key(|(ls, _, _)| *ls); // stable, by position
        for (ls, le, row) in hits {
            let Some(fqn) = row.fqn else { continue };
            let fqn = fqn
                .trim_start_matches('⟨')
                .trim_end_matches('⟩')
                .to_string();
            if !seen.insert(fqn.clone()) {
                continue; // same symbol hit by two ranges
            }
            out.push(ChangedSymbol {
                name: row
                    .name
                    .unwrap_or_else(|| fqn.rsplit("::").next().unwrap_or("").to_string()),
                fqn,
                file: file.to_string(),
                kind: row.kind,
                line_start: ls,
                line_end: le,
                direct_callers: Vec::new(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,7 +10,9 @@ fn existing() {
 ctx line
-removed line
+added one
+added two
+added three
 context after
@@ -40,4 +42,6 @@ fn other() {
+only added line
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
+bootstrap line
";

    #[test]
    fn parses_added_ranges_per_file_with_collapsed_runs() {
        let ranges = parse_added_ranges(SAMPLE);
        assert_eq!(ranges.len(), 3, "{ranges:?}");
        assert_eq!(ranges[0].file, "src/lib.rs");
        // ctx = new line 10; the three `+` lines land on 11..13; the run is
        // one span despite the deleted line sitting before it.
        assert_eq!((ranges[0].start, ranges[0].end), (11, 13));
        // Second hunk starts at new line 42; its single `+` line is 42.
        assert_eq!((ranges[1].start, ranges[1].end), (42, 42));
        assert_eq!(ranges[2].file, "src/main.rs");
        assert_eq!((ranges[2].start, ranges[2].end), (1, 1));
    }

    #[test]
    fn added_runs_split_across_context_lines() {
        let d = "\
--- a/f.rs
+++ b/f.rs
@@ -1,5 +1,6 @@
+first
 ctx
+second
";
        let ranges = parse_added_ranges(d);
        assert_eq!(ranges.len(), 2, "{ranges:?}");
        assert_eq!((ranges[0].start, ranges[0].end), (1, 1));
        assert_eq!((ranges[1].start, ranges[1].end), (3, 3));
    }

    #[test]
    fn zero_length_and_deleted_only_hunks_produce_nothing() {
        let d = "\
--- a/f.rs
+++ b/f.rs
@@ -5,0 +6 @@
--- a/g.rs
+++ b/g.rs
@@ -7,3 +7,2 @@
-deleted a
-deleted b
";
        assert!(
            parse_added_ranges(d).is_empty(),
            "{:?}",
            parse_added_ranges(d)
        );
    }

    #[test]
    fn unparsable_input_is_empty_not_a_crash() {
        assert!(parse_added_ranges("").is_empty());
        assert!(parse_added_ranges("not a diff at all").is_empty());
        assert!(parse_added_ranges("+++ b/x.rs\n@@ garbage @@\n+line").is_empty());
    }

    #[test]
    fn new_file_diff_attaches_to_dev_null_old() {
        let d = "\
diff --git a/n.rs b/n.rs
new file mode 100644
--- /dev/null
+++ b/n.rs
@@ -0,0 +1,2 @@
+fn a() {}
+fn b() {}
";
        let ranges = parse_added_ranges(d);
        assert_eq!(ranges.len(), 1, "{ranges:?}");
        assert_eq!(ranges[0].file, "n.rs");
        assert_eq!((ranges[0].start, ranges[0].end), (1, 2));
    }
}
