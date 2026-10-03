# Benchmarks and regression tests

The suite answers one question for every change to the pipeline, the prompts, the skills or the
model configuration: did it get slower or less effective? A change that fixes the one problem it
was written for must not quietly make five others worse.

It exists because the pipeline is not deterministic (it calls live models) and its failures are
subtle. Two real ones shaped it:

- A job (#26) reported `succeeded` and produced a program that did nothing. The contract copied a
  stub V1 and the contract reviewer was never shown the mission, so nothing checked the contract
  against what was asked for. The pipeline's own gates passed because an empty `main` builds, lints
  and tests cleanly. This is why pass or fail is decided by a hidden acceptance check, never by the
  pipeline's own verdict.
- Another job (#24) produced a correct rectangle drawing program that failed its own tests over one
  newline: the output ended in a doubled trailing newline. Exact comparison of stdout, stderr and
  exit code, with escaped output in the failure message, is how the suite sees this.

Both are now tasks (`rect` covers both), and every future bug report should become one.

## Two tiers

| Tier | What it is | Cost | When |
|------|------------|------|------|
| 1. Structure tests | Scripted-provider tests that pin down how the pipeline is wired | Free, deterministic, seconds | Every change, as part of `cargo test` |
| 2. Live benchmark | Real tasks run through the real pipeline against a live model, then checked by hidden acceptance checks | Real model time and tokens | On demand, before a release, or nightly on the inference box |

Tier 1 catches "a refactor removed a behaviour". Tier 2 catches "the behaviour is still there but
the results got worse".

## Tier 1: structure tests

The tests live in `ai-org-orchestrator/src/pipeline_structure/` and run with the rest of the
workspace:

    cargo test

Each test drives the real `Manager` over a scripted provider that answers from canned text keyed on
the calling agent's system prompt. It then asserts on which agents were called, in what order, what
was in their prompts, and on the emitted event stream. No model is involved. What they guard:

- **Plan review.** The plan reviewer runs before implementation and its corrected plan is the one
  implemented; an approval keeps the original plan; a review call that errors falls back to the
  original plan and the run continues; a nested plan's review is given the plans above it.
- **Debug and repair.** The first failed repair attempt does not consult the debugger but later ones
  do, and the diagnosis reaches the repair prompt; a failing debugger call does not stop the repair
  loop; escalation to the milestone planner happens only after the repair budget is spent and
  carries the diagnosis.
- **Contract review.** The contract reviewer is shown the mission and the contract it reviews; a
  rejected contract is revised with the reviewer's reasons and reviewed again; the entry point
  hands the mission to both the contract author and the reviewer. The last of these reads
  `main.rs` as text, because the contract task is built inline there, so it fails if that wiring is
  edited away.
- **Observability.** An unwritable events file, or an unreachable queue server, does not fail the
  run, and a healthy run records each model call as an event.
- **Call budget.** One easy milestone, taken from plan to production review, stays within a fixed
  number of model calls, and the events agree with the provider's own call count.

### Updating the call budget on purpose

The budget test uses `MAX_CALLS_HAPPY_PATH` in
`ai-org-orchestrator/src/pipeline_structure/budget.rs`. It is set one above what the pipeline uses
today, so adding an agent to a step on the happy path fails the test. If the extra call is
intended, raise the constant in the same change and say why in the commit message. The failure
message lists the roles that were called, so you can see which one is new.

## Tier 2: running the live benchmark

The binary is `rewriter-bench`, from the `bench` crate:

    cargo build --release -p bench
    target/release/rewriter-bench <command>

The commands are `run`, `compare`, `promote`, `validate-tasks` and `list`. Run it from inside the
repository checkout: the environment stamp records the git commit of the current directory.

### `run`

    rewriter-bench run [--tasks a,b] [--repeat N] [--label L] [--direct]
        [--queue-bin P] [--orchestrator-bin P] [--tasks-dir D] [--results-dir D] [--keep]

For each task and each repeat, it prepares a work directory holding the task's `source/` and
`mission.md`, runs the pipeline, reads the run's events, then runs the task's acceptance checks on
the finished crate and records the result.

| Flag | Meaning | Default |
|------|---------|---------|
| `--tasks a,b` | Comma-separated task names. An unknown name is a usage error. | all tasks |
| `--repeat N` | Runs per task. Must be at least 1. | 3 |
| `--label L` | A name for the run, used in the results file name | `run` |
| `--direct` | Run `ai-org-orchestrator` directly instead of going through the queue | off (queue mode) |
| `--queue-bin P` | The `rewriter-queue` binary (queue mode) | found on `PATH` |
| `--orchestrator-bin P` | The `ai-org-orchestrator` binary (`--direct`) | found on `PATH`; `--direct` fails if it is not found |
| `--tasks-dir D` | Where tasks live | `bench/tasks` |
| `--results-dir D` | Where results go | `bench/results` |
| `--keep` | Keep each run's work directory (workspace, output crate, orchestrator log in direct mode) | off, work directories are deleted |

The default is every task, three times each: five tasks makes fifteen pipeline runs. For a quick
check, name the tasks:

    rewriter-bench run --tasks rect --repeat 3 --label before-prompt-change
    rewriter-bench run --tasks hello-trivial,wc-lite --repeat 1 --label smoke
    rewriter-bench run --direct --orchestrator-bin target/release/ai-org-orchestrator --tasks rect

**Queue mode** submits each task with `rewriter-queue submit`, polls `rewriter-queue status` every
5 seconds, then reads the events with `rewriter-queue events <id> --json` (falling back to the
workspace `events.jsonl`). A job must be processed by something, so a worker (`rewriter-queue
worker`, or a `serve` setup) has to be running; the benchmark only submits and waits. If the output
crate is not in the workspace it is fetched with `rewriter-queue fetch`. A job that is still going
when its time budget runs out is cancelled and recorded as a timeout. The budget starts counting
when the job leaves the queued state, so waiting behind other jobs does not use it up.

**Direct mode** runs the orchestrator as a child process with `--workspace`, `--source` and
`--max-iter`, and a clock of the task's time budget. The pipeline outcome is `succeeded` if it
exits 0, `failed` otherwise, and `timeout` if it was killed. There is no job id.

`run` exits 0 whenever it completes, even if tasks failed. It prints a table of every run and the
path of the results file. A failing task is a result, not an error; use `compare` to decide.

### What a results file contains

One `run` writes one pretty-printed JSON file, `bench/results/<timestamp>-<label>.json` (the label
is sanitised to letters, digits, `-`, `_` and `.`). `bench/results/` is gitignored. The file has a
`label`, an environment stamp and one record per run.

Each record:

| Field | Meaning |
|-------|---------|
| `task`, `repeat` | Which task and which repeat (counted from 0) |
| `job_id` | The queue job id; null in direct mode |
| `passed` | The acceptance checks all passed and the run finished within its time budget |
| `failed_checks` | Names of the failed checks, or `setup`, `timeout` or `no-crate` |
| `pipeline_outcome` | `succeeded`, `failed`, `cancelled` or `timeout`, as the pipeline reported it. It never decides `passed`. |
| `wall_secs` | Wall-clock time for the pipeline run |
| `input_tokens`, `output_tokens` | Summed over every model call |
| `model_calls` | Number of model calls, including failed ones |
| `retries` | Number of retry events |
| `gate_failures` | Quality-gate runs that failed |
| `integration_attempts` | Runs of the final whole-crate gate (gate events marked `integration`) |
| `escalations` | Retries recorded by the milestone planner (a milestone sent back for a finer split) |
| `plan_revised` | The plan reviewer recorded a decision, meaning it changed the plan |
| `debugger_consulted` | The debug implementer made at least one call |
| `started` | When the run started |

The metrics are derived from the run's `events.jsonl` (see [run observability](observability.md)).
If no events could be read, they are zero.

The environment stamp records the git commit (short hash, of the directory you ran from), the
`bench` crate version, the names of the providers in `~/.config/rewriter/config.toml`, the host
name, the SHA-256 of the `rewriter-queue` and `ai-org-orchestrator` binaries, and a timestamp. A
value that could not be found is `unknown`; the binaries are hashed from the path you passed or
from `PATH`, so a stamp of `unknown` usually means the binary was not where the benchmark looked.
The stamp holds provider names only: it does not record model names or the inference server's
context settings, so note those in the `--label` when they matter.

### How to read results

A task passes a run only if its hidden acceptance checks all pass. Look at `failed_checks` first:
the names come from `task.toml`, so `no-doubled-trailing-newline` tells you what broke without
opening anything. The failure detail (what was expected and what was got, escaped so stray newlines
are visible) is printed to stderr while the run goes. To dig into a failure, re-run that task with
`--keep` and read the workspace, or use the `job_id` with `rewriter-queue events` and `analysis`.

A `pipeline_outcome` of `succeeded` with `passed: false` is the case the suite is built to catch:
the pipeline believed it was done and was wrong.

Compare runs by pass rate first, then by median time and tokens. A single run is a sample, not a
measurement: a live model gives a different answer each time, so look at the pass rate over
several repeats, not at one run.

`rewriter-bench list` shows the tasks (with iteration cap, time budget, tags and description) and
the results files in `bench/results/`.

## Comparing runs: `compare`

    rewriter-bench compare <baseline> <current> [--pass-rate-drop X] [--time-ratio X]
        [--token-ratio X] [--min-runs N]

It reads two results files, prints a per-task table (pass rate, median time and median tokens,
baseline to current, and a status), then lists any regressions with the job ids of both sides so
you can read the analysis of the run that got worse. The exit status is 0 when there are no
regressions, 1 when there are, and 2 for a usage error such as a missing argument.

### The policy, in plain words

Each task is judged on its own, using only tasks that are in the baseline.

- **Pass rate.** A regression when the pass rate falls by more than `--pass-rate-drop` (default
  0.2, that is 20 percentage points). With three repeats the rate moves in steps of a third, so a
  single extra failure out of three (100% to 67%) is a regression.
- **Time and tokens.** A regression when the current median exceeds the baseline median times the
  ratio (`--time-ratio` and `--token-ratio`, both default 1.5) and the rise is also larger than the
  baseline's own spread. The spread is the median absolute deviation of the baseline's runs, so a
  baseline that was itself noisy needs a bigger rise before it counts. A baseline of zero is never
  flagged.
- **`--min-runs`** (default 3). A task with fewer runs than this on either side is shown as
  `insufficient` and never fails the comparison. A single run therefore can never fail a
  comparison; the benchmark is too noisy for that.
- **A task missing from the current run** but present in the baseline is a regression, because
  silently dropping a task would hide a problem.
- **A task only in the current run** is shown as `new` and is not judged.
- **Improvements** (the mirror image of the rules above) are shown as `improved` and are
  informational only.

If the two runs used different provider names, the report opens with a model note saying results
across different models are not directly comparable. It is a note, not a failure; it is up to you
to decide whether the comparison still means anything.

### Promoting a baseline and gating a release

A baseline is a results file kept in the repository:

    rewriter-bench run --repeat 3 --label baseline-0.10.0
    rewriter-bench promote bench/results/<timestamp>-baseline-0.10.0.json

`promote` validates the file and writes it to `bench/baseline.json`, which is committed (unlike
`bench/results/`). To gate a release, run the suite on the release candidate and compare:

    rewriter-bench run --repeat 3 --label rc1
    rewriter-bench compare bench/baseline.json bench/results/<timestamp>-rc1.json

and fail the release on a non-zero exit. Run with the same `--repeat` as the baseline, on the same
model and server settings. Promote a new baseline deliberately, after a change that is meant to
shift the numbers, and say why in the commit.

## Adding a task

Add a directory under `bench/tasks/<name>/`; the directory name must match the `name` in
`task.toml`:

    bench/tasks/<name>/
      task.toml            the specification (below)
      mission.md           what the pipeline is asked to do
      source/              the V1 input; a stub crate for a greenfield task
      acceptance/          hidden from the pipeline
        tests/*.rs         cargo integration tests, when cargo_tests = true
      solutions/
        good/              a complete crate that passes every check
        bad/               a complete crate that fails at least one check

`rewriter-bench` refuses to load a task that is missing `source/`, `mission.md`, `acceptance/`, or
a `Cargo.toml` in each of `solutions/good` and `solutions/bad`, or that has neither `[[check]]`
entries nor `cargo_tests`, or that has checks but no `binary`. The error names the task.

### `task.toml`

| Field | Meaning | Default |
|-------|---------|---------|
| `name` | Must equal the directory name | required |
| `description` | One line, shown by `list` | required |
| `max_iter` | Iteration cap passed to the pipeline | 10 |
| `time_budget_secs` | A run that takes longer is a failure, not a hang | 3600 |
| `tags` | Free-form labels; by convention one `difficulty:` tag | none |
| `binary` | The binary the finished crate must produce; required when there are `[[check]]` entries | none |
| `cargo_tests` | Also run `acceptance/tests/*.rs` as cargo integration tests | false |
| `[[check]]` | One run of the binary and an exact comparison (below) | none |

A check has a `name`, and optionally `args`, `stdin`, `expect_stdout`, `expect_stderr` and
`expect_exit`. Any `expect_*` you leave out is not checked. Comparison is exact, byte for byte, so
`expect_stdout = "+-+\n+-+\n"` is what catches a doubled trailing newline.

    name = "wc-lite"
    description = "Greenfield stdin counter printing lines, words and bytes"
    max_iter = 8
    time_budget_secs = 1800
    tags = ["greenfield", "cli", "stdin", "difficulty:easy"]
    binary = "wc-lite"

    [[check]]
    name = "no-trailing-newline"
    stdin = "a b c"
    expect_stdout = "0 3 5\n"
    expect_stderr = ""
    expect_exit = 0

### Writing a fair mission

The mission is the only thing the pipeline sees besides `source/`, so every assertion in the hidden
checks must follow from it. If a check expects a behaviour the mission does not state, a pass or
fail tells you nothing about the pipeline. In practice:

- State the exact bytes: the output format, the terminating newline, what goes to stderr, the exit
  status for each error, with examples.
- Define edge cases the checks exercise (empty input, no trailing newline, zero-size cases, bad
  arguments) and say what happens in each.
- Name the package and binary, and say standard library only: acceptance builds offline (see
  Limitations).
- Do not describe the checks or hint at the solution; the point is to see whether the pipeline gets
  there from the specification.

### Hidden checks

Everything under `acceptance/` and `solutions/` is kept out of the pipeline's way: the runner
copies only `source/` and `mission.md` into the run. To check a finished crate the runner copies it
to a scratch directory, runs `cargo build --offline`, runs each `[[check]]` against
`target/debug/<binary>` (10 seconds each), and, when `cargo_tests` is set, copies
`acceptance/tests/*.rs` into the crate's `tests/` and runs them (build and test steps are capped at
5 minutes each). A build failure is a single failed check named `build`. A failing cargo test is
reported under its own test name. Use `cargo_tests` for what exact output cannot express, such as
the structure requirements of the `stack-calc-refactor` task (a short `main.rs`, at least three
other source files).

### Known-good and known-bad solutions

Every task ships a full crate that passes (`solutions/good`) and one that fails
(`solutions/bad`). This proves the checks can tell the difference: a check that passes everything
checks nothing. For example, `rect`'s bad solution draws the right rectangle with a doubled
trailing newline. Run:

    rewriter-bench validate-tasks [--tasks-dir D]

It prints `ok` or `FAIL` for each task and exits 1 if any task fails. A task fails validation when
a solution directory is missing, when the good solution fails any check, when the bad solution
passes every check, or when either solution ran no checks. Run it whenever you add or edit a
task. Some tasks keep extra bad variants (for example `rect/solutions/bad-noop`, a program that
does nothing); `validate-tasks` only runs `good` and `bad`, so the extras are for reference.

## The tasks

| Task | What it tests | Time budget |
|------|---------------|-------------|
| `hello-trivial` | A trivial greeting program. It should be done in one milestone and must not be over-split. | 600s |
| `rect` | Greenfield ASCII rectangle program from a stub (`fn main() {}`). Regression for the doubled trailing newline and the program that did nothing. Eighteen checks cover corners in any order, degenerate shapes and every usage error. | 1800s |
| `wc-lite` | Greenfield stdin counter (lines, words, bytes). Edge cases: empty input, no trailing newline, repeated whitespace, CRLF, multibyte text counted as bytes. | 1800s |
| `contract-trap` | Refactor a crate whose doc comments and README describe stale behaviour. The mission says the true contract is the mission, not the comments, so a pipeline that follows the stale text fails. | 2400s |
| `stack-calc-refactor` | Restructure a single-file RPN calculator into modules with identical behaviour, including every error message and exit code, plus structure checks on the layout. | 2700s |

`rewriter-bench list` prints the current set with tags and iteration caps.

## Cost and time

These are not known yet. No baseline has been recorded, so there is no measured time or token cost
per task, and this page does not guess. What the code does tell you:

- The default run is every task three times, fifteen pipeline runs.
- The time budgets above add up to 9,300 seconds for one pass over all five tasks, so a default run
  is at most about 7.75 hours in the worst case where every run uses its whole budget. Typical runs
  should be well under that, but by how much is to be measured.
- Tokens are recorded per run (`input_tokens` and `output_tokens`), so the first real run gives you
  the numbers.
- Tier 1 costs nothing and takes seconds.

Until a baseline exists, use targeted runs (`--tasks`, `--repeat 1`) for development and keep full
runs for release checks. Recording the first baseline on the inference box is the next step; when it
exists, replace this section with the measured figures.

## Limitations

- **Live models vary.** The same task can pass and fail on consecutive runs. That is why `compare`
  uses medians and a spread, needs at least `--min-runs` runs per task, and never fails a
  comparison on a single run. A real regression can hide in a small number of repeats; raise
  `--repeat` when it matters.
- **Acceptance builds offline and uses the standard library only.** Checks run `cargo build
  --offline`, so a crate that needs a dependency that is not already in the local cargo cache
  fails to build. Missions should say "standard library only", and the tasks do.
- **Stamps are partial.** The environment stamp has provider names, not models or server context
  settings, so two runs on the same provider with a different model look identical to the model
  note in `compare`.
- **Tasks are small Rust programs.** The suite measures the pipeline on short, single-crate Rust
  tasks. It says little about large inputs or the merge path.
- **Queue mode needs infrastructure.** A queue and a worker must be running and pointed at a model;
  the benchmark does not start them.
