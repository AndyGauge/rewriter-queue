# Zed setup

Two independent pieces: the **context server** (lets Zed's agent panel submit
and watch queue jobs) and the **model provider** (lets Zed chat with whatever
backend is serving your models). You can set up either without the other.

## 1. The context server (`zed-extension/`)

This is a real Zed extension — `zed-extension/extension.toml` +
`zed-extension/src/lib.rs`, compiled to WASM — not a hand-edited
`settings.json` block. It registers `rewriter-queue mcp` as a context
server and defaults its arguments to `["mcp"]`, so you don't have to
remember to add that yourself.

**Build and install it as a dev extension:**

```sh
rustup target add wasm32-wasip2
cargo build -p job-queue --release   # the rewriter-queue binary itself
```

In Zed: open the command palette and run **`zed: install dev extension`**,
then point it at the `zed-extension/` directory. Zed builds and loads the
WASM extension itself — you don't need to invoke the WASM build by hand.

**Tell it where the `rewriter-queue` binary is**, if it's not on your
`PATH`, in `settings.json`:

```json
{
  "context_servers": {
    "rewriter-queue": {
      "command": {
        "path": "/absolute/path/to/rewriter-queue/target/release/rewriter-queue"
      }
    }
  }
}
```

Leave out `command` entirely if `rewriter-queue` is already on `PATH`
(e.g. `cargo install --path job-queue`).

**Point it at a queue server** via `~/.config/rewriter/config.toml`:

```toml
[queue]
url = "http://192.0.2.10:8003"   # wherever `rewriter-queue serve` is running
```

...and the token it expects, in `~/.config/rewriter/queue-token`. Without a
`[queue]` section, every command (including the ones the context server
runs on your behalf) falls back to a local, file-backed queue — fine for
trying this out on one machine, but you'll usually want a real server if
you're also running the inference backend on a GPU box (see
[`remote-deployment.md`](remote-deployment.md)).

Once it's connected (check the context-servers panel for a green status,
not a red error dot), just ask Zed's agent about your jobs in plain
language — "what jobs are in the queue?", "submit a synthesis run for
`./src` with a 20-iteration budget", "download job 3's artifacts to
`~/Desktop`" — and it calls the right tool
(`queue_submit`/`queue_list`/`queue_status`/`queue_artifacts`/
`queue_artifact`/`queue_download`/`queue_fetch`/`queue_cancel`) itself.
There's no slash command for this — MCP tools are things the model calls on
its own, not something you invoke directly, unless a server also exposes
MCP *prompts* (this one doesn't).

A live terminal view is also available without going through Zed's agent
at all: `.zed/tasks.json` in this repo defines a **"Queue: watch"** task
that runs `rewriter-queue watch`.

## 2. The model provider

This is unrelated to the extension above — it's Zed's own
`openai_compatible` provider type, pointed at whatever OpenAI-compatible
server (`llama.cpp`, `vLLM`, etc.) you're running your models on. Add it to
`settings.json`:

```json
{
  "language_models": {
    "openai_compatible": {
      "my-backend": {
        "api_url": "http://192.0.2.10:8001/v1",
        "available_models": [
          {
            "name": "your-model-id",
            "display_name": "My Model",
            "max_tokens": 128000,
            "capabilities": {
              "tools": true,
              "images": false,
              "parallel_tool_calls": false,
              "prompt_cache_key": false,
              "chat_completions": true
            }
          }
        ]
      }
    }
  }
}
```

`"tools": true` matters if you want Zed's agent to actually call tools
through this model — verified working with both `llama.cpp`'s `--jinja`
mode and vLLM's `--enable-auto-tool-choice`, but not every locally-served
model reliably emits well-formed tool calls. If the agent starts guessing
at shell commands instead of calling a tool by name, that's usually the
model's tool-calling support, not a Zed or context-server bug — sanity-check
by sending the same tool schema directly to the model's `/v1/chat/completions`
endpoint and see what comes back before assuming the wiring is broken.

Running two backends side by side (e.g. a general model and a
coding-specialized one) on the same box just means two `openai_compatible`
entries pointed at two different ports — see
[`remote-deployment.md`](remote-deployment.md) for sizing multiple backends
into one machine's memory.
