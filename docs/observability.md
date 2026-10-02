# Run observability

Every agent in a job reports what it did to one place, so a running job can be watched in
detail, a finished or failed job can be analysed without reading logs over SSH, and recurring
problems turn into feature requests. This is a structured event stream added on top of the
workspace files (`agents/<name>/...`, `deviations.md`), which are still written as before.

Reporting is best effort. A failure to record or send an event never fails or slows a job.

## What gets recorded

Each event is one JSON object (one line of JSONL), defined in the `run-events` crate:

| Field     | Meaning |
|-----------|---------|
| `ts`      | RFC 3339 timestamp, millisecond precision |
| `job_id`  | The queue job id (absent when the orchestrator is run outside the queue) |
| `stage`   | The pipeline stage that was current when the event was emitted, if any |
| `agent`   | Which agent emitted it (`orchestrator` for stage events, `QualityGate` for gate runs) |
| `kind`    | One of the kinds below |
| `summary` | A one-line description |
| `detail`  | Optional JSON object; keys depend on the kind |

| Kind          | When it is emitted | `detail` keys |
|---------------|--------------------|---------------|
| `stage_start` | A pipeline stage begins (`ObjectiveContract`, `Acceptance criteria`, `Test matrix refinement`, `Inductive analysis`, `Schema`, `V2 synthesis`). Starting a stage ends the previous one. | none |
| `stage_end`   | A stage ends | none |
| `agent_call`  | One per plain model call, and one per agentic call (tokens and latency are summed over its turns; the summary says how many turns) | `provider`, `model`, `input_tokens`, `output_tokens`, `latency_ms`, `ok` |
| `decision`    | An agent logged a decision | `text` (full text, capped at 4000 characters) |
| `deviation`   | A deviation from plan was logged | `text` |
| `finding`     | An agent called `report_finding` | `severity` (`info`, `warn`, `error`), `category`, `evidence` |
| `gate_result` | A quality-gate run (build, clippy, test, fmt) | `passed`, `error_bytes`, `attempt`, and `variant` for parallel milestone build directories |
| `retry`       | A retry | `text` |
| `error`       | An error, including a panic in the orchestrator (a panic hook records it) | `text` |

`attempt` counts consecutive gate runs against the same build directory since it last passed.
A failed model call is an `agent_call` with `ok: false`.

A stage with a `stage_start` and no `stage_end` means the process died in it. Stages that were
resumed from a checkpoint emit no stage events.

## Where events are stored

- **In the job's workspace**: `<workspace>/events.jsonl`. The orchestrator always appends here,
  whether or not a queue server is involved.
- **On the queue server**: `<queue data>/events/<id>.jsonl`, with `<id>` zero-padded to four
  digits like the job files. The orchestrator posts each event to `POST /jobs/<id>/events`.
- **Analyses**: `<queue data>/analysis/<id>.json`, written by the worker after a job ends.
- **Feature requests**: `<queue data>/feature-requests.json`, one list across all jobs.

`<queue data>` is `$REWRITER_QUEUE_DIR` (default `~/.local/share/rewriter/queue`).

When you read a job's events, the server's copy is used if it exists and the workspace file
otherwise, so a local queue (no server) works the same way.

## Environment variables

The worker sets these when it launches the orchestrator:

| Variable | Set when |
|----------|----------|
| `REWRITER_JOB_ID` | Always |
| `REWRITER_QUEUE_URL` | Only when the worker is hosted by `rewriter-queue serve` (a loopback URL for the bind address, so `0.0.0.0:8003` becomes `http://127.0.0.1:8003`) |
| `REWRITER_QUEUE_TOKEN` | Only when the worker is hosted by `serve` |

A standalone `worker` removes any `REWRITER_QUEUE_URL` and `REWRITER_QUEUE_TOKEN` it inherited, so
the orchestrator never posts anywhere unexpected. The orchestrator posts only when both the URL
and token are present and non-empty.

## CLI

All four commands work against a remote server or a local queue. Add `--json` to `events`,
`analysis` and `requests` for machine-readable output.

```sh
rewriter-queue events <id> [--kind <kind>] [--agent <name>] [--since <timestamp>] [--json]
```

Works while the job is running. One line per event, oldest first, in the form
`<ts> <kind> [<stage>] <agent> <summary>`:

```
2026-10-02T14:03:11.204Z stage_start [Schema] orchestrator Schema
2026-10-02T14:03:40.918Z agent_call [Schema] SchemaArchitect SchemaArchitect: 5120in/1873out in 29714ms
2026-10-02T14:05:02.377Z gate_result [V2 synthesis] QualityGate quality gate failed (attempt 1, 2048 bytes of errors)
```

(These lines are illustrative; the shape comes from the rendering code.) `--kind` takes the
snake_case kinds above, and `--since` returns only events strictly after that RFC 3339 timestamp.
With no events it prints `(no events)`. `--json` prints the raw JSONL instead.

```sh
rewriter-queue analysis <id> [--json]
```

```
job #12 failed

<summary>

Root causes:
- <cause> (evidence: <agent@ts>; ...)

Feature requests:
- <id> (<title>)
```

The root-causes and feature-requests sections are omitted when empty. If the job has no analysis
yet the command errors with `no analysis for job <id>`.

```sh
rewriter-queue requests [--json]
```

```
<id> [<status> / <priority>] <title>
    jobs: #12, #15
    <rationale>
```

With none it prints `(no feature requests)`.

```sh
rewriter-queue request <id> <open|accepted|done|rejected>
```

Sets a request's status and prints it in the same format as `requests`. An unknown request id is
an error.

Server routes behind these (same bearer-token auth as the rest): `POST /jobs/<id>/events` (one
event object or a JSON array; the job must exist), `GET /jobs/<id>/events` (query `kind`,
`agent`, `since`; JSONL), `GET /jobs/<id>/analysis`, `GET /feature-requests`,
`POST /feature-requests/<id>/status` (body `{"status": "accepted"}`).

## MCP tools

`rewriter-queue mcp` exposes three new tools alongside the existing ones:

- **`job_events`**: `id` (required), optional `kind`, `agent`, `since`. Returns the events as
  JSONL, oldest first. Works on a running job.
- **`job_analysis`**: `id`. Returns the analysis as JSON; errors if the job has none yet.
- **`feature_requests`**: no arguments. Returns every request as JSON.

There is no MCP tool for changing a request's status; use the CLI.

## Post-run analysis

When a job ends, whatever the outcome, the worker runs
`ai-org-orchestrator analyze --job-id <id> --events <file> --out <file>` over that job's
events (the server's copy if present, else the workspace file). If neither exists the analysis
is skipped. It has a 15 minute timeout, and any failure is one `[worker] analysis ...` line in
the job's log; the job's outcome is never affected.

The analysis has two parts:

1. **Mechanical facts, computed in Rust**: model calls, failed calls, tokens and latency per
   agent; retries per stage; gate runs and each failed gate run (stage, attempt, error bytes);
   stages that never ended; and an inferred outcome. The outcome is `failed` if the last event
   is an `error`, a stage never ended, or the last `stage_end` has `detail.ok == false`;
   `cancelled` if the last event's summary mentions "cancel"; `failed` for an empty event
   file; otherwise `succeeded`.
2. **A `PostRunAnalyst` agent** is given those facts plus the findings, deviations and errors,
   and proposes a summary, root causes and feature requests.

If the model call fails or its reply is not valid JSON, the analysis is still written with the
mechanical summary and no root causes or feature requests.

### How claims are validated

The analyst must cite evidence for every root cause and feature request, and the code checks
each citation against the real events. A citation is accepted if it is either an `agent@ts`
reference matching an event's agent and timestamp exactly, or a quote of at least 8 characters
that appears (case-insensitively) in some event's summary or in a string in its `detail`.
Citations that match nothing are dropped. A root cause or request left with no valid citation
is dropped entirely. Surviving root causes are stored as one string,
`<text> (evidence: <cites>)`.

## Feature requests

A feature request has an `id`, `title`, `rationale`, `evidence`, `source_job_ids`, `priority`
(`low`, `medium`, `high`) and `status`.

**Deduplication.** The `id` is a kebab-case slug of the title (lowercase alphanumerics joined by
single hyphens), so the same title in two jobs gets the same id. When the worker merges an
analysis into `feature-requests.json`:

- a request with a new `id` is added, always as `open`, whatever the analyst said;
- a request whose `id` already exists keeps its current `status`, `title`, `rationale` and
  `priority`, and gains the new job in `source_job_ids` and any new `evidence` entries.

Within one analysis, repeated ids are collapsed to the first.

**Statuses.** The tool stores and shows the statuses but attaches no behaviour to them:

- `open`: newly raised, not yet looked at.
- `accepted`: you intend to build it.
- `done`: built.
- `rejected`: you decided against it. A later job that raises the same problem is merged into
  this request, which stays `rejected`.

Those meanings are the natural reading of the names; the code only stores the value you set.

## The `report_finding` tool

Agents running agentically (with the tool box) can call `report_finding` to record something
they noticed: a missing or misleading tool or skill, malformed input, a pattern that wasted
effort, a risk in the source. It does not change the agent's task or answer.

| Argument   | Required | Notes |
|------------|----------|-------|
| `severity` | yes | `info`, `warn` or `error` |
| `category` | yes | Short kebab-case label (lowercase letters, digits and hyphens, at most 60 characters), for example `missing-tool` |
| `text`     | yes | What was noticed; becomes the event summary (capped at 4000 characters) |
| `evidence` | no  | A quote or file/line reference |

Invalid arguments are returned to the agent as an error string so it can retry. The tool is
described in the reminder text appended to agentic calls. The result is a `finding` event.

## When the queue server is unreachable

The job is unaffected. Each event is first appended to `<workspace>/events.jsonl`. The post to
the server has a 2 second timeout; on a failure the orchestrator logs one line to stderr, then
pauses posting for 30 seconds before trying again. Events emitted during a pause are not
re-sent later, so the server's copy can have gaps (see Limitations). The workspace file is
complete as long as the workspace is writable; if it is not, one line is logged and events are
lost.

## Limitations

- The final integration step has no stage events of its own, because it runs inside the
  `V2 synthesis` stage. Escalations emit `retry` events, and stages resumed from a checkpoint
  emit no stage events, so a stage's measured duration can include a cached stage after it.
- `analyze` skips the startup qualification probes, so its provider ratings are the defaults.
  That is deliberate (it makes one short call), but it means the analyst call is not routed by
  measured quality.
- The feature-request file lock has only been exercised between threads in one process.
- Events dropped while the orchestrator is pausing posts after a failed POST are not re-sent to
  the server; the workspace `events.jsonl` still has them, and analysis merges both copies.
