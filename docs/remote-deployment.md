# Running this on a remote target

The queue server and the inference backend don't have to be the same
machine, and usually shouldn't be — the inference backend wants a GPU, the
queue server doesn't need one at all. This covers running both on a remote
Linux box (a rented GPU instance, a home server, whatever) and reaching it
from your laptop.

## The inference backend

Anything that speaks the OpenAI-compatible `/v1/chat/completions` API
works — `llama.cpp`'s `llama-server`, `vLLM`, or a hosted API you don't run
yourself. If you're self-hosting on a GPU box:

```sh
llama-server \
  -m /path/to/model.gguf \
  -a your-model-id \
  -ngl 999 \
  -c 65536 \
  -fa auto \
  --jinja \
  --host 0.0.0.0 --port 8001
```

**Running more than one model on the same box** is fine as long as the
memory budget actually adds up — size it explicitly rather than finding out
at OOM time. Two independent servers on two ports (e.g. `8001`/`8002`) is
simpler and safer than trying to multiplex one server across models. Watch
in particular:

- **KV cache scales with context, not just model size.** For a dense
  transformer: `2 (K and V) × layers × kv_heads × head_dim × context ×
  bytes_per_element`. A model with a Mixture-of-Experts or hybrid
  linear-attention architecture can have a very different (often much
  smaller) real footprint than its parameter count suggests — check the
  model's own `config.json`, don't assume.
- **`-ctk q8_0 -ctv q8_0` (with `-fa on`) roughly halves KV cache size**
  at a near-lossless quality cost — the quantization error is small and
  fixed per cached token, not cumulative with age, so it mostly costs you
  long-range recall precision in very long contexts, not general quality.
  `q4_0` is meaningfully lossier; reach for `q8_0` first.
- Leave real headroom, not just enough to fit at idle — concurrent
  requests, the OS, and anything else on the box all need memory too.

## The queue server

Runs anywhere that can reach the inference backend(s) over HTTP — doesn't
need to be the GPU box, and arguably shouldn't be if you want to restart
the inference backend independently of the queue.

```sh
rewriter-queue serve --bind 0.0.0.0:8003
```

It needs a bearer token (`~/.config/rewriter/queue-token` or
`REWRITER_QUEUE_TOKEN`) and providers configured in
`~/.config/rewriter/config.toml`:

```toml
[[providers]]
name = "backend-a"
type = "openai-compat"
base_url = "http://<inference-box>:8001/v1"
models = ["your-model-id"]

[queue]
url = "http://<this-box>:8003"
```

### A PATH gotcha that will bite you

`quality_gate()` shells out to `cargo build`/`clippy`/`test` (and, if
installed, `ast-grep`) from inside the orchestrator subprocess the queue
worker spawns. That subprocess inherits whatever PATH the *queue server's
own process* happened to have — which depends entirely on how you started
it. A plain `ssh host 'rewriter-queue serve ...'` gets a minimal, non-login
shell PATH on most systems (skips `.bashrc`/`.profile`, so `~/.cargo/bin`
usually isn't on it) even though an interactive SSH session would have it.
If cargo isn't reliably found, every quality-gate check silently fails with
"cargo build not available" — treated as a normal retryable error, so it
doesn't crash anything, it just quietly never actually validates code
until the iteration budget runs out and broken output gets accepted anyway.

`job-queue/src/worker.rs` already guards against this — it explicitly sets
`PATH` (prepending `$HOME/.cargo/bin`) on the orchestrator process it
spawns, rather than trusting whatever it inherited. If you install cargo
somewhere else, or need other tools (like `ast-grep`) on a non-standard
PATH, that's the place to adjust it.

### Keeping it running

Neither binary is a managed service by default — write a systemd unit if
you want one to survive a reboot or restart on crash. Two examples:

```ini
# /etc/systemd/system/rewriter-queue.service
[Unit]
Description=rewriter-queue server
After=network.target

[Service]
Type=simple
User=youruser
WorkingDirectory=/home/youruser/rewriter-queue
ExecStart=/home/youruser/rewriter-queue/target/release/rewriter-queue serve --bind 0.0.0.0:8003
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```ini
# /etc/systemd/system/llama-server.service
[Unit]
Description=llama.cpp inference server
After=network.target

[Service]
Type=simple
User=youruser
ExecStart=/home/youruser/llama.cpp/build/bin/llama-server \
  -m /home/youruser/models/your-model.gguf -a your-model-id \
  -ngl 999 -c 65536 -fa auto --jinja --host 0.0.0.0 --port 8001
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now rewriter-queue llama-server
```

If you'd rather not deal with `sudo`/systemd at all, `tmux` (or `screen`)
is a fine substitute for a personal box: start each in its own session so
an SSH disconnect doesn't kill it, and accept that a reboot needs a manual
restart.

### The lint plugins need `ast-grep`

`ai-org-orchestrator/lints/` (see the root README) is optional — the
orchestrator degrades gracefully if the binary isn't found — but if you
want it: `cargo install ast-grep --locked` on whichever machine actually
runs the orchestrator subprocess (the queue server's box, per the PATH
note above).

## Security posture — decide this deliberately

None of the above puts a token or TLS in front of the inference backend
itself, and the queue server's Bearer token is the *only* thing gating job
submission (no per-job auth, no rate limiting). That's a reasonable
default for a private LAN or a locked-down VPC where you trust everything
that can reach the port — it is not a default you want facing the public
internet. If you're putting either behind a real network boundary (a cloud
security group, a home router's port forward, anything multi-tenant), add
a reverse proxy with TLS and real auth in front, and give `llama-server`
its own `--api-key` rather than relying on network trust alone.
