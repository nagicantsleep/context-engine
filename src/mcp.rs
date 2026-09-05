// Pre-existing layout: a few helpers live AFTER the inline #[cfg(test)]
// `tests` module rather than before it. Clippy flags this as
// `items_after_test_module`. Reordering the file is out of scope for the
// current change; suppress the lint at module level.
#![allow(clippy::items_after_test_module)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tokio::sync::RwLock;

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};

pub(crate) mod query_gate;
pub(crate) mod readiness;
#[cfg(test)]
mod tests;

use crate::config::Settings;
use crate::embedding::voyage::VoyageClient;
use crate::indexing::IndexEngine;
use crate::llm::LlmClient;
use crate::query::engine::QueryGraphMode;
use crate::store;

// ─── Output budget ───────────────────────────────────────────────────────
// MCP clients (Claude Code, IDE extensions) reject tool outputs exceeding
// ~50,000 characters. We cap at 48K to leave headroom for client framing.

const MAX_TOOL_OUTPUT_CHARS: usize = 48_000;
const MAX_FIRST_LINE_CHARS: usize = 120;
/// chars→tokens heuristic (×4, the standard estimate for code/English).
/// Documented approximation, not a tokenizer: keeps agents' budgets honest
/// without pulling a tokenizer dependency into the binary.
pub const CHARS_PER_TOKEN: usize = 4;

/// Estimated tokens for `text` under the [`CHARS_PER_TOKEN`] heuristic.
/// `char` count (not bytes): CJK/emoji-heavy content counts closer to 1
/// token/char, so this is a lower bound for such text — conservative in the
/// safe direction for budget display (never promises fewer tokens than sent).
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}
/// A single result block ready for budget-aware assembly.
#[derive(Default)]
struct OutputBlock {
    header: String,
    content: String,
    file: String,
    line_start: u32,
    line_end: u32,
    callers: Option<u32>,
    caller_files: Option<u32>,
    caller_names: Vec<String>,
    callee_names: Vec<String>,
    callees: Option<u32>,
    callers_inferred: bool,
    callees_inferred: bool,
}

/// Assemble result blocks into a single string respecting `MAX_TOOL_OUTPUT_CHARS`.
/// `max_tokens`: optional caller cap. Converted to chars (×[`CHARS_PER_TOKEN`])
/// and intersected with the built-in char budget — the tighter bound wins, so
/// omitting it (`None`) preserves today's 48K behavior exactly.
fn assemble_with_budget(blocks: &[OutputBlock], max_tokens: Option<usize>) -> String {
    // Reserve space for the footers so they're never squeezed out: the
    // truncation notice (~130 chars) plus the token estimate line (~35).
    const FOOTER_RESERVE: usize = 200;
    let mut effective_budget = MAX_TOOL_OUTPUT_CHARS - FOOTER_RESERVE;
    if let Some(t) = max_tokens {
        // 0 disables? No — 0 means "no usable budget": clamp to footer so the
        // output is just the truncation notice, never a panic on underflow.
        effective_budget = effective_budget.min(t.saturating_mul(CHARS_PER_TOKEN));
    }

    let mut out = String::new();
    let mut truncated_count = 0usize;
    let mut budget_exceeded = false;

    for block in blocks {
        let full_text = format!("{}\n{}", block.header, block.content);
        let separator = if out.is_empty() { "" } else { "\n\n" };

        if !budget_exceeded {
            let candidate_len = out.len() + separator.len() + full_text.len();
            if candidate_len <= effective_budget {
                out.push_str(separator);
                out.push_str(&full_text);
                continue;
            }
            budget_exceeded = true;
        }

        // Truncated form: header + first line (capped) + elision marker.
        let first_line = block.content.lines().next().unwrap_or("");
        let first_line_display = if first_line.len() > MAX_FIRST_LINE_CHARS {
            let mut end = MAX_FIRST_LINE_CHARS;
            while !first_line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…", &first_line[..end])
        } else {
            first_line.to_string()
        };

        let elision = if block.line_end > block.line_start {
            format!(
                "... (L{}-{} elided, use Read)",
                block.line_start + 1,
                block.line_end
            )
        } else {
            String::new()
        };

        let truncated_text = if elision.is_empty() {
            format!("{}\n{}", block.header, first_line_display)
        } else {
            format!("{}\n{}\n{}", block.header, first_line_display, elision)
        };

        let separator = if out.is_empty() { "" } else { "\n\n" };
        let candidate_len = out.len() + separator.len() + truncated_text.len();
        if candidate_len <= effective_budget {
            out.push_str(separator);
            out.push_str(&truncated_text);
        }
        truncated_count += 1;
    }

    if truncated_count > 0 {
        let footer = format!(
            "\n\n---\n{} of {} results truncated to fit output size limit; \
             use the Read tool with the line ranges above.",
            truncated_count,
            blocks.len()
        );
        out.push_str(&footer);
    }

    // Token estimate: always emitted so agents can budget follow-ups. Counts
    // the body BEFORE this line (self-exclusion keeps the number stable).
    let tokens = estimate_tokens(&out);
    out.push_str(&format!("\n\n---\n~{tokens} tokens (est. chars/4)"));

    out
}

/// Merge output blocks from the same file whose line ranges overlap or are
/// adjacent (next.line_start <= current.line_end + 1). Merged content is
/// re-read from the filesystem; if the read fails, original content strings
/// are concatenated with line-number dedup.
///
/// Preserves first-occurrence position: the merged block occupies the slot of
/// the earliest block in its file group. Blocks from different files pass
/// through unchanged.
fn merge_overlapping_blocks(blocks: Vec<OutputBlock>) -> Vec<OutputBlock> {
    if blocks.len() <= 1 {
        return blocks;
    }

    // Group by file. Normalize path separators for grouping on Windows
    // (the index stores native `\` but sub-query paths may use `/`).
    let normalize_key = |file: &str| -> String {
        if cfg!(windows) {
            file.replace('/', "\\")
        } else {
            file.to_string()
        }
    };

    // Group by normalized file key. Collect (original_index, block).
    let mut by_file: std::collections::HashMap<String, Vec<(usize, OutputBlock)>> =
        std::collections::HashMap::new();
    for (i, block) in blocks.into_iter().enumerate() {
        let key = normalize_key(&block.file);
        by_file.entry(key).or_default().push((i, block));
    }

    // Merge within each file group.
    let mut positioned: Vec<(usize, OutputBlock)> = Vec::new();

    for (_file, mut group) in by_file {
        if group.len() == 1 {
            let (idx, block) = group.remove(0);
            positioned.push((idx, block));
            continue;
        }

        // Sort by line_start within file.
        group.sort_unstable_by_key(|(_, b)| b.line_start);

        // Merge pass: accumulate (min_orig_idx, block, original_contents).
        // min_orig_idx tracks the earliest original position of any block
        // that was merged into this entry — used for output ordering.
        let mut merged: Vec<(usize, OutputBlock, Vec<String>)> = Vec::new();

        for (orig_idx, mut next) in group {
            if let Some((min_idx, current, originals)) = merged.last_mut() {
                if next.line_start <= current.line_end + 1 {
                    current.line_end = current.line_end.max(next.line_end);
                    *min_idx = (*min_idx).min(orig_idx);
                    // Combine caller/callee stats: the two merged blocks usually
                    // belong to DIFFERENT symbols (e.g. an import region vs. a
                    // function), so the count and its names MUST travel together —
                    // adopt them as an atomic triple/pair from whichever block has
                    // the higher count. If we bumped only the count (old behavior),
                    // a merged block could carry callers=Some(N) with empty names,
                    // tripping the `names.is_empty()` fallback in
                    // format_enriched_caller_tag and emitting a bare "[callers:N]".
                    if next.callers.unwrap_or(0) > current.callers.unwrap_or(0) {
                        current.callers = next.callers;
                        current.caller_files = next.caller_files;
                        current.caller_names = std::mem::take(&mut next.caller_names);
                        current.callers_inferred = next.callers_inferred;
                    } else {
                        current.callers_inferred |= next.callers_inferred;
                    }
                    if next.callees.unwrap_or(0) > current.callees.unwrap_or(0) {
                        current.callees = next.callees;
                        current.callee_names = std::mem::take(&mut next.callee_names);
                        current.callees_inferred = next.callees_inferred;
                    } else {
                        current.callees_inferred |= next.callees_inferred;
                    }
                    originals.push(next.content);
                } else {
                    let content_snapshot = next.content.clone();
                    merged.push((orig_idx, next, vec![content_snapshot]));
                }
            } else {
                let content_snapshot = next.content.clone();
                merged.push((orig_idx, next, vec![content_snapshot]));
            }
        }

        // Rebuild content and header for merged blocks.
        for (_, block, originals) in &mut merged {
            if originals.len() > 1 {
                // Multiple blocks were merged — try FS re-read for the full range.
                match crate::query::engine::read_lines_from_fs(
                    &block.file,
                    block.line_start,
                    block.line_end,
                ) {
                    Ok(text) => block.content = text,
                    Err(_) => {
                        // Fallback: union original content lines, dedup by
                        // line-number prefix, sort by line number.
                        block.content = merge_content_fallback(originals);
                    }
                }
            }
            // Rebuild header with updated range + enriched caller/callee tags.
            let caller_tag = format_enriched_caller_tag(
                block.callers,
                &block.caller_names,
                block.caller_files,
                block.callers_inferred,
            );
            let callee_tag = format_enriched_callee_tag(
                block.callees,
                &block.callee_names,
                block.callees_inferred,
            );
            block.header = format!(
                "{}#L{}-{}{}{}",
                block.file, block.line_start, block.line_end, caller_tag, callee_tag
            );
        }

        // Use the tracked min_orig_idx for output ordering.
        for (min_idx, block, _) in merged {
            positioned.push((min_idx, block));
        }
    }

    // Sort by the position index to restore original priority order.
    positioned.sort_by_key(|(idx, _)| *idx);
    positioned.into_iter().map(|(_, b)| b).collect()
}

/// Fallback content merge: union all numbered lines from the original content
/// strings, dedup by line number, sort ascending. Only used when the FS re-read
/// fails (file moved/deleted mid-query).
fn merge_content_fallback(originals: &[String]) -> String {
    let mut by_lineno: std::collections::BTreeMap<u32, &str> = std::collections::BTreeMap::new();
    for content in originals {
        for line in content.lines() {
            if let Some(colon_pos) = line.find(':')
                && let Ok(num) = line[..colon_pos].trim().parse::<u32>()
            {
                by_lineno.entry(num).or_insert(line);
            }
        }
    }
    if by_lineno.is_empty() {
        return originals.join("\n");
    }
    by_lineno.values().copied().collect::<Vec<_>>().join("\n")
}

// ─── Tool argument schema ─────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CodebaseRetrievalArgs {
    /// Natural-language description of the code or information you are looking for.
    pub information_request: String,
    /// Absolute path to the repository root. Must be a configured and indexed repository.
    pub workspace_full_path: String,
    /// Optional: filter results to specific symbol kinds (e.g. ["function", "class"]).
    #[serde(default)]
    pub filter_kind: Option<Vec<String>>,
    /// Optional: filter results to specific languages (e.g. ["rust", "typescript"]).
    #[serde(default)]
    pub filter_lang: Option<Vec<String>>,
    /// Optional: filter results to files matching this path substring.
    #[serde(default)]
    pub filter_path: Option<String>,
    /// Optional: cap the response at ~this many tokens (chars/4 heuristic,
    /// intersected with the 48K char budget — tighter bound wins).
    #[serde(default)]
    pub max_tokens: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FileRetrievalArgs {
    /// Absolute path to the repository root.
    pub workspace_full_path: String,
    /// Relative path to the file within the repository (e.g. "src/main.rs").
    pub file_path: String,
    /// Natural-language description of what you're looking for in this file.
    pub information_request: String,
    /// Number of top-scoring snippets to return. Defaults to 5.
    pub top_k: Option<usize>,
    /// Optional: cap the response at ~this many tokens (chars/4 heuristic,
    /// intersected with the 48K char budget — tighter bound wins).
    #[serde(default)]
    pub max_tokens: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TracePathArgs {
    /// Absolute path to the repository root.
    pub workspace_full_path: String,
    /// Symbol the path starts from: full FQN (`/abs/file.rs::mod::name`),
    /// `file.rs::name`, `::name`, or a bare symbol name. Ambiguous references
    /// return the candidate list instead of guessing.
    pub from_symbol: String,
    /// Symbol the path must reach (same accepted forms as `from_symbol`).
    pub to_symbol: String,
    /// Which edges to follow from `from_symbol`: `callees` (how execution gets
    /// from `from_symbol` to `to_symbol`, default) or `callers` (the chain by
    /// which `from_symbol` is reached from `to_symbol`).
    #[serde(default)]
    pub direction: Option<String>,
    /// Maximum call-graph edges per path. Defaults to 5, capped at 10.
    #[serde(default)]
    pub max_depth: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RepoTracePathArgs {
    /// Symbol the path starts from: full FQN (`/abs/file.rs::mod::name`),
    /// `file.rs::name`, `::name`, or a bare symbol name. Ambiguous references
    /// return the candidate list instead of guessing.
    pub from_symbol: String,
    /// Symbol the path must reach (same accepted forms as `from_symbol`).
    pub to_symbol: String,
    /// Which edges to follow from `from_symbol`: `callees` (default) or `callers`.
    #[serde(default)]
    pub direction: Option<String>,
    /// Maximum call-graph edges per path. Defaults to 5, capped at 10.
    #[serde(default)]
    pub max_depth: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SymbolContextArgs {
    /// Absolute path to the repository root.
    pub workspace_full_path: String,
    /// Symbol to describe: full FQN (`/abs/file.rs::mod::name`), `file.rs::name`,
    /// `::name`, or a bare symbol name. Ambiguous references return the
    /// candidate list instead of guessing.
    pub symbol: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RepoSymbolContextArgs {
    /// Symbol to describe: full FQN (`/abs/file.rs::mod::name`), `file.rs::name`,
    /// `::name`, or a bare symbol name. Ambiguous references return the
    /// candidate list instead of guessing.
    pub symbol: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ImpactArgs {
    /// Absolute path to the repository root.
    pub workspace_full_path: String,
    /// Symbol to analyze: full FQN (`/abs/file.rs::mod::name`), `file.rs::name`,
    /// `::name`, or a bare symbol name. Ambiguous references return the
    /// candidate list instead of guessing.
    pub symbol: String,
    /// How many caller levels to walk (1 = direct callers only). Defaults to
    /// 3, capped at 8.
    #[serde(default)]
    pub max_depth: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RepoImpactArgs {
    /// Symbol to analyze: full FQN (`/abs/file.rs::mod::name`), `file.rs::name`,
    /// `::name`, or a bare symbol name. Ambiguous references return the
    /// candidate list instead of guessing.
    pub symbol: String,
    /// How many caller levels to walk (1 = direct callers only). Defaults to
    /// 3, capped at 8.
    #[serde(default)]
    pub max_depth: Option<usize>,
}

// ─── MCP handler ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct McpHandler {
    /// Used ONLY for `settings.json` access (config_path / ensure_dir_and_load).
    /// settings.json's location is fixed at `~/.context-engine/settings.json`.
    home_dir: PathBuf,
    /// Boot-resolved data directory (CLI > env > `Settings.data_dir` > builtin
    /// default). Used for store/embedding paths. Captured once at startup —
    /// MUST NOT be re-read from `Settings` mid-run.
    data_dir: PathBuf,
    index_engine: Arc<IndexEngine>,
    repo_dbs: Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    settings: Arc<RwLock<crate::config::Settings>>,
    /// Router-backed cross-repo resolver (worker mode; `None` standalone).
    cross_repo: Option<crate::query::cross_repo::CrossRepoResolver>,
    // Required by the #[tool_router] macro; suppress the dead_code lint.
    #[allow(dead_code)]
    tool_router: ToolRouter<McpHandler>,
}

#[tool_router]
impl McpHandler {
    pub fn new(
        home_dir: PathBuf,
        data_dir: PathBuf,
        index_engine: Arc<IndexEngine>,
        repo_dbs: Arc<RwLock<HashMap<String, Surreal<Db>>>>,
        settings: Arc<RwLock<crate::config::Settings>>,
        enabled_tools: &[String],
        cross_repo: Option<crate::query::cross_repo::CrossRepoResolver>,
    ) -> Self {
        let all_tools: &[&str] = &[
            "codebase-retrieval",
            "file-retrieval",
            "trace-path",
            "symbol-context",
            "impact",
        ];
        // `list_repos` is deliberately absent from this gate: it is always
        // exposed (see run_list_repos) — hiding discovery behind the same
        // opt-in list as the retrieval tools would make default settings
        // undiscoverable.
        let mut router = Self::tool_router();
        for &name in all_tools {
            if !enabled_tools.iter().any(|e| e == name) {
                router.disable_route(name);
            }
        }
        Self {
            home_dir,
            data_dir,
            index_engine,
            repo_dbs,
            settings,
            cross_repo,
            tool_router: router,
        }
    }

    /// Discover the repositories this engine can query (read-only).
    ///
    /// Lists every configured repo path with its durable index state (read
    /// from sidecars — never spawns a worker or triggers indexing) and the
    /// sanitized per-repo endpoint name. Use a returned path as
    /// `workspace_full_path` on the retrieval tools.
    #[tool(name = "list_repos", annotations(read_only_hint = true))]
    async fn list_repos(&self) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let text = run_list_repos(&settings, &self.data_dir, None);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Trace a call path between two symbols (read-only).
    ///
    /// Resolves both references against the repo's symbol table (full FQN,
    /// `file.rs::name`, `::name`, or bare name — ambiguous references return
    /// the candidate list) and walks the repo's `calls` graph depth-bounded in
    /// the requested `direction`. Paths are ordered chains of fully-qualified
    /// symbols; `~inferred` marks heuristic edges. Edges owned by this repo's
    /// database only — a path that would cross into another repo ends there.
    #[tool(name = "trace-path")]
    async fn trace_path(
        &self,
        Parameters(args): Parameters<TracePathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let repo = store::normalize_repo_path(&args.workspace_full_path);
        let text = run_trace_path(
            &self.repo_dbs,
            &self.data_dir,
            &settings,
            &repo,
            &args.from_symbol,
            &args.to_symbol,
            args.direction.as_deref(),
            args.max_depth,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Show one symbol's definition source and call-graph context (read-only).
    ///
    /// Resolves the reference (same accepted forms as `trace-path`), then
    /// returns the numbered definition source (secret-fenced, truncated at
    /// 200 lines) plus caller/callee counts and proximity-sorted names — the
    /// same enriched tags the retrieval tools emit.
    #[tool(name = "symbol-context")]
    async fn symbol_context(
        &self,
        Parameters(args): Parameters<SymbolContextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let repo = store::normalize_repo_path(&args.workspace_full_path);
        let text = run_symbol_context(
            &self.repo_dbs,
            &self.data_dir,
            &settings,
            &repo,
            &args.symbol,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Analyze what is affected by changing a symbol (read-only).
    ///
    /// Walks the repo's `calls` graph BACKWARD from the symbol, BFS-grouped by
    /// hop distance (level 1 = direct callers), with a most-affected-files
    /// summary. Edges owned by this repo's database only. Same accepted symbol
    /// reference forms as `trace-path`.
    #[tool(name = "impact")]
    async fn impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let repo = store::normalize_repo_path(&args.workspace_full_path);
        let text = run_impact(
            &self.repo_dbs,
            &self.data_dir,
            &settings,
            &repo,
            &args.symbol,
            args.max_depth,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[doc = include_str!("prompts/mcp_codebase_retrieval.txt")]
    #[tool(name = "codebase-retrieval")]
    async fn codebase_retrieval(
        &self,
        Parameters(args): Parameters<CodebaseRetrievalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Take an owned snapshot of settings — the guard is dropped before the .await below.
        let settings = self.settings.read().await.clone();
        // Build augmented query with structured filter params as inline prefixes
        let augmented_query = build_augmented_query(
            &args.information_request,
            args.filter_kind.as_deref(),
            args.filter_lang.as_deref(),
            args.filter_path.as_deref(),
        );
        let text = run_codebase_retrieval(
            &self.home_dir,
            &self.data_dir,
            &self.index_engine,
            &self.repo_dbs,
            &settings,
            &augmented_query,
            &args.workspace_full_path,
            self.cross_repo.as_ref(),
            args.max_tokens,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[doc = include_str!("prompts/mcp_file_retrieval.txt")]
    #[tool(name = "file-retrieval")]
    async fn file_retrieval(
        &self,
        Parameters(args): Parameters<FileRetrievalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let text = run_file_retrieval(
            &self.data_dir,
            &self.repo_dbs,
            &settings,
            &args.workspace_full_path,
            &args.file_path,
            &args.information_request,
            args.top_k.unwrap_or(5),
            args.max_tokens,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            rmcp::model::Implementation::new("context-engine-rs", env!("CARGO_PKG_VERSION")),
        )
    }
}

// ─── Repo-scoped MCP handler ─────────────────────────────────────────────
// Exposes the same tools but with `workspace_full_path` pre-bound to a fixed
// repo path. Clients don't need to pass it — the endpoint itself is per-repo.

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RepoCodebaseRetrievalArgs {
    /// Natural-language description of the code or information you are looking for.
    pub information_request: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RepoFileRetrievalArgs {
    /// Relative path to the file within the repository (e.g. "src/main.rs").
    pub file_path: String,
    /// Natural-language description of what you're looking for in this file.
    pub information_request: String,
    /// Number of top-scoring snippets to return. Defaults to 5.
    pub top_k: Option<usize>,
}

#[derive(Clone)]
pub struct RepoMcpHandler {
    home_dir: PathBuf,
    data_dir: PathBuf,
    repo_path: String,
    index_engine: Arc<IndexEngine>,
    repo_dbs: Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    settings: Arc<RwLock<crate::config::Settings>>,
    /// Router-backed cross-repo resolver (worker mode; `None` standalone).
    cross_repo: Option<crate::query::cross_repo::CrossRepoResolver>,
    #[allow(dead_code)]
    tool_router: ToolRouter<RepoMcpHandler>,
}

#[tool_router]
impl RepoMcpHandler {
    pub fn new(
        home_dir: PathBuf,
        data_dir: PathBuf,
        repo_path: String,
        index_engine: Arc<IndexEngine>,
        repo_dbs: Arc<RwLock<HashMap<String, Surreal<Db>>>>,
        settings: Arc<RwLock<crate::config::Settings>>,
        enabled_tools: &[String],
        cross_repo: Option<crate::query::cross_repo::CrossRepoResolver>,
    ) -> Self {
        let all_tools: &[&str] = &[
            "codebase-retrieval",
            "file-retrieval",
            "trace-path",
            "symbol-context",
            "impact",
        ];
        // Same deliberate omission of `list_repos` from the gate as the global
        // handler — see run_list_repos.
        let mut router = Self::tool_router();
        for &name in all_tools {
            if !enabled_tools.iter().any(|e| e == name) {
                router.disable_route(name);
            }
        }
        Self {
            home_dir,
            data_dir,
            repo_path,
            index_engine,
            repo_dbs,
            settings,
            cross_repo,
            tool_router: router,
        }
    }

    /// Discover the repositories this engine can query (read-only).
    ///
    /// Same listing as the global handler's `list_repos`, with the endpoint's
    /// pre-bound workspace marked. Reading state only — never triggers
    /// indexing.
    #[tool(name = "list_repos", annotations(read_only_hint = true))]
    async fn list_repos(&self) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let text = run_list_repos(&settings, &self.data_dir, Some(&self.repo_path));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Trace a call path between two symbols in this repo (read-only).
    ///
    /// Same tracing as the global handler's `trace-path`, with this endpoint's
    /// workspace pre-bound — no `workspace_full_path` argument.
    #[tool(name = "trace-path")]
    async fn trace_path(
        &self,
        Parameters(args): Parameters<RepoTracePathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let text = run_trace_path(
            &self.repo_dbs,
            &self.data_dir,
            &settings,
            &self.repo_path,
            &args.from_symbol,
            &args.to_symbol,
            args.direction.as_deref(),
            args.max_depth,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Show one symbol's definition source and call-graph context (read-only).
    ///
    /// Same as the global handler's `symbol-context`, with this endpoint's
    /// workspace pre-bound — no `workspace_full_path` argument.
    #[tool(name = "symbol-context")]
    async fn symbol_context(
        &self,
        Parameters(args): Parameters<RepoSymbolContextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let text = run_symbol_context(
            &self.repo_dbs,
            &self.data_dir,
            &settings,
            &self.repo_path,
            &args.symbol,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Analyze what is affected by changing a symbol in this repo (read-only).
    ///
    /// Same reverse call-graph analysis as the global handler's `impact`, with
    /// this endpoint's workspace pre-bound — no `workspace_full_path` argument.
    #[tool(name = "impact")]
    async fn impact(
        &self,
        Parameters(args): Parameters<RepoImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let text = run_impact(
            &self.repo_dbs,
            &self.data_dir,
            &settings,
            &self.repo_path,
            &args.symbol,
            args.max_depth,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[doc = include_str!("prompts/mcp_codebase_retrieval.txt")]
    #[tool(name = "codebase-retrieval")]
    async fn codebase_retrieval(
        &self,
        Parameters(args): Parameters<RepoCodebaseRetrievalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let text = run_codebase_retrieval(
            &self.home_dir,
            &self.data_dir,
            &self.index_engine,
            &self.repo_dbs,
            &settings,
            &args.information_request,
            &self.repo_path,
            self.cross_repo.as_ref(),
            None,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[doc = include_str!("prompts/mcp_file_retrieval_repo.txt")]
    #[tool(name = "file-retrieval")]
    async fn file_retrieval(
        &self,
        Parameters(args): Parameters<RepoFileRetrievalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let settings = self.settings.read().await.clone();
        let text = run_file_retrieval(
            &self.data_dir,
            &self.repo_dbs,
            &settings,
            &self.repo_path,
            &args.file_path,
            &args.information_request,
            args.top_k.unwrap_or(5),
            None,
        )
        .await;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RepoMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            rmcp::model::Implementation::new("context-engine-rs", env!("CARGO_PKG_VERSION")),
        )
    }
}

// ─── Shared query funnel ──────────────────────────────────────────────────

/// Run the codebase retrieval tool logic.
///
/// Returns plain-text results or an error/guidance string. Never panics, never
/// returns `Err` — all failure paths produce a human-readable string.
///
/// `home_dir` locates the fixed `settings.json` file. `data_dir` is the
/// boot-resolved data directory used for the per-repo RocksDB / embedding cache
/// paths. They are intentionally NOT collapsed into a single parameter — see
/// `Settings.data_dir` for the bootstrap rationale (Shape C).
///
/// This is the single shared funnel used by both the MCP tool and the REST
/// endpoint (`POST /api/mcp-tool`), so their outputs are byte-identical.
/// Choose the message for a query that produced no result blocks, distinguishing a
/// transient *warming* shard (retry) from a genuine empty ("no results"). Pure
/// function of the three signals so it is unit-testable without a live query.
///
/// Precedence: `warming` wins — an empty result while the shard is still loading
/// must NOT be reported as "no results" (the index is complete on disk). Only when
/// the shard is resident (`warming=false`) do we report a genuine empty, with the
/// rerank-rejected wording when the reranker actively rejected all candidates.
const MCP_PARTIAL_RESULTS_PREFIX: &str =
    "(index update is still publishing; showing only content-verified partial results)\n\n";

fn select_empty_or_warming_message(
    warming: bool,
    rerank_rejected: bool,
    information_request: &str,
) -> String {
    if warming {
        return "The index for this workspace is still warming (loading into memory). \
                It is complete on disk — retry the same request in a few seconds."
            .to_string();
    }
    if rerank_rejected {
        return "No relevant code found. The indexed codebase does not appear to \
                contain information related to this query. Please verify the query \
                is relevant to this project, or try alternative tools such as Grep \
                for exact-match searches."
            .to_string();
    }
    format!("No results found for: {information_request}")
}

/// Shared read-only repo discovery behind the `list_repos` MCP tool (used by
/// both the global and per-repo handlers). Reads ONLY the settings snapshot
/// passed in plus durable sidecars — it never registers a repo, spawns a
/// worker, or triggers indexing, so it is safe on the cold global `/mcp` route.
///
/// `this_repo` (per-repo endpoint) marks which entry is the pre-bound
/// workspace. `list_repos` is intentionally NOT gated by `enabled_mcp_tools`
/// (unlike the retrieval tools): it is the discovery surface that makes the
/// gated tools usable under default settings, and it exposes nothing the
/// router does not already serve through read-only routes.
fn run_list_repos(settings: &Settings, data_dir: &Path, this_repo: Option<&str>) -> String {
    let repos = &settings.repos;
    if repos.is_empty() {
        return "No repos configured yet. A repo is added automatically the first \
               time `codebase-retrieval` is called with its full path, or via the \
               Web UI settings.\n"
            .to_string();
    }

    let mut out = format!(
        "{} configured repo{}; enabled MCP tools: {}\n",
        repos.len(),
        if repos.len() == 1 { "" } else { "s" },
        settings.enabled_mcp_tools.join(", "),
    );

    for (i, repo) in repos.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n", i + 1, repo));
        if Some(repo.as_str()) == this_repo {
            out.push_str("   this workspace (workspace_full_path is pre-bound here)\n");
        }
        out.push_str(&format!(
            "   per-repo MCP endpoint name: {}\n",
            store::sanitize_repo_name(repo)
        ));
        match crate::router::sidecar::read_sidecar(data_dir, repo) {
            Some(meta) => match meta.last_indexed_at {
                Some(at) => out.push_str(&format!(
                    "   index: {} — {} file(s), last indexed {at} (model {})\n",
                    meta.state, meta.file_count, meta.embedding_model
                )),
                None => out.push_str("   index: never indexed (querying it triggers indexing)\n"),
            },
            None => {
                out.push_str("   index: no durable state yet (querying it triggers indexing)\n")
            }
        }
    }

    out.push_str(
        "\nPass one of these paths as `workspace_full_path`. A query in one repo \
         can also surface chunks from another when call-graph edges cross the \
         repo boundary (lazy cross-repo navigation).\n",
    );
    out
}

pub async fn run_codebase_retrieval(
    home_dir: &Path,
    data_dir: &Path,
    index_engine: &Arc<IndexEngine>,
    repo_dbs: &Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    settings: &Settings,
    information_request: &str,
    workspace_full_path: &str,
    cross: Option<&crate::query::cross_repo::CrossRepoResolver>,
    max_tokens: Option<usize>,
) -> String {
    // 1. Validate workspace_full_path.
    let repo = workspace_full_path.trim();
    if repo.is_empty() {
        return "Error: workspace_full_path is required. Pass the full path to the workspace \
                (repository) root directory. Call `list_repos` to see the workspaces this \
                engine already knows."
            .to_string();
    }
    let repo = &crate::store::normalize_repo_path(repo);

    // 2. Auto-register the repo if it is not yet configured.
    if !settings.repos.iter().any(|r| r == repo) {
        // Guard: path must exist and be a directory before we accept it.
        if !std::path::Path::new(repo).is_dir() {
            return format!(
                "Error: workspace '{}' does not exist or is not a directory. \
                 Call `list_repos` to see configured workspaces.",
                repo
            );
        }

        // Best-effort: append to settings.json on disk so the repo survives restart.
        match crate::config::ensure_dir_and_load(home_dir) {
            Ok(mut disk) => {
                if !disk.repos.iter().any(|r| r == repo) {
                    disk.repos.push(repo.to_string());
                    disk.version = crate::config::CURRENT_VERSION;
                    let target = crate::config::config_path(home_dir);
                    if let Err(e) = crate::config::write_settings_atomic(&target, &disk) {
                        tracing::warn!(repo = %repo, error = %e, "failed to persist auto-added repo to settings.json");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "failed to read settings.json for auto-add");
            }
        }

        // Register at runtime: seed status entry + spawn watcher.
        // Falls through to the existing freshness/trigger/wait/query flow below.
        index_engine.register_repo(repo).await;
    }

    // 3. Confirm embedding keys are present.
    if settings.embedding.api_keys.is_empty() {
        return "Error: no embedding API keys configured. \
                Add a Voyage AI key in the Context Engine UI first."
            .to_string();
    }

    // 4. One shared readiness decision selects both the wait budget and graph
    // mode. MCP and REST therefore cannot drift on the ResolveEdges fast path.
    let (query_graph_mode, query_warm_wait, output_prefix) = match readiness::await_index_ready(
        settings,
        index_engine,
        repo_dbs,
        data_dir,
        repo,
    )
    .await
    {
        readiness::IndexReadiness::Ready { warm_budget } => (QueryGraphMode::Full, warm_budget, ""),
        readiness::IndexReadiness::ReadyVectorOnly { warm_budget } => (
            QueryGraphMode::VectorOnly,
            warm_budget,
            crate::prompts::MCP_GRAPH_PENDING,
        ),
        readiness::IndexReadiness::Timeout => {
            return crate::prompts::MCP_DEGRADE_INDEXING.to_string();
        }
        readiness::IndexReadiness::Failed(error) => {
            let message = format!("{error:#}");
            return crate::prompts::render(
                crate::prompts::MCP_DEGRADE_INDEX_FAILED,
                &[("err", &message)],
            );
        }
    };

    let output = do_query(
        index_engine,
        repo_dbs,
        settings,
        information_request,
        repo,
        query_graph_mode,
        query_warm_wait,
        cross,
        max_tokens,
    )
    .await;
    format!("{output_prefix}{output}")
}

/// Build an augmented query string that prepends structured filter params as inline
/// filter prefixes (e.g. `kind:function lang:rust path:src/ <original query>`).
/// The `run_query` filter parser will strip these back out before embedding.
fn build_augmented_query(
    information_request: &str,
    filter_kind: Option<&[String]>,
    filter_lang: Option<&[String]>,
    filter_path: Option<&str>,
) -> String {
    let mut prefixes = Vec::new();
    if let Some(kinds) = filter_kind {
        for k in kinds {
            prefixes.push(format!("kind:{}", k));
        }
    }
    if let Some(langs) = filter_lang {
        for l in langs {
            prefixes.push(format!("lang:{}", l));
        }
    }
    if let Some(path) = filter_path
        && !path.is_empty()
    {
        prefixes.push(format!("path:{}", path));
    }
    if prefixes.is_empty() {
        information_request.to_string()
    } else {
        format!("{} {}", prefixes.join(" "), information_request)
    }
}

/// Format an enriched caller tag: `[callers: fn_a, fn_b, fn_c +N more]`
/// When callers > 3, shows first 3 names + count of remaining.
/// Returns empty string when no callers.
fn format_enriched_caller_tag(
    count: Option<u32>,
    names: &[String],
    _file_count: Option<u32>,
    inferred: bool,
) -> String {
    let c = match count {
        Some(c) if c > 0 => c,
        _ => return String::new(),
    };
    if names.is_empty() {
        // Fallback to count-only format if names weren't fetched
        return format!(" [callers:{c}]");
    }
    let max_display = 30;
    let display_names: Vec<&str> = names.iter().take(max_display).map(|s| s.as_str()).collect();
    let remaining = c.saturating_sub(display_names.len() as u32);
    let suffix = if inferred { " ~inferred" } else { "" };
    if remaining > 0 {
        format!(
            " [callers: {} +{} more{}]",
            display_names.join(", "),
            remaining,
            suffix
        )
    } else {
        format!(" [callers: {}{}]", display_names.join(", "), suffix)
    }
}

/// Format an enriched callee tag: `[calls: fn_x, fn_y +N more]`
/// Returns empty string when no callees.
fn format_enriched_callee_tag(count: Option<u32>, names: &[String], inferred: bool) -> String {
    let c = match count {
        Some(c) if c > 0 => c,
        _ => return String::new(),
    };
    if names.is_empty() {
        return format!(" [calls:{c}]");
    }
    let max_display = 30;
    let display_names: Vec<&str> = names.iter().take(max_display).map(|s| s.as_str()).collect();
    let remaining = c.saturating_sub(display_names.len() as u32);
    let suffix = if inferred { " ~inferred" } else { "" };
    if remaining > 0 {
        format!(
            " [calls: {} +{} more{}]",
            display_names.join(", "),
            remaining,
            suffix
        )
    } else {
        format!(" [calls: {}{}]", display_names.join(", "), suffix)
    }
}

/// Shared `trace-path` runner (global + per-repo handlers): resolve both
/// symbol references against the repo's symbol table, then depth-bounded DFS
/// over the repo's own `calls` table. Read-only — the DB is opened through the
/// same `get_or_open` path as `file-retrieval`; the tool never registers a
/// repo, spawns a worker, or triggers indexing.
#[allow(clippy::too_many_arguments)]
async fn run_trace_path(
    repo_dbs: &Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    data_dir: &Path,
    settings: &Settings,
    repo: &str,
    from_symbol: &str,
    to_symbol: &str,
    direction: Option<&str>,
    max_depth: Option<usize>,
) -> String {
    let direction = match direction {
        None => crate::query::trace_path::Direction::Callees,
        Some(d) => match crate::query::trace_path::Direction::parse(d) {
            Some(d) => d,
            None => {
                return format!("Error: unknown direction '{d}' (valid: callees, callers).");
            }
        },
    };

    let db =
        match store::get_or_open(repo_dbs, data_dir, repo, settings.repo_generation(repo)).await {
            Ok(d) => d,
            Err(e) => return format!("Error: could not open index database: {e}"),
        };

    if store::read_db_schema_version(&db).await < 2 {
        return "Error: this repo's index predates FQN call edges (schema < 2). \
                Re-index the repo, then retry."
            .to_string();
    }

    let from = match crate::query::trace_path::resolve_unique(&db, from_symbol, "from_symbol").await
    {
        Ok(s) => s,
        Err(e) => return format!("Error: {e}"),
    };
    let to = match crate::query::trace_path::resolve_unique(&db, to_symbol, "to_symbol").await {
        Ok(s) => s,
        Err(e) => return format!("Error: {e}"),
    };

    let depth = max_depth
        .unwrap_or(crate::query::trace_path::DEFAULT_MAX_DEPTH)
        .clamp(1, crate::query::trace_path::MAX_DEPTH_CAP);
    let outcome = crate::query::trace_path::trace_paths(
        &db,
        &from.fqn,
        &from.file,
        &to.fqn,
        direction,
        depth,
        crate::query::trace_path::DEFAULT_MAX_PATHS,
    )
    .await;

    if outcome.paths.is_empty() {
        return format!(
            "No call path found from `{}` to `{}` within depth {depth} (following {}). \
             The symbols may be unrelated, or the connecting edges may live in another \
             repo's database (cross-repo edges are owned by the caller's repo).",
            from.fqn,
            to.fqn,
            direction.label(),
        );
    }

    let mut out = format!(
        "{} call path(s) from `{}` to `{}` (following {}, depth ≤ {depth}):\n",
        outcome.paths.len(),
        from.fqn,
        to.fqn,
        direction.label(),
    );
    for (i, path) in outcome.paths.iter().enumerate() {
        out.push_str(&format!("\n{}. `{}`\n", i + 1, path[0].fqn));
        for node in &path[1..] {
            out.push_str(&format!(
                "   → `{}`{}\n",
                node.fqn,
                if node.inferred { " ~inferred" } else { "" }
            ));
        }
    }
    if outcome.truncated {
        out.push_str("\n(search stopped at the path/budget cap — more paths may exist)\n");
    }
    out
}

/// Shared `symbol-context` runner (global + per-repo handlers): one read-only
/// call that resolves a symbol and returns its definition source (numbered,
/// secret-fenced through `read_lines_from_fs`) together with its call-graph
/// context (caller/callee counts + proximity-sorted names — the same enriched
/// tags retrieval output uses). Opens the DB through the same `get_or_open`
/// path as `file-retrieval`; never registers a repo or triggers indexing.
async fn run_symbol_context(
    repo_dbs: &Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    data_dir: &Path,
    settings: &Settings,
    repo: &str,
    symbol: &str,
) -> String {
    let db =
        match store::get_or_open(repo_dbs, data_dir, repo, settings.repo_generation(repo)).await {
            Ok(d) => d,
            Err(e) => return format!("Error: could not open index database: {e}"),
        };
    let schema_version = store::read_db_schema_version(&db).await;
    let sym = match crate::query::trace_path::resolve_unique(&db, symbol, "symbol").await {
        Ok(s) => s,
        Err(e) => return format!("Error: {e}"),
    };

    let stats =
        crate::query::engine::query_caller_callee_stats(&db, &sym.fqn, &sym.file, schema_version)
            .await;

    // Definition source, numbered + secret-fenced. An unreadable file degrades
    // to metadata-only output (the index row stays authoritative for the
    // range); an oversized definition is truncated with an explicit note.
    const MAX_DEFINITION_LINES: usize = 200;
    const MAX_DEFINITION_CHARS: usize = 16_000;
    let (source, source_note) = match crate::query::engine::read_lines_from_fs(
        &sym.file,
        sym.line_start.max(1) as u32,
        sym.line_end.max(1) as u32,
    ) {
        Ok(text) => {
            let line_count = text.lines().count();
            if line_count > MAX_DEFINITION_LINES || text.len() > MAX_DEFINITION_CHARS {
                let mut cut = text
                    .lines()
                    .take(MAX_DEFINITION_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                if cut.len() > MAX_DEFINITION_CHARS {
                    let mut end = MAX_DEFINITION_CHARS;
                    while !cut.is_char_boundary(end) {
                        end -= 1;
                    }
                    cut.truncate(end);
                }
                (
                    cut,
                    format!(
                        "\n(definition truncated — {line_count} lines indexed; \
                             use codebase-retrieval or file-retrieval for the full range)"
                    ),
                )
            } else {
                (text, String::new())
            }
        }
        Err(_) => (
            String::new(),
            "\n(definition source could not be read from disk — showing index \
                 metadata only)"
                .to_string(),
        ),
    };

    let mut out = format!("`{}`", sym.fqn);
    if sym.kind.as_deref().is_some_and(|k| !k.is_empty()) {
        out.push_str(&format!(" ({})", sym.kind.as_deref().expect("checked")));
    }
    out.push_str(&format!(
        " — {}:{}-{}\n",
        sym.file, sym.line_start, sym.line_end
    ));
    if !source.is_empty() {
        out.push_str(&format!("\n{source}\n"));
    }
    out.push_str(&source_note);
    match stats {
        Some(s) => {
            let caller_tag = format_enriched_caller_tag(
                Some(s.caller_count),
                &s.caller_names,
                Some(s.caller_file_count),
                s.callers_inferred,
            );
            let callee_tag = format_enriched_callee_tag(
                Some(s.callee_count),
                &s.callee_names,
                s.callees_inferred,
            );
            if !caller_tag.is_empty() {
                out.push_str(&format!("\n{}", caller_tag.trim_start()));
            }
            if !callee_tag.is_empty() {
                out.push_str(&format!("\n{}", callee_tag.trim_start()));
            }
        }
        None => out.push_str("\n(no call-graph edges recorded for this symbol)"),
    }
    out
}

/// Shared `impact` runner (global + per-repo handlers): reverse call-graph
/// analysis — every symbol that transitively CALLS the target, BFS-grouped by
/// hop distance. Read-only, repo-local edges only (same scope as trace-path);
/// output adds a most-affected-files summary for triage.
async fn run_impact(
    repo_dbs: &Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    data_dir: &Path,
    settings: &Settings,
    repo: &str,
    symbol: &str,
    max_depth: Option<usize>,
) -> String {
    let db =
        match store::get_or_open(repo_dbs, data_dir, repo, settings.repo_generation(repo)).await {
            Ok(d) => d,
            Err(e) => return format!("Error: could not open index database: {e}"),
        };
    if store::read_db_schema_version(&db).await < 2 {
        return "Error: this repo's index predates FQN call edges (schema < 2). \
                Re-index the repo, then retry."
            .to_string();
    }
    let sym = match crate::query::trace_path::resolve_unique(&db, symbol, "symbol").await {
        Ok(s) => s,
        Err(e) => return format!("Error: {e}"),
    };
    let depth = max_depth
        .unwrap_or(crate::query::trace_path::DEFAULT_IMPACT_DEPTH)
        .clamp(1, crate::query::trace_path::MAX_IMPACT_DEPTH_CAP);
    let outcome = crate::query::trace_path::impacted(
        &db,
        &sym.fqn,
        depth,
        crate::query::trace_path::MAX_IMPACT_NODES,
    )
    .await;

    let total: usize = outcome.levels.iter().map(|l| l.len()).sum();
    if total == 0 {
        return format!(
            "No callers recorded for `{}` within {depth} level(s). The symbol may be \
             unused in this repo, be an entry point, or be called only from another \
             repo (cross-repo edges are owned by the caller's database).",
            sym.fqn
        );
    }

    let kind_suffix = sym
        .kind
        .as_deref()
        .filter(|k| !k.is_empty())
        .map(|k| format!(" ({k})"))
        .unwrap_or_default();
    let mut out = format!(
        "Impact for `{}`{kind_suffix} — {total} caller(s) reached within {depth} level(s):",
        sym.fqn
    );

    const MAX_LISTED_PER_LEVEL: usize = 40;
    for (i, level) in outcome.levels.iter().enumerate() {
        let hop = if i == 0 {
            "direct caller(s)"
        } else {
            "caller(s) through them"
        };
        out.push_str(&format!("\n\nlevel {} — {} {hop}:", i + 1, level.len()));
        for node in level.iter().take(MAX_LISTED_PER_LEVEL) {
            out.push_str(&format!(
                "\n  - `{}`{}",
                node.fqn,
                if node.inferred { " ~inferred" } else { "" }
            ));
        }
        if level.len() > MAX_LISTED_PER_LEVEL {
            out.push_str(&format!(
                "\n  … +{} more at this level",
                level.len() - MAX_LISTED_PER_LEVEL
            ));
        }
    }

    // Most-affected files: where the churn would land.
    let mut per_file: HashMap<&str, usize> = HashMap::new();
    for node in outcome.levels.iter().flatten() {
        *per_file.entry(node.file.as_str()).or_default() += 1;
    }
    let mut files: Vec<(&str, usize)> = per_file.into_iter().collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let summary: Vec<String> = files
        .iter()
        .take(5)
        .map(|(f, n)| format!("{f} ×{n}"))
        .collect();
    out.push_str(&format!("\n\nmost affected files: {}", summary.join(", ")));

    if outcome.truncated {
        out.push_str("\n(node budget reached — the caller graph continues beyond this listing)");
    }
    out
}

/// Returns a string — never panics, never returns Err.
///
/// Note: neither `home_dir` nor `data_dir` is needed here — both DB opens and
/// vector access go through `index_engine` / `repo_dbs`, which were constructed
/// with the boot-resolved `data_dir`. Keeping the signature path-free
/// documents that this function never re-derives a base directory mid-run.
async fn do_query(
    index_engine: &Arc<IndexEngine>,
    repo_dbs: &Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    settings: &Settings,
    information_request: &str,
    repo: &str,
    graph_mode: QueryGraphMode,
    warm_wait: Duration,
    cross: Option<&crate::query::cross_repo::CrossRepoResolver>,
    max_tokens: Option<usize>,
) -> String {
    let voyage_client = match VoyageClient::new_for_provider(
        crate::embedding::voyage::Provider::parse(&settings.embedding.provider),
        settings.embedding.model.clone(),
        settings.embedding.api_keys.clone(),
        settings.embedding.voyage_base_url.as_deref(),
        settings.embedding.dimensions,
    ) {
        Ok(c) => c,
        Err(e) => return format!("Error: failed to create embedding client: {e}"),
    };

    let llm_client: Option<LlmClient> = LlmClient::new(&settings.llm);

    match crate::query::engine::run_query_with_filters_and_mode(
        information_request,
        30,
        Some(repo),
        &voyage_client,
        index_engine,
        repo_dbs,
        settings.llm.rerank_min_prune_lines,
        llm_client.as_ref(),
        warm_wait,
        settings.llm.agentic_rag,
        settings.llm.agentic_rag_max_turns,
        settings.llm.agentic_rag_max_chunk_chars,
        settings.llm.agentic_rag_grep_read,
        None,
        graph_mode,
        cross,
    )
    .await
    {
        Err(e) => format!("Error: query failed: {e}"),
        Ok(result) => {
            // Warming takes precedence over the empty-handling: an empty result with
            // `warming` set means the repo's vector shard was not resident after the
            // bounded warm-wait expired — the index IS complete, it just hasn't loaded
            // into memory yet. Returning "No results found" here would falsely tell the
            // caller the codebase has nothing relevant; instead signal a retry. The
            // decision is a pure function of (warming, empty, rerank_rejected) so it is
            // unit-tested directly (see select_empty_or_warming_message).
            if result.results.is_empty() {
                let rerank_rejected = result.rerank.as_ref().is_some_and(|r| {
                    !r.fallback_used && r.skip_reason.is_none() && !r.raw_response.is_empty()
                });
                return select_empty_or_warming_message(
                    result.warming,
                    rerank_rejected,
                    information_request,
                );
            }
            let blocks: Vec<OutputBlock> = result
                .results
                .iter()
                .map(|r| {
                    let caller_tag = format_enriched_caller_tag(
                        r.callers,
                        &r.caller_names,
                        r.caller_files,
                        r.callers_inferred,
                    );
                    let callee_tag =
                        format_enriched_callee_tag(r.callees, &r.callee_names, r.callees_inferred);
                    OutputBlock {
                        header: format!(
                            "{}#L{}-{}{}{}",
                            r.file, r.line_start, r.line_end, caller_tag, callee_tag
                        ),
                        content: r.content.clone(),
                        file: r.file.clone(),
                        line_start: r.line_start,
                        line_end: r.line_end,
                        callers: r.callers,
                        caller_files: r.caller_files,
                        caller_names: r.caller_names.clone(),
                        callee_names: r.callee_names.clone(),
                        callees: r.callees,
                        callers_inferred: r.callers_inferred,
                        callees_inferred: r.callees_inferred,
                    }
                })
                .collect();
            let blocks = merge_overlapping_blocks(blocks);
            // Sort generated-file blocks after hand-written ones, preserving
            // relative order within each group (stable partition).
            let (hand_written, generated): (Vec<_>, Vec<_>) = blocks
                .into_iter()
                .partition(|b| !crate::parsing::generated::is_generated_file(&b.file));
            let mut blocks = hand_written;
            blocks.extend(generated);
            let assembled = assemble_with_budget(&blocks, max_tokens);
            if result.warming {
                format!("{MCP_PARTIAL_RESULTS_PREFIX}{assembled}")
            } else {
                assembled
            }
        }
    }
}

// ─── File retrieval ───────────────────────────────────────────────────────

/// Build the DB lookup key for a file: join workspace root + relative file_path,
/// normalizing separators to the OS-native convention (the walker stores absolute
/// paths using `Path::to_str()` which produces native separators).
fn build_db_key(workspace: &str, file_path: &str) -> String {
    let workspace = workspace.trim_end_matches(['/', '\\']);
    let file_path = file_path.trim_start_matches(['/', '\\']);
    let file_path_native = if cfg!(windows) {
        file_path.replace('/', "\\")
    } else {
        file_path.replace('\\', "/")
    };
    let repo_path = std::path::Path::new(workspace);
    let abs_file = repo_path.join(&file_path_native);
    abs_file.to_string_lossy().to_string()
}

/// Single-file semantic retrieval: embed query → fetch file chunks from DB →
/// cosine rank in-memory → return top-k snippets.
pub async fn run_file_retrieval(
    data_dir: &Path,
    repo_dbs: &Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    settings: &Settings,
    workspace_full_path: &str,
    file_path: &str,
    information_request: &str,
    top_k: usize,
    max_tokens: Option<usize>,
) -> String {
    let repo = workspace_full_path.trim();
    if repo.is_empty() {
        return "Error: workspace_full_path is required. Call `list_repos` to see the \
                workspaces this engine already knows."
            .to_string();
    }
    let repo = &crate::store::normalize_repo_path(repo);
    let file_path = file_path.trim();
    if file_path.is_empty() {
        return "Error: file_path is required.".to_string();
    }
    if information_request.trim().is_empty() {
        return "Error: information_request is required.".to_string();
    }

    if settings.embedding.api_keys.is_empty() {
        return "Error: no embedding API keys configured.".to_string();
    }

    // Open DB for this repo.
    let db =
        match store::get_or_open(repo_dbs, data_dir, repo, settings.repo_generation(repo)).await {
            Ok(d) => d,
            Err(e) => return format!("Error: could not open index database: {e}"),
        };

    let db_key = build_db_key(repo, file_path);

    // Fetch all chunks for this file (with embeddings).
    let chunks = match chunks_for_file_with_embeddings(&db, &db_key).await {
        Ok(c) => c,
        Err(e) => return format!("Error: failed to fetch chunks: {e}"),
    };

    if chunks.is_empty() {
        return format!("No indexed chunks found for file: {file_path}");
    }

    // Embed the query.
    let voyage_client = match VoyageClient::new_for_provider(
        crate::embedding::voyage::Provider::parse(&settings.embedding.provider),
        settings.embedding.model.clone(),
        settings.embedding.api_keys.clone(),
        settings.embedding.voyage_base_url.as_deref(),
        settings.embedding.dimensions,
    ) {
        Ok(c) => c,
        Err(e) => return format!("Error: failed to create embedding client: {e}"),
    };

    let query_vec = match voyage_client.embed_query(information_request).await {
        Ok(v) => v,
        Err(e) => return format!("Error: embedding failed: {e}"),
    };

    if query_vec.is_empty() {
        return "Error: embedding returned empty vector.".to_string();
    }

    // Cosine score each chunk against the query vector.
    let mut scored: Vec<(f32, &FileChunkRow)> = chunks
        .iter()
        .filter(|c| !c.embedding.is_empty())
        .map(|c| (cosine_similarity(&query_vec, &c.embedding), c))
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Widen candidate pool for the reranker (top_k * 4), then let LLM narrow.
    let candidate_count = (top_k * 4).min(scored.len());
    let candidates = &scored[..candidate_count];

    // Convert to MergeChunk for reranker compatibility.
    let mut merge_chunks: Vec<crate::query::merger::MergeChunk> = candidates
        .iter()
        .map(|(score, c)| crate::query::merger::MergeChunk {
            file: db_key.clone(),
            line_start: c.line_start,
            line_end: c.line_end,
            score: *score,
            content: c.content.clone(),
            symbol: None,
            symbol_fqn: None,
            symbol_kind: None,
        })
        .collect();
    // Stored chunk content is the reranker input and the output fallback —
    // same secret fence as the engine's numbered FS reads.
    for chunk in &mut merge_chunks {
        chunk.content = crate::query::content_fence::redact_secrets(&chunk.content);
    }

    // Read numbered content from disk for accurate reranker input.
    let numbered: Vec<Option<String>> = merge_chunks
        .iter()
        .map(|c| crate::query::engine::read_lines_from_fs(&c.file, c.line_start, c.line_end).ok())
        .collect();

    let caller_stats: Vec<Option<(u32, u32)>> = vec![None; merge_chunks.len()];

    // Rerank via LLM (degrades gracefully to cosine order if no keys).
    let llm_client = LlmClient::new(&settings.llm);
    let rerank_output = crate::query::reranker::rerank(
        information_request,
        &merge_chunks,
        &numbered,
        &caller_stats,
        settings.llm.rerank_min_prune_lines,
        llm_client.as_ref(),
    )
    .await;

    // Cap to requested top_k after reranking.
    let final_count = top_k.min(rerank_output.reranked_indices.len());
    let display_path = &db_key;
    let mut blocks: Vec<OutputBlock> = Vec::new();

    for k in 0..final_count {
        let idx = rerank_output.reranked_indices[k];
        let Some(chunk) = merge_chunks.get(idx) else {
            continue;
        };
        let numbered_text = numbered.get(idx).and_then(|n| n.as_deref());
        let selection = rerank_output
            .line_selections
            .get(k)
            .and_then(|s| s.as_ref());

        match (numbered_text, selection) {
            (Some(text), Some(ranges)) if !ranges.is_empty() => {
                for &(s, e) in ranges {
                    let sliced = crate::query::engine::slice_numbered(text, chunk.line_start, s, e);
                    blocks.push(OutputBlock {
                        header: format!("{}#L{}-{}", display_path, s, e),
                        content: sliced,
                        file: display_path.clone(),
                        line_start: s,
                        line_end: e,
                        callers: None,
                        caller_files: None,
                        ..Default::default()
                    });
                }
            }
            (Some(text), _) => {
                blocks.push(OutputBlock {
                    header: format!("{}#L{}-{}", display_path, chunk.line_start, chunk.line_end),
                    content: text.to_string(),
                    file: display_path.clone(),
                    line_start: chunk.line_start,
                    line_end: chunk.line_end,
                    callers: None,
                    caller_files: None,
                    ..Default::default()
                });
            }
            (None, _) => {
                let fallback = chunk
                    .content
                    .lines()
                    .enumerate()
                    .map(|(i, line)| format!("{}: {}", chunk.line_start + i as u32, line))
                    .collect::<Vec<_>>()
                    .join("\n");
                blocks.push(OutputBlock {
                    header: format!("{}#L{}-{}", display_path, chunk.line_start, chunk.line_end),
                    content: fallback,
                    file: display_path.clone(),
                    line_start: chunk.line_start,
                    line_end: chunk.line_end,
                    callers: None,
                    caller_files: None,
                    ..Default::default()
                });
            }
        }
    }

    if blocks.is_empty() {
        return format!("No relevant chunks found for query in file: {file_path}");
    }

    let blocks = merge_overlapping_blocks(blocks);
    let mut out = assemble_with_budget(&blocks, max_tokens);
    out.push_str(crate::prompts::MCP_FILE_RETRIEVAL_HINT);

    out
}

struct FileChunkRow {
    line_start: u32,
    line_end: u32,
    content: String,
    embedding: Vec<f32>,
}

async fn chunks_for_file_with_embeddings(
    db: &Surreal<Db>,
    file: &str,
) -> anyhow::Result<Vec<FileChunkRow>> {
    #[derive(serde::Deserialize)]
    struct Row {
        line_start: i64,
        line_end: i64,
        content: String,
        #[serde(deserialize_with = "store::ops::de_embedding_dual")]
        embedding: Vec<f32>,
    }
    let rows: Vec<Row> = db
        .query(
            "SELECT line_start, line_end, content, embedding \
             FROM chunk WHERE file = $file ORDER BY line_start",
        )
        .bind(("file", file.to_string()))
        .await?
        .take(0)?;

    Ok(rows
        .into_iter()
        .map(|r| FileChunkRow {
            line_start: r.line_start as u32,
            line_end: r.line_end as u32,
            content: r.content,
            embedding: r.embedding,
        })
        .collect())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}
