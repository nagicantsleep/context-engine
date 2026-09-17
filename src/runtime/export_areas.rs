//! One-shot `context-engine export-areas` subcommand: derive functional areas
//! from a repo's call graph (connected components over the exported graph —
//! deterministic union-find, NO LLM and no clustering per decision 0001) and
//! write one markdown guidance file per area (`area-<n>.md`) that an agent can
//! read to navigate a functional module without exploring blind.
//!
//! Same boot/data-dir precedence and read-only DB access as `export-graph`;
//! the only difference is the output: area markdown files instead of one
//! graph artifact.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tokio::sync::RwLock;

use context_engine_rs::config;
use context_engine_rs::export;
use context_engine_rs::store;

/// Arguments for one `export-areas` invocation (parsed from the CLI).
pub struct ExportAreasArgs {
    pub repo: String,
    pub out_dir: Option<PathBuf>,
    /// Areas with more symbols than this are skipped (0 = keep everything).
    /// A whole-repo connected blob is real but useless as "guidance".
    pub max_area_size: usize,
    pub data_dir: Option<PathBuf>,
}

/// Run the export to completion and return the process exit code.
///
/// 0 = areas written; 1 = export rejected (old schema, scan failure);
/// 2 = environment error (no home dir, unreadable settings).
pub async fn run(args: &ExportAreasArgs) -> i32 {
    let home_dir = match dirs::home_dir() {
        Some(h) => h,
        None => {
            eprintln!(
                "error: could not determine user home directory; set HOME (Unix) or USERPROFILE (Windows)"
            );
            return 2;
        }
    };
    let settings = match config::ensure_dir_and_load(&home_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "error: could not load settings from {}: {e}",
                home_dir.display()
            );
            return 2;
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
        Err(e) => {
            eprintln!("error: could not open index database: {e}");
            return 1;
        }
    };

    let export_result = export::build_graph_export(&db, &repo, usize::MAX, usize::MAX).await;
    let graph = match export_result {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    if graph.truncated {
        // usize::MAX caps mean this cannot happen today; keep the honest guard
        // anyway so a future cap change cannot silently produce partial areas.
        eprintln!("error: export hit caps — areas would be partial; aborting");
        return 1;
    }

    let areas = export::to_areas(&graph);
    let out_dir = args
        .out_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("areas"));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("error: create {}: {e}", out_dir.display());
        return 1;
    }

    let mut written = 0usize;
    let mut skipped = 0usize;
    for area in &areas {
        if args.max_area_size > 0 && area.size > args.max_area_size {
            skipped += 1;
            continue;
        }
        let mut md = format!(
            "# Area {id}\n\nDerived from the call graph (connected component {id}, \
             {size} symbols). No LLM was involved: every entry below is an indexed \
             symbol or file path.\n\n## Files (most symbols first)\n",
            id = area.id,
            size = area.size
        );
        for f in &area.files {
            md.push_str(&format!("- {f}\n"));
        }
        md.push_str("\n## Symbols\n");
        for s in &area.symbols {
            md.push_str(&format!("- `{s}`\n"));
        }
        let path = out_dir.join(format!("area-{}.md", area.id));
        if let Err(e) = std::fs::write(&path, md.as_bytes()) {
            eprintln!("error: write {}: {e}", path.display());
            return 1;
        }
        written += 1;
    }

    println!(
        "wrote {written} area file(s) to {} ({} skipped by --max-area-size; {} nodes / {} edges total)",
        out_dir.display(),
        skipped,
        graph.node_count,
        graph.edge_count
    );
    0
}
