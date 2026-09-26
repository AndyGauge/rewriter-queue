# Changelog

All notable changes to this project are documented here.

## [0.5.0]

### Added

- Machine-learning-style effort estimation. MilestonePlanner now rates each milestone's
  difficulty on a continuous 1-255 `EFFORT:` scale — separate from the existing binary
  RISK gate, which only decides whether to split before attempting. Every time a
  milestone finishes (converged or not), what it actually cost — iterations used,
  wall-clock time, tokens — is appended to a calibration history file
  (`~/.local/share/rewriter/estimation-history.jsonl` by default,
  `REWRITER_ESTIMATION_HISTORY` to override). Before a milestone runs, and before a
  freshly-parsed plan starts executing, a least-squares fit of that growing history
  against effort produces a predicted iteration count / ETA / token cost, printed the
  same way the existing `[milestones]` planning summary is — so the same effort rating
  maps to a tighter prediction as more real milestones complete, on this machine, across
  every job, without needing a training pipeline: under `MIN_SAMPLES` (3) past records it
  falls back to a flagged, un-calibrated cold-start guess instead of pretending to a track
  record it doesn't have yet. Disabled by default for anything that constructs a `Manager`
  without opting in via `with_estimation_history` — every existing test does exactly
  that, so nothing but the real `main.rs` binary ever reads or writes real calibration
  state.

## [0.4.3]

### Fixed

- Escalating a milestone to MilestonePlanner for a re-split could silently discard that
  milestone's work entirely. Both escalation paths in `implement_one_milestone` (a
  HARD-risk milestone, and one that didn't converge in its iteration budget) `return`ed
  the recursive re-split's result directly and unconditionally — but MilestonePlanner
  can decline to split further, or answer with something `parse_milestone_plan` can't
  read into any milestones, either of which comes back as an empty map. Root-caused
  against a real run (job #14: "0 milestone(s) scheduled into 0 waves"). An empty
  re-split now falls through instead of being returned as-is: the HARD-risk path
  attempts the milestone directly (it was never attempted before escalating, so
  there's nothing else to fall back to), and the convergence-failure path keeps its own
  pre-escalation best-effort attempt rather than discarding it for nothing.

## [0.4.2]

### Fixed

- Agentic calls could exhaust their whole turn budget without ever answering, on a
  source tree with more than a handful of files. Root-caused against a real run's tool-
  call log: `objective_contract.md` read three times, `schema.rs` three times, one
  source file three times, `list_files` called five times — every single call one at a
  time, even though a turn can request several tool calls at once and `run_agentic`
  already executes all of them before the next turn. Nothing had ever told the model
  that. `run_agentic` now appends a one-time reminder whenever tools are offered:
  batch independent reads into the same turn, and don't re-request something already
  in your own history. Turn budget (`AGENTIC_MAX_TURNS`) also raised from 20 to 40 as a
  safety margin regardless.

## [0.4.1]

### Fixed

- The integration gate now escalates to MilestonePlanner instead of just failing. Every
  other bounded-retry point in this pipeline — a milestone that won't converge, a plan
  that didn't cover the full scope — goes back to the architect for a smarter fix once
  its own repair budget is exhausted; the integration gate was the one exception, just
  giving up after `max_repair_attempts` patch attempts. It now does the same thing: once
  patch-based repair alone can't clear the remaining errors, MilestonePlanner is handed
  the specific compiler/test failures and asked to plan targeted fix milestones — not a
  redesign, just exactly what the failures implicate — implemented through the same
  milestone machinery, merged in, and re-checked. Bounded at 2 escalation rounds, since
  each one costs a full milestone-implementation cycle, not one extra patch call.

## [0.4.0]

### Added

- Gherkin acceptance criteria and real system tests. A new stage, `AcceptanceCriteria`,
  runs right after the ObjectiveContract and turns it into real Gherkin — `Feature`/
  `Scenario`/`Given`/`When`/`Then` covering the contract's stated behaviors, named edge
  cases, and explicit exclusions — saved as a new artifact, `acceptance.feature`. A
  second new step, `SystemTestWriter`, turns those scenarios into real `#[test]`
  functions against the finished crate's actual public API (`tests/system_tests.rs`),
  written in before the existing integration gate runs — so a crate that compiles but
  doesn't actually do what the contract's own scenarios say fails the same hard gate a
  non-compiling crate already did, with no second gate to keep in sync.

### Changed

- **Breaking**: the mission is now a pipeline artifact like any other, not a
  special-cased file. It lives at `<workspace>/artifacts/mission.md` (was
  `<workspace>/mission.md`) and is read the same way as `objective_contract.md` or
  `schema.rs` — including being readable via the `read_artifact` tool every agentic
  stage already has. A run refuses to start at all without one, printing exactly what
  to create and where, instead of silently proceeding with no stated intent. Existing
  workspaces need their `mission.md` moved under `artifacts/` before resubmitting.

## [0.3.2]

### Added

- Agentic tool-calling. Every pipeline stage that used to get the whole V1 source tree
  (and every upstream artifact) pasted directly into its prompt — ObjectiveContract,
  test-matrix refinement, inductive analysis, schema design, and MilestonePlanner's own
  planning call — now reads what it actually needs on demand instead: `list_files`,
  `read_file`, `read_artifact`/`write_artifact`, and `fan_out` for delegating genuinely
  independent sub-questions to concurrent copies of itself. Implemented as real tool
  calling in `inference-providers` (Anthropic and OpenAI-compatible backends — the two
  paths actually in use — with the registry refusing to route a tool-bearing request to
  a provider that can't handle it, and pinning a multi-turn conversation to whichever
  provider served its first turn). `main.rs`'s stage sequence is unchanged; each stage
  just gets a source-directory path and a toolbox instead of a pre-read blob.
- Implementer pattern library. `TargetImplementer`'s core skill was becoming an
  ever-growing dumping ground for every domain-specific gotcha found in this project's
  history. Domain patterns (path handling, move semantics, error handling, trait
  objects) are now separate, optional files MilestonePlanner can prescribe per milestone
  by name — most milestones need none of them, so most calls stay small.

### Fixed

- Validated the tool-calling loop against a live backend and found a real bug before it
  shipped: the agentic turn budget was wired to the same `--max-iter` used for the
  review-revision loop, so two turns of legitimate file exploration could exhaust the
  entire budget before the model ever got to answer, failing the whole run. Turn budget
  is now a separate, more generous constant.
- `ensure_module_declarations` only ever added `mod` lines, never `pub use` re-exports.
  A milestone writes its file assuming a flat namespace (`crate::Error`, matching how
  the schema presents it) with no visibility into which sibling module will actually
  define that type — without a re-export, that reference doesn't resolve. Root-caused
  against a real run that failed the (now working) integration gate for exactly this
  reason; fixed by re-exporting every module's public items at the crate root, not just
  declaring the module.

## [0.3.1]

### Added

- Line-range patching. Every patch call site (`try_patch`, `try_patch_document`) offered
  only SEARCH/REPLACE — an exact-text match that fails whenever a model doesn't copy
  existing whitespace precisely, or the snippet isn't unique. `patch.rs` now also
  supports `## FILE: path:12-30` + `<<<<<<< NEW ... >>>>>>> NEW`, replacing an inclusive,
  1-indexed line range — a single number (`path:12`) for one line, `end == start - 1`
  (e.g. `12-11`) to insert before a line without removing anything, and appending past
  the last line the same way. Both forms are always available side by side through one
  shared `apply_patch()`, and can be mixed freely within a single patch response (edits
  to the same file are applied line-range-first, then SEARCH/REPLACE, both validated —
  overlapping or out-of-range line edits are rejected rather than guessed at). The
  "current content" shown in every patch prompt is now line-numbered so a model can
  actually target a line-range edit accurately.

## [0.3.0]

### Added

- Final integration gate. Every milestone (and every chunk in the legacy fan-out
  path) only ever had its own quality gate run against its own slice in isolation —
  nothing ever built the fully assembled crate together, so a crate with two
  milestones each writing their own version of the crate root could pass every
  individual check and still not compile. Now, once milestones are merged and module
  declarations wired up, the whole crate is built, linted, and tested for real; a
  failure goes through the same patch-and-retry loop as everything else and, if it
  still won't build after every attempt, the run fails outright instead of silently
  returning broken code as a "finished" V2.
- Soundness as a hard gate. `FormalMethodsReviewer` calling a program `UNSOUND` — a
  real panic on valid input, or a violated totality/determinism guarantee — used to
  only be logged as a deferred deviation, the same as a stylistic disagreement. It's
  now repaired the same way a build failure is (a targeted patch, re-verified against
  the build, reviewer asked again) and fails the run if it's still `UNSOUND` after
  every attempt.
- Scope gap-fill. A milestone plan can be wrong in a way no single milestone's own
  convergence failure ever reveals: it simply didn't enumerate enough milestones for
  the schema's full scope (three planned for a 10-iteration budget when the schema
  actually needed six). After the initial plan is implemented, `MilestonePlanner` is
  handed everything built so far and asked whether the contract and schema are fully
  covered; if not, it plans milestones for exactly the gap, which get implemented and
  merged the same way, and the question is asked again — up to three rounds before a
  deviation is logged instead of replanning indefinitely.
- Registry exploration. Time-decayed backoff (0.2.0) means a demoted provider's
  penalty fades even without new traffic, but a provider the router never actually
  picks still never gets a fresh error-rate/latency sample to prove it's recovered.
  Every 20th Hora-0 (lowest-stakes) call now goes to whichever eligible provider was
  least recently used instead of the top-ranked one, so demoted providers keep
  getting real data point refreshes; deeper, higher-stakes calls never explore.

### Fixed

- All three of the above were root-caused against a real run (job #10): a crate that
  didn't compile (`mod lib;` referencing a second, independently-written crate root)
  shipped anyway because nothing ever built the assembled whole, alongside real
  soundness violations (a double-verification panic, a non-deterministic timestamp)
  that were logged but never blocked delivery, and a milestone plan that silently
  under-scoped the work rather than planning for all of it.

## [0.2.0]

### Added

- Milestone dependency graph. `MilestonePlanner` can now write an explicit
  `DEPENDS_ON` line on each milestone: omit it and a milestone depends on the one
  before it (the default, fully sequential, unchanged from before); write
  `DEPENDS_ON: none` to declare a milestone genuinely independent; name specific
  milestones to wait on exactly those. The synthesis pipeline schedules the plan into
  waves with Kahn's algorithm — everything in a wave has its dependencies already
  satisfied, so same-wave milestones are provably safe to build at the same time.
  Wave count is the plan's critical path length, logged at plan time.
- Isolated per-milestone workspaces for parallel fan-out. When a wave has more than
  one milestone, each one gets its own build directory (seeded with the current
  `Cargo.toml`) before running concurrently, so simultaneous `cargo build`/`clippy`/
  `test` runs — and the quality gate's stale-file cleanup — can't race each other or
  the shared crate directory. A wave of one (the common case) still runs inline in the
  shared directory exactly as before.
- `Registry::snapshot()` / `ProviderSnapshot`: a point-in-time read of every provider's
  latency, error rate, backoff, quality score, and context-window status, for anything
  that wants to observe routing decisions instead of just trusting them.
- `coin_live` example demonstrating live provider scoring against real backends.

### Fixed

- A provider that failed once could get stuck out of rotation indefinitely: backoff
  only decayed on a *successful* call, but a sufficiently backed-off provider was
  never picked to try again, so it never got the chance to succeed and recover.
  Backoff now decays continuously with wall-clock time (a half-life), independent of
  whether the provider has been called since.
- Error rate was computed from whatever samples happened to exist, so one failure out
  of one call read as a 100% error rate and could add a full point to a provider's
  routing score even for a low-stakes (depth 0) request. Error rate now has a minimum
  sample-weight floor, so a provider needs a real track record, good or bad, before
  its error rate swings the score much.

## [0.1.0]

Initial release: resumable job queue (`rewriter-queue`), the multi-agent synthesis
pipeline (`ai-org-orchestrator`), the provider registry (`inference-providers`), a Zed
extension, and the reviewer skill set. Dual MIT/Apache-2.0 licensed, docs site, and
cross-platform release binaries built by CI.
