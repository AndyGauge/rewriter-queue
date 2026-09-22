# Installation

## Prebuilt binaries

Every [release](https://github.com/AndyGauge/rewriter-queue/releases) ships
prebuilt archives for:

| Platform | Archive |
|---|---|
| Linux x86_64 | `rewriter-queue-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 (ARM64) | `rewriter-queue-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Intel | `rewriter-queue-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `rewriter-queue-aarch64-apple-darwin.tar.gz` |

```sh
curl -LO https://github.com/AndyGauge/rewriter-queue/releases/latest/download/rewriter-queue-<your-target>.tar.gz
tar xzf rewriter-queue-<your-target>.tar.gz
```

Each archive contains two binaries — `rewriter-queue` and
`ai-org-orchestrator` — plus the license files. **Keep both binaries in the
same directory.** The queue's worker finds the orchestrator by looking
beside its own executable path, not on `PATH`, so unpacking them apart from
each other breaks job execution.

Put that directory on your `PATH`, or reference `rewriter-queue`'s full
path directly (e.g. in Zed's `context_servers` config or the Claude Code
plugin's MCP config).

No Windows build yet — `job-queue`'s worker uses Unix process-group APIs
(`libc::killpg`) to reliably kill a job's whole process tree on cancel or
timeout, which doesn't have a direct Windows equivalent as written. Runs
fine under WSL in the meantime.

## Building from source

Needs a recent stable Rust toolchain ([rustup.rs](https://rustup.rs)).

```sh
git clone https://github.com/AndyGauge/rewriter-queue
cd rewriter-queue
cargo build --release
```

Binaries land in `target/release/rewriter-queue` and
`target/release/ai-org-orchestrator` — same "keep them together" rule
applies.

## The lint plugins (optional)

`ai-org-orchestrator`'s quality gate can run [ast-grep](https://ast-grep.github.io/)
rules from `ai-org-orchestrator/lints/rules/` as a non-blocking check
alongside `cargo build`/`clippy`/`test`. It degrades gracefully if the
binary isn't installed — this is opt-in, not a hard dependency:

```sh
cargo install ast-grep --locked
```

## The Zed extension and Claude Code plugin

Not part of the release archive — see [Zed setup](zed-setup.md) and
[`claude-code-plugin/README.md`](https://github.com/AndyGauge/rewriter-queue/tree/master/claude-code-plugin)
for how to install each.
