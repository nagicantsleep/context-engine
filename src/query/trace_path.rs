//! `trace-path` — read-only call-path tracing between two symbols.
//!
//! Answers "how does execution get from A to B" (direction = callees, the
//! default) or "through what chain is A reached from B" (direction = callers)
//! by depth-bounded DFS over the repo's `calls` table.
//!
//! Scope (v1): edges owned by THIS repo's database only. Per decision 0002 an
//! edge lives in the CALLER's DB, so a path that crosses a repo boundary
//! terminates here — it is never fabricated across databases. This keeps the
//! tool read-only, single-DB (no fan-out, per decision 0001), and bounded.
//!
//! Bounded by construction: per-path visited set (no cycles), a depth cap
//! (edges per path), a path cap, and a global expansion budget. When the
//! budget or the path cap stops the search early the outcome is flagged
//! `truncated` so the output can say "more paths may exist" instead of
//! implying completeness.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

/// Default edges per path.
pub const DEFAULT_MAX_DEPTH: usize = 5;
/// Hard cap on `max_depth` regardless of caller input.
pub const MAX_DEPTH_CAP: usize = 10;
/// Paths returned per trace.
pub const DEFAULT_MAX_PATHS: usize = 3;
/// Hard cap on returned paths.
const MAX_PATHS_CAP: usize = 8;
/// Global node-expansion budget for one trace.
const MAX_EXPANSIONS: usize = 512;
/// Neighbors considered per node (deterministic fqn-sorted truncation).
const MAX_NEIGHBORS_PER_NODE: usize = 32;
/// Symbol-lookup candidate bound.
const RESOLVE_LIMIT: i64 = 200;

/// Which edges to walk away from the start symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow callee edges: `from` calls … calls `to`.
    Callees,
    /// Follow caller edges: `from` is called by … called by `to`.
    Callers,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Direction> {
        match s.trim().to_ascii_lowercase().as_str() {
            "callees" | "downstream" => Some(Direction::Callees),
            "callers" | "upstream" => Some(Direction::Callers),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Direction::Callees => "callees",
            Direction::Callers => "callers",
        }
    }
}

/// A symbol resolved from user input.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSymbol {
    pub fqn: String,
    pub file: String,
    pub line_start: i64,
    pub line_end: i64,
    /// Declared kind (`function`, `method`, `class`, …) — `None` when the row
    /// predates kind extraction or the column is NULL.
    pub kind: Option<String>,
}

/// One node on a traced path.
#[derive(Debug, Clone, PartialEq)]
pub struct PathNode {
    pub fqn: String,
    pub file: String,
    /// True when the hop INTO this node is an inferred edge (confidence < 1.0
    /// — same rule as the `~inferred` MCP tags). Always false for the start.
    pub inferred: bool,
}

/// Result of one trace.
#[derive(Debug, Default)]
pub struct TraceOutcome {
    pub paths: Vec<Vec<PathNode>>,
    /// True when the search stopped early (expansion budget or path cap) —
    /// results may be partial.
    pub truncated: bool,
}

// ─── Impact analysis ────────────────────────────────────────────────────────
//
// The reverse question to `trace_paths`: not "how does A reach B" but "what
// breaks if B changes" — the transitive CALLER closure of one symbol, BFS over
// caller edges and grouped by hop distance (level 1 = direct callers).
//
// Same scope and bounds as path tracing: edges owned by THIS repo's DB only
// (decision 0002), read-only, deterministic ordering, and a global node budget
// with an honest `truncated` flag. Depth-cap exhaustion is a stated parameter,
// not partial coverage. Cycles are absorbed by the visited set — a symbol
// reports once, at its shallowest level.

/// Default BFS levels for impact analysis.
pub const DEFAULT_IMPACT_DEPTH: usize = 3;
/// Hard cap on impact depth regardless of caller input.
pub const MAX_IMPACT_DEPTH_CAP: usize = 8;
/// Global visited-node budget for one impact analysis.
pub const MAX_IMPACT_NODES: usize = 512;

/// One impacted symbol (a reachable caller).
#[derive(Debug, Clone, PartialEq)]
pub struct ImpactNode {
    pub fqn: String,
    pub file: String,
    /// True when the edge INTO this node is inferred (confidence < 1.0).
    pub inferred: bool,
}

/// Result of one impact analysis. `levels[i]` holds the callers at hop
/// distance `i + 1`.
#[derive(Debug, Default)]
pub struct ImpactOutcome {
    pub levels: Vec<Vec<ImpactNode>>,
    /// True when the node budget stopped the search while callers remained.
    pub truncated: bool,
}

pub async fn impacted(
    db: &Surreal<Db>,
    fqn: &str,
    max_depth: usize,
    max_nodes: usize,
) -> ImpactOutcome {
    let max_depth = max_depth.min(MAX_IMPACT_DEPTH_CAP).max(1);
    let mut outcome = ImpactOutcome::default();
    let mut visited: HashSet<String> = HashSet::from([fqn.to_string()]);
    // Frontier of (fqn, inferred-into) pairs; the start's "edge" is not an edge.
    let mut frontier: Vec<(String, bool)> = vec![(fqn.to_string(), false)];

    for _ in 0..max_depth {
        if frontier.is_empty() {
            break;
        }
        if visited.len() >= max_nodes {
            outcome.truncated = true;
            break;
        }
        let mut level: Vec<ImpactNode> = Vec::new();
        let mut next_frontier: Vec<(String, bool)> = Vec::new();
        for (node_fqn, _) in &frontier {
            if visited.len() >= max_nodes {
                outcome.truncated = true;
                break;
            }
            for (fqn, file, inferred) in neighbors(db, node_fqn, Direction::Callers)
                .await
                .unwrap_or_default()
            {
                if visited.contains(&fqn) {
                    continue; // cycles + cross-level dedup
                }
                visited.insert(fqn.clone());
                level.push(ImpactNode {
                    fqn: fqn.clone(),
                    file,
                    inferred,
                });
                next_frontier.push((fqn, inferred));
            }
        }
        if level.is_empty() {
            break;
        }
        outcome.levels.push(level);
        frontier = next_frontier;
    }
    outcome
}

#[derive(Clone, Deserialize)]
struct SymbolRow {
    fqn: String,
    file: String,
    // `Option` because a NULL column fails serde's i64 deserialization even
    // with `#[serde(default)]` (default covers MISSING fields only). Old or
    // partially-seeded rows may carry NULLs; the resolved symbol then reads 0.
    #[serde(default)]
    line_start: Option<i64>,
    #[serde(default)]
    line_end: Option<i64>,
    #[serde(default)]
    kind: Option<String>,
}

/// Resolve a user-supplied symbol reference against the repo's symbol table.
///
/// Accepted forms: full FQN (`/abs/file.rs::mod::name`), `file.rs::name`,
/// `::name`, or a bare `name`. Bare names match the indexed `name` column;
/// any `::` context the input carries filters by FQN suffix. Returns ALL
/// distinct matches — the caller turns >1 into a disambiguation message
/// rather than guessing.
pub async fn resolve_symbol(db: &Surreal<Db>, input: &str) -> Result<Vec<ResolvedSymbol>, String> {
    let tail = input.trim();
    if tail.is_empty() {
        return Err("symbol reference is empty".to_string());
    }
    let tail = tail.strip_prefix("::").unwrap_or(tail).trim();
    let name = tail.rsplit("::").next().unwrap_or(tail).trim();
    if name.is_empty() {
        return Err(format!("symbol reference '{input}' has no name segment"));
    }

    let fetched: Vec<SymbolRow> = db
        .query(
            "SELECT meta::id(id) AS fqn, file, line_start, line_end, kind FROM symbol \
             WHERE name = $name LIMIT $lim",
        )
        .bind(("name", name.to_string()))
        .bind(("lim", RESOLVE_LIMIT))
        .await
        .map_err(|e| format!("symbol lookup failed: {e}"))?
        .take(0)
        .map_err(|e| format!("symbol lookup failed: {e}"))?;
    let mut rows: Vec<SymbolRow> = fetched
        .into_iter()
        // `meta::id` yields the RECORD-ID string form, which wraps escaped
        // ids in `⟨…⟩` — symbol ids always are (their FQNs start with `/`).
        // Strip back to the bare FQN the rest of this module (and the calls
        // table's in_name/out_name columns) work with.
        .map(|mut r| {
            r.fqn = r
                .fqn
                .trim_start_matches('⟨')
                .trim_end_matches('⟩')
                .to_string();
            r
        })
        .collect();

    // Exact-FQN fast path: an input that IS a full FQN resolves uniquely.
    let exact: Vec<SymbolRow> = rows.iter().filter(|r| r.fqn == tail).cloned().collect();
    if !exact.is_empty() {
        rows = exact;
    } else if tail != name {
        rows.retain(|r| r.fqn.ends_with(tail));
    }

    rows.sort_by(|a, b| a.fqn.cmp(&b.fqn));
    rows.dedup_by(|a, b| a.fqn == b.fqn);
    Ok(rows
        .into_iter()
        .map(|r| ResolvedSymbol {
            fqn: r.fqn,
            file: r.file,
            line_start: r.line_start.unwrap_or(0),
            line_end: r.line_end.unwrap_or(0),
            kind: r.kind,
        })
        .collect())
}

/// Resolve a user-supplied reference to exactly ONE symbol. Zero hits and
/// ambiguous hits become errors whose message is agent-actionable
/// (disambiguation hints + a bounded candidate listing) — the tool layer
/// prints it verbatim.
pub async fn resolve_unique(
    db: &Surreal<Db>,
    input: &str,
    arg_name: &str,
) -> Result<ResolvedSymbol, String> {
    let hits = resolve_symbol(db, input).await?;
    match hits.len() {
        0 => Err(format!(
            "symbol '{input}' ({arg_name}) not found in the index. Use symbol \
             names exactly as they appear in code, and make sure the repo is indexed."
        )),
        1 => Ok(hits.into_iter().next().expect("len checked")),
        n => {
            let mut listing = String::new();
            for hit in hits.iter().take(10) {
                listing.push_str(&format!("  - {} ({})\n", hit.fqn, hit.file));
            }
            if n > 10 {
                listing.push_str(&format!("  … ({n} total)\n"));
            }
            Err(format!(
                "'{input}' ({arg_name}) is ambiguous — {n} symbols match. \
                 Disambiguate with file context (`file.rs::name`) or pass the full FQN:\n{listing}"
            ))
        }
    }
}

#[derive(Deserialize)]
struct EdgeRow {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    endpoint_file: String,
    #[serde(default)]
    confidence: Option<f32>,
}

/// Neighbors of `fqn` along `direction`, deduplicated with extracted edges
/// (confidence NULL or ≥1.0) outranking inferred ones, deterministically
/// fqn-sorted, and truncated to a fixed branching bound.
async fn neighbors(
    db: &Surreal<Db>,
    fqn: &str,
    direction: Direction,
) -> Result<Vec<(String, String, bool)>, String> {
    let sql = match direction {
        Direction::Callees => {
            "SELECT out_name AS endpoint, out_file AS endpoint_file, confidence \
             FROM calls WHERE in_name = $fqn"
        }
        Direction::Callers => {
            "SELECT in_name AS endpoint, in_file AS endpoint_file, confidence \
             FROM calls WHERE out_name = $fqn"
        }
    };
    let rows: Vec<EdgeRow> = db
        .query(sql)
        .bind(("fqn", fqn.to_string()))
        .await
        .map_err(|e| format!("edge query failed: {e}"))?
        .take(0)
        .map_err(|e| format!("edge query failed: {e}"))?;

    // fqn → (file, inferred confidence). None = extracted (strongest).
    let mut best: HashMap<String, (String, Option<f32>)> = HashMap::new();
    for row in rows {
        if row.endpoint.is_empty() {
            continue;
        }
        let incoming = row.confidence.filter(|c| *c < 1.0);
        match best.entry(row.endpoint.clone()) {
            Entry::Vacant(v) => {
                v.insert((row.endpoint_file, incoming));
            }
            Entry::Occupied(mut o) => {
                let better = match (o.get().1, incoming) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(a), Some(b)) => b > a,
                };
                if better {
                    o.insert((row.endpoint_file, incoming));
                }
            }
        }
    }

    let mut out: Vec<(String, String, bool)> = best
        .into_iter()
        .map(|(fqn, (file, confidence))| (fqn, file, confidence.is_some()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    out.truncate(MAX_NEIGHBORS_PER_NODE);
    Ok(out)
}

/// Trace up to `max_paths` acyclic paths between two RESOLVED FQNs.
///
/// `start_fqn`/`goal_fqn` must come from [`resolve_symbol`] (same repo DB);
/// both endpoints carry their file for display. Follows `direction` edges,
/// at most `max_depth` edges per path.
pub async fn trace_paths(
    db: &Surreal<Db>,
    start_fqn: &str,
    start_file: &str,
    goal_fqn: &str,
    direction: Direction,
    max_depth: usize,
    max_paths: usize,
) -> TraceOutcome {
    let max_paths = max_paths.min(MAX_PATHS_CAP).max(1);
    let max_depth = max_depth.min(MAX_DEPTH_CAP).max(1);
    let mut outcome = TraceOutcome::default();

    let start = PathNode {
        fqn: start_fqn.to_string(),
        file: start_file.to_string(),
        inferred: false,
    };
    if start_fqn == goal_fqn {
        outcome.paths.push(vec![start]);
        return outcome;
    }

    let mut walker = Walker {
        db,
        direction,
        goal: goal_fqn.to_string(),
        max_depth,
        max_paths,
        expansions: 0,
        truncated: false,
        paths: Vec::new(),
    };
    let mut path = vec![start.clone()];
    let mut on_path: HashSet<String> = HashSet::new();
    walker.walk(&start, 0, &mut path, &mut on_path).await;
    outcome.paths = walker.paths;
    outcome.truncated = walker.truncated;
    outcome
}

struct Walker<'a> {
    db: &'a Surreal<Db>,
    direction: Direction,
    goal: String,
    max_depth: usize,
    max_paths: usize,
    expansions: usize,
    truncated: bool,
    paths: Vec<Vec<PathNode>>,
}

impl Walker<'_> {
    /// DFS. `path` already ends with `node`; `on_path` holds the chain's fqns.
    async fn walk(
        &mut self,
        node: &PathNode,
        edges_used: usize,
        path: &mut Vec<PathNode>,
        on_path: &mut HashSet<String>,
    ) {
        if self.paths.len() >= self.max_paths || self.expansions >= MAX_EXPANSIONS {
            self.truncated = true;
            return;
        }
        if edges_used >= self.max_depth {
            // Natural depth exhaustion — the depth bound is a stated
            // parameter, not partial coverage, so this is NOT `truncated`.
            return;
        }
        self.expansions += 1;

        let neighbors = match neighbors(self.db, &node.fqn, self.direction).await {
            Ok(n) => n,
            Err(_) => return, // a dead read ends this branch, never the trace
        };
        for (fqn, file, inferred) in neighbors {
            if on_path.contains(&fqn) {
                continue; // cycle guard
            }
            let next = PathNode {
                fqn,
                file,
                inferred,
            };
            path.push(next.clone());
            on_path.insert(next.fqn.clone());
            if next.fqn == self.goal {
                self.paths.push(path.clone());
            } else {
                // Depth is capped at MAX_DEPTH_CAP, so the recursion is
                // bounded — boxing only satisfies the compiler's future-size
                // check for the recursive async call.
                Box::pin(self.walk(&next, edges_used + 1, path, on_path)).await;
            }
            on_path.remove(&next.fqn);
            path.pop();
            if self.paths.len() >= self.max_paths {
                self.truncated = true;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn seed_db(repo: &str) -> (TempDir, Surreal<Db>) {
        let home = TempDir::new().unwrap();
        let db = crate::store::open_db(home.path(), repo, 0).await.unwrap();
        crate::store::ops::set_meta(&db, crate::store::DB_SCHEMA_VERSION_KEY, "2")
            .await
            .unwrap();
        (home, db)
    }

    async fn add_symbol(db: &Surreal<Db>, fqn: &str, name: &str, file: &str) {
        db.query(format!(
            "CREATE symbol:`⟨{fqn}⟩` SET name = $name, file = $file, \
             line_start = 1, line_end = 2;",
        ))
        .bind(("name", name.to_string()))
        .bind(("file", file.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    }

    async fn add_edge(
        db: &Surreal<Db>,
        caller: &str,
        callee: &str,
        file: &str,
        confidence: Option<f32>,
    ) {
        db.query(
            "INSERT INTO calls { in_name: $in_name, out_name: $out_name, \
             in_file: $file, out_file: $file, line: 1, confidence: $confidence }",
        )
        .bind(("in_name", caller.to_string()))
        .bind(("out_name", callee.to_string()))
        .bind(("file", file.to_string()))
        .bind(("confidence", confidence))
        .await
        .unwrap()
        .check()
        .unwrap();
    }

    const A: &str = "/p/main.rs::main";
    const B: &str = "/p/lib.rs::run";
    const C: &str = "/p/lib.rs::inner";

    /// main → run → inner, all in one repo DB.
    async fn seed_chain() -> (TempDir, Surreal<Db>) {
        let (home, db) = seed_db("/p/trace").await;
        add_symbol(&db, A, "main", "/p/main.rs").await;
        add_symbol(&db, B, "run", "/p/lib.rs").await;
        add_symbol(&db, C, "inner", "/p/lib.rs").await;
        add_edge(&db, A, B, "/p/main.rs", None).await;
        add_edge(&db, B, C, "/p/lib.rs", None).await;
        (home, db)
    }

    #[tokio::test]
    async fn resolves_bare_name_and_suffix_context() {
        let (_home, db) = seed_chain().await;

        // Bare name.
        let hits = resolve_symbol(&db, "run").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fqn, B);

        // `::name` form.
        let hits = resolve_symbol(&db, "::inner").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fqn, C);

        // Full FQN fast path.
        let hits = resolve_symbol(&db, A).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fqn, A);

        // Missing symbol is a clean error, not a panic.
        assert!(resolve_symbol(&db, "nope").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ambiguous_name_returns_all_candidates() {
        let (home, db) = seed_db("/p/ambig").await;
        add_symbol(&db, "/x/a.rs::util", "util", "/x/a.rs").await;
        add_symbol(&db, "/x/b.rs::util", "util", "/x/b.rs").await;
        let hits = resolve_symbol(&db, "util").await.unwrap();
        assert_eq!(hits.len(), 2, "caller must disambiguate: {hits:?}");
        // Suffix context narrows it.
        let hits = resolve_symbol(&db, "b.rs::util").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].fqn, "/x/b.rs::util");
        drop(home);
    }

    #[tokio::test]
    async fn traces_two_hop_callee_path() {
        let (_home, db) = seed_chain().await;
        let outcome = trace_paths(&db, A, "/p/main.rs", C, Direction::Callees, 5, 3).await;
        assert!(!outcome.truncated);
        assert_eq!(outcome.paths.len(), 1);
        let path = &outcome.paths[0];
        assert_eq!(path.len(), 3);
        assert_eq!(path[0].fqn, A);
        assert_eq!(path[1].fqn, B);
        assert_eq!(path[2].fqn, C);
        assert!(!path[1].inferred && !path[2].inferred);
    }

    #[tokio::test]
    async fn traces_caller_direction_reverse() {
        let (_home, db) = seed_chain().await;
        // From inner UP to main: inner ← run ← main.
        let outcome = trace_paths(&db, C, "/p/lib.rs", A, Direction::Callers, 5, 3).await;
        assert_eq!(outcome.paths.len(), 1);
        assert_eq!(outcome.paths[0].len(), 3);
        assert_eq!(outcome.paths[0][1].fqn, B);
    }

    #[tokio::test]
    async fn depth_cap_ends_the_search_without_truncation_flag() {
        let (_home, db) = seed_chain().await;
        // Depth 1 cannot span a 2-edge chain — a stated bound, not partiality.
        let outcome = trace_paths(&db, A, "/p/main.rs", C, Direction::Callees, 1, 3).await;
        assert!(outcome.paths.is_empty());
        assert!(!outcome.truncated);
    }

    #[tokio::test]
    async fn cycle_is_never_walked_twice() {
        let (_home, db) = seed_chain().await;
        add_edge(&db, B, A, "/p/lib.rs", None).await; // main → run → main → …
        let outcome = trace_paths(&db, A, "/p/main.rs", C, Direction::Callees, 6, 3).await;
        assert_eq!(outcome.paths.len(), 1, "cycle must not multiply paths");
        assert_eq!(outcome.paths[0].len(), 3);
    }

    #[tokio::test]
    async fn inferred_edges_are_flagged_and_lose_to_extracted() {
        let (_home, db) = seed_chain().await;
        // A second, extracted (confidence NULL) edge into C plus an inferred one.
        add_edge(&db, B, C, "/p/lib.rs", Some(0.5)).await; // duplicate, weaker
        let outcome = trace_paths(&db, A, "/p/main.rs", C, Direction::Callees, 5, 3).await;
        assert!(!outcome.paths[0][2].inferred, "extracted edge must win");

        // Now ONLY an inferred edge into C (delete the extracted one by
        // rebuilding the chain without it).
        let (home2, db2) = seed_db("/p/trace2").await;
        add_symbol(&db2, A, "main", "/p/main.rs").await;
        add_symbol(&db2, C, "inner", "/p/lib.rs").await;
        add_edge(&db2, A, C, "/p/main.rs", Some(0.5)).await;
        let outcome = trace_paths(&db2, A, "/p/main.rs", C, Direction::Callees, 5, 3).await;
        assert!(outcome.paths[0][1].inferred, "inferred hop must be flagged");
        drop(home2);
    }

    #[tokio::test]
    async fn path_cap_stops_the_search_and_flags_truncation() {
        let (_home, db) = seed_chain().await;
        // Two distinct branches main → C: direct + via run.
        add_edge(&db, A, C, "/p/main.rs", None).await;
        let outcome = trace_paths(&db, A, "/p/main.rs", C, Direction::Callees, 5, 1).await;
        assert_eq!(outcome.paths.len(), 1);
        assert!(outcome.truncated, "path cap means more paths may exist");
    }

    #[tokio::test]
    async fn start_equals_goal_is_a_single_node_path() {
        let (_home, db) = seed_chain().await;
        let outcome = trace_paths(&db, B, "/p/lib.rs", B, Direction::Callees, 5, 3).await;
        assert_eq!(outcome.paths.len(), 1);
        assert_eq!(outcome.paths[0].len(), 1);
        assert!(!outcome.truncated);
    }

    // ── Impact analysis ─────────────────────────────────────────────────

    #[tokio::test]
    async fn impact_groups_callers_by_level_and_dedups() {
        let (_home, db) = seed_chain().await;
        // Shape: A→B→C plus D→B and A→D and E→A.
        //   level 1 from C: [B]
        //   level 2: callers of B: [A, D]
        //   level 3: callers of A: [E]; callers of D: none new
        add_edge(&db, "/p/extra.rs::d", B, "/p/extra.rs", None).await;
        add_symbol(&db, "/p/extra.rs::d", "d", "/p/extra.rs").await;
        add_edge(&db, A, "/p/extra.rs::d", "/p/main.rs", None).await;
        add_edge(&db, "/p/entry.rs::e", A, "/p/entry.rs", None).await;
        add_symbol(&db, "/p/entry.rs::e", "e", "/p/entry.rs").await;

        let outcome = impacted(&db, C, 3, 512).await;
        assert_eq!(outcome.levels.len(), 3);
        assert_eq!(outcome.levels[0].len(), 1);
        assert_eq!(outcome.levels[0][0].fqn, B);
        let lvl2: Vec<&str> = outcome.levels[1].iter().map(|n| n.fqn.as_str()).collect();
        assert!(
            lvl2.contains(&A) && lvl2.contains(&"/p/extra.rs::d"),
            "{lvl2:?}"
        );
        assert_eq!(outcome.levels[2].len(), 1);
        assert_eq!(outcome.levels[2][0].fqn, "/p/entry.rs::e");
        assert!(!outcome.truncated);
    }

    #[tokio::test]
    async fn impact_absorbs_cycles_without_duplicating() {
        let (_home, db) = seed_chain().await;
        // Cycle: C → A (so A is both a level-1 caller of B and reachable
        // again later). The visited set must keep A at its shallowest level.
        add_edge(&db, C, A, "/p/lib.rs", None).await;
        let outcome = impacted(&db, B, 5, 512).await;
        let all: Vec<&str> = outcome
            .levels
            .iter()
            .flatten()
            .map(|n| n.fqn.as_str())
            .collect();
        assert_eq!(all.iter().filter(|f| **f == A).count(), 1, "{all:?}");
        assert_eq!(outcome.levels[0][0].fqn, A, "shallowest level wins");
    }

    #[tokio::test]
    async fn impact_depth_cap_is_not_truncation() {
        let (_home, db) = seed_chain().await;
        // Chain deeper than the cap: depth 1 from C sees only B.
        let outcome = impacted(&db, C, 1, 512).await;
        assert_eq!(outcome.levels.len(), 1);
        assert!(!outcome.truncated, "stated depth bound, not partiality");
    }

    #[tokio::test]
    async fn impact_budget_stop_flags_truncated() {
        let (_home, db) = seed_chain().await;
        // max_nodes counts the analyzed symbol too — 2 stops after level 1.
        let outcome = impacted(&db, C, 3, 2).await;
        assert_eq!(outcome.levels.len(), 1);
        assert!(outcome.truncated, "callers remained when the budget ended");
    }

    #[tokio::test]
    async fn impact_of_unused_symbol_is_empty() {
        let (_home, db) = seed_chain().await;
        add_symbol(&db, "/p/orphan.rs::orphan", "orphan", "/p/orphan.rs").await;
        let outcome = impacted(&db, "/p/orphan.rs::orphan", 3, 512).await;
        assert!(outcome.levels.is_empty());
        assert!(!outcome.truncated);
    }
}
