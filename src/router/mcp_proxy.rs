//! Router-side GLOBAL `/mcp` handler: a PROXYING MCP server.
//!
//! ## Why this exists (the repo-addressing problem)
//!
//! The global `/mcp` endpoint exposes the retrieval and graph tools, but each
//! tool call carries its OWN `workspace_full_path` — so a single `/mcp` session
//! is NOT bound to one repo. In the monolith the handler held the `IndexEngine`
//! + `repo_dbs` and ran the tool directly. The router holds NEITHER (that is
//! the whole point of process-per-project). So the router's global `/mcp`
//! cannot run a repo-backed tool itself.
//!
//! The correct, complete behavior — not a "use /mcp-repo instead" punt — is to
//! PROXY each repo-backed tool call to the worker that owns the call's
//! `workspace_full_path`: resolve the repo, acquire/spawn its worker via
//! [`ProxyCtx`], and forward the call to that worker's `/api/mcp-tool*` REST
//! endpoint. Each REST endpoint runs the SAME funnel the MCP tool would, so
//! the output is byte-identical to the monolith — the client cannot tell it
//! was proxied.
//!
//! `list_repos` is the one exception: it needs no repo DB (read-only settings
//! + sidecars), so the router serves it directly instead of spawning a worker.
//!
//! Per-repo `/mcp-repo/:id` (pre-bound workspace) is proxied separately as a raw
//! HTTP passthrough in `router::mod`; this module is ONLY the global, multi-repo
//! `/mcp` surface.

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, GetPromptRequestParams, GetPromptResult, ListPromptsResult,
        ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult,
        ServerCapabilities, ServerInfo,
    },
    schemars,
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
};
use serde_json::json;
use std::path::PathBuf;

use super::proxy::{ProxyCtx, forward_json_to_worker};
use crate::store::normalize_repo_path;

/// Args for the proxied global `codebase-retrieval` tool. Mirrors
/// `mcp::CodebaseRetrievalArgs` (the fields the funnel reads): the free-form
/// request plus the workspace path that selects the repo/worker.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProxyCodebaseRetrievalArgs {
    /// Natural-language description of the code or information you are looking for.
    pub information_request: String,
    /// Full path to the workspace/repository to search. Selects which worker
    /// handles the call.
    pub workspace_full_path: String,
}

/// Args for the proxied global `file-retrieval` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProxyFileRetrievalArgs {
    /// Full path to the workspace/repository. Selects which worker handles the call.
    pub workspace_full_path: String,
    /// Relative path to the file within the repository (e.g. "src/main.rs").
    pub file_path: String,
    /// Natural-language description of what you're looking for in this file.
    pub information_request: String,
    /// Number of top-scoring snippets to return. Defaults to 5.
    pub top_k: Option<usize>,
}

/// Args for the proxied global `trace-path` tool. Mirrors `mcp::TracePathArgs`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProxyTracePathArgs {
    /// Full path to the workspace/repository. Selects which worker handles the call.
    pub workspace_full_path: String,
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

/// Args for the proxied global `symbol-context` tool. Mirrors
/// `mcp::SymbolContextArgs`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProxySymbolContextArgs {
    /// Full path to the workspace/repository. Selects which worker handles the call.
    pub workspace_full_path: String,
    /// Symbol to describe: full FQN (`/abs/file.rs::mod::name`), `file.rs::name`,
    /// `::name`, or a bare symbol name. Ambiguous references return the
    /// candidate list instead of guessing.
    pub symbol: String,
}

/// Args for the proxied global `impact` tool. Mirrors `mcp::ImpactArgs`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProxyImpactArgs {
    /// Full path to the workspace/repository. Selects which worker handles the call.
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

/// Proxying global MCP handler. Holds the [`ProxyCtx`] so each tool call can
/// acquire/spawn the right worker and forward to it, plus the directories the
/// router-side `list_repos` reads (settings home + sidecar data dir).
#[derive(Clone)]
pub struct ProxyMcpHandler {
    proxy: ProxyCtx,
    home_dir: PathBuf,
    data_dir: PathBuf,
    #[allow(dead_code)]
    tool_router: ToolRouter<ProxyMcpHandler>,
}

/// Gated tool names on the global `/mcp` surface. `list_repos` is deliberately
/// absent: it is always exposed (see `list_repos`) — hiding discovery behind
/// the same opt-in list as the retrieval tools would make default settings
/// undiscoverable.
const GATED_TOOLS: &[&str] = &[
    "codebase-retrieval",
    "file-retrieval",
    "trace-path",
    "symbol-context",
    "impact",
    "changes-impact",
];

fn apply_tool_gate(router: &mut ToolRouter<ProxyMcpHandler>, enabled_tools: &[String]) {
    for &name in GATED_TOOLS {
        if !enabled_tools.iter().any(|e| e == name) {
            router.disable_route(name);
        }
    }
}

#[tool_router]
impl ProxyMcpHandler {
    pub fn new(
        proxy: ProxyCtx,
        home_dir: PathBuf,
        data_dir: PathBuf,
        enabled_tools: &[String],
    ) -> Self {
        let mut router = Self::tool_router();
        apply_tool_gate(&mut router, enabled_tools);
        Self {
            proxy,
            home_dir,
            data_dir,
            tool_router: router,
        }
    }

    #[doc = include_str!("../prompts/mcp_codebase_retrieval.txt")]
    #[tool(name = "codebase-retrieval")]
    async fn codebase_retrieval(
        &self,
        Parameters(args): Parameters<ProxyCodebaseRetrievalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = normalize_repo_path(args.workspace_full_path.trim());
        if repo.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Error: workspace_full_path is required.".to_string(),
            )]));
        }
        // Forward to the worker's /api/mcp-tool — the SAME funnel run_codebase_
        // retrieval the monolith MCP tool uses, so output is byte-identical.
        let body = json!({
            "information_request": args.information_request,
            "workspace_full_path": args.workspace_full_path,
        });
        let text = forward_json_to_worker(&self.proxy, &repo, "/api/mcp-tool", body)
            .await
            .unwrap_or_else(|e| format!("Error: {e}"));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[doc = include_str!("../prompts/mcp_file_retrieval.txt")]
    #[tool(name = "file-retrieval")]
    async fn file_retrieval(
        &self,
        Parameters(args): Parameters<ProxyFileRetrievalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = normalize_repo_path(args.workspace_full_path.trim());
        if repo.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Error: workspace_full_path is required.".to_string(),
            )]));
        }
        let body = json!({
            "workspace_full_path": args.workspace_full_path,
            "file_path": args.file_path,
            "information_request": args.information_request,
            "top_k": args.top_k,
        });
        let text = forward_json_to_worker(&self.proxy, &repo, "/api/mcp-tool/file-retrieval", body)
            .await
            .unwrap_or_else(|e| format!("Error: {e}"));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Discover the repositories this engine can query (read-only).
    ///
    /// Served DIRECTLY by the router (no worker spawn): it reads only the live
    /// settings and durable per-repo sidecars. Lists every configured repo path
    /// with its index state and the sanitized per-repo endpoint name. Use a
    /// returned path as `workspace_full_path` on the retrieval tools.
    #[tool(name = "list_repos", annotations(read_only_hint = true))]
    async fn list_repos(&self) -> Result<CallToolResult, ErrorData> {
        let settings = match crate::config::ensure_dir_and_load(&self.home_dir) {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error: could not read settings: {e}"
                ))]));
            }
        };
        let text = crate::mcp::run_list_repos(&settings, &self.data_dir, None);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Trace a call path between two symbols (read-only).
    ///
    /// Resolves both references against the repo's symbol table and walks the
    /// repo's `calls` graph depth-bounded in the requested `direction`.
    /// Repo-local edges only; `~inferred` marks heuristic edges. Forwarded to
    /// the repo's worker.
    #[tool(name = "trace-path", annotations(read_only_hint = true))]
    async fn trace_path(
        &self,
        Parameters(args): Parameters<ProxyTracePathArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = normalize_repo_path(args.workspace_full_path.trim());
        if repo.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Error: workspace_full_path is required.".to_string(),
            )]));
        }
        let body = json!({
            "workspace_full_path": args.workspace_full_path,
            "from_symbol": args.from_symbol,
            "to_symbol": args.to_symbol,
            "direction": args.direction,
            "max_depth": args.max_depth,
        });
        let text = forward_json_to_worker(&self.proxy, &repo, "/api/mcp-tool/trace-path", body)
            .await
            .unwrap_or_else(|e| format!("Error: {e}"));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Show one symbol's definition source plus its call-graph context
    /// (read-only). Forwarded to the repo's worker.
    #[tool(name = "symbol-context", annotations(read_only_hint = true))]
    async fn symbol_context(
        &self,
        Parameters(args): Parameters<ProxySymbolContextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = normalize_repo_path(args.workspace_full_path.trim());
        if repo.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Error: workspace_full_path is required.".to_string(),
            )]));
        }
        let body = json!({
            "workspace_full_path": args.workspace_full_path,
            "symbol": args.symbol,
        });
        let text = forward_json_to_worker(&self.proxy, &repo, "/api/mcp-tool/symbol-context", body)
            .await
            .unwrap_or_else(|e| format!("Error: {e}"));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Reverse call-graph impact analysis for one symbol: everything that
    /// transitively calls it, grouped by caller distance, with a
    /// most-affected-files summary (read-only). Forwarded to the repo's worker.
    #[tool(name = "impact", annotations(read_only_hint = true))]
    async fn impact(
        &self,
        Parameters(args): Parameters<ProxyImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = normalize_repo_path(args.workspace_full_path.trim());
        if repo.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Error: workspace_full_path is required.".to_string(),
            )]));
        }
        let body = json!({
            "workspace_full_path": args.workspace_full_path,
            "symbol": args.symbol,
            "max_depth": args.max_depth,
        });
        let text = forward_json_to_worker(&self.proxy, &repo, "/api/mcp-tool/impact", body)
            .await
            .unwrap_or_else(|e| format!("Error: {e}"));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Map a unified diff's added lines onto indexed symbols and report the
    /// affected callers (read-only). Forwarded to the repo's worker: the git
    /// fallback runs INSIDE the worker so it sees the same checkout the index
    /// was built from.
    #[tool(name = "changes-impact", annotations(read_only_hint = true))]
    async fn changes_impact(
        &self,
        Parameters(args): Parameters<ProxyChangesImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let repo = normalize_repo_path(args.workspace_full_path.trim());
        if repo.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Error: workspace_full_path is required.".to_string(),
            )]));
        }
        let body = json!({
            "workspace_full_path": args.workspace_full_path,
            "git_diff": args.git_diff,
            "max_depth": args.max_depth,
        });
        let text = forward_json_to_worker(&self.proxy, &repo, "/api/mcp-tool/changes-impact", body)
            .await
            .unwrap_or_else(|e| format!("Error: {e}"));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

/// Args for the proxied global `changes-impact` tool. Mirrors `mcp::ChangesImpactArgs`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProxyChangesImpactArgs {
    /// Full path to the workspace/repository. Selects which worker handles the call.
    pub workspace_full_path: String,
    /// Unified diff to analyze. When omitted, the worker runs `git diff HEAD`
    /// inside the repo itself.
    #[serde(default)]
    pub git_diff: Option<String>,
    /// How many caller levels to walk beyond each changed symbol. Defaults to
    /// 2, capped at 5.
    #[serde(default)]
    pub max_depth: Option<usize>,
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ProxyMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "context-engine-rs",
            env!("CARGO_PKG_VERSION"),
        ))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        prompt_get(&request.name)
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        prompt_list()
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        resource_list()
    }

    /// Served DIRECTLY by the router (no worker spawn): same sidecar-backed
    /// listing as `list_repos`.
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let settings = match crate::config::ensure_dir_and_load(&self.home_dir) {
            Ok(s) => s,
            Err(e) => {
                return Err(ErrorData::internal_error(
                    format!("could not read settings: {e}"),
                    None,
                ));
            }
        };
        resource_read(&request.uri, &settings, &self.data_dir, None).await
    }
}

// Same guided workflows and discovery resource as the monolith handlers —
// shared via `crate::mcp` helpers.
fn prompt_list() -> Result<ListPromptsResult, ErrorData> {
    crate::mcp::proxy_prompt_list()
}

fn prompt_get(name: &str) -> Result<GetPromptResult, ErrorData> {
    crate::mcp::proxy_prompt_get(name)
}

fn resource_list() -> Result<ListResourcesResult, ErrorData> {
    crate::mcp::proxy_resource_list()
}

async fn resource_read(
    uri: &str,
    settings: &crate::config::Settings,
    data_dir: &std::path::Path,
    this_repo: Option<&str>,
) -> Result<ReadResourceResult, ErrorData> {
    crate::mcp::proxy_resource_read(uri, settings, data_dir, this_repo).await
}

/// Extract the `result` string the worker's `/api/mcp-tool` REST handlers wrap
/// their funnel output in (`{"result": "..."}`). Falls back to the raw body for
/// any other shape so an error surface is never swallowed.
pub fn unwrap_mcp_tool_result(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default settings enable `codebase-retrieval` + the three read-only
    /// graph tools; the gate must hide `file-retrieval` while `list_repos`
    /// stays visible.
    #[test]
    fn gate_hides_disabled_tools_but_never_list_repos() {
        let mut router = ProxyMcpHandler::tool_router();
        apply_tool_gate(
            &mut router,
            &[
                "codebase-retrieval",
                "trace-path",
                "symbol-context",
                "impact",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        );
        assert!(!router.is_disabled("codebase-retrieval"));
        assert!(router.is_disabled("file-retrieval"));
        assert!(!router.is_disabled("trace-path"));
        assert!(!router.is_disabled("symbol-context"));
        assert!(!router.is_disabled("impact"));
        // list_repos is not in the gate list, so it was never disabled.
        assert!(!router.is_disabled("list_repos"));
    }

    /// Enabling every tool in settings leaves the whole surface visible.
    #[test]
    fn gate_keeps_enabled_tools_visible() {
        let mut router = ProxyMcpHandler::tool_router();
        apply_tool_gate(
            &mut router,
            &[
                "codebase-retrieval",
                "file-retrieval",
                "trace-path",
                "symbol-context",
                "impact",
                "changes-impact",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        );
        for &name in GATED_TOOLS {
            assert!(!router.is_disabled(name), "{name} should be enabled");
        }
        assert!(!router.is_disabled("list_repos"));
    }

    /// The `{"result": ...}` envelope unwrap keeps plain error bodies intact.
    #[test]
    fn unwrap_keeps_non_envelope_bodies() {
        assert_eq!(unwrap_mcp_tool_result("{\"result\":\"ok\"}"), "ok");
        assert_eq!(unwrap_mcp_tool_result("plain error"), "plain error");
    }
}
