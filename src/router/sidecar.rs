//! Per-repo SIDECAR metadata: a tiny JSON file the worker writes after each
//! index completion, that the ROUTER reads to render the repo list / cold
//! `/index-stats` / `/graph` WITHOUT opening RocksDB (and without spawning a
//! worker just to display a count).
//!
//! ## Why a sidecar (not the DB)
//!
//! `graph_cache` / `stats_cache` live INSIDE each repo's RocksDB (`index_meta`).
//! Reading them needs an exclusive-LOCK DB open — exactly the eager-open the
//! process-per-project design exists to avoid. So the router cannot read them
//! for a cold repo. The sidecar is the router-readable channel: a few numbers +
//! a timestamp, written by the worker (which already has the DB open) at index
//! completion.
//!
//! ## Deliberately LIGHT
//!
//! It carries ONLY what the cold UI needs: file count, last-indexed time, state,
//! embedding model/dim. It does NOT copy `graph_cache` / `stats_cache` — those
//! can be large at kernel scale and the full graph/stats views are served by
//! proxying to a (now-spawned) worker. Keeping the sidecar to fixed-size scalars
//! means no scale concern and a trivially-validated shape.
//!
//! ## Crash-safety / corruption
//!
//! Written with the project's standard atomic tempfile+rename (`config.rs`
//! pattern), so the router never observes a half-written file. A
//! missing/corrupt/old-shape file is NOT an error: the reader returns `None` and
//! the router renders a "not indexed / unknown" placeholder, mirroring how
//! `store::ops::get_cached_graph` treats a cold/corrupt cache as `Ok(None)`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store::ops::SymbolWithPos;
use crate::store::{normalize_repo_path, sanitize_repo_name};

/// Light, fixed-shape per-repo metadata for cold (no-worker) display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSidecar {
    /// Indexed file count (from `stats_cache.files` / `count_indexed_files`).
    pub file_count: u64,
    /// RFC3339 last successful index completion, or None if never indexed.
    pub last_indexed_at: Option<String>,
    /// Coarse state string for the UI: "indexed" once a successful run exists.
    /// (Live "indexing"/"error" state belongs to a running worker; the sidecar
    /// only records the durable last-known-good.)
    pub state: String,
    /// Embedding model name the index was built with.
    pub embedding_model: String,
    /// Embedding dimension (0 if unknown).
    pub embedding_dim: u64,
    /// Sidecar shape version — bump if the shape changes so an old file is
    /// treated as `None` (placeholder) rather than mis-parsed.
    pub schema: u32,
}

/// Current sidecar shape version. A file with a different `schema` is treated as
/// absent (cold placeholder), never mis-read.
pub const SIDECAR_SCHEMA: u32 = 1;

/// Directory holding sidecar files: `<data_dir>/sidecar/`.
fn sidecar_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sidecar")
}

/// Path to a repo's sidecar file: `<data_dir>/sidecar/<sanitized-repo>.json`.
/// Keyed by the SAME `sanitize_repo_name` used for the RocksDB dir so the router
/// and worker agree on the filename.
pub fn sidecar_path(data_dir: &Path, repo: &str) -> PathBuf {
    sidecar_dir(data_dir).join(format!("{}.json", sanitize_repo_name(repo)))
}

/// Write a repo's sidecar atomically (tempfile + fsync + rename), matching the
/// crash-safe pattern used for `settings.json` and the vector shard files. A
/// half-written file never appears under the real name. Best-effort at the call
/// site: a write failure must not fail an index run (the worker logs + moves on;
/// the router falls back to its cold placeholder).
pub fn write_sidecar(data_dir: &Path, repo: &str, meta: &RepoSidecar) -> Result<()> {
    let dir = sidecar_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create sidecar dir {}", dir.display()))?;
    let target = sidecar_path(data_dir, repo);

    let json = serde_json::to_vec_pretty(meta).context("serialize sidecar")?;

    // Atomic write: tempfile in the SAME dir → write → fsync → persist (rename).
    let mut tmp = tempfile::NamedTempFile::new_in(&dir).context("create sidecar tempfile")?;
    {
        use std::io::Write as _;
        tmp.write_all(&json).context("write sidecar tempfile")?;
        tmp.as_file().sync_all().context("fsync sidecar tempfile")?;
    }
    tmp.persist(&target)
        .with_context(|| format!("persist sidecar {}", target.display()))?;
    Ok(())
}

/// Read a repo's sidecar. Returns `Ok(None)` when the file is absent, unreadable,
/// corrupt, or a different `schema` — in all those cases the router renders the
/// cold "not indexed / unknown" placeholder rather than erroring. Only an
/// unexpected IO error (other than not-found) propagates.
pub fn read_sidecar(data_dir: &Path, repo: &str) -> Option<RepoSidecar> {
    let path = sidecar_path(data_dir, repo);
    let bytes = std::fs::read(&path).ok()?;
    let meta: RepoSidecar = serde_json::from_slice(&bytes).ok()?;
    if meta.schema != SIDECAR_SCHEMA {
        return None; // old/unknown shape → treat as cold
    }
    Some(meta)
}

// ── Auxiliary sidecar payloads (graph + files list) ──────────────────────────
//
// The light `meta.json` (above) is polled frequently (every 3s by the UI's
// index-status loop), so it stays fixed-shape + tiny. The graph and files-list
// payloads can be larger (graph ~hundreds of KB at scale) and are read ONLY when
// a repo detail is opened — so they live in SEPARATE sibling files keyed by the
// same sanitized name: `<sanitized>.graph.json` / `<sanitized>.files.json`. This
// keeps the frequent meta poll from ever dragging the heavy graph payload, and
// lets the router serve a cold (possibly stale — accepted) graph/files view on
// detail-open WITHOUT spawning a worker.

/// Path to an auxiliary sidecar file: `<data_dir>/sidecar/<sanitized>.<kind>.json`.
fn aux_path(data_dir: &Path, repo: &str, kind: &str) -> PathBuf {
    sidecar_dir(data_dir).join(format!("{}.{}.json", sanitize_repo_name(repo), kind))
}

/// Write an arbitrary JSON-serializable payload to a repo's `<kind>` sidecar,
/// using the SAME atomic tempfile+fsync+rename pattern as `write_sidecar`. Used
/// for the `graph` and `files` payloads. Best-effort at the call site — a write
/// failure must not fail an index run.
pub fn write_aux_json<T: serde::Serialize>(
    data_dir: &Path,
    repo: &str,
    kind: &str,
    value: &T,
) -> Result<()> {
    let dir = sidecar_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create sidecar dir {}", dir.display()))?;
    let target = aux_path(data_dir, repo, kind);
    let json = serde_json::to_vec(value).context("serialize aux sidecar")?;
    let mut tmp = tempfile::NamedTempFile::new_in(&dir).context("create aux tempfile")?;
    {
        use std::io::Write as _;
        tmp.write_all(&json).context("write aux tempfile")?;
        tmp.as_file().sync_all().context("fsync aux tempfile")?;
    }
    tmp.persist(&target)
        .with_context(|| format!("persist aux sidecar {}", target.display()))?;
    Ok(())
}

/// Read + deserialize a repo's `<kind>` aux sidecar. Returns `None` when absent,
/// unreadable, or corrupt — the caller serves a cold/empty placeholder, never
/// errors (mirrors `read_sidecar` / `store::ops::get_cached_graph` degradation).
pub fn read_aux_json<T: serde::de::DeserializeOwned>(
    data_dir: &Path,
    repo: &str,
    kind: &str,
) -> Option<T> {
    let bytes = std::fs::read(aux_path(data_dir, repo, kind)).ok()?;
    serde_json::from_slice::<T>(&bytes).ok()
}

// ── Symbol-table sidecar (cross-repo Phase 2 resolution) ─────────────────────
//
// The per-repo SYMBOL TABLE as a sibling aux sidecar
// (`<sanitized>.symbols.json`). A process-per-project WORKER holds exactly one
// RocksDB handle (its own repo) — RocksDB takes an exclusive per-directory
// lock, so a worker can NEVER open another live worker's DB to look up symbols
// during cross-repo Phase 2 edge resolution. This sidecar is the lock-free
// channel: after a successful index run the owning worker publishes its full
// `(fqn, file, name, line_start, line_end)` table here, and every other
// worker's Phase 2 reads these files to resolve raw edges into foreign repos.
//
// Precedence at resolution time: own DB > live foreign DBs (standalone mode)
// > symbol sidecars. Entries mirror `store::ops::SymbolWithPos` so conversion
// is field-for-field.

/// Current symbol-sidecar shape version.
pub const SYMBOLS_SIDECAR_SCHEMA: u32 = 1;

/// One exported symbol: exactly the fields Phase 2 needs for deterministic
/// candidate tie-breaking (`select_best_candidate` sorts by file/line range)
/// and RELATE endpoints (`fqn`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSidecarEntry {
    pub fqn: String,
    pub file: String,
    pub name: String,
    pub line_start: i64,
    pub line_end: i64,
}

/// Full symbol table payload for one repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSidecar {
    /// Shape version — a mismatching value makes readers treat the file as
    /// absent (cross-repo resolution silently skips that repo).
    pub schema: u32,
    /// Normalized repo path the entries belong to (readers skip their OWN
    /// repo's sidecar; live-DB symbols always win over sidecar copies).
    pub repo: String,
    /// RFC3339 publish timestamp (diagnostics only).
    pub indexed_at: String,
    /// Number of entries (redundant with `symbols.len()` but lets a reader
    /// sanity-check before materializing the vec).
    pub count: u64,
    pub symbols: Vec<SymbolSidecarEntry>,
}

/// Write a repo's symbol-table sidecar atomically (same tempfile+fsync+rename
/// pattern as every other sidecar). Best-effort at the call site: a failure
/// must not fail an index run — it only disables cross-repo resolution INTO
/// this repo until the next successful publish.
pub fn write_symbol_sidecar(
    data_dir: &Path,
    repo: &str,
    mut symbols: Vec<SymbolWithPos>,
) -> Result<()> {
    // Deterministic order keeps diffs readable and gives readers a stable
    // iteration order regardless of DB scan order.
    symbols.sort_unstable_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line_start.cmp(&b.line_start))
            .then(a.line_end.cmp(&b.line_end))
            .then(a.fqn.cmp(&b.fqn))
    });
    let payload = SymbolSidecar {
        schema: SYMBOLS_SIDECAR_SCHEMA,
        repo: normalize_repo_path(repo),
        indexed_at: chrono::Utc::now().to_rfc3339(),
        count: symbols.len() as u64,
        symbols: symbols
            .into_iter()
            .map(|s| SymbolSidecarEntry {
                fqn: s.fqn,
                file: s.file,
                name: s.name,
                line_start: s.line_start,
                line_end: s.line_end,
            })
            .collect(),
    };
    write_aux_json(data_dir, repo, "symbols", &payload)
}

/// Read one repo's symbol sidecar. `None` when absent/corrupt/wrong-schema —
/// cross-repo resolution degrades to "that repo contributes no candidates".
pub fn read_symbol_sidecar(data_dir: &Path, repo: &str) -> Option<SymbolSidecar> {
    let payload: SymbolSidecar = read_aux_json(data_dir, repo, "symbols")?;
    if payload.schema != SYMBOLS_SIDECAR_SCHEMA {
        return None;
    }
    Some(payload)
}

/// Load EVERY other repo's symbol sidecar from the shared sidecar dir. Used by
/// Phase 2 in worker mode where foreign repos have no open DB handles. Skips
/// the caller's own repo (`own_repo`) — live local symbols always win.
pub fn load_foreign_symbol_sidecars(data_dir: &Path, own_repo: &str) -> Vec<SymbolSidecar> {
    let dir = sidecar_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let own = normalize_repo_path(own_repo);
    let suffix = format!(".{SYMBOLS_AUX_KIND}.json");
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(&suffix) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(payload) = serde_json::from_slice::<SymbolSidecar>(&bytes) else {
            continue; // corrupt/old shape → skip that repo silently
        };
        if payload.schema != SYMBOLS_SIDECAR_SCHEMA || payload.repo == own {
            continue;
        }
        out.push(payload);
    }
    out
}

const SYMBOLS_AUX_KIND: &str = "symbols";

/// Remove ALL of a repo's sidecar files (meta + graph + files). Called on repo /
/// index removal so the router's cold view doesn't keep serving stale data for a
/// repo whose index was just deleted. Best-effort: a file that isn't there (or
/// can't be removed) is ignored — the next index rewrites them anyway.
pub fn remove_all_sidecars(data_dir: &Path, repo: &str) {
    let _ = std::fs::remove_file(sidecar_path(data_dir, repo));
    let _ = std::fs::remove_file(aux_path(data_dir, repo, "graph"));
    let _ = std::fs::remove_file(aux_path(data_dir, repo, "files"));
    // Symbol table (cross-repo Phase 2 source) must die with the index so a
    // deleted repo never resolves fresh edges against stale symbols.
    let _ = std::fs::remove_file(aux_path(data_dir, repo, SYMBOLS_AUX_KIND));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample() -> RepoSidecar {
        RepoSidecar {
            file_count: 42,
            last_indexed_at: Some("2026-06-25T00:00:00+00:00".to_string()),
            state: "indexed".to_string(),
            embedding_model: "voyage-code-3".to_string(),
            embedding_dim: 1024,
            schema: SIDECAR_SCHEMA,
        }
    }

    #[test]
    fn roundtrip_write_then_read() {
        let dir = TempDir::new().unwrap();
        let repo = r"d:\projects\rust\foo";
        write_sidecar(dir.path(), repo, &sample()).unwrap();
        let got = read_sidecar(dir.path(), repo).expect("sidecar should read back");
        assert_eq!(got.file_count, 42);
        assert_eq!(got.embedding_dim, 1024);
        assert_eq!(got.state, "indexed");
    }

    #[test]
    fn missing_file_reads_as_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_sidecar(dir.path(), r"d:\never\indexed").is_none());
    }

    #[test]
    fn corrupt_file_reads_as_none_not_error() {
        let dir = TempDir::new().unwrap();
        let repo = r"d:\projects\rust\bar";
        // Write a valid sidecar, then clobber it with garbage (simulating a
        // worker that died mid-write — though the atomic rename prevents that,
        // this proves the reader degrades on ANY corruption).
        write_sidecar(dir.path(), repo, &sample()).unwrap();
        std::fs::write(sidecar_path(dir.path(), repo), b"{ not valid json").unwrap();
        assert!(
            read_sidecar(dir.path(), repo).is_none(),
            "corrupt sidecar must read as None (cold placeholder), never panic/error"
        );
    }

    #[test]
    fn wrong_schema_reads_as_none() {
        let dir = TempDir::new().unwrap();
        let repo = r"d:\projects\rust\baz";
        let mut m = sample();
        m.schema = 999; // future/unknown shape
        write_sidecar(dir.path(), repo, &m).unwrap();
        assert!(
            read_sidecar(dir.path(), repo).is_none(),
            "unknown schema must read as None so an old shape is never mis-read"
        );
    }

    fn sym(fqn: &str, file: &str, name: &str, ls: i64, le: i64) -> SymbolWithPos {
        SymbolWithPos {
            fqn: fqn.to_string(),
            file: file.to_string(),
            name: name.to_string(),
            line_start: ls,
            line_end: le,
        }
    }

    #[test]
    fn symbol_sidecar_roundtrip_and_sort() {
        let dir = TempDir::new().unwrap();
        let repo = "/repo/writer";
        write_symbol_sidecar(
            dir.path(),
            repo,
            vec![
                sym("/repo/writer/z.rs::zed", "/repo/writer/z.rs", "zed", 1, 5),
                sym(
                    "/repo/writer/a.rs::alpha",
                    "/repo/writer/a.rs",
                    "alpha",
                    10,
                    20,
                ),
                sym("/repo/writer/a.rs::beta", "/repo/writer/a.rs", "beta", 1, 5),
            ],
        )
        .unwrap();
        let got = read_symbol_sidecar(dir.path(), repo).expect("symbol sidecar reads back");
        assert_eq!(got.schema, SYMBOLS_SIDECAR_SCHEMA);
        assert_eq!(got.repo, repo);
        assert_eq!(got.count, 3);
        assert_eq!(got.symbols.len(), 3);
        // Sorted by (file, line_start, line_end, fqn).
        assert_eq!(got.symbols[0].name, "beta");
        assert_eq!(got.symbols[1].name, "alpha");
        assert_eq!(got.symbols[2].name, "zed");
    }

    #[test]
    fn foreign_loader_skips_own_and_corrupt() {
        let dir = TempDir::new().unwrap();
        let own = "/repo/self";
        write_symbol_sidecar(
            dir.path(),
            own,
            vec![sym("/repo/self/a.rs::a", "/repo/self/a.rs", "a", 1, 2)],
        )
        .unwrap();
        write_symbol_sidecar(
            dir.path(),
            "/repo/other",
            vec![sym("/repo/other/b.rs::b", "/repo/other/b.rs", "b", 3, 4)],
        )
        .unwrap();
        // Corrupt a third repo's payload — loader must skip it silently.
        std::fs::write(
            aux_path(dir.path(), "/repo/broken", SYMBOLS_AUX_KIND),
            b"{nope",
        )
        .unwrap();

        let mut loaded = load_foreign_symbol_sidecars(dir.path(), own);
        assert_eq!(loaded.len(), 1, "own repo and corrupt payloads are skipped");
        assert_eq!(loaded.pop().unwrap().repo, "/repo/other");

        // Missing dir entirely → empty, never an error.
        assert!(load_foreign_symbol_sidecars(TempDir::new().unwrap().path(), own).is_empty());
    }

    #[test]
    fn remove_all_clears_symbols_payload() {
        let dir = TempDir::new().unwrap();
        let repo = "/repo/gone";
        write_symbol_sidecar(
            dir.path(),
            repo,
            vec![sym(
                format!("{repo}/a.rs::a").as_str(),
                format!("{repo}/a.rs").as_str(),
                "a",
                1,
                2,
            )],
        )
        .unwrap();
        assert!(read_symbol_sidecar(dir.path(), repo).is_some());
        remove_all_sidecars(dir.path(), repo);
        assert!(
            read_symbol_sidecar(dir.path(), repo).is_none(),
            "index removal must drop the symbol table so no stale cross-repo resolution survives"
        );
    }
}
