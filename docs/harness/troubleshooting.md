# Troubleshooting

## Database Lock Timeout

### Symptoms

```
Error: Database lock timeout after 30s
Operation: acquire write lock on RocksDB
```

Indexing stalls or fails during write-heavy operations.

### Causes and Solutions

#### Windows Defender or Antivirus Interference

**Most common cause on Windows.** Real-time scanning locks RocksDB SST files.

Measured impact: 7+ second delays on lock acquisition in similar projects.

**Solution**: Add database directory to exclusions.

```powershell
# PowerShell as Administrator
Add-MpPreference -ExclusionPath "$env:USERPROFILE\.context-engine\data"
Add-MpPreference -ExclusionPath "E:\Workspaces\context-engine\target"
```

Verify:

```powershell
Get-MpPreference | Select-Object -ExpandProperty ExclusionPath
```

See [Windows setup](setup-windows.md) for details.

#### WSL Memory Configuration

WSL2 default memory limit (50% of system RAM) can cause swapping under heavy indexing.

**Solution**: Limit WSL memory in `C:\Users\<username>\.wslconfig`:

```ini
[wsl2]
memory=8GB
swap=2GB
pageReporting=false
```

Restart WSL:

```powershell
wsl --shutdown
```

See [WSL setup](setup-wsl.md) for details.

#### Low File Handle Limit (Linux/macOS)

RocksDB opens many SST files. Default ulimit (1024) is too low.

**Solution**: Increase file handle limit.

```bash
# Temporary
ulimit -n 65536

# Permanent: add to /etc/security/limits.conf
* soft nofile 65536
* hard nofile 65536
```

Log out and back in, then verify:

```bash
ulimit -n
```

See [Linux setup](setup-linux.md) or [macOS setup](setup-macos.md) for details.

#### Multiple Processes Accessing Same Database

Only one process can hold write lock. If multiple `context-engine-rs index` commands run on same repo, the second will timeout.

**Solution**: Wait for first indexing to complete, or use `pkill context-engine-rs` to stop all instances.

```bash
# Check for running instances
ps aux | grep context-engine-rs

# Kill all instances
pkill context-engine-rs
```

## Symbol Not Found Errors

### Symptoms

Query returns no results for a function you know exists:

```sql
SELECT * FROM calls WHERE in_name = 'my_function';
-- Returns 0 rows
```

### Causes and Solutions

#### Incomplete Indexing

Indexing was interrupted or failed partway through.

**Solution**: Force full rebuild.

```bash
context-engine-rs index /path/to/repo --force
```

#### Language Not Supported

Context-engine supports 20+ languages via tree-sitter. Verify your language is included:

```bash
# Check Cargo.toml for tree-sitter-<language> dependency
grep tree-sitter Cargo.toml
```

Supported: Python, JavaScript, TypeScript, Rust, Go, Java, C, C++, C#, PHP, Ruby, Objective-C, Swift, Kotlin, Dart, Lua, Luau, Svelte, Pascal, Liquid.

**Solution**: If language missing, add tree-sitter parser to `Cargo.toml` and update `src/parsing/mod.rs`.

#### Symbol in Generated Code

Some symbols are in generated files (e.g., `node_modules`, `target`, `build`). Context-engine ignores common generated directories by default.

**Solution**: Check if file is ignored:

```bash
# List indexed files
surreal sql --query "SELECT DISTINCT file_path FROM symbol ORDER BY file_path" \
  --conn rocksdb:~/.context-engine/data/<repo-hash>/db --ns context --db main
```

If missing, add directory to index explicitly or update ignore rules.

#### Taint Edge Not Detected

Data-flow edges (taint propagation) require explicit flow-type detection. Coverage varies by language.

**Solution**: Check `src/parsing/taint.rs` for language-specific taint rules. Current coverage:

- Full support: Python, JavaScript, TypeScript, Rust
- Partial support: Java, C, C++, Go
- No taint tracking: Ruby, PHP, Swift (only control-flow calls indexed)

File an issue if your use case needs better taint coverage for a specific language.

## High Memory Usage

### Symptoms

Process consumes 4GB+ RAM during indexing or queries.

### Causes and Solutions

#### SurrealDB In-Memory Cache

SurrealDB loads indexes and recent query results into memory.

**Solution**: Limit via environment variable.

```bash
export SURREAL_MAX_MEMORY=2G
context-engine-rs index /path/to/repo
```

Or edit `~/.context-engine/config.toml`:

```toml
[database]
max_memory = "2G"
```

**Trade-off**: Lower memory usage, slower queries (more disk reads).

#### Large Repository

Linux kernel (40k+ files) produces 3M+ edges. Entire graph in memory is impractical.

**Solution**: Query incrementally. Don't `SELECT * FROM calls` without filters.

```sql
-- BAD: loads entire call graph
SELECT * FROM calls;

-- GOOD: filter by function name
SELECT * FROM calls WHERE in_name = 'specific_function';

-- GOOD: filter by file
SELECT * FROM calls WHERE file_path CONTAINS 'src/auth';
```

#### Memory Leak

Rare, but possible in long-running `watch` or `serve` mode.

**Solution**: Restart process. File an issue with reproduction steps.

## Slow Queries

### Symptoms

Query takes 10+ seconds on medium-sized repos.

### Causes and Solutions

#### Missing Indexes

SurrealDB schema defines indexes on `in_name`, `out_name`, `file_path`. Verify they exist:

```sql
INFO FOR TABLE calls;
```

Expected output includes:

```
DEFINE INDEX idx_in_name ON TABLE calls COLUMNS in_name
DEFINE INDEX idx_out_name ON TABLE calls COLUMNS out_name
DEFINE INDEX idx_file_path ON TABLE calls COLUMNS file_path
```

**Solution**: If missing, recreate database with `--force`:

```bash
context-engine-rs index /path/to/repo --force
```

#### Unfiltered Queries

Querying entire call graph without filters scans millions of rows.

**Solution**: Add WHERE clause (see High Memory Usage above).

#### Explain Plan

Use `EXPLAIN` to see query execution:

```sql
EXPLAIN SELECT * FROM calls WHERE in_name = 'my_function';
```

Look for "INDEX SCAN" vs "TABLE SCAN". If table scan, index is not being used.

#### RocksDB Compaction

After many incremental updates, RocksDB accumulates SST files. Compaction improves read performance.

**Solution**: Force compaction (not currently exposed in CLI; requires manual RocksDB operation).

**Workaround**: Force full rebuild, which writes compacted SST files.

```bash
context-engine-rs index /path/to/repo --force
```

## Taint Edge Not Detected

### Symptoms

Expected data-flow edge missing from call graph.

Example: `result = process(user_input)` should create DataFlowsTo edge from `process` to caller, but query returns nothing.

### Causes and Solutions

#### Language-Specific Taint Rules

Taint detection requires pattern matching on AST nodes. Coverage varies by language.

**Solution**: Check `src/parsing/taint.rs` for your language. If pattern missing, file an issue or add rule:

```rust
// Example: detect Python assignment
if node.kind() == "assignment" {
    let left = node.child_by_field_name("left");
    let right = node.child_by_field_name("right");
    if let Some(call) = extract_call_from_node(right) {
        // This is a taint edge: right flows to left
        emit_data_flow_edge(call, current_function);
    }
}
```

Contribute upstream if your pattern is broadly useful.

#### Complex Flow (Indirect Assignment)

Direct assignment (`x = f()`) is detected. Indirect flow through container mutation (`list.append(f())`) may not be.

**Solution**: Current design tracks direct call sites only. For whole-program taint analysis, use specialized tools (CodeQL, Semgrep) that build full SSA form.

Context-engine prioritizes fast incremental indexing over exhaustive taint propagation.

#### Inter-procedural Flow

Flow through function parameters requires tracking argument-to-parameter mapping. Not yet implemented.

Example:

```python
def caller():
    data = get_user_input()
    process(data)  # Should track that data flows into process's first param

def process(param):
    execute_query(param)  # Should track that param flows to execute_query
```

**Workaround**: Query both call edges separately and join in application code.

## Build Errors

### "linker 'link.exe' not found" (Windows)

Visual Studio build tools missing.

**Solution**: Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022) with C++ workload.

Verify:

```bash
where link.exe
```

### "cannot find -lssl" (Linux)

OpenSSL development libraries missing.

**Solution**:

```bash
# Debian/Ubuntu
sudo apt install libssl-dev

# Fedora/RHEL
sudo dnf install openssl-devel
```

### Tree-sitter compilation fails

Rare, usually due to missing C compiler.

**Solution**: Install platform build tools (see setup guides).

## Index Corruption

### Symptoms

Crashes, assertion failures, or invalid query results.

### Solution

Force full rebuild:

```bash
context-engine-rs index /path/to/repo --force
```

If corruption persists, delete database directory:

```bash
rm -rf ~/.context-engine/data/<repo-hash>
context-engine-rs index /path/to/repo
```

**Report**: File issue with reproduction steps if corruption occurs without disk failure or system crash.

## Platform-Specific Issues

### Windows: Path Too Long

Default 260-character limit causes "file not found" errors on deep directory trees.

**Solution**: Enable long paths. See [Windows setup](setup-windows.md).

### WSL: "Operation not supported"

WSL1 doesn't support RocksDB mmap.

**Solution**: Upgrade to WSL2. See [WSL setup](setup-wsl.md).

### macOS: Binary Blocked by Gatekeeper

"unidentified developer" error.

**Solution**: Remove quarantine attribute.

```bash
xattr -d com.apple.quarantine ./target/release/context-engine-rs
```

See [macOS setup](setup-macos.md).

### Linux: SELinux Denials

Permission denied on RocksDB operations.

**Solution**: Create SELinux policy or set permissive mode. See [Linux setup](setup-linux.md).

## Getting Help

If issue persists:

1. Check existing issues: https://github.com/nagicantsleep/context-engine/issues
2. Collect diagnostic info:

```bash
# System info
uname -a
cargo --version
context-engine-rs --version

# Database status
ls -lh ~/.context-engine/data/

# Logs (if server mode)
context-engine-rs serve --log-level debug
```

3. File issue with: reproduction steps, expected behavior, actual behavior, diagnostic output.
