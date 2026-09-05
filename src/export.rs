//! Portable `graph.json` export — the repo's call graph as a self-describing
//! JSON artifact with per-edge confidence.
//!
//! This is deliberately DIFFERENT from the UI's `/graph` payload
//! (`store::ops::CallGraph`): that one is bounded tiny (250 nodes / 600 edges)
//! and omits edge confidence, because it serves the index-status poll. An
//! export is an explicit, user-invoked batch operation, so it carries the full
//! shape downstream tools need: nodes, edges, and `confidence` per edge (with
//! the derived `inferred` flag, same `confidence < 1.0` rule as the MCP tags).
//!
//! Contract (`format: "context-engine-graph/v1"`):
//!   * edges drive the node set — every exported node participates in ≥1
//!     retained call edge; symbols without edges are not exported;
//!   * an edge whose endpoint has no symbol row (a stale edge left behind by
//!     deletion) is DROPPED and counted in `dangling_edges_dropped` — the
//!     artifact never contains a dangling reference;
//!   * duplicate edges collapse strongest-first: an extracted edge (confidence
//!     NULL) beats an inferred one; among inferred edges the highest
//!     confidence wins;
//!   * `truncated` reports a caps hit (`max_edges` / symbol-fetch window), not
//!     a graph property — the artifact may then be partial.
//!
//! Requires a v2+ index (in_name/out_name carry full FQNs).

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

/// Version tag written into every export artifact.
pub const GRAPH_EXPORT_FORMAT: &str = "context-engine-graph/v1";

/// One exported node (a symbol participating in ≥1 retained edge).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExportNode {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub file: String,
    pub line_start: i64,
    pub line_end: i64,
}

/// One exported edge (caller → callee) with its provenance.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExportEdge {
    pub source: String,
    pub target: String,
    /// Parser-extracted edges serialize `null`; inferred ones carry the
    /// recorded confidence weight.
    pub confidence: Option<f32>,
    pub inferred: bool,
}

/// The export artifact. `node_count`/`edge_count` are redundant with the
/// arrays' lengths on purpose: a truncated export states its size up front.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GraphExport {
    pub format: &'static str,
    pub repo: String,
    pub schema_version: u32,
    pub exported_at: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub truncated: bool,
    pub dangling_edges_dropped: usize,
    pub nodes: Vec<ExportNode>,
    pub edges: Vec<ExportEdge>,
}

/// Build the export for one repo database. `max_edges` bounds the edge scan;
/// `max_nodes` bounds the symbol-row fetch window — endpoints missing from
/// that window (or with no symbol row at all) count as dangling and their
/// edges are dropped. Fails on a v1 (pre-FQN) index.
pub async fn build_graph_export(
    db: &Surreal<Db>,
    repo: &str,
    max_nodes: usize,
    max_edges: usize,
) -> Result<GraphExport, String> {
    let schema_version = crate::store::read_db_schema_version(db).await;
    if schema_version < 2 {
        return Err("this repo's index predates FQN call edges (schema < 2). \
             Re-index the repo, then retry."
            .to_string());
    }

    // 1. Edge scan (bounded by max_edges, +1 to detect overflow), deduped
    //    strongest-first per (source, target) pair.
    #[derive(Deserialize)]
    struct RawEdge {
        #[serde(default)]
        source: String,
        #[serde(default)]
        target: String,
        #[serde(default)]
        confidence: Option<f32>,
    }
    let raw: Vec<RawEdge> = db
        .query("SELECT in_name AS source, out_name AS target, confidence FROM calls LIMIT $lim")
        .bind(("lim", (max_edges as i64) + 1))
        .await
        .map_err(|e| format!("edge scan failed: {e}"))?
        .take(0)
        .map_err(|e| format!("edge scan failed: {e}"))?;
    let mut truncated = raw.len() > max_edges;

    // (source, target) → confidence: None = extracted (strongest).
    let mut deduped: HashMap<(String, String), Option<f32>> = HashMap::new();
    for row in raw.into_iter().take(max_edges) {
        if row.source.is_empty() || row.target.is_empty() {
            continue;
        }
        let incoming = row.confidence.filter(|c| *c < 1.0);
        match deduped.entry((row.source, row.target)) {
            Entry::Vacant(v) => {
                v.insert(incoming);
            }
            Entry::Occupied(mut o) => {
                let better = match (*o.get(), incoming) {
                    (None, _) => false,
                    (Some(_), None) => true,
                    (Some(a), Some(b)) => b > a,
                };
                if better {
                    o.insert(incoming);
                }
            }
        }
    }

    // 2. Symbol rows for the endpoints. The fetch window is capped; endpoints
    //    outside it (or with no row at all) are dangling.
    #[derive(Deserialize)]
    struct RawSymbol {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        file: String,
        #[serde(default)]
        line_start: Option<i64>,
        #[serde(default)]
        line_end: Option<i64>,
    }
    let symbol_rows: Vec<RawSymbol> = db
        .query(
            "SELECT meta::id(id) AS id, name, kind, file, line_start, line_end FROM symbol \
             LIMIT $lim",
        )
        .bind(("lim", (max_nodes as i64) + 1))
        .await
        .map_err(|e| format!("symbol scan failed: {e}"))?
        .take(0)
        .map_err(|e| format!("symbol scan failed: {e}"))?;
    if symbol_rows.len() > max_nodes {
        truncated = true;
    }
    // `meta::id` yields the RECORD-ID string form (escaped ids are wrapped in
    // `⟨…⟩`) — strip to the bare FQN the calls columns use.
    let symbols: HashMap<String, RawSymbol> = symbol_rows
        .into_iter()
        .map(|mut r| {
            r.id =
                r.id.trim_start_matches('⟨')
                    .trim_end_matches('⟩')
                    .to_string();
            (r.id.clone(), r)
        })
        .collect();

    // 3. Nodes = retained-edge endpoints with a symbol row; dangling edges
    //    (either endpoint missing) are dropped and counted.
    let mut endpoints: HashSet<String> = HashSet::new();
    for ((source, target), _) in &deduped {
        endpoints.insert(source.clone());
        endpoints.insert(target.clone());
    }
    let dangling: HashSet<&String> = endpoints
        .iter()
        .filter(|fqn| !symbols.contains_key(*fqn))
        .collect();
    let mut dangling_edges_dropped = 0usize;

    let mut nodes: Vec<ExportNode> = Vec::new();
    for fqn in &endpoints {
        let Some(row) = symbols.get(fqn) else {
            continue;
        };
        nodes.push(ExportNode {
            id: fqn.clone(),
            name: row.name.clone(),
            kind: row.kind.clone(),
            file: row.file.clone(),
            line_start: row.line_start.unwrap_or(0),
            line_end: row.line_end.unwrap_or(0),
        });
    }
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut edges: Vec<ExportEdge> = Vec::new();
    for ((source, target), confidence) in deduped {
        if dangling.contains(&source) || dangling.contains(&target) {
            dangling_edges_dropped += 1;
            continue;
        }
        edges.push(ExportEdge {
            source,
            target,
            confidence,
            inferred: confidence.is_some(),
        });
    }
    edges.sort_by(|a, b| a.source.cmp(&b.source).then(a.target.cmp(&b.target)));

    Ok(GraphExport {
        format: GRAPH_EXPORT_FORMAT,
        repo: repo.to_string(),
        schema_version,
        exported_at: chrono::Utc::now().to_rfc3339(),
        node_count: nodes.len(),
        edge_count: edges.len(),
        truncated,
        dangling_edges_dropped,
        nodes,
        edges,
    })
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
            "CREATE symbol:`⟨{fqn}⟩` SET name = '{name}', file = '{file}', \
             kind = 'function', line_start = 1, line_end = 2;"
        ))
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

    #[tokio::test]
    async fn exports_nodes_edges_and_confidence() {
        let (_home, db) = seed_db("/p/export").await;
        add_symbol(&db, A, "main", "/p/main.rs").await;
        add_symbol(&db, B, "run", "/p/lib.rs").await;
        add_symbol(&db, C, "inner", "/p/lib.rs").await;
        add_edge(&db, A, B, "/p/main.rs", None).await;
        add_edge(&db, B, C, "/p/lib.rs", Some(0.5)).await;
        // A symbol WITHOUT edges must not appear (edges drive the node set).
        add_symbol(&db, "/p/orphan.rs::orphan", "orphan", "/p/orphan.rs").await;

        let export = build_graph_export(&db, "/p/export", 1000, 1000)
            .await
            .unwrap();
        assert_eq!(export.format, GRAPH_EXPORT_FORMAT);
        assert_eq!(export.node_count, 3, "orphan must be excluded: {export:?}");
        assert_eq!(export.edge_count, 2);
        assert!(!export.truncated);
        assert_eq!(export.dangling_edges_dropped, 0);

        let json = serde_json::to_value(&export).unwrap();
        let edges = json["edges"].as_array().unwrap();
        // Edges are (source, target)-sorted; look them up by pair, not index.
        let ab = edges
            .iter()
            .find(|e| e["source"] == A && e["target"] == B)
            .expect("A→B edge present");
        let bc = edges
            .iter()
            .find(|e| e["source"] == B && e["target"] == C)
            .expect("B→C edge present");
        assert_eq!(ab["confidence"], serde_json::Value::Null);
        assert_eq!(ab["inferred"], false);
        assert_eq!(bc["confidence"], 0.5);
        assert_eq!(bc["inferred"], true);
        let nodes = json["nodes"].as_array().unwrap();
        assert!(
            nodes
                .iter()
                .all(|n| n["file"].is_string() && n["line_start"] == 1)
        );
    }

    #[tokio::test]
    async fn duplicate_edge_collapses_extracted_first() {
        let (_home, db) = seed_db("/p/dedup").await;
        add_symbol(&db, A, "main", "/p/main.rs").await;
        add_symbol(&db, B, "run", "/p/lib.rs").await;
        add_edge(&db, A, B, "/p/main.rs", Some(0.4)).await;
        add_edge(&db, A, B, "/p/main.rs", None).await; // extracted wins
        let export = build_graph_export(&db, "/p/dedup", 1000, 1000)
            .await
            .unwrap();
        assert_eq!(export.edge_count, 1);
        assert_eq!(export.edges[0].confidence, None);
        assert!(!export.edges[0].inferred);
    }

    #[tokio::test]
    async fn dangling_edge_is_dropped_and_counted() {
        let (_home, db) = seed_db("/p/dangling").await;
        add_symbol(&db, A, "main", "/p/main.rs").await;
        // Edge into a symbol with NO row (stale after deletion).
        add_edge(&db, A, "/p/gone.rs::ghost", "/p/main.rs", None).await;
        let export = build_graph_export(&db, "/p/dangling", 1000, 1000)
            .await
            .unwrap();
        assert_eq!(export.edge_count, 0);
        assert_eq!(export.dangling_edges_dropped, 1);
        // The dangling endpoint must not leak into nodes either.
        assert_eq!(export.node_count, 1);
        assert_eq!(export.nodes[0].id, A);
    }

    #[tokio::test]
    async fn caps_report_truncated() {
        let (_home, db) = seed_db("/p/caps").await;
        add_symbol(&db, A, "main", "/p/main.rs").await;
        add_symbol(&db, B, "run", "/p/lib.rs").await;
        add_edge(&db, A, B, "/p/main.rs", None).await;
        let export = build_graph_export(&db, "/p/caps", 1000, 0).await.unwrap();
        assert!(export.truncated, "edge cap hit must be reported");
        assert_eq!(export.edge_count, 0);
    }

    #[tokio::test]
    async fn v1_index_is_rejected_with_guidance() {
        let home = TempDir::new().unwrap();
        let db = crate::store::open_db(home.path(), "/p/old", 0)
            .await
            .unwrap();
        let err = build_graph_export(&db, "/p/old", 1000, 1000)
            .await
            .unwrap_err();
        assert!(err.contains("schema < 2"), "{err}");
    }
}
