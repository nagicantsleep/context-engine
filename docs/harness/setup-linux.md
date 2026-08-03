# Linux Setup

## Prerequisites

### Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Verify installation
rustc --version
cargo --version
```

### Install Build Dependencies

#### Debian/Ubuntu

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev git
```

#### Fedora/RHEL

```bash
sudo dnf groupinstall -y "Development Tools"
sudo dnf install -y pkg-config openssl-devel git
```

#### Arch Linux

```bash
sudo pacman -S base-devel pkg-config openssl git
```

## Increase File Handle Limit

RocksDB opens many files simultaneously. Default ulimit (1024) is too low and causes "Too many open files" errors during indexing.

### Temporary (current session)

```bash
ulimit -n 65536
```

### Permanent

Edit `/etc/security/limits.conf`:

```bash
sudo nano /etc/security/limits.conf
```

Add these lines:

```
* soft nofile 65536
* hard nofile 65536
```

Log out and back in, then verify:

```bash
ulimit -n
# Should show 65536
```

Alternatively, for systemd user sessions, create `/etc/systemd/user.conf.d/limits.conf`:

```bash
sudo mkdir -p /etc/systemd/user.conf.d
sudo nano /etc/systemd/user.conf.d/limits.conf
```

Contents:

```ini
[Manager]
DefaultLimitNOFILE=65536
```

Reboot, then verify with `ulimit -n`.

## Build Context Engine

```bash
git clone https://github.com/your-org/vibervn-context-engine.git
cd vibervn-context-engine
cargo build --release
```

First build compiles tree-sitter parsers for 20+ languages. Expect 5-10 minutes on a 4-core machine.

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
context-engine-rs index ~/vibervn-context-engine

# Check database
ls ~/.context-engine/data/
```

Expected output: one directory per indexed repo, named by content hash.

## Optional: Systemd Service

Run context-engine as a background service for continuous indexing.

Create `/etc/systemd/system/context-engine.service`:

```ini
[Unit]
Description=Context Engine Server
After=network.target

[Service]
Type=simple
User=youruser
WorkingDirectory=/home/youruser/vibervn-context-engine
ExecStart=/home/youruser/.cargo/bin/context-engine-rs serve --host 127.0.0.1 --port 8080
Restart=on-failure
RestartSec=10

# Resource limits
LimitNOFILE=65536
MemoryMax=8G

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable context-engine
sudo systemctl start context-engine

# Check status
sudo systemctl status context-engine

# View logs
journalctl -u context-engine -f
```

## SELinux Considerations

If SELinux is enabled (Fedora, RHEL), it may block RocksDB file operations.

Check SELinux status:

```bash
getenforce
# If "Enforcing", SELinux is active
```

### Option 1: Allow context-engine

```bash
# Create a policy module (after first denial)
ausearch -c 'context-engine' --raw | audit2allow -M context-engine
sudo semodule -i context-engine.pp
```

### Option 2: Permissive mode (not recommended for production)

```bash
sudo setenforce 0
```

To persist, edit `/etc/selinux/config`:

```
SELINUX=permissive
```

## AppArmor Considerations

Ubuntu and Debian use AppArmor. If encountering permission denials, check logs:

```bash
sudo dmesg | grep DENIED
```

Create a profile if needed, or disable AppArmor for development:

```bash
sudo systemctl stop apparmor
sudo systemctl disable apparmor
```

## Common Issues

### "Too many open files"

File handle limit too low. Increase ulimit (see above).

### "libssl.so: cannot open shared object file"

Missing OpenSSL development libraries:

```bash
# Debian/Ubuntu
sudo apt install libssl-dev

# Fedora/RHEL
sudo dnf install openssl-devel
```

### "permission denied" writing to database

Check database directory permissions:

```bash
ls -la ~/.context-engine/data/
```

Should be owned by your user. If not:

```bash
sudo chown -R $USER:$USER ~/.context-engine/
```

### High memory usage during indexing

SurrealDB caches indexes in memory. For large repos, limit memory via environment:

```bash
export SURREAL_MAX_MEMORY=4G
context-engine-rs index /path/to/repo
```

## Performance Tuning

### I/O Scheduler

For SSD, use `none` or `mq-deadline`:

```bash
# Check current scheduler
cat /sys/block/nvme0n1/queue/scheduler

# Set to none (best for NVMe SSD)
echo none | sudo tee /sys/block/nvme0n1/queue/scheduler
```

### Filesystem

Use ext4 or XFS with noatime for RocksDB directories:

```bash
# Add to /etc/fstab
/dev/nvme0n1p1 /home ext4 defaults,noatime 0 2
```

Remount:

```bash
sudo mount -o remount /home
```

## Next Steps

- [Usage guide](usage.md) for indexing and querying
- [Troubleshooting](troubleshooting.md) for detailed debugging
