# Tutorial

This walks through the whole loop once, end to end: get a binary, point it at
a model, submit something to rewrite, and watch it work. Fifteen minutes if
you've already got an OpenAI-compatible inference server running somewhere;
longer if you need to stand one up first (see
[Remote deployment](remote-deployment.md) for that part).

## 1. Get the binaries

Download the archive for your platform from the
[latest release](https://github.com/AndyGauge/rewriter-queue/releases/latest)
and unpack it — see [Installation](installation.md) if you'd rather build
from source. Either way you end up with two binaries sitting next to each
other: `rewriter-queue` and `ai-org-orchestrator`. They need to stay in the
same directory — the queue's worker finds the orchestrator by looking beside
its own binary, not on `PATH`.

Put that directory on your `PATH`, or just remember the full path for the
commands below.

## 2. Tell it where your model lives

Create `~/.config/rewriter/config.toml`:

```toml
[[providers]]
name = "local"
type = "openai-compat"
base_url = "http://localhost:8001/v1"
models = ["your-model-id"]
```

Point `base_url` at any OpenAI-compatible `/v1/chat/completions` endpoint —
a `llama.cpp`/`vLLM` server you're running, or a hosted API. If you don't
have one yet, that's the whole subject of
[Remote deployment](remote-deployment.md).

## 3. Pick a queue mode

For trying this out on one machine, skip the server entirely — every
command below falls back to a local, file-backed queue when there's no
`[queue]` section in the config. If you're running the inference backend on
a separate box and want to submit jobs from your laptop, start the server
there instead and add a `[queue] url = "..."` pointing at it (see the root
README's quickstart for the server command and token setup).

This tutorial uses local mode — nothing extra to start.

## 4. Submit something

Pick a small, real piece of code you want rewritten or reviewed — a single
source file or a small directory is a better first run than a whole
project. You need an empty directory for the workspace; the tool writes its
working state there as it goes.

```sh
mkdir /tmp/my-first-run
rewriter-queue submit \
  --source /path/to/the/code \
  --workspace /tmp/my-first-run \
  --max-iter 10
```

That prints something like `queued job #1 (0 ahead)`. In local mode with
nothing else queued, it starts immediately.

## 5. Watch it work

```sh
rewriter-queue watch
```

A live view that refreshes once a second: which job is running, what stage
it's on (`ObjectiveContract` → test matrix → inductive analysis → schema →
`V2 synthesis`), and how long it's been going. Ctrl-C to stop watching —
the job keeps running either way.

Want more detail on one job specifically?

```sh
rewriter-queue status 1
```

Shows the last 20 lines of its log, including which agent is running and
the token counts for each call.

## 6. See what it's produced, before it's even done

You don't have to wait for the job to finish to look at what it's built so
far:

```sh
rewriter-queue artifacts 1          # list what's been emitted
rewriter-queue artifact 1 objective_contract.md   # read one of them
```

`objective_contract.md` is usually the first thing worth reading — it's the
agreed-on shape of what's being built (public API, invariants, edge cases,
explicit exclusions) before any code gets written.

## 7. Get the result

In local mode there's nothing to download — the workspace directory you
passed to `submit` *is* where everything lands, no copy needed:

```sh
ls /tmp/my-first-run/artifacts/v2/   # the actual synthesized code
```

(`rewriter-queue download 1 --out ./result` is for the remote-server case —
see [Remote deployment](remote-deployment.md) — where the job ran on a
different machine and you need its `artifacts/` tree copied to yours in one
call instead of one file at a time.)

## 8. If it gets interrupted

Kill `rewriter-queue serve` (or just Ctrl-C a local-mode run) mid-job and
resubmit isn't what you want — restart the server (or just try `watch`
again in local mode) and the same job picks back up from wherever it had
already checkpointed, not from the beginning. This is true down to the
level of individual agent calls, not just whole pipeline stages — see the
root README's design notes if you're curious how.

## What's next

- [Installation](installation.md) — the binary download details, and
  building from source if you'd rather.
- [Zed setup](zed-setup.md) — submit and watch jobs from Zed's agent panel
  instead of a terminal.
- The [Claude Code plugin](https://github.com/AndyGauge/rewriter-queue/tree/master/claude-code-plugin)
  — the same queue tools inside Claude Code, plus each reviewer persona as
  a standalone slash command.
- [Remote deployment](remote-deployment.md) — running the inference backend
  and the queue server on a separate (e.g. GPU) machine from the one you're
  submitting jobs from.
