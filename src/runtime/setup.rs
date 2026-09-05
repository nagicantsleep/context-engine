//! One-shot `context-engine setup` subcommand: wire one configured repo up for
//! external coding agents (Claude Code / Codex / Opencode) by writing their MCP
//! config + prompt-guidance files directly into the repo on disk.
//!
//! This is the CLI twin of the Web UI's "Auto Setup" button (`post_mcp_setup`
//! in server.rs): the same `mcp_setup::run_setup` writer, the same validation
//! (the repo must already be configured in settings — never an arbitrary path),
//! the same `enabled_mcp_tools` source so prompt guidance never advertises a
//! disabled tool. It never boots the router or touches RocksDB; `settings.json`
//! (fixed at the settings home, like every other reader) is the only runtime
//! state read, and only files inside the repo root are written.

use std::path::PathBuf;

use context_engine_rs::config;
use context_engine_rs::mcp_setup::{self, FileStatus};
use context_engine_rs::store;

/// Arguments for one `setup` invocation (parsed from the CLI subcommand).
pub struct SetupArgs {
    pub repo: String,
    pub tool: String,
    pub port: u16,
    pub bind: String,
    pub url: Option<String>,
}

/// Run setup to completion and return the process exit code.
///
/// 0 = every file written or already up to date; 1 = the repo is not
/// configured, or at least one file errored (failing files are left untouched,
/// per `mcp_setup`'s never-destructive policy); 2 = usage/environment error.
pub fn run(args: &SetupArgs) -> i32 {
    let targets = match mcp_setup::parse_tool_list(&args.tool) {
        Ok(t) => t,
        Err(e) => exit_with_error(&e, 2),
    };

    // Settings home is fixed — the same policy as the router and MCP handler.
    let home_dir = match dirs::home_dir() {
        Some(h) => h,
        None => exit_with_error(
            "could not determine user home directory; set HOME (Unix) or USERPROFILE (Windows)",
            2,
        ),
    };
    let settings = match config::ensure_dir_and_load(&home_dir) {
        Ok(s) => s,
        Err(e) => exit_with_error(
            &format!("could not load settings from {}: {e}", home_dir.display()),
            2,
        ),
    };

    // Compare through the same normalization the repo-id round trip applies, so
    // a trailing slash or separator difference doesn't read as "not configured".
    let repo = store::normalize_repo_path(&args.repo);
    if !settings.repos.contains(&repo) {
        eprintln!("error: repo is not configured in the engine: {repo}");
        eprintln!("add it first (Web UI settings, or PUT /api/config), then re-run setup.");
        eprintln!("configured repos:");
        for r in settings.repos.iter().take(10) {
            eprintln!("  {r}");
        }
        if settings.repos.len() > 10 {
            eprintln!("  … ({} total)", settings.repos.len());
        }
        return 1;
    }

    // Mirror the UI's URL policy: any scheme/host is valid (reverse proxies are
    // a supported setup), but the path is always this repo's own MCP endpoint.
    let origin = args
        .url
        .clone()
        .unwrap_or_else(|| format!("http://{}:{}", args.bind, args.port));
    let endpoint_url = mcp_setup::build_endpoint_url(&origin, &repo);

    println!("context-engine setup");
    println!("  repo: {repo}");
    println!("  mcp:  {endpoint_url}");

    let repo_root = PathBuf::from(&repo);
    let mut any_error = false;
    for target in targets {
        println!("  {}:", target.name());
        let actions = mcp_setup::run_setup(
            &repo_root,
            target,
            &endpoint_url,
            &settings.enabled_mcp_tools,
        );
        for action in actions {
            if action.status == FileStatus::Error {
                any_error = true;
                let detail = action.detail.as_deref().unwrap_or("unknown error");
                println!("    [error]    {} — {detail}", action.file);
            } else {
                println!("    [{:>9}] {}", action.status.to_string(), action.file);
            }
        }
    }

    if any_error {
        eprintln!("setup finished with errors — the failing files were left untouched");
        1
    } else {
        println!(
            "done. start the engine (`npx context-engine@latest`), then use the MCP tools from your agent."
        );
        0
    }
}

fn exit_with_error(message: &str, code: i32) -> ! {
    eprintln!("error: {message}");
    std::process::exit(code)
}
