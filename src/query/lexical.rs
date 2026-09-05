//! Lexical fallback + Reciprocal Rank Fusion — plan item 1.1.
//!
//! The query pipeline was vector-only: when the embedding missed (exact symbol
//! names, rare terms) the query returned nothing even though the `chunk` rows
//! existed. This module adds a bounded SurrealDB token scan over the already
//! indexed `chunk` rows (no new index, no schema migration, no DB/sidecar
//! writes) and fuses it with the vector candidates via RRF.
//!
//! Fusion happens BEFORE graph expansion so lexical hits seed callers/callees
//! too. RRF decides ORDER only; returned scores keep source meaning (vector
//! cosine, else lexical term-coverage), on the [0,1] scale the expansion
//! factors (caller ×0.6, callee ×0.5, floor 0.15) expect.
//! Gate: `CONTEXT_ENGINE_LEXICAL=1` (also `true`/`on`/`yes`/`enabled`) enables
//! the scan; default is disabled until the latency gate in `scripts/ab_bench.sh`
//! proves neutral per the active plan's recovery rule.

use std::collections::{HashMap, HashSet};

use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tracing::warn;

use crate::path_in_repo;
use crate::query::merger::MergeChunk;

/// RRF persistence constant — the literature default (Cormack et al., 2009).
/// Higher `k` compresses rank gaps; 60 keeps deep-list contributions alive
/// without letting rank-50 noise rival rank-1 hits.
pub const RRF_K: f64 = 60.0;
/// Upper bound on query terms per lexical scan (most-specific first).
pub const MAX_LEXICAL_TERMS: usize = 8;

/// Per-DB row cap for the lexical scan: enough headroom for RRF to matter,
/// small enough to keep the scan a bounded local read.
pub fn lexical_limit(top_k: usize) -> usize {
    (top_k * 2).max(20).min(100)
}

/// True only when the operator explicitly opted in via `CONTEXT_ENGINE_LEXICAL`.
/// Default-off until the latency benchmark passes.
pub fn lexical_enabled() -> bool {
    std::env::var("CONTEXT_ENGINE_LEXICAL")
        .ok()
        .is_some_and(|v| lexical_flag_on(&v))
}

/// Pure opt-in parser behind [`lexical_enabled`]: unit-testable without
/// mutating process env (Rust runs tests in parallel; env writes race).
pub fn lexical_flag_on(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes" | "enabled"
    )
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "for", "with", "from", "that", "this", "these", "those", "are",
    "is", "was", "were", "been", "have", "has", "had", "does", "show", "find", "all", "any", "how",
    "what", "where", "which", "when", "why", "who", "in", "of", "to", "on", "at", "by", "as",
    "into",
];

/// Split a free-form query into lowercase search terms.
///
/// Splits snake_case and camelCase (code identifiers), drops stopwords and
/// terms shorter than 2 chars, dedups, and keeps the 8 most-specific (longest)
/// terms so one scan stays bounded.
pub fn tokenize_query(query: &str) -> Vec<String> {
    let mut spaced = String::with_capacity(query.len() + 8);
    let mut prev: Option<char> = None;
    for c in query.chars() {
        if let Some(p) = prev
            && (p.is_lowercase() || p.is_ascii_digit())
            && c.is_uppercase()
        {
            spaced.push(' ');
        }
        spaced.push(c);
        prev = Some(c);
    }
    let mut seen = HashSet::new();
    let mut terms: Vec<String> = spaced
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|w| {
            let t = w.to_lowercase();
            if t.chars().count() < 2 || STOPWORDS.contains(&t.as_str()) {
                return None;
            }
            if !seen.insert(t.clone()) {
                return None;
            }
            Some(t)
        })
        .collect();
    terms.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    terms.truncate(MAX_LEXICAL_TERMS);
    terms
}

#[derive(serde::Deserialize)]
struct LexicalRow {
    file: String,
    line_start: i64,
    line_end: i64,
    content: String,
    symbol_ref: Option<String>,
}

fn parse_symbol_ref(symbol_ref: Option<&str>) -> (Option<String>, Option<String>) {
    match symbol_ref {
        Some(s) => {
            let full = s
                .strip_prefix("symbol:⟨")
                .and_then(|x| x.strip_suffix("⟩"))
                .map(|f| f.to_string());
            let short = full
                .as_deref()
                .map(|fqn| fqn.rsplit("::").next().unwrap_or(fqn).to_string());
            (short, full)
        }
        None => (None, None),
    }
}

/// Bounded per-term scan over the indexed `chunk` rows for the in-scope DBs.
///
/// One query per term (content OR file path), each capped at
/// [`lexical_limit`]: per-term pools make a multi-term hit a candidate
/// whenever ANY of its terms returns it within that term's cap — far more
/// robust than a single global OR + LIMIT (arbitrary first-N rows), but NOT a
/// guarantee: a row beyond the cap for EVERY term stays invisible. Pools merge
/// by distinct-term coverage (ties: shorter content first), each scored with
/// coverage in (0,1]. Returns `[]` for an empty/unusable query — the caller
/// treats that as "no lexical signal", never as an error.
pub async fn lexical_candidates(
    db_map: &HashMap<String, Surreal<Db>>,
    clean_query: &str,
    repo_filter: Option<&str>,
    top_k: usize,
) -> Vec<MergeChunk> {
    let terms = tokenize_query(clean_query);
    if terms.is_empty() || db_map.is_empty() {
        return vec![];
    }
    let limit = lexical_limit(top_k);

    // Scope to the queried repo's DB; fall back to every DB when no key
    // matches (row-level `path_in_repo` filtering still applies below).
    let dbs: Vec<&Surreal<Db>> = if let Some(f) = repo_filter {
        let matched: Vec<&Surreal<Db>> = db_map
            .iter()
            .filter(|(k, _)| *k == f || path_in_repo(f, k))
            .map(|(_, db)| db)
            .collect();
        if matched.is_empty() {
            db_map.values().collect()
        } else {
            matched
        }
    } else {
        db_map.values().collect()
    };

    // One query per term over content + file path — the same case-insensitive
    // CONTAINS form `store::ops` already uses against `file_meta`. Per-term
    // pools (not one global OR): each term's matches are candidates regardless
    // of how many weak rows other terms produce.
    let sql = "SELECT file, line_start, line_end, content, symbol_ref FROM chunk \
         WHERE string::lowercase(content) CONTAINS string::lowercase($t) \
         OR string::lowercase(file) CONTAINS string::lowercase($t) LIMIT $limit";

    let mut rows: Vec<LexicalRow> = Vec::new();
    for db in dbs {
        for t in &terms {
            match db
                .query(sql)
                .bind(("t", t.clone()))
                .bind(("limit", limit as i64))
                .await
            {
                Ok(mut r) => {
                    let batch: Vec<LexicalRow> = r.take(0).unwrap_or_default();
                    rows.extend(batch);
                }
                Err(e) => {
                    warn!(error = %e, "lexical scan failed; continuing with vector results")
                }
            }
        }
    }

    // Score by distinct-term coverage; dedup by chunk key.
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    let mut scored: Vec<(usize, MergeChunk)> = Vec::new();
    for r in rows {
        if r.line_start < 0 || r.line_end < 0 {
            continue;
        }
        let (ls, le) = (r.line_start as u32, r.line_end as u32);
        if let Some(f) = repo_filter
            && !path_in_repo(&r.file, f)
        {
            continue;
        }
        if !seen.insert((r.file.clone(), ls, le)) {
            continue;
        }
        let hay = format!(
            "{}\n{}\n{}",
            r.file.to_lowercase(),
            r.content.to_lowercase(),
            r.symbol_ref.as_deref().unwrap_or_default().to_lowercase()
        );
        let matched = terms.iter().filter(|t| hay.contains(t.as_str())).count();
        if matched == 0 {
            continue;
        }
        let (symbol, fqn) = parse_symbol_ref(r.symbol_ref.as_deref());
        // symbol_kind left unset: resolving it per hit would be an N+1 DB
        // round-trip; kind filters see the fused set, where vector-side rows
        // already carry kind.
        scored.push((
            matched,
            MergeChunk {
                file: r.file,
                line_start: ls,
                line_end: le,
                score: matched as f32 / terms.len() as f32,
                content: r.content,
                symbol,
                symbol_fqn: fqn,
                symbol_kind: None,
            },
        ));
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.content.len().cmp(&b.1.content.len()))
            .then(a.1.file.cmp(&b.1.file))
            .then(a.1.line_start.cmp(&b.1.line_start))
    });
    let chunks: Vec<MergeChunk> = scored.into_iter().map(|(_, c)| c).take(limit).collect();
    // Rows were just read, but fence anyway: a file-path-only match can carry
    // an empty body, and downstream assumes resolved content.
    crate::query::content_fence::apply(chunks).kept
}

/// Fuse vector and lexical candidates with Reciprocal Rank Fusion.
///
/// RRF decides ORDER only: rank orders come from input order (vector hydrate
/// order = vector ranking; lexical coverage order). A chunk present in both
/// lists sums both contributions, so agreement outranks single-list hits. The
/// returned `score` keeps source meaning — vector cosine where the vector list
/// has the chunk, else lexical term-coverage — so `CodeResult.score` stays a
/// confidence the UI can render as a percentage (per 0003). On overlap the
/// vector chunk's score/metadata wins (primary path unchanged); order is
/// deterministic (RRF desc, vector rank, lexical rank, file).
pub fn rrf_fuse(vector: &[MergeChunk], lexical: &[MergeChunk]) -> Vec<MergeChunk> {
    type Key = (String, u32, u32);
    fn key(c: &MergeChunk) -> Key {
        (c.file.clone(), c.line_start, c.line_end)
    }
    if vector.is_empty() && lexical.is_empty() {
        return vec![];
    }
    let v_rank: HashMap<Key, usize> = vector
        .iter()
        .enumerate()
        .map(|(i, c)| (key(c), i + 1))
        .collect();
    let l_rank: HashMap<Key, usize> = lexical
        .iter()
        .enumerate()
        .map(|(i, c)| (key(c), i + 1))
        .collect();

    // Prefer vector metadata on overlap; collect each key once.
    let mut base: HashMap<Key, MergeChunk> = HashMap::new();
    for c in vector {
        base.entry(key(c)).or_insert_with(|| c.clone());
    }
    for c in lexical {
        base.entry(key(c)).or_insert_with(|| c.clone());
    }

    let mut ranked: Vec<(f64, usize, usize, Key)> = Vec::with_capacity(base.len());
    for k in base.keys() {
        let vr = v_rank.get(k).copied();
        let lr = l_rank.get(k).copied();
        let mut raw = 0.0;
        if let Some(r) = vr {
            raw += 1.0 / (RRF_K + r as f64);
        }
        if let Some(r) = lr {
            raw += 1.0 / (RRF_K + r as f64);
        }
        ranked.push((
            raw,
            vr.unwrap_or(usize::MAX),
            lr.unwrap_or(usize::MAX),
            k.clone(),
        ));
    }
    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });

    ranked
        .into_iter()
        .filter_map(|(_, _, _, k)| base.remove(&k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::open_db;
    use tempfile::TempDir;

    fn chunk_with(file: &str, ls: u32, le: u32, score: f32) -> MergeChunk {
        MergeChunk {
            file: file.to_string(),
            line_start: ls,
            line_end: le,
            score,
            content: String::new(),
            symbol: None,
            symbol_fqn: None,
            symbol_kind: None,
        }
    }

    fn chunk(file: &str, ls: u32, le: u32) -> MergeChunk {
        chunk_with(file, ls, le, 0.9)
    }

    #[test]
    fn tokenize_splits_identifiers_and_drops_stopwords() {
        let terms = tokenize_query("where is fetch_chunk_content defined?");
        assert!(terms.contains(&"fetch".to_string()), "got {terms:?}");
        assert!(terms.contains(&"chunk".to_string()), "got {terms:?}");
        assert!(terms.contains(&"content".to_string()), "got {terms:?}");
        assert!(!terms.contains(&"where".to_string()), "got {terms:?}");
        assert!(!terms.contains(&"is".to_string()), "got {terms:?}");
    }

    #[test]
    fn tokenize_splits_camel_case_and_caps_terms() {
        let terms = tokenize_query("runQuery fetch");
        assert!(terms.contains(&"run".to_string()), "got {terms:?}");
        assert!(terms.contains(&"query".to_string()), "got {terms:?}");
        assert!(terms.contains(&"fetch".to_string()), "got {terms:?}");
        // Cap still holds on long inputs.
        let many = tokenize_query("alpha beta gamma delta epsilon zeta eta theta iota kappa");
        assert!(many.len() <= MAX_LEXICAL_TERMS, "got {many:?}");
    }
    #[test]
    fn lexical_flag_parses_opt_in_without_env() {
        for on in ["1", "true", "on", "yes", "enabled", " TRUE "] {
            assert!(lexical_flag_on(on), "{on:?} must opt in");
        }
        for off in ["", "0", "false", "off", "no", "disabled", "2", "maybe"] {
            assert!(!lexical_flag_on(off), "{off:?} must stay off");
        }
    }
    #[test]
    fn tokenize_empty_or_stopwords_only_yields_nothing() {
        assert!(tokenize_query("").is_empty());
        assert!(tokenize_query("what is the").is_empty());
    }

    #[test]
    fn rrf_orders_by_agreement_but_keeps_source_scores() {
        let mut v = vec![
            chunk_with("/a.rs", 1, 5, 0.8),
            chunk_with("/b.rs", 1, 5, 0.7),
        ];
        v[1].symbol = Some("shared".to_string());
        let mut l = vec![
            chunk_with("/b.rs", 1, 5, 0.5),
            chunk_with("/c.rs", 1, 5, 0.4),
        ];
        l[0].symbol = Some("lexical-name".to_string());
        let fused = rrf_fuse(&v, &l);
        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].file, "/b.rs", "in both lists must win");
        assert!(
            (fused[0].score - 0.7).abs() < f32::EPSILON,
            "vector cosine survives overlap, got {}",
            fused[0].score
        );
        assert_eq!(
            fused[0].symbol.as_deref(),
            Some("shared"),
            "vector metadata wins on overlap"
        );
        let c = fused.iter().find(|c| c.file == "/c.rs").unwrap();
        assert!(
            (c.score - 0.4).abs() < f32::EPSILON,
            "lexical-only keeps coverage score, got {}",
            c.score
        );
    }

    #[test]
    fn rrf_single_list_preserves_order_and_empty_is_safe() {
        assert!(rrf_fuse(&[], &[]).is_empty());
        let v = vec![chunk("/a.rs", 1, 5), chunk("/b.rs", 1, 5)];
        let fused = rrf_fuse(&v, &[]);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].file, "/a.rs");
        assert!(
            (fused[0].score - 0.9).abs() < f32::EPSILON,
            "source score untouched, got {}",
            fused[0].score
        );
        // Fallback path: lexical-only input keeps its order too.
        let l = vec![chunk("/c.rs", 1, 5)];
        let fused = rrf_fuse(&[], &l);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].file, "/c.rs");
    }

    async fn insert_chunk(db: &Surreal<Db>, file: &str, ls: i64, le: i64, content: &str) {
        db.query("CREATE chunk SET file = $f, line_start = $ls, line_end = $le, content = $c")
            .bind(("f", file.to_string()))
            .bind(("ls", ls))
            .bind(("le", le))
            .bind(("c", content.to_string()))
            .await
            .expect("insert chunk");
    }

    #[tokio::test]
    async fn lexical_finds_chunk_by_content_term() {
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/test/lexical", 0).await.unwrap();
        insert_chunk(
            &db,
            "/test/lexical/src/db.rs",
            1,
            10,
            "pub fn open_pool() {}",
        )
        .await;
        insert_chunk(&db, "/test/lexical/src/main.rs", 1, 10, "fn main() {}").await;
        let map: HashMap<String, Surreal<Db>> = HashMap::from([("/test/lexical".to_string(), db)]);
        let got = lexical_candidates(&map, "open_pool", Some("/test/lexical"), 5).await;
        assert!(!got.is_empty(), "content term must match");
        assert_eq!(got[0].file, "/test/lexical/src/db.rs");
    }

    #[tokio::test]
    async fn lexical_exact_hit_survives_early_weak_matches() {
        // More weak single-term rows than `lexical_limit(5)` (=20) precede the
        // exact multi-term hit: per-term pools must still surface it. A single
        // global OR + LIMIT would take arbitrary first-N rows and miss it.
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/test/lexical_pools", 0)
            .await
            .unwrap();
        for i in 0..30 {
            insert_chunk(
                &db,
                &format!("/test/lexical_pools/src/noise{i:02}.rs"),
                1,
                5,
                "fn unrelated_alpha_helper() {}",
            )
            .await;
        }
        insert_chunk(
            &db,
            "/test/lexical_pools/src/exact.rs",
            1,
            5,
            "fn unrelated_alpha_zeta_exact_target() {}",
        )
        .await;
        let map: HashMap<String, Surreal<Db>> =
            HashMap::from([("/test/lexical_pools".to_string(), db)]);
        let got = lexical_candidates(
            &map,
            "unrelated_alpha zeta_exact_target",
            Some("/test/lexical_pools"),
            5,
        )
        .await;
        let exact = got
            .iter()
            .find(|c| c.file.ends_with("exact.rs"))
            .expect("exact multi-term hit must be a candidate despite 30 early weak rows");
        assert_eq!(
            got[0].file, exact.file,
            "highest coverage (both terms) must rank first"
        );
    }

    #[tokio::test]
    async fn lexical_documents_beyond_cap_blind_spot() {
        // Honest boundary of per-term pools: a row beyond the cap for EVERY
        // term stays invisible. 2×limit noise rows all precede the exact hit
        // for the shared term, and the exact hit's rare term never appears
        // elsewhere — the rare-term pool still surfaces it via its own cap.
        // If this fails, the scan needs indexed/orderable scoring.
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/test/lexical_cap", 0).await.unwrap();
        let limit = lexical_limit(5);
        for i in 0..(limit * 2) {
            insert_chunk(
                &db,
                &format!("/test/lexical_cap/src/noise{i:03}.rs"),
                1,
                5,
                "fn captest_common_helper() {}",
            )
            .await;
        }
        insert_chunk(
            &db,
            "/test/lexical_cap/src/exact.rs",
            1,
            5,
            "fn captest_common_captest_rarefind_xyz() {}",
        )
        .await;
        let map: HashMap<String, Surreal<Db>> =
            HashMap::from([("/test/lexical_cap".to_string(), db)]);
        let got = lexical_candidates(
            &map,
            "captest_common captest_rarefind_xyz",
            Some("/test/lexical_cap"),
            5,
        )
        .await;
        assert!(
            got.iter().any(|c| c.file.ends_with("exact.rs")),
            "rare-term pool must surface the exact hit despite 2×limit noise on the common term"
        );
    }
    #[tokio::test]
    async fn lexical_respects_repo_filter_and_empty_query() {
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/test/lexical_filter", 0)
            .await
            .unwrap();
        insert_chunk(&db, "/other/repo/a.rs", 1, 5, "fn unique_fn_xyz() {}").await;
        let map: HashMap<String, Surreal<Db>> =
            HashMap::from([("/test/lexical_filter".to_string(), db)]);
        let got = lexical_candidates(&map, "unique_fn_xyz", Some("/test/lexical_filter"), 5).await;
        assert!(got.is_empty(), "rows outside the repo filter must not leak");
        let empty = lexical_candidates(&map, "what is the", Some("/test/lexical_filter"), 5).await;
        assert!(empty.is_empty());
    }
}
