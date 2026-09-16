//! Retrieval eval set for the chunking-quality benchmark.
//!
//! Two sources, tried in order:
//!
//! 1. `eval_set()` — ~40 curated (query, expected_file, expected_symbol)
//!    triples derived from the notepad-ade source (the harness's original
//!    target). The expected line range is NOT hardcoded — it is resolved at
//!    run time from the symbol name via the FROZEN extraction (`parse_file`),
//!    so the eval set is independent of any chunk boundary and cannot favour
//!    either chunker. Each query is phrased as a natural-language intent a
//!    developer would type, not as the literal symbol name.
//! 2. `derive_eval_set(repo)` — automatic fallback for ANY OTHER repo: walks
//!    the repo with the same walker the chunk metrics use, picks Function and
//!    Method symbols (the same kinds the cut-through metric reasons about),
//!    and phrases each as a keyword query from the symbol name plus enclosing
//!    scope. Ground truth resolves through the same frozen `parse_file` path,
//!    so the chunker-independence guarantee holds for derived pairs too.
//!
//! `expected_file` is relative to the repo root and uses forward slashes.

use std::collections::HashSet;

use context_engine_rs::indexing::walker::walk_repo;
use context_engine_rs::parsing::parse_file;
use context_engine_rs::parsing::symbols::SymbolKind;

/// A (query, relative_file, symbol_name) eval triple.
pub struct EvalPair {
    pub query: String,
    pub rel_file: String,
    pub symbol: String,
}

/// Build the eval set for `repo`: the curated notepad-ade set when the repo
/// actually contains those files, otherwise a derived set from the repo's own
/// Function/Method symbols. `cap` bounds the derived set size (0 = default 40).
pub fn build_eval_set(repo: &str, cap: usize) -> Vec<EvalPair> {
    let curated_ok = !eval_set().is_empty()
        && std::fs::exists(repo.trim_end_matches(['/', '\\']).to_string() + "/" + eval_set()[0].1)
            .map(|ok| ok)
            .unwrap_or(false);
    if curated_ok {
        return eval_set()
            .into_iter()
            .map(|(q, f, s)| EvalPair {
                query: q.to_string(),
                rel_file: f.to_string(),
                symbol: s.to_string(),
            })
            .collect();
    }
    let cap = if cap == 0 { 40 } else { cap };
    derive_eval_set(repo, cap)
}

/// Derive (query, relative_file, symbol_name) triples from the repo's own
/// Function/Method symbols. Query = scope names + snake/camel-split symbol
/// name (deterministic, no thesaurus), which is exactly the kind of
/// identifier-anchored intent the curated set models. `cap` bounds the count.
pub fn derive_eval_set(repo: &str, cap: usize) -> Vec<EvalPair> {
    let root = repo.trim_end_matches(['/', '\\']).to_string();
    let files = walk_repo(&root);
    let mut pairs: Vec<EvalPair> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for path in &files {
        if pairs.len() >= cap {
            break;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        if source.contains('\0') {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .trim_start_matches('/')
            .replace('\\', "/")
            .to_string();
        let parsed = parse_file(path, &source);
        for s in &parsed.symbols {
            if !matches!(s.kind, SymbolKind::Function | SymbolKind::Method) {
                continue;
            }
            if !seen.insert(s.qualified.name.clone()) {
                continue;
            }
            let mut words: Vec<String> = s
                .qualified
                .scope_path
                .iter()
                .map(|w| split_identifier(w))
                .collect();
            words.push(split_identifier(&s.qualified.name));
            let query = words.join(" ");
            pairs.push(EvalPair {
                query,
                rel_file: rel.clone(),
                symbol: s.qualified.name.clone(),
            });
            if pairs.len() >= cap {
                break;
            }
        }
    }
    pairs
}

/// Split an identifier into lowercase words at camelCase humps and around
/// underscores (e.g. `nextFireTimes` → `next fire times`).
fn split_identifier(name: &str) -> String {
    let mut spaced = String::with_capacity(name.len() + 8);
    let mut prev: Option<char> = None;
    for c in name.chars() {
        if let Some(p) = prev
            && (p.is_lowercase() || p.is_ascii_digit())
            && c.is_uppercase()
        {
            spaced.push(' ');
        }
        spaced.push(c);
        prev = Some(c);
    }
    spaced
        .split(['_', '-', '.'])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns the curated (query, relative_file, symbol_name) eval triples.
pub fn eval_set() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // CronExpression.cpp
        (
            "parse a cron expression string into a schedule",
            "NotepadADE/src/CronExpression.cpp",
            "parse",
        ),
        (
            "compute the next time a cron schedule fires",
            "NotepadADE/src/CronExpression.cpp",
            "nextFireTime",
        ),
        (
            "list the upcoming fire times for a cron schedule",
            "NotepadADE/src/CronExpression.cpp",
            "nextFireTimes",
        ),
        // ai/SsePartialParser.cpp
        (
            "extract the streamed token text from an SSE json chunk",
            "NotepadADE/src/ai/SsePartialParser.cpp",
            "extractToken",
        ),
        (
            "detect the finish/stop signal in a streaming response",
            "NotepadADE/src/ai/SsePartialParser.cpp",
            "hasFinishSignal",
        ),
        (
            "feed new bytes into the server-sent-events parser",
            "NotepadADE/src/ai/SsePartialParser.cpp",
            "feed",
        ),
        (
            "parse a single SSE event payload",
            "NotepadADE/src/ai/SsePartialParser.cpp",
            "processEvent",
        ),
        // ai/DiffCompressor.cpp
        (
            "compress a unified diff to fit a byte budget",
            "NotepadADE/src/ai/DiffCompressor.cpp",
            "compress",
        ),
        (
            "parse a diff into per-file groups",
            "NotepadADE/src/ai/DiffCompressor.cpp",
            "parseGroups",
        ),
        (
            "serialize file groups back into diff text",
            "NotepadADE/src/ai/DiffCompressor.cpp",
            "serializeGroups",
        ),
        // AcpErrorClassifier.cpp
        (
            "classify an agent error message into a kind",
            "NotepadADE/src/AcpErrorClassifier.cpp",
            "classify",
        ),
        (
            "produce a user-friendly error message from an error kind",
            "NotepadADE/src/AcpErrorClassifier.cpp",
            "friendlyMessage",
        ),
        (
            "build a login hint for a failed agent command",
            "NotepadADE/src/AcpErrorClassifier.cpp",
            "loginHint",
        ),
        // ai/PromptAssembler.cpp
        (
            "assemble the final prompt from template and blocks",
            "NotepadADE/src/ai/PromptAssembler.cpp",
            "assemble",
        ),
        (
            "render the rules block of a prompt",
            "NotepadADE/src/ai/PromptAssembler.cpp",
            "renderRulesBlock",
        ),
        (
            "render the diff section of a commit prompt",
            "NotepadADE/src/ai/PromptAssembler.cpp",
            "renderDiffBlock",
        ),
        (
            "provide the default prompt template",
            "NotepadADE/src/ai/PromptAssembler.cpp",
            "defaultTemplate",
        ),
        // ai/LlmHttpClient.cpp
        (
            "build the JSON request payload for the LLM call",
            "NotepadADE/src/ai/LlmHttpClient.cpp",
            "buildPayload",
        ),
        (
            "open a streaming HTTP connection to the LLM",
            "NotepadADE/src/ai/LlmHttpClient.cpp",
            "openStream",
        ),
        (
            "normalize the chat completions endpoint URL",
            "NotepadADE/src/ai/LlmHttpClient.cpp",
            "normalizeChatCompletionsUrl",
        ),
        (
            "cancel the in-flight LLM request",
            "NotepadADE/src/ai/LlmHttpClient.cpp",
            "cancel",
        ),
        (
            "handle incoming bytes as they arrive on the stream",
            "NotepadADE/src/ai/LlmHttpClient.cpp",
            "onReadyRead",
        ),
        // ai/RulesLocator.cpp
        (
            "locate the rules file for a workspace",
            "NotepadADE/src/ai/RulesLocator.cpp",
            "locate",
        ),
        (
            "truncate rules content to a byte budget",
            "NotepadADE/src/ai/RulesLocator.cpp",
            "truncateToBudget",
        ),
        (
            "read a file only if it exists",
            "NotepadADE/src/ai/RulesLocator.cpp",
            "readIfExists",
        ),
        // ai/CommitMessageGenerator.cpp
        (
            "trigger generation of a commit message",
            "NotepadADE/src/ai/CommitMessageGenerator.cpp",
            "trigger",
        ),
        (
            "check whether commit message generation can fire",
            "NotepadADE/src/ai/CommitMessageGenerator.cpp",
            "canFireGenerate",
        ),
        // ai/PromptImprover.cpp
        (
            "check whether the prompt can be improved",
            "NotepadADE/src/ai/PromptImprover.cpp",
            "canImprove",
        ),
        (
            "trigger prompt improvement from a user draft",
            "NotepadADE/src/ai/PromptImprover.cpp",
            "trigger",
        ),
        // AcpProtocol.cpp
        (
            "extract complete framed messages from a byte buffer",
            "NotepadADE/src/AcpProtocol.cpp",
            "acpExtractFrames",
        ),
        (
            "pick the auto-approve permission option",
            "NotepadADE/src/AcpProtocol.cpp",
            "pickAutoApproveOptionId",
        ),
        (
            "check if a path is inside the working directory",
            "NotepadADE/src/AcpProtocol.cpp",
            "pathIsInsideWorkingDir",
        ),
        (
            "quote an argument for a windows command line",
            "NotepadADE/src/AcpProtocol.cpp",
            "windowsCommandLineQuote",
        ),
        // AcpAgentRegistry.cpp
        (
            "load the agent registry from disk",
            "NotepadADE/src/AcpAgentRegistry.cpp",
            "load",
        ),
        (
            "persist user-defined agents",
            "NotepadADE/src/AcpAgentRegistry.cpp",
            "persistUserAgents",
        ),
        (
            "the built-in claude code agent definition",
            "NotepadADE/src/AcpAgentRegistry.cpp",
            "builtinClaudeCodeDefinition",
        ),
        // AcpConnection.cpp
        (
            "spawn an agent process for a connection",
            "NotepadADE/src/AcpConnection.cpp",
            "spawn",
        ),
        (
            "set the auto-approve policy provider callback",
            "NotepadADE/src/AcpConnection.cpp",
            "setAutoApprovePolicyProvider",
        ),
        // AcpHistoryStore.cpp
        (
            "compute the file path for a session's history",
            "NotepadADE/src/AcpHistoryStore.cpp",
            "filePathForSession",
        ),
        (
            "ensure a debounce timer exists for a session",
            "NotepadADE/src/AcpHistoryStore.cpp",
            "ensureTimer",
        ),
        // AcpAgentManager.cpp
        (
            "open an agent dock for an agent id",
            "NotepadADE/src/AcpAgentManager.cpp",
            "openAgent",
        ),
        (
            "delete a session's stored history",
            "NotepadADE/src/AcpAgentManager.cpp",
            "deleteSessionHistory",
        ),
        (
            "restart an agent session",
            "NotepadADE/src/AcpAgentManager.cpp",
            "restartSession",
        ),
    ]
}
