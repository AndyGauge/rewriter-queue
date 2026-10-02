# Run observability: plan

## Goal

Every agent in a job reports what it did and found to one place, so that (1) a running job can be
watched in detail, (2) a finished or failed job can be analysed afterwards without reading logs
over SSH, and (3) recurring problems turn into feature requests automatically.

Today agents write files in the job workspace (`agents/<name>/task.md|output.md|decisions.md`,
`deviations.md`). Nothing aggregates them. That stays as the fallback; this plan adds a structured
event stream on top.

## Non-goals

No database dependency (JSONL files only). No web UI. No change to what agents decide or how the
pipeline runs. A reporting failure must never fail or slow a job.

## Shared interface (already built)

The `run-events` crate holds `Event`, `EventKind`, `Severity`, `FeatureRequest`, `Priority`,
`RequestStatus`, `Analysis` and `parse_events`. **Do not change its types** without recording the
change in the coordination file; two other packages depend on them. A new optional field is fine,
a rename or removal is not.

`Event.detail` keys by kind (JSON object):
- `agent_call`: `provider`, `model`, `input_tokens`, `output_tokens`, `latency_ms`, `ok`
- `finding`: `severity` (`info|warn|error`), `category` (short kebab-case), `evidence`
- `gate_result`: `passed`, `error_bytes`, `attempt`
- others: free-form, optional

## Data flow

1. The queue worker launches the orchestrator with env vars: `REWRITER_JOB_ID`,
   `REWRITER_QUEUE_URL`, `REWRITER_QUEUE_TOKEN` (the last two only when a queue server is
   configured).
2. The orchestrator's emitter appends every event to `<workspace>/events.jsonl` (always) and, when
   the env vars are present, POSTs it to `POST /jobs/<id>/events` on the queue server (best effort,
   short timeout, errors swallowed after one log line).
3. The queue server stores events per job at `<queue data>/events/<id>.jsonl`.
4. When a job ends, the worker runs `ai-org-orchestrator analyze` over that job's events. It writes
   an `Analysis` to `<queue data>/analysis/<id>.json`, and merges its feature requests into
   `<queue data>/feature-requests.json`.
5. `rewriter-queue` and its MCP surface read all of this back.

## Work packages and file ownership

Each package owns its files exclusively. Touch a file you don't own only for a one- or two-line
hook, and say so in the coordination file.

### A. Store, server, client, CLI, MCP (`job-queue/`)

Owns: `job-queue/src/events.rs` (new), `server.rs`, `client.rs`, `backend.rs`, `main.rs`,
`mcp.rs`, `worker.rs`, `job-queue/Cargo.toml`.

- `events.rs`: append events for a job; read a job's events (with optional `kind`, `agent`, `since`
  filters); write/read an `Analysis`; load/merge/save the feature-request list (dedupe by `id`; an
  existing request keeps its `status`, gains new `source_job_ids` and `evidence`).
- Server routes (same bearer-token auth as the rest): `POST /jobs/<id>/events` (one event or a JSON
  array), `GET /jobs/<id>/events`, `GET /jobs/<id>/analysis`, `GET /feature-requests`,
  `POST /feature-requests/<id>/status`.
- CLI: `events <id>`, `analysis <id>`, `requests`, `request <id> <open|accepted|done|rejected>`;
  all work against a local queue too (no server).
- MCP tools: `job_events`, `job_analysis`, `feature_requests`.
- Worker: set the three env vars when launching the orchestrator; when the job finishes (any
  outcome) run `<orchestrator> analyze --job-id <id> --events <path> --out <path>` and ingest the
  result. An analysis failure is logged and ignored.

### B. Emitter and agent reporting tool (`ai-org-orchestrator/`)

Owns: `ai-org-orchestrator/src/observer.rs` (new), `manager.rs`, `workspace.rs`, `tools.rs`,
`ai-org-orchestrator/Cargo.toml`, plus `skills/agentic-batching.md`-style reminder text for the new
tool (a new file `skills/reporting.md`).

- `observer.rs`: an `Observer` that appends to `events.jsonl` and posts to the queue as in the data
  flow. Thread-safe (jobs fan out milestones across threads). Never panics, never blocks a call for
  more than a short timeout.
- Emit events from: every model call (`Manager::run`, `run_agentic` turns summarised per call, with
  token and latency numbers), `Workspace::log_agent_decision`, `log_deviation`, every gate run
  (build/clippy/test and the integration gate), stage start/end, retries and escalations, errors.
- Add a `report_finding` tool to the agent toolbox (`severity`, `category`, `text`, `evidence`) so
  an agent can report something it noticed. It must be offered to every agentic role and described
  in the reminder text appended to agentic calls.
- Nothing about existing behaviour changes if the observer cannot write.

### C. Post-run analysis and feature requests (`ai-org-orchestrator/`, `skills/`)

Owns: `skills/post-run-analyst.md` (new), `ai-org-orchestrator/src/analysis.rs` (new), and the
`analyze` subcommand wiring in `main.rs` (a few lines, coordinate with B).

- `analyze --job-id N --events FILE --out FILE`: reads the events, computes the mechanical facts
  itself in Rust (per-agent calls, tokens, latency; retries per stage; gate failures per attempt;
  stages that never finished; outcome inferred from the last events), then asks a
  `PostRunAnalyst` agent to explain root causes and propose feature requests from those facts plus
  the findings, deviations and errors. Output is an `Analysis` JSON.
- The analyst must cite evidence for every root cause and feature request and may only cite events
  that exist. Feature request `id`s are stable kebab-case slugs of the title so the same problem in
  two jobs merges. If the model call fails, still write an `Analysis` containing the mechanical
  facts and no feature requests.
- Skill file in the same style as the existing skills; register the agent in `agents.rs` tiers
  (coordinate with B, one line each).

### D. Review, lint, tests (after A, B and C land)

Reviewers read the combined diff and fix what they find in the owning files; a lint agent runs
`cargo fmt --check`, `cargo clippy --all-targets` (no new warnings), `cargo test`, and the ast-grep
rules in `ai-org-orchestrator/lints`; a docs agent updates `README.md`, `docs/` and `CHANGELOG.md`.

## Coordination protocol

- Shared notes file: the path in the coordination section of your task. Append short, dated
  entries when you (a) need something from another package, (b) change or extend a shared
  interface, (c) touch a file you don't own, (d) find a problem in another package. Read it before
  you start and before you finish.
- Every package builds and tests on its own against the `run-events` crate as committed.
- Work in your own git worktree. Do not push. Commit nothing; leave changes in the worktree for
  the integrator.
- Match the surrounding code: no comments unless the why is non-obvious, no features beyond the
  package, no new warnings.

## Acceptance

- A job run through the queue produces `events.jsonl` in its workspace and the same events on the
  server, including at least one `agent_call`, `gate_result` and stage events.
- After the job ends, `rewriter-queue analysis <id>` returns an analysis and `rewriter-queue
  requests` lists any generated requests.
- With the queue server unreachable, a job still completes and still writes `events.jsonl`.
- All tests pass, clippy shows no new warnings, `cargo fmt --check` is clean.
