//! One-shot `context-engine export-graph` subcommand: write a repo's call
//! graph as a portable `graph.json` artifact (nodes/edges/confidence, format
//! `context-engine-graph/v1`). The heavy lifting lives in the lib's
//! `export::build_graph_export`; this glue resolves settings/data-dir the same
//! way boot does, opens the repo DB read-only, writes the file, and prints a
//! summary. It never registers a repo or triggers indexing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tokio::sync::RwLock;

use context_engine_rs::config;
use context_engine_rs::export;
use context_engine_rs::store;

/// Arguments for one `export-graph` invocation (parsed from the CLI).
pub struct ExportGraphArgs {
    pub repo: String,
    pub out: Option<PathBuf>,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub data_dir: Option<PathBuf>,
}

/// Run the export to completion and return the process exit code.
///
/// 0 = artifact written; 1 = export rejected (old schema, scan failure);
/// 2 = environment error (no home dir, unreadable settings).
pub async fn run(args: &ExportGraphArgs) -> i32 {
    if args.max_nodes == 0 || args.max_edges == 0 {
        return exit_with_error("--max-nodes/--max-edges must be positive", 2);
    }

    let home_dir = match dirs::home_dir() {
        Some(h) => h,
        None => {
            return exit_with_error(
                "could not determine user home directory; set HOME (Unix) or USERPROFILE (Windows)",
                2,
            );
        }
    };
    let settings = match config::ensure_dir_and_load(&home_dir) {
        Ok(s) => s,
        Err(e) => {
            return exit_with_error(
                &format!("could not load settings from {}: {e}", home_dir.display()),
                2,
            );
        }
    };

    // Same precedence as boot: CLI flag > env > Settings.data_dir > builtin.
    let data_dir = args
        .data_dir
        .clone()
        .or_else(|| {
            std::env::var("CONTEXT_ENGINE_DATA_DIR")
                .ok()
                .map(PathBuf::from)
        })
        .or_else(|| settings.data_dir.clone())
        .unwrap_or_else(|| config::default_data_dir(&home_dir));

    let repo = store::normalize_repo_path(&args.repo);
    let repo_dbs: Arc<RwLock<HashMap<String, Surreal<Db>>>> = Arc::new(RwLock::new(HashMap::new()));
    let db = match store::get_or_open(&repo_dbs, &data_dir, &repo, settings.repo_generation(&repo))
        .await
    {
        Ok(d) => d,
        Err(e) => return exit_with_error(&format!("could not open index database: {e}"), 1),
    };

    let export = match export::build_graph_export(&db, &repo, args.max_nodes, args.max_edges).await
    {
        Ok(e) => e,
        Err(e) => return exit_with_error(&e, 1),
    };

    let out_path = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("graph.json"));
    let json = match serde_json::to_string_pretty(&export) {
        Ok(j) => j,
        Err(e) => return exit_with_error(&format!("serialize export: {e}"), 1),
    };
    if let Err(e) = std::fs::write(&out_path, json.as_bytes()) {
        return exit_with_error(&format!("write {}: {e}", out_path.display()), 1);
    }

    println!("exported {}", out_path.display());
    println!("  repo: {repo} (schema v{})", export.schema_version);
    println!(
        "  nodes: {}  edges: {}",
        export.node_count, export.edge_count
    );
    if export.dangling_edges_dropped > 0 {
        println!(
            "  dangling edges dropped: {} (endpoints without a symbol row)",
            export.dangling_edges_dropped
        );
    }
    if export.truncated {
        println!(
            "  TRUNCATED at caps (max-nodes {}, max-edges {}) — artifact is partial; \
             raise the caps and re-run for the full graph",
            args.max_nodes, args.max_edges
        );
    }
    0
}

fn exit_with_error(message: &str, code: i32) -> i32 {
    eprintln!("error: {message}");
    code
}
