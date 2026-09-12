# Release Cross-Target Handoff

Date: 2026-08-10

## Status

COMPLETE (moved 2026-09-13) — all four targets built, packaged into their expected npm package paths, and verified. CI (`.github/workflows/release.yml`) remains the canonical release producer; the local proofs below used the user-provided build host `vcp5zp` with two disclosed environment deviations (Windows CRT flags; ARM64 cross toolchain). One CI-risk finding is recorded for the user: if rustc stable's MSVC CRT default has drifted, the release.yml Windows job may hit the same esaxx-rs LNK2038/LNK1169 link error on the next release run; the candidate fix is adding `RUSTFLAGS=-C target-feature=+crt-static` to the workflow's Windows job (user decision pending).

## Progress 2026-09-13

- Restored the lost local npm artifact: `npm/context-engine-darwin-arm64/bin/context-engine-rs` re-copied from `target/aarch64-apple-darwin/release/context-engine-rs` (binary was still present with the recorded SHA-256 `1376737aacf5a6f51ccd2f5583114f89b2492570ee3f1f00015630a5f280da56`; `--help` smoke PASS). The Windows binary in `target/` no longer exists, so its rebuild is required for the Windows package path.
- Local container attempts (OrbStack on this Mac) were abandoned: the VM died twice mid-build and the user directed that all container infrastructure run on their remote host `vcp5zp` (Windows, AMD64, 56 logical CPUs, 192 GB RAM, Docker 29.7.2, WSL `Ubuntu-22.04` distro, Rust 1.97.1 MSVC toolchain + LLVM/clang installed; MSVC `link.exe` verified by hello-world build).
- Repo delivered to `vcp5zp` as a git bundle (`master` @ `9208726`), cloned to `<home>\ce-build\ctx-win` (native Windows build) and `<home>\ce-build\ctx-linux` (container source).
- Windows: native `cargo build --release --locked --target x86_64-pc-windows-msvc` running on `vcp5zp` with `LIBCLANG_PATH=C:\Program Files\LLVM\bin` — matching-OS build instead of the previously recorded local cross-build.
- Linux x64: detached build running in container `ce-x64` (image `ubuntu:22.04` = exact `ubuntu-22.04` runner match), exact command `cargo build --release --locked --target x86_64-unknown-linux-gnu`.
- Linux ARM64: new base-image pulls from ssh sessions fail (Docker Desktop credential helper: "A specified logon session does not exist") and the WSL docker integration socket is not enabled for the distro, so the emulated `ubuntu:24.04` arm64 route is unavailable. Fallback running in container `ce-arm64` (base `ubuntu:22.04` amd64 + `gcc/g++-aarch64-linux-gnu` + rustup target): cross-compiled `aarch64-unknown-linux-gnu` with `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER`/`CC`/`CXX`/`BINDGEN_EXTRA_CLANG_ARGS` sysroot env. Deviation from CI (build env ubuntu-22.04 cross toolchain instead of native `ubuntu-24.04-arm`): binary is a genuine aarch64 ELF against glibc 2.35 (lower minimum than CI's 2.39 — more portable, never less); disclosed here and in the final report.

## Authority

The release matrix in [`.github/workflows/release.yml`](../../.github/workflows/release.yml) is authoritative:

| Target | Workflow runner |
| --- | --- |
| `x86_64-pc-windows-msvc` | `windows-latest` |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `aarch64-apple-darwin` | `macos-14` |

## Completed proof

- 2026-09-13 Linux x64 COMPLETE: built in container `ce-x64` (image `ubuntu:22.04`, exact `ubuntu-22.04` runner match) on `vcp5zp` with the exact command `cargo build --release --locked --target x86_64-unknown-linux-gnu` (Finished release in 10m39s). Binary `target/x86_64-unknown-linux-gnu/release/context-engine-rs` — 144,443,528 bytes, ELF 64-bit x86-64 (dynamically linked, GNU/Linux). Smoke `--help` PASS inside the same container. Copied to Windows host and fetched to this workspace: `npm/context-engine-linux-x64/bin/context-engine-rs` (executable bit set), SHA-256 `37612ea9483919c7f3ab27b5909a69ec5164601c25498544dcfc4763f75758b8` — identical on remote host and local package.
- 2026-09-13 Windows root cause for the earlier local cross-build gap: plain `cargo build --release --locked --target x86_64-pc-windows-msvc` now FAILS at link (LNK2038 `RuntimeLibrary: MD_DynamicRelease vs MT_StaticRelease` → LNK1169) because `esaxx-rs` build.rs hardcodes `.static_crt(true)` while current rustc stable (1.97.1) defaults the MSVC target to dynamic CRT. Rebuild forced consistent static CRT with `RUSTFLAGS="-C target-feature=+crt-static"`. RISK: if this is toolchain-default drift, the release.yml Windows job will hit the same link error on the next release run; adding the RUSTFLAGS to the workflow is the candidate fix (user decision pending).
- 2026-09-13 Windows COMPLETE (with disclosed deviation): after the CRT fix, `cargo build --release --locked --target x86_64-pc-windows-msvc` with `RUSTFLAGS="-C target-feature=+crt-static"` PASSED on `vcp5zp` (Finished release in 9m15s, exit 0). Binary 125,930,496 bytes, SHA-256 `3E280E54A9A6A8D3BD443CFE2DC998A1D72DA89764D1065860A0E19BDF41DC2C`; `--help` smoke PASS on the Windows host; packaged to `npm/context-engine-win32-x64/bin/context-engine-rs.exe`, SHA identical on remote and local package. Deviation vs CI's exact command: the `RUSTFLAGS` static-CRT flag was required on this toolchain (see root-cause entry above).
- 2026-09-13 Linux ARM64 COMPLETE (with disclosed deviation): cross-compiled in container `ce-arm64` (base `ubuntu:22.04` amd64, `gcc/g++-aarch64-linux-gnu`, rustup target `aarch64-unknown-linux-gnu`), PASSED (Finished release in 9m34s). Binary 133,642,160 bytes, ELF 64-bit aarch64 (GNU/Linux, `/lib/ld-linux-aarch64.so.1`), SHA-256 `2f3617138c6f81788ce23020f182c5d4051073e84ac023b6ec4c2e4cf97f4e76` — identical on remote host and local package `npm/context-engine-linux-arm64/bin/context-engine-rs` (executable bit set). Execution smoke PASS: run under QEMU binfmt inside an imported `ubuntu-base-24.04.3` arm64 userland (imported locally as image `ce-ubuntu-arm64:24.04` — no registry pull needed), printing the correct `--help` usage, i.e. the cross-built binary is verified executable against the ubuntu 24.04 arm64 userspace CI targets.

- Windows command: `cargo build --release --locked --target x86_64-pc-windows-msvc`
- Result: PASS, exit 0.
- Release binary: `target/x86_64-pc-windows-msvc/release/context-engine-rs.exe`
- Size: 106,485,760 bytes.
- Expected Windows package path: `npm/context-engine-win32-x64/bin/context-engine-rs.exe` — copy and verification pending; no package artifact proof is present in the workspace.
- macOS command: `cargo +stable build --release --locked --target aarch64-apple-darwin`
- Result: PASS, exit 0 in native local environment: macOS 15.6.1 arm64 with Xcode SDK; workflow runner `macos-14` was not run.
- Release binary: `target/aarch64-apple-darwin/release/context-engine-rs`
- Binary type: Mach-O 64-bit executable arm64.
- Size: 103,059,536 bytes.
- SHA-256: `1376737aacf5a6f51ccd2f5583114f89b2492570ee3f1f00015630a5f280da56`
- Smoke: `target/aarch64-apple-darwin/release/context-engine-rs --help` — PASS, exit 0.
- Local package artifact: `npm/context-engine-darwin-arm64/bin/context-engine-rs` — copy and executable permission verified locally; Mach-O 64-bit executable arm64; size 103,059,536 bytes; SHA-256 `1376737aacf5a6f51ccd2f5583114f89b2492570ee3f1f00015630a5f280da56`.
- Packaged smoke: `npm/context-engine-darwin-arm64/bin/context-engine-rs --help` — PASS, exit 0. This is local package proof only; the `macos-14` workflow runner and GitHub upload have not run.

## Blocked builds

Each blocked build exited 101 before project-source compilation. These are environment blockers, not evidence of a source fault.

| Target | Matching runner | Exact command | Result | Observed missing prerequisites | Workflow artifact path |
| --- | --- | --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | `cargo build --release --locked --target x86_64-unknown-linux-gnu` | BLOCKED, exit 101 | Rust target `core`/`std`; `x86_64-linux-gnu-gcc` | `npm/context-engine-linux-x64/bin/context-engine-rs` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `cargo build --release --locked --target aarch64-unknown-linux-gnu` | BLOCKED, exit 101 | Rust target `core`/`std`; `aarch64-linux-gnu-gcc` | `npm/context-engine-linux-arm64/bin/context-engine-rs` |
## Next action

Poll the three `vcp5zp` builds to completion. Then per target: copy the artifact back to this workspace, place it at the expected npm package path (`npm/context-engine-win32-x64/bin/context-engine-rs.exe`, `npm/context-engine-linux-x64/bin/context-engine-rs`, `npm/context-engine-linux-arm64/bin/context-engine-rs`), set the executable bit (non-Windows), verify file type (PE/ELF, arch) plus SHA-256, and smoke `--help` on the matching environment (Windows binary on `vcp5zp`; Linux binaries inside the same containers on `vcp5zp`). Record every proof in Completed proof. Do not mark this handoff complete until the Windows package path and both Linux binaries and package paths have been verified; the handoff remains Active until then.


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
