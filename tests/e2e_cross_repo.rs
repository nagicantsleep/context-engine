//! SUBPROCESS end-to-end proof for cross-repo navigation in router mode.
//!
//! Real topology, real processes: the router spawns TWO independent workers
//! (one per repo). Repo B is indexed first and publishes its SYMBOL SIDECAR;
//! repo A's Phase 2 then resolves its call to `shared_target` against that
//! sidecar (A holds no handle to B — RocksDB LOCK makes that impossible), and
//! at query time BFS follows the materialized edge into B by calling back
//! through the router (`/api/cross-repo/chunk` → worker B `/api/graph-chunk`).
//!
//! Embeddings come from an in-test mock Voyage gateway (fixed 4-dim vector),
//! so no network or API keys are needed. LLM rerank stays disabled (no keys).
//!
//! Run: cargo test --test e2e_cross_repo -- --ignored --nocapture

use std::time::Duration;

use axum::{Json, Router, routing::post};
use reqwest::Client;
use tempfile::TempDir;
use tokio::net::TcpListener;

use context_engine_rs::config::{Settings, config_path, write_settings_atomic};
use context_engine_rs::router::sidecar::read_symbol_sidecar;

mod e2e_common;

use e2e_common::{poke_action, start_router};

/// Wait until `pred` holds or `secs` elapse (500ms cadence).
async fn wait_until<F, Fut>(secs: u64, mut pred: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if pred().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
#[ignore = "spawns two real worker subprocesses; run with --ignored --nocapture"]
async fn query_in_repo_a_expands_into_indexed_repo_b_via_router() {
    // ── Mock Voyage gateway (fixed embedding keeps retrieval deterministic) ──
    let gateway = Router::new().route(
        "/v1/embeddings",
        post(|| async {
            Json(serde_json::json!({
                "data": [{"embedding": [1.0, 0.0, 0.0, 0.0]}]
            }))
        }),
    );
    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(gateway_listener, gateway).await.unwrap();
    });

    // ── Two repos: B defines the target, A calls into it ────────────────────
    let home = TempDir::new().unwrap();
    let repo_b_dir = home.path().join("lib-b");
    std::fs::create_dir_all(&repo_b_dir).unwrap();
    std::fs::write(
        repo_b_dir.join("shared.rs"),
        b"pub fn shared_target() -> bool { true }\n",
    )
    .unwrap();
    let repo_b = repo_b_dir.to_string_lossy().to_string();

    let repo_a_dir = home.path().join("app-a");
    std::fs::create_dir_all(&repo_a_dir).unwrap();
    std::fs::write(
        repo_a_dir.join("main.rs"),
        b"fn caller() { shared_target(); }\npub fn unused_entry() {}\n",
    )
    .unwrap();
    let repo_a = repo_a_dir.to_string_lossy().to_string();

    let mut settings = Settings {
        machine_id: Some("cross-repo-e2e-machine".to_string()),
        ..Settings::default()
    };
    settings.repos = vec![repo_b.clone(), repo_a.clone()];
    settings.worker_idle_secs = 3600;
    settings.mcp_index_wait_secs = 30;
    settings.embedding.api_keys = vec!["cross-repo-e2e-key".to_string()];
    settings.embedding.voyage_base_url = Some(format!("http://{gateway_addr}/v1"));
    write_settings_atomic(&config_path(home.path()), &settings).unwrap();

    let (addr, proxy) = start_router(&home).await;
    let client = Client::new();
    let data_dir = home.path(); // e2e_common boots the router with data_dir == home

    // Body returns Result so EVERY path reaches kill_all below (a panicking
    // worker left alive holds the inherited stdout pipe and hangs the runner).
    let outcome: Result<(), String> = async {
        // ── Index B FIRST; its symbol sidecar must be published ─────────────
        let status = poke_action(&client, addr, &repo_b).await;
        if !status.is_success() {
            return Err(format!("index poke for repo B failed: {status}"));
        }
        if !wait_until(60, || async { read_symbol_sidecar(data_dir, &repo_b).is_some() }).await {
            return Err(
                "repo B must publish a symbol sidecar after a successful index".to_string(),
            );
        }

        // ── Index A AFTER B: lazy materialization resolves A→B via sidecar ──
        let status = poke_action(&client, addr, &repo_a).await;
        if !status.is_success() {
            return Err(format!("index poke for repo A failed: {status}"));
        }
        if !wait_until(60, || async { read_symbol_sidecar(data_dir, &repo_a).is_some() }).await {
            return Err("repo A must finish indexing (and publish its own sidecar)".to_string());
        }
        // The readiness gate may answer VectorOnly while post-index graph
        // cache recompute drains; retry until BFS actually runs (graph_ms>0)
        // or budget expires.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut last_body = serde_json::Value::Null;
        let foreign_hit = loop {
            let response = client
                .post(format!("http://{addr}/api/query"))
                .json(&serde_json::json!({
                    "repo": repo_a,
                    "query": "shared_target",
                    "top_k": 10
                }))
                .timeout(Duration::from_secs(90))
                .send()
                .await
                .map_err(|error| format!("query through router→workerA→router→workerB: {error}"))?;
            if !response.status().is_success() {
                return Err(format!("query failed with {}", response.status()));
            }
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            let graph_ran = body["timing"]["graph_ms"].as_u64().unwrap_or(0) > 0;
            let hit = body["results"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .find(|r| r["file"].as_str().is_some_and(|f| f.starts_with(repo_b.as_str())))
                .cloned();
            if let Some(hit) = hit {
                break Ok(hit);
            }
            last_body = body;
            if !graph_ran && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            break Err(format!(
                "no result from repo B (graph_ran={graph_ran}); last={}",
                serde_json::to_string(&last_body).unwrap_or_default()
            ));
        };
        let foreign_hit = foreign_hit.map_err(|error| error)?;
        if !foreign_hit["content"]
            .as_str()
            .unwrap_or_default()
            .contains("shared_target")
        {
            return Err(format!(
                "foreign chunk must carry repo B's content: {foreign_hit}"
            ));
        }
        Ok(())
    }
    .await;

    // Deterministic teardown on BOTH success and failure paths.
    proxy.registry.kill_all().await;
    outcome.expect("cross-repo e2e flow");
 }
