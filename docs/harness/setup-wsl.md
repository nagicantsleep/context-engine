# WSL2 Setup

## Prerequisites

### Install WSL2

Windows 11 or Windows 10 version 2004+ required.

```powershell
# Run PowerShell as Administrator
wsl --install
```

This installs Ubuntu by default. Restart when prompted.

### Verify WSL2 (not WSL1)

```bash
wsl -l -v
```

Output should show VERSION 2. If VERSION 1, upgrade:

```powershell
wsl --set-version Ubuntu 2
```

RocksDB requires WSL2. WSL1 lacks proper mmap support and will fail with "Operation not supported" errors.

### Install Rust in WSL

```bash
# Inside WSL terminal
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Verify
rustc --version
cargo --version
```

## Memory Configuration

SurrealDB loads indexes into memory. Default WSL2 memory limit is 50% of system RAM, which can cause OOM on large repositories (Linux kernel: 40k+ files).

Create or edit `C:\Users\<username>\.wslconfig`:

```ini
[wsl2]
# Limit memory to 8GB (adjust based on system RAM)
memory=8GB

# Limit swap (prevents disk thrashing)
swap=2GB

# Disable page reporting (improves RocksDB performance)
pageReporting=false

# Increase file descriptor limit
processors=4
```

Restart WSL after editing:

```powershell
wsl --shutdown
```

## File System Performance

WSL2 has two file systems:

1. **WSL ext4** (`/home/...`): native Linux performance
2. **Windows mount** (`/mnt/c/...`): 10x slower due to 9P protocol overhead

**Store repositories and database on WSL ext4, not /mnt/c.**

```bash
# SLOW: repository on Windows filesystem
cd /mnt/c/Users/cloen/Projects/myrepo
context-engine-rs index .

# FAST: repository on WSL filesystem
cd ~/projects/myrepo
context-engine-rs index .
```

Measured difference on Linux kernel indexing: 45 minutes (ext4) vs 7+ hours (/mnt/c).

## Clone Repositories in WSL

```bash
# Clone directly to WSL filesystem
cd ~
mkdir -p projects
cd projects
git clone https://github.com/user/repo.git
```

## Build Context Engine

```bash
cd ~/projects/context-engine
cargo build --release
```

First build compiles tree-sitter parsers for 20+ languages. Expect 5-10 minutes.

## Add to PATH

```bash
# Add to ~/.bashrc or ~/.zshrc
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

## Verify Installation

```bash
context-engine-rs --version

# Index a test repository
context-engine-rs index ~/projects/context-engine

# Check database
ls ~/.context-engine/data/
```

Expected output: one directory per indexed repo, named by content hash.

## Accessing Data from Windows

The database lives in WSL filesystem. Access from Windows tools:

### Option 1: Network Path

```powershell
# Access WSL filesystem from Windows
\\wsl$\Ubuntu\home\<username>\.context-engine\data
```

Use in Windows Explorer, VS Code, or other tools.

### Option 2: Expose HTTP API

Run the context-engine server in WSL:

```bash
context-engine-rs serve --host 0.0.0.0 --port 8080
```

Query from Windows:

```powershell
curl http://localhost:8080/api/query -d '{"query": "SELECT * FROM calls LIMIT 10"}'
```

## Common Issues

### "cannot allocate memory" during indexing

WSL memory limit too low. Increase `memory=` in `.wslconfig` (see above) and restart WSL.

### "too many open files"

Default ulimit is 1024, too low for RocksDB. Increase in `/etc/security/limits.conf`:

```bash
# Add these lines
* soft nofile 65536
* hard nofile 65536
```

Restart WSL or reboot. Verify:

```bash
ulimit -n
# Should show 65536
```

### Indexing is extremely slow

Repository is on `/mnt/c`. Move to WSL ext4 filesystem (see File System Performance above).

### "Operation not supported" from RocksDB

WSL1 detected. Upgrade to WSL2 (see Prerequisites).

## Integration with Windows Tools

### VS Code

Install "Remote - WSL" extension. Open WSL project:

```bash
cd ~/projects/context-engine
code .
```

VS Code runs in WSL, accessing files directly on ext4.

### Git Credentials

Share Windows Git credentials with WSL:

```bash
git config --global credential.helper "/mnt/c/Program\ Files/Git/mingw64/bin/git-credential-manager.exe"
```

## Next Steps

- [Usage guide](usage.md) for indexing and querying
- [Troubleshooting](troubleshooting.md) for detailed debugging
