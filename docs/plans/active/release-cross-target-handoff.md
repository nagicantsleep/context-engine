# Release Cross-Target Handoff

Date: 2026-08-10

## Status

Active — Windows build-only verified; Windows package path pending; local macOS build/package artifacts verified; Linux x64/ARM64 builds and package paths pending

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
- Expected Windows package path: `npm/vibervn-context-engine-win32-x64/bin/context-engine-rs.exe` — copy and verification pending; no package artifact proof is present in the workspace.
- macOS command: `cargo +stable build --release --locked --target aarch64-apple-darwin`
- Result: PASS, exit 0 in native local environment: macOS 15.6.1 arm64 with Xcode SDK; workflow runner `macos-14` was not run.
- Release binary: `target/aarch64-apple-darwin/release/context-engine-rs`
- Binary type: Mach-O 64-bit executable arm64.
- Size: 103,059,536 bytes.
- SHA-256: `1376737aacf5a6f51ccd2f5583114f89b2492570ee3f1f00015630a5f280da56`
- Smoke: `target/aarch64-apple-darwin/release/context-engine-rs --help` — PASS, exit 0.
- Local package artifact: `npm/vibervn-context-engine-darwin-arm64/bin/context-engine-rs` — copy and executable permission verified locally; Mach-O 64-bit executable arm64; size 103,059,536 bytes; SHA-256 `1376737aacf5a6f51ccd2f5583114f89b2492570ee3f1f00015630a5f280da56`.
- Packaged smoke: `npm/vibervn-context-engine-darwin-arm64/bin/context-engine-rs --help` — PASS, exit 0. This is local package proof only; the `macos-14` workflow runner and GitHub upload have not run.

## Blocked builds

Each blocked build exited 101 before project-source compilation. These are environment blockers, not evidence of a source fault.

| Target | Matching runner | Exact command | Result | Observed missing prerequisites | Workflow artifact path |
| --- | --- | --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | `cargo build --release --locked --target x86_64-unknown-linux-gnu` | BLOCKED, exit 101 | Rust target `core`/`std`; `x86_64-linux-gnu-gcc` | `npm/vibervn-context-engine-linux-x64/bin/context-engine-rs` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `cargo build --release --locked --target aarch64-unknown-linux-gnu` | BLOCKED, exit 101 | Rust target `core`/`std`; `aarch64-linux-gnu-gcc` | `npm/vibervn-context-engine-linux-arm64/bin/context-engine-rs` |
## Next action

The existing Windows release binary is build-only proof. Copy it to and verify the expected Windows package path `npm/vibervn-context-engine-win32-x64/bin/context-engine-rs.exe` without rerunning the Windows cross-build solely for packaging. Then run and verify the two pending Linux targets and their package paths on matching workflow runners/containers: `x86_64-unknown-linux-gnu` on `ubuntu-22.04` and `aarch64-unknown-linux-gnu` on `ubuntu-24.04-arm`. Provision the required target, toolchain, libclang, and SDK prerequisites, then run each target's exact release command above and check each expected package path. Do not mark this handoff complete until the Windows package path and both Linux binaries and package paths have been verified; the handoff remains Active until then.


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
