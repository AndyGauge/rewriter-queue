# Benchmark and regression suite: plan

## Goal

Know, for every change to the pipeline, prompts, skills or model configuration, whether it got
slower or less effective. A change that looks good on the one problem it was written for must not
silently make five others worse.

The pipeline is non-deterministic (live models) and its failures are subtle (job #24 produced a
correct program that failed its own tests over one newline). So the suite has two tiers.

## Tier 1: structure tests (deterministic, free, run on every change)

Scripted-provider tests that pin down how the pipeline is wired, so a refactor cannot quietly
remove a behaviour: the plan reviewer runs before implementation and its corrected plan is used;
the debugger is consulted from the second failed integration attempt and its diagnosis reaches the
repair prompt; escalation happens after the repair budget and not before; a failed reviewer or
debugger call falls back instead of failing the job; observability failures never fail a job;
call counts and token budgets for a canned scenario stay inside recorded bounds.
These live next to the code as ordinary `cargo test` tests plus a `pipeline-structure` test file
that drives `Manager` end to end with scripted providers and asserts on the emitted event stream
(the run-events crate makes this cheap).

## Tier 2: live benchmark (real model, run on demand and nightly on the inference box)

### Task corpus: `bench/tasks/<name>/`

- `task.toml`: name, description, max_iter, tags, expected difficulty, time budget.
- `source/` (the V1 input, possibly a stub for greenfield tasks) and `mission.md`.
- `acceptance/`: a hidden check the pipeline never sees: a script or cargo tests run against the
  finished crate (build, run the binary on listed inputs, compare exact stdout/stderr/exit code).
  Pass/fail is decided here, never by the pipeline's own gates.
- Seed set, taken from real failures: the rectangle binary (trailing newline), a stub-source
  greenfield task, a small multi-module refactor, a task with a trap in the contract, a
  trivial task that must not be over-split. Add every future bug report as a task.

### Runner: a `rewriter-bench` binary

- `run [--tasks a,b] [--repeat N] [--label L]`: for each task and repeat, submit through the
  queue (or run the orchestrator directly with `--direct`), wait, collect results from the run's
  `events.jsonl` and analysis, then run the acceptance check on the output.
- Per run it records: task, repeat, pass/fail with the failing acceptance line, wall time,
  input/output tokens, model calls, retries, gate failures, integration attempts, escalations,
  whether plan review changed the plan, whether the debugger was consulted, and final outcome.
- Environment stamp stored with every result: git commit and version, provider/model names and
  server context settings, queue and orchestrator binary hashes, host.

### Results and comparison

- Results are appended as JSONL under `bench/results/` (gitignored) and a chosen run can be
  promoted to `bench/baseline.json` (committed).
- `compare <baseline> <new>`: with N repeats per task, compares pass rate and the median and
  spread of time and tokens. A regression is reported when the pass rate drops, or when median
  time or tokens rise more than a tolerance beyond the baseline's own spread. Single runs are
  reported but never fail the comparison (too noisy). Exit status is non-zero on a regression so it
  can gate a release.
- A short report: per-task table, what changed, and a link from each regression to the run's
  analysis so the cause can be read.

### Cost control

Default is a small smoke set with N=3. A full run is opt-in. Tasks carry a time budget; a task
that exceeds it is a failure, not a hang. The runner cleans up workspaces.

## Work packages

- A. Tier 1 structure tests (`ai-org-orchestrator` tests, `pipeline-structure`).
- B. Runner and results store (`bench/` crate `rewriter-bench`: run, record, environment stamp).
- C. Task corpus and acceptance checks (`bench/tasks/`), including a checker that proves each
  acceptance check passes on a known-good solution and fails on a known-bad one.
- D. Compare, baseline promotion and report.
- E. Review, lint, docs and a first real baseline run on the inference box.

## Acceptance

- `cargo test` includes the structure tests and fails if the plan-review or debug wiring is removed.
- `rewriter-bench run --tasks rect --repeat 3` produces a results file with pass rate and metrics.
- `rewriter-bench compare` flags an artificially worsened result (for example a prompt change that
  breaks the rectangle task) and passes an unchanged one.
- Every acceptance check is shown to pass a good solution and fail a bad one.
