use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::Deserialize;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tracing::warn;

use crate::query::find_db_for_file;
use crate::query::merger::MergeChunk;

/// An expanded chunk produced by BFS graph traversal.
pub struct ExpandedChunk {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub score: f32,
    pub content: String,
    pub symbol: Option<String>,
    pub symbol_fqn: Option<String>,
    pub symbol_kind: Option<String>,
}

// ─── DB row types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SymbolRow {
    /// Full FQN from `meta::id(id)` (file::scope::name), used as the BFS seed key.
    /// Matches the stored `calls.in_name` / `out_name` form so methods resolve.
    #[serde(default)]
    fqn: String,
    file: String,
    name: String,
    line_start: i64,
    line_end: i64,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct ChunkRow {
    file: String,
    line_start: i64,
    line_end: i64,
    content: String,
}

// ─── Graph expansion ──────────────────────────────────────────────────────

const CALLER_SCORE_FACTOR: f32 = 0.6;
const CALLEE_SCORE_FACTOR: f32 = 0.5;
const SCORE_FLOOR: f32 = 0.15;
const MAX_DEPTH: usize = 2;
const MAX_BONUS_CHUNKS: usize = 30;

/// Expand base search results via BFS over the call graph.
///
/// For each chunk in `base_chunks`, finds overlapping symbols, then BFS-expands
/// callers (score × 0.6) and callees (score × 0.5) up to 2 levels deep.
/// Returns up to MAX_BONUS_CHUNKS additional chunks.
///
/// `schema_version`: when >= 2, uses indexed `WHERE out_name=$name` queries.
/// When < 2 (migration in progress), falls back to the old link-deref query.
pub async fn graph_expand(
    base_chunks: &[MergeChunk],
    db_map: &HashMap<String, Surreal<Db>>,
    schema_version: u32,
) -> Vec<ExpandedChunk> {
    if db_map.is_empty() {
        return vec![];
    }

    // ── Attribution counters (measurement only; no behavior change) ──
    let start = std::time::Instant::now();
    let base_chunks_count: usize = base_chunks.len();
    let mut overlapping_calls: u64 = 0; // # of query_overlapping_symbols invocations
    let mut overlapping_total: u64 = 0; // sum of overlapping.len() across calls
    let mut nodes_popped: u64 = 0; // # of BFS loop-body executions
    let mut queue_max: usize = 0; // max queue length observed
    let mut callers_queries: u64 = 0; // # of query_callers calls
    let mut callees_queries: u64 = 0; // # of query_callees calls
    let mut fetch_calls: u64 = 0; // # of fetch_chunk_for_fqn calls

    let mut all_expanded: Vec<ExpandedChunk> = Vec::new();
    let mut expanded_index: HashMap<String, usize> = HashMap::new();
    let mut best_result_score: HashMap<String, f32> = HashMap::new();
    let mut best_state_score: HashMap<(String, usize), f32> = HashMap::new();

    let base_keys: HashSet<(String, u32, u32)> = base_chunks
        .iter()
        .map(|c| (c.file.clone(), c.line_start, c.line_end))
        .collect();

    for base_chunk in base_chunks {
        let db = match find_db_for_file(db_map, &base_chunk.file) {
            Some(db) => db,
            None => continue,
        };

        let overlapping = match query_overlapping_symbols(
            db,
            &base_chunk.file,
            base_chunk.line_start,
            base_chunk.line_end,
        )
        .await
        {
            Ok(syms) => syms,
            Err(e) => {
                overlapping_calls += 1;
                warn!(error = %e, file = %base_chunk.file, "failed to query overlapping symbols");
                continue;
            }
        };
        overlapping_calls += 1;
        overlapping_total += overlapping.len() as u64;

        if overlapping.is_empty() {
            continue;
        }

        let mut queue: Vec<(String, f32, usize)> = overlapping
            .iter()
            .map(|s| (strip_id_brackets(&s.fqn), base_chunk.score, 0))
            .collect();
        for (fqn, score, depth) in &queue {
            best_state_score
                .entry((fqn.clone(), *depth))
                .and_modify(|best| *best = best.max(*score))
                .or_insert(*score);
        }
        queue_max = queue_max.max(queue.len());

        while let Some((fqn, score, depth)) = queue.pop() {
            nodes_popped += 1;
            // A stronger path may have reached this endpoint after this frontier
            // entry was queued. Skip stale work; the stronger entry is also queued.
            if best_state_score
                .get(&(fqn.clone(), depth))
                .is_some_and(|best| score < *best)
            {
                continue;
            }
            if depth >= MAX_DEPTH {
                continue;
            }

            // Expand callers.
            let caller_score = score * CALLER_SCORE_FACTOR;
            if caller_score >= SCORE_FLOOR {
                callers_queries += 1;
                let callers = query_callers(db_map, &fqn, schema_version)
                    .await
                    .unwrap_or_default();
                for (caller_fqn, caller_file, edge_confidence) in callers {
                    let confidence_multiplier = edge_confidence.unwrap_or(1.0);
                    let caller_score = score * CALLER_SCORE_FACTOR * confidence_multiplier;
                    let next_depth = depth + 1;
                    if caller_score < SCORE_FLOOR
                        || best_state_score
                            .get(&(caller_fqn.clone(), next_depth))
                            .is_some_and(|best| caller_score <= *best)
                    {
                        continue;
                    }
                    let Some(endpoint_db) = find_db_for_file(db_map, &caller_file) else {
                        continue;
                    };
                    fetch_calls += 1;
                    if let Some(chunk) =
                        fetch_chunk_for_fqn(endpoint_db, &caller_fqn, caller_score, &base_keys)
                            .await
                    {
                        // Preserve the result cap: once full, an unseen endpoint
                        // cannot be returned or improve an existing result, so do not
                        // broaden traversal through it. Existing endpoints may still be
                        // replaced and requeued when a stronger path arrives.
                        if !expanded_index.contains_key(&caller_fqn)
                            && all_expanded.len() >= MAX_BONUS_CHUNKS
                        {
                            continue;
                        }
                        best_state_score.insert((caller_fqn.clone(), next_depth), caller_score);
                        let improves_result = best_result_score
                            .get(&caller_fqn)
                            .is_none_or(|best| caller_score > *best);
                        if improves_result {
                            best_result_score.insert(caller_fqn.clone(), caller_score);
                            if let Some(&index) = expanded_index.get(&caller_fqn) {
                                all_expanded[index] = chunk;
                            } else {
                                expanded_index.insert(caller_fqn.clone(), all_expanded.len());
                                all_expanded.push(chunk);
                            }
                        }
                        queue.push((caller_fqn, caller_score, next_depth));
                        queue_max = queue_max.max(queue.len());
                    }
                }
            }

            // Expand callees.
            let callee_score = score * CALLEE_SCORE_FACTOR;
            if callee_score >= SCORE_FLOOR {
                callees_queries += 1;
                let callees = query_callees(db_map, &fqn, schema_version)
                    .await
                    .unwrap_or_default();
                for (callee_fqn, callee_file, edge_confidence) in callees {
                    let confidence_multiplier = edge_confidence.unwrap_or(1.0);
                    let callee_score = score * CALLEE_SCORE_FACTOR * confidence_multiplier;
                    let next_depth = depth + 1;
                    if callee_score < SCORE_FLOOR
                        || best_state_score
                            .get(&(callee_fqn.clone(), next_depth))
                            .is_some_and(|best| callee_score <= *best)
                    {
                        continue;
                    }
                    let Some(endpoint_db) = find_db_for_file(db_map, &callee_file) else {
                        continue;
                    };
                    fetch_calls += 1;
                    if let Some(chunk) =
                        fetch_chunk_for_fqn(endpoint_db, &callee_fqn, callee_score, &base_keys)
                            .await
                    {
                        // Preserve the result cap: once full, an unseen endpoint
                        // cannot be returned or improve an existing result, so do not
                        // broaden traversal through it. Existing endpoints may still be
                        // replaced and requeued when a stronger path arrives.
                        if !expanded_index.contains_key(&callee_fqn)
                            && all_expanded.len() >= MAX_BONUS_CHUNKS
                        {
                            continue;
                        }
                        best_state_score.insert((callee_fqn.clone(), next_depth), callee_score);
                        let improves_result = best_result_score
                            .get(&callee_fqn)
                            .is_none_or(|best| callee_score > *best);
                        if improves_result {
                            best_result_score.insert(callee_fqn.clone(), callee_score);
                            if let Some(&index) = expanded_index.get(&callee_fqn) {
                                all_expanded[index] = chunk;
                            } else {
                                expanded_index.insert(callee_fqn.clone(), all_expanded.len());
                                all_expanded.push(chunk);
                            }
                        }
                        queue.push((callee_fqn, callee_score, next_depth));
                        queue_max = queue_max.max(queue.len());
                    }
                }
            }
        }
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    // db.query(...).await executions issued across all helper fns:
    //   query_overlapping_symbols = 1 each, query_callers = 1, query_callees = 1,
    //   fetch_chunk_for_fqn = 2 each (symbol lookup + chunk lookup).
    let db_queries_total: u64 =
        overlapping_calls + callers_queries + callees_queries + fetch_calls * 2;
    let expanded_returned = all_expanded.len();
    tracing::debug!(
        base_chunks = base_chunks_count,
        overlapping_total,
        nodes_popped,
        queue_max,
        callers_queries,
        callees_queries,
        fetch_calls,
        db_queries_total,
        expanded_returned,
        elapsed_ms,
        "graph_expand attribution"
    );

    all_expanded
}

// ─── Helpers ──────────────────────────────────────────────────────────────

/// Strip the SurrealDB complex-ID wrapper `⟨…⟩` returned by `meta::id(id)`.
/// A record id `symbol:⟨/foo.cpp::Bar::baz⟩` projects as `⟨/foo.cpp::Bar::baz⟩`;
/// this recovers the plain FQN that `calls.in_name` / `out_name` store.
fn strip_id_brackets(id: &str) -> String {
    id.strip_prefix("⟨")
        .and_then(|s| s.strip_suffix("⟩"))
        .unwrap_or(id)
        .to_string()
}

async fn query_overlapping_symbols(
    db: &Surreal<Db>,
    file: &str,
    chunk_start: u32,
    chunk_end: u32,
) -> Result<Vec<SymbolRow>> {
    let rows: Vec<SymbolRow> = db
        .query(
            "SELECT meta::id(id) AS fqn, file, name, line_start, line_end, kind FROM symbol \
             WHERE file = $file AND line_start <= $chunk_end AND line_end >= $chunk_start",
        )
        .bind(("file", file.to_string()))
        .bind(("chunk_end", chunk_end as i64))
        .bind(("chunk_start", chunk_start as i64))
        .await?
        .take(0)?;
    Ok(rows)
}

/// Query callers of the symbol identified by `fqn`.
///
/// Uses indexed `in_name`/`out_name` columns which now store full FQNs.
/// The `schema_version` parameter is retained for API compatibility but
/// the v1 link-deref fallback is no longer accurate since in_name/out_name
/// now store FQNs (v2+ schema). For v1 DBs the fallback path is kept
/// for graceful degradation.
async fn query_callers(
    db_map: &HashMap<String, Surreal<Db>>,
    fqn: &str,
    schema_version: u32,
) -> Result<Vec<(String, String, Option<f32>)>> {
    #[derive(Deserialize)]
    struct Row {
        in_name: String,
        in_file: String,
        #[serde(default)]
        confidence: Option<f32>,
    }

    let mut best: HashMap<(String, String), Option<f32>> = HashMap::new();
    for db in db_map.values() {
        let rows: Vec<Row> = if schema_version >= 2 {
            db.query("SELECT in_name, in_file, confidence FROM calls WHERE out_name = $fqn")
                .bind(("fqn", fqn.to_string()))
                .await?
                .take(0)?
        } else {
            let name = fqn.rsplit("::").next().unwrap_or(fqn);
            #[derive(Deserialize)]
            struct V1Row {
                in_file: String,
            }
            let v1_rows: Vec<V1Row> = db
                .query("SELECT in_file FROM calls WHERE out.name = $name LIMIT 20")
                .bind(("name", name.to_string()))
                .await?
                .take(0)?;
            for row in v1_rows {
                best.entry((format!("{}::{}", row.in_file, name), row.in_file))
                    .or_insert(None);
            }
            continue;
        };
        for row in rows {
            merge_endpoint_confidence(&mut best, row.in_name, row.in_file, row.confidence);
        }
    }
    let mut callers: Vec<_> = best
        .into_iter()
        .map(|((fqn, file), confidence)| (fqn, file, confidence))
        .collect();
    callers.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    callers.truncate(20);
    Ok(callers)
}

/// Query callees across all caller databases. Cross-repo edges live in the
/// caller's DB, so looking only in the endpoint DB loses valid edges.
async fn query_callees(
    db_map: &HashMap<String, Surreal<Db>>,
    fqn: &str,
    schema_version: u32,
) -> Result<Vec<(String, String, Option<f32>)>> {
    #[derive(Deserialize)]
    struct Row {
        out_name: String,
        out_file: String,
        #[serde(default)]
        confidence: Option<f32>,
    }

    let mut best: HashMap<(String, String), Option<f32>> = HashMap::new();
    for db in db_map.values() {
        let rows: Vec<Row> = if schema_version >= 2 {
            db.query("SELECT out_name, out_file, confidence FROM calls WHERE in_name = $fqn")
                .bind(("fqn", fqn.to_string()))
                .await?
                .take(0)?
        } else {
            let name = fqn.rsplit("::").next().unwrap_or(fqn);
            #[derive(Deserialize)]
            struct V1Row {
                out_file: String,
            }
            let v1_rows: Vec<V1Row> = db
                .query("SELECT out_file FROM calls WHERE in.name = $name LIMIT 20")
                .bind(("name", name.to_string()))
                .await?
                .take(0)?;
            for row in v1_rows {
                best.entry((format!("{}::{}", row.out_file, name), row.out_file))
                    .or_insert(None);
            }
            continue;
        };
        for row in rows {
            merge_endpoint_confidence(&mut best, row.out_name, row.out_file, row.confidence);
        }
    }
    let mut callees: Vec<_> = best
        .into_iter()
        .map(|((fqn, file), confidence)| (fqn, file, confidence))
        .collect();
    callees.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    callees.truncate(20);
    Ok(callees)
}

/// Keep the strongest duplicate edge. `None` means parser-extracted and has
/// multiplier 1.0, so it outranks inferred confidence below 1.0.
fn merge_endpoint_confidence(
    best: &mut HashMap<(String, String), Option<f32>>,
    fqn: String,
    file: String,
    confidence: Option<f32>,
) {
    let new_weight = confidence.unwrap_or(1.0);
    best.entry((fqn, file))
        .and_modify(|current| {
            if new_weight > current.unwrap_or(1.0) {
                *current = confidence;
            }
        })
        .or_insert(confidence);
}

async fn fetch_chunk_for_fqn(
    db: &Surreal<Db>,
    fqn: &str,
    score: f32,
    base_keys: &HashSet<(String, u32, u32)>,
) -> Option<ExpandedChunk> {
    // Resolve by full record id (the symbol id IS the FQN). This avoids the old
    // `rfind("::")` split, which mis-derived file_prefix for methods/namespaced
    // symbols (e.g. "x.cpp::Foo::bar" → file "x.cpp::Foo", matching no file) and
    // silently dropped every method-target expansion.
    let thing =
        surrealdb::sql::Thing::from(("symbol", surrealdb::sql::Id::String(fqn.to_string())));

    // Direct record fetch via `FROM $thing` — NOT `FROM symbol WHERE id = $thing`.
    // SurrealDB 2.6.5 does NOT optimize `WHERE id = $thing` into a primary-key lookup:
    // EXPLAIN shows "Iterate Table (FULL TABLE SCAN)" and a timed exec measured 14044 ms
    // to return 1 row on the 2.63M-row kernel symbol table. Binding the Thing as the FROM
    // target is a direct record fetch (0.137 ms — ~100,000× faster) and returns the
    // identical row. Same correct pattern as `fetch_symbol_kind` in engine.rs.
    let sym_rows: Vec<SymbolRow> = db
        .query(
            "SELECT meta::id(id) AS fqn, file, name, line_start, line_end, kind FROM $thing LIMIT 1",
        )
        .bind(("thing", thing))
        .await
        .ok()?
        .take(0)
        .ok()?;

    let sym = sym_rows.into_iter().next()?;

    let chunk_rows: Vec<ChunkRow> = db
        .query(
            "SELECT file, line_start, line_end, content FROM chunk \
             WHERE file = $file AND line_start <= $sym_end AND line_end >= $sym_start \
             ORDER BY line_start LIMIT 1",
        )
        .bind(("file", sym.file.clone()))
        .bind(("sym_end", sym.line_end))
        .bind(("sym_start", sym.line_start))
        .await
        .ok()?
        .take(0)
        .ok()?;

    let row = chunk_rows.into_iter().next()?;
    let ls = row.line_start as u32;
    let le = row.line_end as u32;

    if base_keys.contains(&(row.file.clone(), ls, le)) {
        return None;
    }

    Some(ExpandedChunk {
        file: row.file,
        line_start: ls,
        line_end: le,
        score,
        content: row.content,
        symbol: Some(sym.name),
        symbol_fqn: Some(strip_id_brackets(&sym.fqn)),
        symbol_kind: sym.kind,
    })
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::open_db;
    use tempfile::TempDir;

    /// Insert one symbol via a bound `Thing` so its record id IS the namespaced
    /// FQN — mirrors how `flush_symbol_batch_native` writes ids (clean string id,
    /// no literal ⟨⟩ baked in). This produces the exact record-id form that the
    /// old `rfind("::")` file-prefix split mis-derived and silently dropped.
    async fn insert_symbol(
        db: &Surreal<Db>,
        fqn: &str,
        file: &str,
        name: &str,
        line_start: i64,
        line_end: i64,
    ) {
        let thing =
            surrealdb::sql::Thing::from(("symbol", surrealdb::sql::Id::String(fqn.to_string())));
        db.query(
            "CREATE $t SET name = $n, kind = 'method', file = $f, \
             line_start = $ls, line_end = $le, signature = NONE, parent = NONE",
        )
        .bind(("t", thing))
        .bind(("n", name.to_string()))
        .bind(("f", file.to_string()))
        .bind(("ls", line_start))
        .bind(("le", line_end))
        .await
        .expect("insert symbol");
    }

    async fn insert_chunk(
        db: &Surreal<Db>,
        file: &str,
        line_start: i64,
        line_end: i64,
        content: &str,
    ) {
        db.query("CREATE chunk SET file = $f, line_start = $ls, line_end = $le, content = $c")
            .bind(("f", file.to_string()))
            .bind(("ls", line_start))
            .bind(("le", line_end))
            .bind(("c", content.to_string()))
            .await
            .expect("insert chunk");
    }

    /// Locks the fix: `fetch_chunk_for_fqn` must resolve a symbol whose id is a
    /// method/namespaced FQN (`x.cpp::Foo::bar`) directly by record id and return
    /// the overlapping chunk. The previous `rfind("::")` split derived file
    /// `x.cpp::Foo` from this FQN, matched no file, and silently dropped the
    /// expansion — this asserts the method-FQN resolution that bug broke.
    #[tokio::test]
    async fn fetch_chunk_for_fqn_resolves_namespaced_method_fqn() {
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/test/graph_expand", 0).await.unwrap();

        let fqn = "x.cpp::Foo::bar";
        let file = "x.cpp";
        // Symbol spans lines 10..20; chunk covers 8..25 (overlaps the symbol).
        insert_symbol(&db, fqn, file, "bar", 10, 20).await;
        insert_chunk(&db, file, 8, 25, "void Foo::bar() {}").await;

        let base_keys: HashSet<(String, u32, u32)> = HashSet::new();
        let got = fetch_chunk_for_fqn(&db, fqn, 0.42, &base_keys).await;

        let chunk = got.expect(
            "method-FQN symbol must resolve by record id and return its overlapping chunk \
             (the rfind(\"::\") split silently dropped this case)",
        );
        assert_eq!(chunk.file, file);
        assert_eq!(chunk.line_start, 8);
        assert_eq!(chunk.line_end, 25);
        assert_eq!(chunk.score, 0.42);
        assert_eq!(chunk.symbol.as_deref(), Some("bar"));
        // meta::id(id) wraps the special-char FQN in ⟨⟩; strip_id_brackets recovers it.
        assert_eq!(chunk.symbol_fqn.as_deref(), Some(fqn));
        assert_eq!(chunk.symbol_kind.as_deref(), Some("method"));
    }

    /// The base_keys dedup path: when the resolved chunk's (file, start, end) is
    /// already in `base_keys`, the function must suppress it (returns None) to
    /// avoid re-emitting a chunk that the base search already surfaced.
    #[tokio::test]
    async fn fetch_chunk_for_fqn_dedups_against_base_keys() {
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/test/graph_expand_dedup", 0)
            .await
            .unwrap();

        let fqn = "x.cpp::Foo::bar";
        let file = "x.cpp";
        insert_symbol(&db, fqn, file, "bar", 10, 20).await;
        insert_chunk(&db, file, 8, 25, "void Foo::bar() {}").await;

        let mut base_keys: HashSet<(String, u32, u32)> = HashSet::new();
        base_keys.insert((file.to_string(), 8, 25));

        let got = fetch_chunk_for_fqn(&db, fqn, 0.42, &base_keys).await;
        assert!(
            got.is_none(),
            "chunk already present in base_keys must be deduped (None)"
        );
    }

    /// Extracted edge must score higher than an Inferred(0.5) edge from the same seed.
    /// Locks the BFS confidence-multiplier behaviour:
    ///   score * CALLEE_SCORE_FACTOR * 1.0  >  score * CALLEE_SCORE_FACTOR * 0.5
    #[tokio::test]
    async fn bfs_weights_inferred_edges_lower_than_extracted() {
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/test/bfs_confidence", 0)
            .await
            .unwrap();

        // Seed symbol overlaps with the base chunk we pass to graph_expand.
        insert_symbol(&db, "/seed.rs::seed_fn", "/seed.rs", "seed_fn", 1, 10).await;

        // Callee A — extracted (confidence NULL)
        insert_symbol(&db, "/a.rs::a_fn", "/a.rs", "a_fn", 1, 10).await;
        insert_chunk(&db, "/a.rs", 1, 10, "fn a_fn() {}").await;

        // Callee B — inferred confidence 0.5
        insert_symbol(&db, "/b.rs::b_fn", "/b.rs", "b_fn", 1, 10).await;
        insert_chunk(&db, "/b.rs", 1, 10, "fn b_fn() {}").await;

        // Extracted edge seed_fn → a_fn (confidence absent = NULL = Extracted)
        db.query(
            "INSERT INTO calls { \
               in_name: '/seed.rs::seed_fn', out_name: '/a.rs::a_fn', \
               line: 5, in_file: '/seed.rs', out_file: '/a.rs', \
               in: (SELECT VALUE id FROM symbol WHERE meta::id(id) = '/seed.rs::seed_fn' LIMIT 1)[0], \
               out: (SELECT VALUE id FROM symbol WHERE meta::id(id) = '/a.rs::a_fn' LIMIT 1)[0] \
             }",
        )
        .await
        .expect("insert extracted call");

        // Inferred edge seed_fn → b_fn (confidence 0.5)
        db.query(
            "INSERT INTO calls { \
               in_name: '/seed.rs::seed_fn', out_name: '/b.rs::b_fn', \
               line: 6, in_file: '/seed.rs', out_file: '/b.rs', \
               in: (SELECT VALUE id FROM symbol WHERE meta::id(id) = '/seed.rs::seed_fn' LIMIT 1)[0], \
               out: (SELECT VALUE id FROM symbol WHERE meta::id(id) = '/b.rs::b_fn' LIMIT 1)[0], \
               confidence: 0.5 \
             }",
        )
        .await
        .expect("insert inferred call");

        let seed_chunk = MergeChunk {
            file: "/seed.rs".to_string(),
            line_start: 1,
            line_end: 10,
            score: 1.0,
            content: "fn seed_fn() {}".to_string(),
            symbol: None,
            symbol_fqn: None,
            symbol_kind: None,
        };

        let mut db_map = HashMap::new();
        db_map.insert("/test/bfs_confidence".to_string(), db);

        // schema_version=2: uses the fast indexed path for callers/callees.
        let expanded = graph_expand(&[seed_chunk], &db_map, 2).await;

        let a_score = expanded.iter().find(|c| c.file == "/a.rs").map(|c| c.score);
        let b_score = expanded.iter().find(|c| c.file == "/b.rs").map(|c| c.score);

        assert!(
            a_score.is_some(),
            "extracted callee must appear in expansion"
        );
        assert!(
            b_score.is_some(),
            "inferred callee must appear in expansion"
        );
        assert!(
            a_score.unwrap() > b_score.unwrap(),
            "extracted edge (mult 1.0) must score higher than inferred edge (mult 0.5): \
             a={:?} b={:?}",
            a_score,
            b_score
        );
    }

    async fn insert_call(
        db: &Surreal<Db>,
        from_fqn: &str,
        from_file: &str,
        to_fqn: &str,
        to_file: &str,
        confidence: Option<f32>,
    ) {
        db.query(
            "INSERT INTO calls { in_name: $from, out_name: $to, in_file: $from_file, \
             out_file: $to_file, line: 1, confidence: $confidence }",
        )
        .bind(("from", from_fqn.to_string()))
        .bind(("to", to_fqn.to_string()))
        .bind(("from_file", from_file.to_string()))
        .bind(("to_file", to_file.to_string()))
        .bind(("confidence", confidence))
        .await
        .expect("insert call")
        .check()
        .expect("insert call statement");
    }

    #[tokio::test]
    async fn cross_repo_callee_is_fetched_from_owning_db() {
        let home = TempDir::new().unwrap();
        let db_a = open_db(home.path(), "/repo/a", 0).await.unwrap();
        let db_b = open_db(home.path(), "/repo/b", 0).await.unwrap();
        let a_file = "/repo/a/a.rs";
        let b_file = "/repo/b/b.rs";
        let a_fqn = "/repo/a/a.rs::a";
        let b_fqn = "/repo/b/b.rs::b";
        insert_symbol(&db_a, a_fqn, a_file, "a", 1, 5).await;
        insert_symbol(&db_b, b_fqn, b_file, "b", 1, 5).await;
        insert_chunk(&db_b, b_file, 1, 5, "fn b() {}").await;
        insert_call(&db_a, a_fqn, a_file, b_fqn, b_file, None).await;

        let base = MergeChunk {
            file: a_file.into(),
            line_start: 1,
            line_end: 5,
            score: 1.0,
            content: "fn a() {}".into(),
            symbol: None,
            symbol_fqn: None,
            symbol_kind: None,
        };
        let db_map = HashMap::from([("/repo/a".to_string(), db_a), ("/repo/b".to_string(), db_b)]);
        let expanded = graph_expand(&[base], &db_map, 2).await;
        assert!(
            expanded
                .iter()
                .any(|c| c.file == b_file && c.symbol_fqn.as_deref() == Some(b_fqn))
        );
    }

    async fn duplicate_confidence_result(order: &[Option<f32>]) -> Vec<(String, f32)> {
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/repo/weighted", 0).await.unwrap();
        let seed_file = "/repo/weighted/seed.rs";
        let target_file = "/repo/weighted/target.rs";
        let low_file = "/repo/weighted/low.rs";
        let seed_fqn = "/repo/weighted/seed.rs::seed";
        let target_fqn = "/repo/weighted/target.rs::target";
        let low_fqn = "/repo/weighted/low.rs::low";
        insert_symbol(&db, seed_fqn, seed_file, "seed", 1, 5).await;
        insert_symbol(&db, target_fqn, target_file, "target", 1, 5).await;
        insert_symbol(&db, low_fqn, low_file, "low", 1, 5).await;
        insert_chunk(&db, target_file, 1, 5, "fn target() {}").await;
        insert_chunk(&db, low_file, 1, 5, "fn low() {}").await;
        for confidence in order {
            insert_call(
                &db,
                seed_fqn,
                seed_file,
                target_fqn,
                target_file,
                *confidence,
            )
            .await;
        }
        insert_call(&db, seed_fqn, seed_file, low_fqn, low_file, Some(0.2)).await;
        let base = MergeChunk {
            file: seed_file.into(),
            line_start: 1,
            line_end: 5,
            score: 1.0,
            content: "fn seed() {}".into(),
            symbol: None,
            symbol_fqn: None,
            symbol_kind: None,
        };
        let db_map = HashMap::from([("/repo/weighted".to_string(), db)]);
        let mut result: Vec<_> = graph_expand(&[base], &db_map, 2)
            .await
            .into_iter()
            .map(|c| (c.file, c.score))
            .collect();
        result.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        result
    }

    async fn duplicate_endpoint_best_path_result(seed_order: &[(&str, f32)]) -> ExpandedChunk {
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/repo/best_path", 0).await.unwrap();
        let a_file = "/repo/best_path/a.rs";
        let b_file = "/repo/best_path/b.rs";
        let target_file = "/repo/best_path/target.rs";
        let a_fqn = "/repo/best_path/a.rs::a";
        let b_fqn = "/repo/best_path/b.rs::b";
        let target_fqn = "/repo/best_path/target.rs::target";
        insert_symbol(&db, a_fqn, a_file, "a", 1, 5).await;
        insert_symbol(&db, b_fqn, b_file, "b", 1, 5).await;
        insert_symbol(&db, target_fqn, target_file, "target", 1, 5).await;
        insert_chunk(&db, target_file, 1, 5, "fn target() {}").await;
        insert_call(&db, a_fqn, a_file, target_fqn, target_file, None).await;
        insert_call(&db, b_fqn, b_file, target_fqn, target_file, None).await;

        let bases: Vec<MergeChunk> = seed_order
            .iter()
            .map(|(file, score)| MergeChunk {
                file: (*file).to_string(),
                line_start: 1,
                line_end: 5,
                score: *score,
                content: format!("fn {}() {{}}", if *file == a_file { "a" } else { "b" }),
                symbol: None,
                symbol_fqn: None,
                symbol_kind: None,
            })
            .collect();
        let db_map = HashMap::from([("/repo/best_path".to_string(), db)]);
        let mut targets: Vec<_> = graph_expand(&bases, &db_map, 2)
            .await
            .into_iter()
            .filter(|chunk| chunk.symbol_fqn.as_deref() == Some(target_fqn))
            .collect();
        assert_eq!(targets.len(), 1, "duplicate endpoint must be returned once");
        targets.pop().unwrap()
    }

    #[tokio::test]
    async fn duplicate_endpoint_keeps_highest_score_path_independent_of_seed_order() {
        // A(.5) -> T yields .25; B(.4) -> T yields .20. The old LIFO +
        // first-seen traversal let B claim T first when A preceded B in the seed
        // list and permanently discarded A's stronger path.
        let a_file = "/repo/best_path/a.rs";
        let b_file = "/repo/best_path/b.rs";
        for order in [
            vec![(a_file, 0.5), (b_file, 0.4)],
            vec![(b_file, 0.4), (a_file, 0.5)],
        ] {
            let target = duplicate_endpoint_best_path_result(&order).await;
            assert!(
                (target.score - 0.25).abs() < f32::EPSILON,
                "score={}",
                target.score
            );
            assert_eq!(
                target.symbol_fqn.as_deref(),
                Some("/repo/best_path/target.rs::target")
            );
        }
    }

    #[tokio::test]
    async fn duplicate_edges_use_best_confidence_independent_of_insertion_order() {
        let inferred_first = duplicate_confidence_result(&[Some(0.2), None]).await;
        let extracted_first = duplicate_confidence_result(&[None, Some(0.2)]).await;
        assert_eq!(inferred_first, extracted_first);
        assert_eq!(
            inferred_first,
            vec![("/repo/weighted/target.rs".to_string(), 0.5)]
        );
    }
}
