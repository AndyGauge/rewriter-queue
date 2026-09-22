# rewriter-queue

A resumable job queue for multi-agent LLM code synthesis. You submit a source
tree and a mission; a small organization of agent roles (implementer,
security/maintenance/production-readiness reviewers, a merge agent for
large inputs) iterates on it — patching rather than rewriting where it can,
checkpointing every real step so an interrupted run resumes instead of
starting over — until the reviewers are satisfied or the iteration budget
runs out. Submit and watch jobs from a terminal, from Zed, or from Claude
Code.

This isn't a hosted service. It's three small Rust binaries you run
yourself, against whatever OpenAI-compatible inference endpoint you point
them at — a local `llama.cpp`/`vLLM` server, or a hosted API.

## What's here

- **`job-queue`** (binary: `rewriter-queue`) — the queue server, CLI, and MCP
  server. Submit a job, watch it live, cancel it, list or fetch its
  artifacts individually or all at once. Jobs survive a server restart or
  crash: an interrupted job is automatically requeued and resumes from
  whatever it had already checkpointed, not from scratch.
- **`ai-org-orchestrator`** — the actual multi-agent pipeline. Loads its
  agent personas from `skills/*.md` at compile time (edit the skill, not
  the Rust, to change how an agent behaves), runs an `ObjectiveContract` →
  test-matrix → inductive-analysis → schema → synthesis pipeline, and
  checkpoints every individual agent call so a crash mid-run only redoes
  the step it was on. On a quality-gate failure, it asks for a minimal
  SEARCH/REPLACE patch against the current code before falling back to a
  full rewrite — cheaper and faster, especially against a small local
  model.
- **`inference-providers`** — a provider-agnostic registry with cost/latency/
  quality-aware routing across however many backends you configure
  (OpenAI-compatible, Anthropic, Gemini, Ollama, the `claude` CLI). Backed
  by a qualification-query battery that probes each provider's real context
  window and output quality on startup, not just what it claims to support.
- **`zed-extension`** — a real, compiled Zed extension that registers
  `rewriter-queue mcp` as a context server, so Zed's agent panel gets the
  queue's tools directly.
- **`skills/`** — the agent personas themselves, as plain markdown. Each one
  is a single-purpose reviewer or implementer with a strict verdict format
  (`APPROVE`/`REJECT`, `SOUND`/`UNSOUND`, `READY`/`NOT_READY`, ...) and an
  explicit list of what it does *not* review, so responsibilities don't
  overlap.
- **`ai-org-orchestrator/lints/`** — the quality gate's plugin directory.
  Drop an [ast-grep](https://ast-grep.github.io/) rule `.yml` file into
  `lints/rules/` and the next run picks it up automatically — no
  recompiling. Matches are logged as non-blocking findings, the same as
  the LLM reviewers' output, not treated as a hard gate.
- **`claude-code-plugin/`** — packages the queue's MCP tools plus each
  skill as a Claude Code slash command. See its own README.

Full docs, starting with a tutorial: **https://andygauge.github.io/rewriter-queue/**

## Quickstart

Grab a prebuilt binary for your platform from the
[latest release](https://github.com/AndyGauge/rewriter-queue/releases/latest)
— no local Rust toolchain needed. (Building from source is `cargo build
--release`, if you'd rather; see [`docs/installation.md`](docs/installation.md).)

1. **Point it at an inference backend.** Write `~/.config/rewriter/config.toml`:

   ```toml
   [[providers]]
   name = "local"
   type = "openai-compat"
   base_url = "http://localhost:8001/v1"   # your llama.cpp / vLLM / etc. server
   models = ["your-model-id"]
   ```

   See [`docs/remote-deployment.md`](docs/remote-deployment.md) for running
   the inference backend itself on a separate (e.g. GPU) machine.

2. **Start the queue server** (can be the same machine as the inference
   backend, or your laptop — it just needs to reach it over HTTP):

   ```sh
   ./target/release/rewriter-queue serve --bind 0.0.0.0:8003
   ```

   The server needs a bearer token: put one in
   `~/.config/rewriter/queue-token`, or set `REWRITER_QUEUE_TOKEN`. If
   you're driving the queue from the same machine it's running on, you can
   skip the server entirely — every `rewriter-queue` subcommand falls back
   to a local, file-backed queue when no `[queue] url` is configured.

3. **Submit a job:**

   ```sh
   ./target/release/rewriter-queue submit \
     --source /path/to/the/code/to/rewrite \
     --workspace /path/to/a/fresh/empty/dir \
     --max-iter 10
   ```

4. **Watch it:**

   ```sh
   ./target/release/rewriter-queue watch
   ```

5. **Get the result once it's done:**

   ```sh
   ./target/release/rewriter-queue download <id> --out ./result
   ```

For the editor integrations, see [`docs/zed-setup.md`](docs/zed-setup.md)
and [`claude-code-plugin/README.md`](claude-code-plugin/README.md).

## Design notes worth knowing before you extend this

- **Checkpointing is per-agent-call, not per-stage.** `Workspace::checkpoint`
  in `ai-org-orchestrator/src/workspace.rs` is the one primitive everything
  else is built from: memoize an expensive call under a unique id, and a
  resumed run replays every already-done id instantly and only redoes the
  first missing one. A large logical stage should be decomposed into many
  small, uniquely-id'd checkpoints rather than one big one — see how the
  dual-review loop checkpoints each iteration's worker/security/maintenance
  calls independently in `synthesis.rs`.
- **Gates are for the one thing that isn't a pure function of checkpointed
  data.** `Workspace::gate_usize` exists specifically for decisions that
  read live state (e.g. how many capable providers are currently healthy)
  and could legitimately answer differently on a resumed run than the
  original one — everything else replays deterministically for free from
  ordinary checkpoints and doesn't need this.
- **The patch mechanism has a mandatory fallback.** `patch.rs`'s
  SEARCH/REPLACE format requires an exact, unambiguous match against the
  current file; if it doesn't apply, the caller always falls back to a full
  regeneration rather than guessing. A bad patch should never corrupt a
  file or stall the loop.
- **Non-blocking findings vs. hard gates.** `cargo build`/`clippy`/`test`
  failures feed back into the retry loop and block acceptance. Everything
  else — the LLM review panel, the ast-grep lint plugins — is logged to
  `deviations.md` and never blocks. A structural pattern match or a review
  persona's opinion can't reliably distinguish a real problem from a
  justified exception the way a compiler error can.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Contributions are accepted under
the same dual license, per the standard Rust ecosystem convention, unless
you explicitly state otherwise.
