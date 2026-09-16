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
//! cosine, else the normalized lexical score below), on the [0,1] scale the
//! expansion factors (caller ×0.6, callee ×0.5, floor 0.15) expect.
//!
//! Normalization pass (2026-09-16): raw term coverage saturates at 1.0, which
//! let prose/test rows mentioning every query term outrank the identifier
//! definition the query names (the measured 2026-09-13 gate failure). Lexical
//! rows are now scored by rarity-weighted coverage (IDF derived from the
//! already-scanned pools — no extra DB round-trip) plus an identifier-run
//! bonus; see [`score_rows`].
//! Gate: ON by default since the 2026-09-16 normalization gate (recall/IoU
//! byte-equal to the vector-only baseline at r@1/r@5/r@10 on the 40-pair A/B,
//! rerank stage unaffected); `CONTEXT_ENGINE_LEXICAL=0` (also `false`/`off`/
//! `no`/`disabled`) is the operator kill switch for latency-sensitive hosts.

use std::cmp::Ordering;
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

/// Ceiling of the normalized lexical score (`score_rows` output). Witness
/// rows (the queried identifier's definition) top out here; the expansion
/// factors (caller ×0.6, callee ×0.5, floor 0.15) stay well inside [0,1].
pub const LEXICAL_MAX_SCORE: f64 = 0.8;

/// Scale for usage-only (mention) evidence. Lexical rows feed the same
/// merge sort as vector rows, whose cosine band on real corpora is roughly
/// 0.5–0.9; a mention must stay BELOW that band so the fallback can never
/// displace the vector list's top definitions (the 2026-09-13 failure was
/// score saturation at 1.0 doing exactly that). 0.55 sits under typical
/// strong matches while keeping sane ordering inside the lexical list.
pub const USAGE_SCALE: f64 = 0.55;

/// Additive bonus when the row WITNESSES the queried identifier: its symbol
/// short name, or a whole body token, equals ALL query terms concatenated in
/// order (separators stripped). A definition witness may rise to
/// [`LEXICAL_MAX_SCORE`]; a mention never can.
pub const WITNESS_BONUS: f64 = 0.25;

/// RRF contribution weight for a lexical rank. Strictly below the vector
/// weight (1.0) so a lexical-only row cannot tie a vector row at the same
/// rank in the fused seed order; agreement rows still sum both signals.
pub const LEXICAL_RRF_WEIGHT: f32 = 0.7;

/// Lower field weight for path-only matches: a file-path hit without body or
/// symbol evidence is a topic hint, not a definition witness.
const PATH_FIELD_WEIGHT: f64 = 0.5;

/// Per-field-match run-length floor for the identifier bonus: a token must
/// contain at least 2 query terms (a compound identifier like
/// `BoundedSessionStore` ⇒ `bounded` + `session` + `store`) to count as the
/// identifier the query names. Single-word matches stay coverage-only.
const MIN_IDENTIFIER_RUN: usize = 2;

/// A lowercased row projected into the fields lexical scoring reads.
/// `path` = file path, `body` = chunk content, `tail` = full symbol ref,
/// `name` = the symbol's SHORT name (after the last `::`, empty when the row
/// has no symbol). The name field exists for the definition stratification in
/// [`score_rows`]: a row whose SYMBOL NAME is the queried identifier is a
/// definition witness, categorically stronger than a body mention.
struct LexWindow {
    path: String,
    body: String,
    tail: String,
    name: String,
}
/// Upper bound on query terms per lexical scan (most-specific first).
pub const MAX_LEXICAL_TERMS: usize = 8;

/// Per-DB row cap for the lexical scan: enough headroom for RRF to matter,
/// small enough to keep the scan a bounded local read.
pub fn lexical_limit(top_k: usize) -> usize {
    (top_k * 2).max(20).min(100)
}

/// On unless the operator explicitly opted OUT via `CONTEXT_ENGINE_LEXICAL`
/// (kill switch: `0`/`false`/`off`/`no`/`disabled`). Default-on after the
/// 2026-09-16 gate proved the normalized fusion recall-neutral.
pub fn lexical_enabled() -> bool {
    std::env::var("CONTEXT_ENGINE_LEXICAL")
        .ok()
        .is_none_or(|v| !lexical_flag_off(&v))
}

/// Pure kill-switch parser behind [`lexical_enabled`]: unit-testable without
/// mutating process env (Rust runs tests in parallel; env writes race). Any
/// value other than an explicit off token (including garbage) keeps fusion on.
pub fn lexical_flag_off(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no" | "disabled"
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

#[derive(Clone, serde::Deserialize)]
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

    // One pool per term; pool sizes double as document frequencies for the
    // rarity weighting in `score_rows` — no extra DB round-trips.
    let mut pools: HashMap<String, Vec<LexicalRow>> = HashMap::new();
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
                    pools.entry(t.clone()).or_default().extend(batch);
                }
                Err(e) => {
                    warn!(error = %e, "lexical scan failed; continuing with vector results")
                }
            }
        }
    }
    let rows: Vec<LexicalRow> = pools.values().flatten().cloned().collect();

    // Dedup by chunk key; score each row with the NORMALIZED lexical score
    // (rarity-weighted coverage + identifier-run bonus) — see [`score_rows`].
    let df: HashMap<&str, usize> = pools
        .iter()
        .map(|(t, rows)| (t.as_str(), rows.len()))
        .collect();
    let max_df = df.values().copied().max().unwrap_or(1).max(1);
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    let mut scored: Vec<MergeChunk> = Vec::new();
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
        let (symbol, fqn) = parse_symbol_ref(r.symbol_ref.as_deref());
        let window = LexWindow {
            path: r.file.to_lowercase(),
            body: r.content.to_lowercase(),
            tail: r.symbol_ref.as_deref().unwrap_or_default().to_lowercase(),
            name: symbol.as_deref().unwrap_or_default().to_lowercase(),
        };
        let hay = format!("{}\n{}\n{}", window.path, window.body, window.tail);
        if !terms.iter().any(|t| hay.contains(t.as_str())) {
            continue;
        }
        // symbol_kind left unset: resolving it per hit would be an N+1 DB
        // round-trip; kind filters see the fused set, where vector-side rows
        // already carry kind.
        scored.push(MergeChunk {
            file: r.file,
            line_start: ls,
            line_end: le,
            score: score_rows(&window, &terms, &df, max_df) as f32,
            content: r.content,
            symbol,
            symbol_fqn: fqn,
            symbol_kind: None,
        });
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(a.content.len().cmp(&b.content.len()))
            .then(a.file.cmp(&b.file))
            .then(a.line_start.cmp(&b.line_start))
    });
    let chunks: Vec<MergeChunk> = scored.into_iter().take(limit).collect();
    // Rows were just read, but fence anyway: a file-path-only match can carry
    // an empty body, and downstream assumes resolved content.
    crate::query::content_fence::apply(chunks).kept
}

/// Normalized lexical relevance for one row, in [0, [`LEXICAL_MAX_SCORE`]].
///
/// Two stratified bands:
///
/// * usage band ≤ [`USAGE_SCALE`]: `USAGE_SCALE · min(coverage + run, 1)`.
///   coverage = Σ matched term weights / term count, where a term's weight
///   FALLS with its pool size (IDF direction: a hapax term is strong
///   evidence, a ubiquitous term is weak) — `w = 1 − 0.5·ln(1+df)/ln(1+max_df)`.
///   The identifier-run bonus = `2·(run/term_count)²` when ONE identifier
///   token (alphanumerics + `_`, so snake_case stays whole) of body+tail
///   contains ≥ 2 query terms.
/// * witness band ≤ [`LEXICAL_MAX_SCORE`]: adds [`WITNESS_BONUS`] when the
///   row WITNESSES the identifier — its symbol short name, or a whole
///   body/tail token, equals the query terms CONCATENATED IN ORDER with
///   separators stripped (`bounded`+`session`+`store` ==
///   `bounded_session_store`), and the query has ≥ 2 terms. Exact equality
///   is what separates the definition chunk from `test_<ident>` helpers and
///   import mentions, and the +0.25 tier is what keeps a mention (capped at
///   [`USAGE_SCALE`], below the vector cosine band) from ever displacing
///   the vector list's top definitions in the merge sort — the measured
///   2026-09-13 gate failure.
///
/// Matching reads `window`'s four fields; run detection scans body+tail only.
fn score_rows(
    window: &LexWindow,
    terms: &[String],
    df: &HashMap<&str, usize>,
    max_df: usize,
) -> f64 {
    let log_norm = (1.0 + max_df as f64).ln().max(1.0);
    let mut weight_sum = 0.0f64;
    for t in terms {
        let in_path = window.path.contains(t.as_str());
        let in_body = window.body.contains(t.as_str());
        let in_tail = window.tail.contains(t.as_str());
        if !(in_path || in_body || in_tail) {
            continue;
        }
        let df_t = df.get(t.as_str()).copied().unwrap_or(1).max(1) as f64;
        let w = 1.0 - 0.5 * (1.0 + df_t).ln() / log_norm;
        weight_sum += if in_body || in_tail {
            w
        } else {
            w * PATH_FIELD_WEIGHT
        };
    }
    let coverage = weight_sum / terms.len().max(1) as f64;

    // Longest identifier token (body+tail) containing the most distinct query
    // terms — a compound-identifier witness. `_` STAYS inside the token
    // (snake_case identifiers like `bounded_session_store` would otherwise
    // split into single-term fragments and lose the run).
    let mut run = 0usize;
    for token in window
        .body
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
    {
        let n = terms.iter().filter(|t| token.contains(t.as_str())).count();
        if n > run {
            run = n;
        }
    }
    for token in window
        .tail
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
    {
        let n = terms.iter().filter(|t| token.contains(t.as_str())).count();
        if n > run {
            run = n;
        }
    }
    let bonus = if run >= MIN_IDENTIFIER_RUN {
        2.0 * (run as f64 / terms.len().max(1) as f64).powi(2)
    } else {
        0.0
    };
    let usage = (coverage + bonus).min(1.0);
    let witness = if terms.len() >= 2 {
        let target: String = terms.iter().map(|t| t.as_str()).collect();
        // Per-token equality: a WHOLE identifier token (separators stripped)
        // must equal the concatenated terms. A prose phrase splits into
        // separate tokens, so "the bounded session store keeps rows" is NOT a
        // witness, while `bounded_session_store` and `BoundedSessionStore` are.
        let token_is_witness = |s: &str| {
            s.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|tok| {
                    let stripped: String = tok.chars().filter(|c| c.is_alphanumeric()).collect();
                    !stripped.is_empty() && stripped == target
                })
        };
        token_is_witness(&window.body)
            || token_is_witness(&window.tail)
            || (!window.name.is_empty() && {
                let stripped: String = window
                    .name
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                !stripped.is_empty() && stripped == target
            })
    } else {
        false
    };
    USAGE_SCALE * usage + if witness { WITNESS_BONUS } else { 0.0 }
}

/// Fuse vector and lexical candidates with Reciprocal Rank Fusion.
///
/// RRF decides ORDER only: rank orders come from input order (vector hydrate
/// order = vector ranking; lexical normalized-score order). A chunk present in
/// both lists sums both contributions, so agreement outranks single-list hits.
/// Contributions are weight-split — a vector rank counts 1.0 while a lexical
/// rank counts [`LEXICAL_RRF_WEIGHT`] of it — because the lexical list is a
/// fallback signal, not a co-equal ranker. The returned `score`
/// keeps source meaning — vector cosine where the vector list has the chunk,
/// else the normalized lexical score — so `CodeResult.score` stays a
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

    let mut ranked: Vec<(f32, usize, usize, Key)> = Vec::with_capacity(base.len());
    for k in base.keys() {
        let vr = v_rank.get(k).copied();
        let lr = l_rank.get(k).copied();
        let mut raw = 0.0f32;
        if let Some(r) = vr {
            raw += 1.0f32 / (RRF_K as f32 + r as f32);
        }
        if let Some(r) = lr {
            raw += LEXICAL_RRF_WEIGHT / (RRF_K as f32 + r as f32);
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
            .unwrap_or(Ordering::Equal)
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
    fn lexical_flag_is_a_kill_switch_default_on() {
        // Default (unset env) is ON — handled in lexical_enabled via is_none_or.
        for off in ["0", "false", "off", "no", "disabled", " OFF "] {
            assert!(lexical_flag_off(off), "{off:?} must opt out");
        }
        // On tokens stay on; garbage also stays on (kill switch, not opt-in).
        for on in ["1", "true", "on", "yes", "enabled", "", "2", "maybe"] {
            assert!(!lexical_flag_off(on), "{on:?} must stay on");
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

    #[test]
    fn rrf_weighted_overlap_beats_two_single_list_hits() {
        // One chunk in both lists must outrank any chunk in exactly one list,
        // even with the lexical contribution weight-split to LEXICAL_MAX_SCORE.
        let v = vec![
            chunk_with("/a.rs", 1, 5, 0.9),
            chunk_with("/b.rs", 1, 5, 0.8),
        ];
        let l = vec![chunk_with("/b.rs", 1, 5, 0.5)];
        let fused = rrf_fuse(&v, &l);
        assert_eq!(fused[0].file, "/b.rs", "overlap must still win");
    }
    fn win(path: &str, body: &str, tail: &str) -> LexWindow {
        // Production paths build the window from lowercased content (see
        // lexical_candidates); mirror that invariant here.
        LexWindow {
            path: path.to_lowercase(),
            body: body.to_lowercase(),
            tail: tail.to_lowercase(),
            name: tail.rsplit("::").next().unwrap_or(tail).to_lowercase(),
        }
    }

    fn uniform_df(terms: &[String], n: usize) -> (HashMap<&str, usize>, usize) {
        let df = terms.iter().map(|t| (t.as_str(), n)).collect();
        (df, n)
    }
    #[test]
    fn identifier_run_outranks_prose_at_equal_coverage() {
        // The 2026-09-13 gate failure, distilled: prose mentioning every term
        // must NOT outrank the identifier definition the query names, and the
        // definition (whole identifier token) must reach the witness ceiling.
        let terms = vec![
            "bounded".to_string(),
            "session".to_string(),
            "store".to_string(),
        ];
        let (df, max_df) = uniform_df(&terms, 10);
        let id = score_rows(
            &win(
                "/a.rs",
                "pub struct bounded_session_store { target: u32 }",
                "symbol:⟨/a.rs::bounded_session_store⟩",
            ),
            &terms,
            &df,
            max_df,
        );
        let prose = score_rows(
            &win("/b.rs", "the bounded session store keeps rows", ""),
            &terms,
            &df,
            max_df,
        );
        assert!(
            id > prose,
            "identifier {id} must outrank prose mention {prose}"
        );
        assert!(
            prose > 0.0 && prose <= USAGE_SCALE,
            "prose stays in the usage band, got {prose}"
        );
        assert!(
            (id - LEXICAL_MAX_SCORE).abs() < 1e-9,
            "witness reaches the ceiling, got {id}"
        );
    }

    #[test]
    fn identifier_bonus_never_exceeds_the_cap() {
        // A 6-term identifier run — bonus alone would overshoot without the cap.
        let terms: Vec<String> = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (df, max_df) = uniform_df(&terms, 5);
        let s = score_rows(
            &win(
                "/a.rs",
                "fn alpha_beta_gamma_delta_epsilon_zeta() {}",
                "alpha_beta_gamma_delta_epsilon_zeta",
            ),
            &terms,
            &df,
            max_df,
        );
        assert!((s - LEXICAL_MAX_SCORE).abs() < 1e-9, "capped, got {s}");
    }

    #[test]
    fn mention_without_name_match_stays_below_full_score() {
        // A whole-identifier body mention (imports, uses, calls) IS a witness
        // — but a bare name-only or partial-mention row is not, and stays in
        // the usage band, strictly below the witness ceiling.
        let terms: Vec<String> = ["bounded", "session", "store"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (df, max_df) = uniform_df(&terms, 10);
        let def = score_rows(
            &win(
                "/a.rs",
                "pub struct bounded_session_store { target: u32 }",
                "symbol:⟨/a.rs::bounded_session_store⟩",
            ),
            &terms,
            &df,
            max_df,
        );
        let partial = score_rows(
            &win("/b.rs", "the bounded session store keeps rows", ""),
            &terms,
            &df,
            max_df,
        );
        assert!((def - LEXICAL_MAX_SCORE).abs() < 1e-9);
        assert!(
            partial < def - 0.2,
            "non-witness {partial} must stay clearly below witness {def}"
        );
    }

    #[test]
    fn camel_case_token_is_a_witness_snake_prose_is_not() {
        // `BoundedSessionStore` (camelCase) witnesses the same query that
        // snake prose does not — per-token equality, not phrase matching.
        let terms: Vec<String> = ["bounded", "session", "store"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (df, max_df) = uniform_df(&terms, 10);
        let camel = score_rows(
            &win("/a.rs", "BoundedSessionStore::new(pool)", ""),
            &terms,
            &df,
            max_df,
        );
        let prose = score_rows(
            &win("/b.rs", "our bounded session store semantics", ""),
            &terms,
            &df,
            max_df,
        );
        assert!(
            (camel - LEXICAL_MAX_SCORE).abs() < 1e-9,
            "camel token witnesses, got {camel}"
        );
        assert!(
            prose <= USAGE_SCALE,
            "prose phrase must not witness, got {prose}"
        );
    }

    #[test]
    fn partial_identifier_stays_below_full_identifier() {
        // The 5-term query "unrelated alpha zeta exact target" spells out the
        // full identifier — that row IS the definition witness (0.8). A noise
        // row sharing only 2 terms stays in the usage band (< 0.55).
        let terms: Vec<String> = ["unrelated", "alpha", "zeta", "exact", "target"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (df, max_df) = uniform_df(&terms, 10);
        let full = score_rows(
            &win("/exact.rs", "fn unrelated_alpha_zeta_exact_target() {}", ""),
            &terms,
            &df,
            max_df,
        );
        let partial = score_rows(
            &win("/noise.rs", "fn unrelated_alpha_helper() {}", ""),
            &terms,
            &df,
            max_df,
        );
        assert!(
            (full - LEXICAL_MAX_SCORE).abs() < 1e-9,
            "the full identifier row is the witness, got {full}"
        );
        assert!(
            partial < USAGE_SCALE,
            "partial identifier run must stay in the usage band, got {partial}"
        );
        assert!(full > partial);
    }

    #[test]
    fn rare_term_match_outranks_ubiquitous_term_match() {
        // IDF direction: a term matched everywhere is weak evidence.
        let terms = vec!["common".to_string(), "rarefind".to_string()];
        let mut df = HashMap::new();
        df.insert("common", 20);
        df.insert("rarefind", 1);
        let rare = score_rows(
            &win("/a.rs", "calls rarefind here", "rarefind"),
            &terms,
            &df,
            20,
        );
        let common = score_rows(&win("/b.rs", "calls common here", ""), &terms, &df, 20);
        assert!(rare > common, "rare {rare} must outrank common {common}");
    }

    #[test]
    fn path_only_match_is_downweighted_and_empty_match_is_zero() {
        let terms = vec!["absent".to_string()];
        let mut df = HashMap::new();
        df.insert("absent", 3);
        let none = score_rows(&win("/a.rs", "nothing relevant here", ""), &terms, &df, 3);
        assert_eq!(none, 0.0);
        let path_only = score_rows(
            &win("/absent.rs", "nothing relevant here", ""),
            &terms,
            &df,
            3,
        );
        assert!(
            path_only > 0.0 && path_only < 0.5,
            "path-only hit must stay a weak topic hint, got {path_only}"
        );
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
