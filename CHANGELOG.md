# Changelog

All notable changes to this project are documented here.

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
