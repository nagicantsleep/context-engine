# macOS Setup

## Prerequisites

### Install Xcode Command Line Tools

```bash
xcode-select --install
```

If already installed, verify:

```bash
xcode-select -p
# Should show /Library/Developer/CommandLineTools or /Applications/Xcode.app/Contents/Developer
```

### Install Homebrew

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

Follow post-install instructions to add Homebrew to PATH.

### Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Verify installation
rustc --version
cargo --version
```

## Build Context Engine

```bash
git clone https://github.com/your-org/vibervn-context-engine.git
cd vibervn-context-engine
cargo build --release
```

First build compiles tree-sitter parsers for 20+ languages. Expect 5-10 minutes.

## Gatekeeper and Codesigning

macOS Gatekeeper may block the binary as an "unidentified developer." Two options:

### Option 1: Allow via System Preferences

When you first run the binary, macOS shows a security prompt. Click "Open Anyway" in System Preferences > Security & Privacy.

### Option 2: Remove Quarantine Attribute

```bash
xattr -d com.apple.quarantine ./target/release/context-engine-rs
```

This removes the quarantine flag without disabling Gatekeeper globally.

## Apple Silicon (M1/M2/M3) Considerations

All dependencies have native ARM builds. No Rosetta 2 required.

Verify native architecture:

```bash
file ./target/release/context-engine-rs
# Should show "arm64", not "x86_64"
```

If it shows x86_64, you're running Rosetta Rust. Reinstall:

```bash
rustup self uninstall
# Then reinstall Rust (see above)
```

## Disable Spotlight for Database Directories

Spotlight indexes RocksDB files, causing high CPU and I/O during indexing. Exclude database directories:

```bash
# Add database directory to Spotlight exclusions
sudo mdutil -i off ~/.context-engine/data
```

Or via System Preferences:
1. Open System Preferences > Spotlight > Privacy
2. Click "+" and add `~/.context-engine/data`

Verify exclusion:

```bash
mdutil -s ~/.context-engine/data
# Should show "Indexing disabled"
```

Also exclude build artifacts:

```bash
sudo mdutil -i off ~/vibervn-context-engine/target
```

## Add to PATH

```bash
# Add to ~/.zshrc (macOS default shell)
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

For bash users:

```bash
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bash_profile
source ~/.bash_profile
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

## Common Issues

### "xcrun: error: invalid active developer path"

Xcode Command Line Tools not installed. Run:

```bash
xcode-select --install
```

### "library not loaded: /usr/local/opt/openssl/lib/libssl.dylib"

OpenSSL library path issue. Link current OpenSSL:

```bash
brew install openssl@3
export OPENSSL_DIR=$(brew --prefix openssl@3)
cargo clean
cargo build --release
```

### "Too many open files"

macOS default file limit is low (256). Increase:

```bash
ulimit -n 10240
```

To persist, create `/Library/LaunchDaemons/limit.maxfiles.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>limit.maxfiles</string>
    <key>ProgramArguments</key>
    <array>
      <string>launchctl</string>
      <string>limit</string>
      <string>maxfiles</string>
      <string>65536</string>
      <string>200000</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
  </dict>
</plist>
```

Load the configuration:

```bash
sudo chown root:wheel /Library/LaunchDaemons/limit.maxfiles.plist
sudo launchctl load -w /Library/LaunchDaemons/limit.maxfiles.plist
```

Reboot, then verify:

```bash
ulimit -n
# Should show 65536 or higher
```

### Binary blocked by Gatekeeper

See Gatekeeper section above. Use `xattr -d com.apple.quarantine` to unblock.

### High CPU usage during indexing

Spotlight is indexing RocksDB files. Disable for database directories (see above).

### Slow compilation on older Intel Macs

Tree-sitter grammar compilation is CPU-intensive. Use `--jobs` flag to limit parallelism and prevent thermal throttling:

```bash
cargo build --release --jobs 2
```

## Performance Optimization

### File System Caching

macOS aggressively caches file metadata. For databases on external drives, disable atime updates:

Mount with `noatime`:

```bash
# For external drive (example: /Volumes/External)
sudo mount -u -o noatime /Volumes/External
```

### Metal GPU Acceleration

Not currently used by context-engine, but future embedding calculations may leverage Metal via candle framework.

## Next Steps

- [Usage guide](usage.md) for indexing and querying
- [Troubleshooting](troubleshooting.md) for detailed debugging
