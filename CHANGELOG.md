# Changelog

All notable changes to this project are documented here.

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
