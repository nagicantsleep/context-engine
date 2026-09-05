use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod runtime;

/// One-shot subcommands. They run and exit without booting the router/worker.
#[derive(Subcommand, Debug)]
enum Command {
    /// Write MCP config + agent prompt-guidance files into a configured repo
    /// (the CLI twin of the Web UI's "Auto Setup" button).
    Setup {
        /// Repo path already configured in the engine's settings.
        #[arg(long, value_name = "PATH")]
        repo: String,

        /// Comma-separated agent tools to configure: claude, codex, opencode,
        /// or `all` for every supported tool.
        #[arg(long, default_value = "all", value_name = "TOOLS")]
        tool: String,

        /// Router port used to build the MCP URL (must match the running engine).
        #[arg(long, default_value_t = 6699, value_name = "PORT")]
        port: u16,

        /// Router host used to build the MCP URL.
        #[arg(long, default_value = "127.0.0.1", value_name = "HOST")]
        bind: String,

        /// Explicit MCP origin override (e.g. a reverse-proxy URL); wins over
        /// --bind/--port.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
    },
    /// Export a repo's call graph as a portable `graph.json` artifact
    /// (nodes/edges/confidence; format `context-engine-graph/v1`).
    ExportGraph {
        /// Repo path already indexed by the engine.
        #[arg(long, value_name = "PATH")]
        repo: String,

        /// Output file [default: ./graph.json]
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,

        /// Node cap for the artifact (the report says when it is hit).
        #[arg(long, default_value_t = 20_000, value_name = "N")]
        max_nodes: usize,

        /// Edge cap for the artifact (the report says when it is hit).
        #[arg(long, default_value_t = 100_000, value_name = "N")]
        max_edges: usize,

        /// Data-directory base override (CLI > env > settings > builtin).
        #[arg(long, env = "CONTEXT_ENGINE_DATA_DIR", value_name = "PATH")]
        data_dir: Option<PathBuf>,
    },
}

#[derive(Parser, Debug)]
#[command(name = "context-engine", about = "Context Engine settings server")]
struct Cli {
    /// Port to listen on [env: CONTEXT_ENGINE_PORT]
    #[arg(long, env = "CONTEXT_ENGINE_PORT")]
    port: Option<u16>,

    /// Bind address [env: CONTEXT_ENGINE_BIND]
    #[arg(long, env = "CONTEXT_ENGINE_BIND")]
    bind: Option<String>,

    /// Data directory base. RocksDB lives below this directory while settings
    /// remain in the settings home.
    #[arg(long, env = "CONTEXT_ENGINE_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Shared content-addressed embedding-cache root.
    #[arg(long, env = "CONTEXT_ENGINE_EMBEDDINGS_DIR")]
    embeddings_dir: Option<PathBuf>,

    /// Internal settings-home override propagated from router to worker.
    #[arg(long, hide = true)]
    home_dir: Option<PathBuf>,

    /// Run as the process-per-project worker for this repository.
    #[arg(long, value_name = "REPO")]
    worker: Option<String>,

    /// Worker idle window before scale-to-zero. Ignored in router mode.
    #[arg(long, env = "CONTEXT_ENGINE_WORKER_IDLE_SECS")]
    worker_idle_secs: Option<u64>,

    /// Base URL of the owning router (set by the router at spawn). Enables
    /// cross-repo BFS expansion via `/api/cross-repo/chunk` callbacks.
    #[arg(long, hide = true, env = "CONTEXT_ENGINE_ROUTER_URL")]
    router_url: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // One-shot subcommands never boot the engine, so they also skip tracing:
    // their stdout is the report and errors go to stderr.
    match cli.command {
        Some(Command::Setup {
            repo,
            tool,
            port,
            bind,
            url,
        }) => {
            let code = runtime::setup::run(&runtime::setup::SetupArgs {
                repo,
                tool,
                port,
                bind,
                url,
            });
            std::process::exit(code);
        }
        Some(Command::ExportGraph {
            repo,
            out,
            max_nodes,
            max_edges,
            data_dir,
        }) => {
            let code = runtime::export_graph::run(&runtime::export_graph::ExportGraphArgs {
                repo,
                out,
                max_nodes,
                max_edges,
                data_dir,
            })
            .await;
            std::process::exit(code);
        }
        None => {}
    }

    init_tracing(cli.worker.is_some());

    let bind = cli.bind.as_deref().unwrap_or("127.0.0.1").to_owned();
    match cli.worker.clone() {
        Some(repo) => runtime::worker::run(&cli, &bind, repo).await,
        None => runtime::router::run(&cli, &bind).await,
    }
}

fn init_tracing(worker_mode: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("context_engine_rs=info,warn"));
    if worker_mode {
        // Worker stdout is reserved for the readiness handshake.
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
