# Harness Framework in Context Engine

## What Harness Provides

Harness is a Git-native agent orchestration framework that provides:

- **Agent orchestration**: delegate work to specialized agents (explorer, planner, executor, reviewer) with automatic context management
- **Skill system**: reusable, composable agent capabilities invoked via `$skill-name` syntax
- **Permission model**: user-controlled execution gates for file writes, git operations, and bash commands
- **Durable planning**: Git-tracked execution plans in `docs/plans/active/` for multi-session work
- **Decision log**: architectural and product decisions in `docs/decisions/` with template-driven capture

## Why Context Engine Uses Harness

Context Engine is a code-knowledge graph that indexes repositories to answer "what calls this function?" and "where does this data flow?" The harness framework provides:

1. **Multi-agent indexing**: explorer agents find source files, executor agents parse syntax trees, reviewer agents validate taint-edge detection
2. **Durable execution**: indexing large repos (Linux kernel: 40k+ files) spans multiple sessions; plans track progress and resume points
3. **Skill composition**: `$onboard-repository` skill coordinates reading README, inferring structure, and proposing setup improvements
4. **Safe mutations**: permission gates prevent accidental DB corruption or git history rewrites during development

## Platform Setup

Choose your platform:

- [Windows setup](setup-windows.md) — native Windows with Git Bash
- [WSL setup](setup-wsl.md) — Windows Subsystem for Linux
- [Linux setup](setup-linux.md) — native Linux
- [macOS setup](setup-macos.md) — macOS with Homebrew

## Quick Start

After platform setup:

```bash
# Build the context engine
cargo build --release

# Index a repository
./target/release/context-engine-rs index /path/to/repo

# Query the graph (example: find all function calls)
# Connect to SurrealDB at ~/.context-engine/data/<repo-hash>/db
surreal sql --conn rocksdb:~/.context-engine/data/<repo-hash>/db --ns context --db main
```

Example query:

```sql
-- Find all calls to a specific function
SELECT * FROM calls WHERE in_name = 'process_request';

-- Filter by flow type (data flow vs control flow)
SELECT * FROM calls WHERE flow_type = 'DataFlowsTo';
```

## Project Structure

```
context-engine-rs/
├── src/
│   ├── store/           # SurrealDB + RocksDB backend
│   ├── parsing/         # Tree-sitter parsers for 20+ languages
│   ├── indexing/        # Graph construction and incremental updates
│   └── api/             # HTTP API for agent queries
├── docs/
│   ├── harness/         # This directory
│   ├── plans/           # Durable execution plans
│   └── decisions/       # Architectural decisions
└── .harness-core/       # Harness framework (do not edit)
```

## Dependencies

- **Rust**: 1.70+ (edition 2024 features)
- **SurrealDB 2.x**: embedded graph database with RocksDB backend
- **RocksDB**: persistent key-value store (via surrealdb dependency)
- **Tree-sitter 0.25**: incremental parsing for 20+ languages

## Common Workflows

See [usage.md](usage.md) for detailed examples:

- Indexing a repository
- Querying the call graph
- Multi-repo namespacing
- CI integration for impact analysis

## Troubleshooting

See [troubleshooting.md](troubleshooting.md) for common issues:

- Database lock timeouts (antivirus interference, ulimit)
- High memory usage (SurrealDB cache tuning)
- Slow queries (missing indexes)
- Symbol resolution failures (incomplete indexing)
