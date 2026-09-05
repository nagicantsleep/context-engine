//! Query-time cross-repo chunk resolution over the router.
//!
//! A process-per-project WORKER holds exactly one RocksDB handle (its own
//! repo). When BFS expansion in `graph_expand` follows an edge whose endpoint
//! lives in ANOTHER repo, the endpoint chunk cannot be fetched from a local DB
//! — the worker asks the ROUTER, which resolves the owning repo from the
//! request's `file` path and proxies to that repo's live (or freshly spawned)
//! worker (`GET /api/cross-repo/chunk` → `GET /api/graph-chunk`).
//!
//! Degradation contract: any failure (router down, spawn budget exceeded, repo
//! unindexed, symbol missing) yields `None` and the caller DROPS that
//! expansion subtree — the same observable behavior as a missing local
//! endpoint DB. No fabricated/empty-content rows are ever emitted (they would
//! corrupt rerank input and UI rendering).

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Chunk payload served by a worker's `GET /api/graph-chunk` handler and
/// consumed by BFS expansion. Mirrors `graph_expand::ExpandedChunk`'s data
/// fields (score is assigned by the caller, not transported).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteChunkData {
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub content: String,
    pub symbol: Option<String>,
    pub symbol_fqn: Option<String>,
    pub symbol_kind: Option<String>,
}

/// Router-addressed resolver for cross-repo chunk fetches. Cheap to clone.
///
/// Present ONLY in worker mode (the router passes `--router-url` at spawn).
/// Standalone/monolith boots never construct one — every endpoint is already
/// reachable through the in-process `repo_dbs` map.
#[derive(Clone)]
pub struct CrossRepoResolver {
    router_base: String,
    client: reqwest::Client,
}

/// Per-request budget covering the full router-side chain: cold worker spawn
/// (bounded by `router::spawn::SPAWN_READY_TIMEOUT` = 20s) + RocksDB open +
/// query. One generous ceiling instead of two chained budgets.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

impl CrossRepoResolver {
    pub fn new(router_base: String) -> Self {
        Self {
            router_base: router_base.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .build()
                .expect("reqwest client"),
        }
    }

    /// Fetch the chunk for `fqn` whose `file` path identifies the owning repo.
    /// `None` on ANY failure — callers must treat that as "endpoint absent".
    pub async fn fetch_chunk(&self, fqn: &str, file: &str) -> Option<RemoteChunkData> {
        let url = format!("{}/api/cross-repo/chunk", self.router_base);
        let resp = self
            .client
            .get(&url)
            .query(&[("fqn", fqn), ("file", file)])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<RemoteChunkData>().await.ok()
    }

    pub fn router_base(&self) -> &str {
        &self.router_base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_returns_none_on_connection_failure() {
        // Port 1 is reserved (unroutable) — connection refused/error fast.
        let resolver = CrossRepoResolver::new("http://127.0.0.1:1".to_string());
        assert!(
            resolver
                .fetch_chunk("/repo/b/b.rs::b", "/repo/b/b.rs")
                .await
                .is_none(),
            "unreachable router must degrade to None, never panic"
        );
    }

    #[tokio::test]
    async fn fetch_parses_success_payload() {
        use axum::routing::get;
        use axum::{Json, Router};
        let app = Router::new().route(
            "/api/cross-repo/chunk",
            get(|| async {
                Json(RemoteChunkData {
                    file: "/repo/b/b.rs".to_string(),
                    line_start: 1,
                    line_end: 5,
                    content: "fn b() {}".to_string(),
                    symbol: Some("b".to_string()),
                    symbol_fqn: Some("/repo/b/b.rs::b".to_string()),
                    symbol_kind: Some("function".to_string()),
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let resolver = CrossRepoResolver::new(format!("http://{addr}"));
        let got = resolver
            .fetch_chunk("/repo/b/b.rs::b", "/repo/b/b.rs")
            .await
            .expect("stub router serves the chunk");
        assert_eq!(got.symbol_fqn.as_deref(), Some("/repo/b/b.rs::b"));
        assert_eq!(got.line_start, 1);
    }
}
