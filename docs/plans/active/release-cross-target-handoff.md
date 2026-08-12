# Release Cross-Target Handoff

Date: 2026-08-10

## Status

Active — Windows artifact verified; Linux/macOS builds pending suitable environments

## Authority

The release matrix in [`.github/workflows/release.yml`](../../.github/workflows/release.yml) is authoritative:

| Target | Workflow runner |
| --- | --- |
| `x86_64-pc-windows-msvc` | `windows-latest` |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `aarch64-apple-darwin` | `macos-14` |

## Completed proof

- Windows command: `cargo build --release --locked --target x86_64-pc-windows-msvc`
- Result: PASS, exit 0.
- Release binary: `target/x86_64-pc-windows-msvc/release/context-engine-rs.exe`
- Size: 106,485,760 bytes.

## Blocked builds

Each blocked build exited 101 before project-source compilation. These are environment blockers, not evidence of a source fault.

| Target | Matching runner | Exact command | Result | Observed missing prerequisites | Workflow artifact path |
| --- | --- | --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | `cargo build --release --locked --target x86_64-unknown-linux-gnu` | BLOCKED, exit 101 | Rust target `core`/`std`; `x86_64-linux-gnu-gcc` | `npm/vibervn-context-engine-linux-x64/bin/context-engine-rs` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `cargo build --release --locked --target aarch64-unknown-linux-gnu` | BLOCKED, exit 101 | Rust target `core`/`std`; `aarch64-linux-gnu-gcc` | `npm/vibervn-context-engine-linux-arm64/bin/context-engine-rs` |
| `aarch64-apple-darwin` | `macos-14` | `cargo build --release --locked --target aarch64-apple-darwin` | BLOCKED, exit 101 | Rust target `core`/`std`; macOS `cc`/SDK environment | `npm/vibervn-context-engine-darwin-arm64/bin/context-engine-rs` |

## Next action

On each workflow-matching runner/container, provision the required target, toolchain, libclang, and SDK prerequisites, then run that target's exact release command above. Do not mark this handoff complete until each expected binary exists and its workflow artifact path has been checked.

## Recovery and validation

Preserve current changes. After suitable environments are available, run the common checks first:

```text
git diff --check
cargo fmt --all -- --check
```

Then run exactly one target-specific release build, selected to match the current workflow runner:

| Workflow runner | Target | Exact command |
| --- | --- | --- |
| Windows runner | `x86_64-pc-windows-msvc` | `cargo build --release --locked --target x86_64-pc-windows-msvc` |
| Ubuntu x64 runner | `x86_64-unknown-linux-gnu` | `cargo build --release --locked --target x86_64-unknown-linux-gnu` |
| Ubuntu ARM64 runner | `aarch64-unknown-linux-gnu` | `cargo build --release --locked --target aarch64-unknown-linux-gnu` |
| macOS 14 runner | `aarch64-apple-darwin` | `cargo build --release --locked --target aarch64-apple-darwin` |

Do not run the other target builds on that runner.

The prior builds did not change source files; the working-tree source state was unchanged by those builds.
