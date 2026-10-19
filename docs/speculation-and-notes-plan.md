# Speculative work and a shared notes queue

Two ideas that need each other. **Speculation** uses channels nothing else is using to start later work from the best draft of its input, so the pipeline learns early. **The notes queue** is where everything the pipeline learns goes, and where every stage looks before it starts: feedback to earlier stages, coordination between workers, and "do not hit this footgun again". Without the notes, speculation produces answers nobody reads. Without speculation, the notes arrive too late to matter.

## Why now: what job #40 showed

Job #40 (the cairn finishing job) failed after 50 minutes in its *first* stage. The contract agent was given a mission that listed build-stage work, so it spent 55 turns writing test files and a `FLAGS.md` with `write_artifact` and never gave a final answer. It was one agent working while 7 of the server's 8 slots sat idle. Three things followed:

1. The work it did was good. `FLAGS.md` quotes the real V1 queries, and its three entries match the three the mission names. Nothing carried it to the stage that needed it.
2. Nothing stopped it. A circuit breaker extended the turn limit once and then gave up. A note like "the contract stage writes the contract only" would have been right on the first turn if anything had told it.
3. The next attempt starts knowing none of this, unless a person writes it into the mission.

## Built so far

Committed on the `sg-release` branch of the OSS repo; none of it released.

| Piece | Where | State |
|---|---|---|
| `channels` per provider; the pipeline counts concurrent requests a provider can serve, not providers | `inference-providers` | built, 46 tests |
| Requests in flight and free channels | `Registry::in_flight`, `free_channels` | built, tested |
| Cancel flag: a conversation stops at its next turn | `Manager::with_cancel` | built, not yet exercised end to end |
| Scheduler: priority order, only above a reserve of free channels, dependencies between tasks, stop | `speculate.rs` | built, 9 tests |
| Draft watcher: wait until a draft has stopped changing | `speculate::wait_for_draft` | built, tested |
| Acceptance criteria and test matrix run beside the inductive analysis and schema | `main.rs` | built, deployed, not yet seen on a real run |
| Speculation around the contract stage: later stages start from the settled draft; kept if the approved contract is the draft, discarded and cancelled if not | `main.rs` | built, deployed; first real run is job #41 |
| Crash safety: a marker lists early results; the next start removes unconfirmed ones | `main.rs` | built |

## The notes queue

A note is anything a worker wants someone else, now or later, to see. Submitting costs nothing and needs no review: "everyone just yolo submits". Quality comes from retrieval and from what repeats, not from gatekeeping on the way in.

```
Note { id, time, author { job, stage, agent }, kind, tags, text, evidence, status, hits }
kind   = feedback | coordination | footgun | fact
tags   = stage names, modules, files, functions, error classes
status = open | resolved | obsolete
hits   = times the same thing was submitted again, or retrieved and acted on
```

- **feedback**: something a later stage learned that an earlier one should have known ("implementing `month_summary` needed 27 attempts: its contract names six duties").
- **coordination**: who is doing what ("speculating on the schema from contract draft 3f2a"; "claimed: central function `round_half_even`"), so two workers do not both do it and a stale claim can be recognised.
- **footgun**: do not repeat this ("a repair that resets a function to `todo!()` wipes working neighbours"; "the contract stage writes the contract only").
- **fact**: stable things worth not rediscovering ("the V1 cancelled filter is `OrderStatus <> 'Cancelled'`, in `CairnBuild.bas`").

**Submitting.** An orchestrator tool `submit_note` next to `report_finding`, a `rewriter-queue note` command, an MCP tool, and an HTTP `POST /notes` on the queue server so other tools (`sg`, a person, another machine) use the same board. Automatic sources: quality-gate failures, reviewer rejections, `sg`'s failure ledger and blame results, cancelled or discarded speculation, post-run analysis findings, and the speculative probes below.

**Retrieving.** Before each agent starts, the harness searches the board with the task text, the stage, and any module or function names in it, and puts the top few under a heading such as "Notes from earlier work. They may be wrong or out of date; check them." Ranking is keyword relevance (BM25) with a boost for matching tags, a decay for age and a drop for resolved or obsolete notes. No embedding model is needed to start; if retrieval proves too weak, the servers can run one. A `search_notes` tool lets an agent look again mid-task. Retrieved notes are capped by tokens.

**Merging and promotion.** A near-duplicate adds a hit to the existing note instead of a new one. A footgun that keeps getting hit is the strongest signal on the board; past a threshold it is promoted into something that cannot be skipped: a line in the relevant skill, or an ast-grep rule in `ai-org-orchestrator/lints/rules/` (that directory already exists and is picked up without recompiling).

**What exists to build on.** `milestone_notes.md` already lets one milestone leave notes for the next, within a job. Events and `report_finding` already record findings per job, and feature requests are already merged across jobs. The board generalises these: across jobs, across tools, readable by every stage.

## Speculation and priority

Spare capacity is spent where the answer is most likely to change the plan: on work **other work depends on** and work whose outcome is **least predictable**. `speculate::priority(dependents, unknown)` is `(1 + dependents) * (0.25 + unknown)`.

- Today's tasks use fixed guesses (inductive analysis: 3 dependents, 0.7 unknown; schema 2, 0.8; test matrix 1, 0.9; acceptance criteria 1, 0.3).
- For implementation probes the numbers come from data: *dependents* is the transitive dependents of a function in the schema's call graph (the graph already exists in `snippet-graph`), and *unknown* rises with the size and branching of the V1 code it replaces, with the number of notes and past failures that mention it, and with having no test or precedent.
- A probe is an incremental implementation of one function on a draft schema, with its tests. Central functions (rounding, totals) go first because everything else rests on them; unknown ones go first because a surprise there is the expensive kind. Probes that pass are kept as seeds for synthesis; probes that fail become notes, and a failure in a central function goes to the schema stage as feedback.

## Tasks

| ID | Task | Depends on |
|---|---|---|
| **Notes queue** | | |
| N1 | Note schema and file-backed store on the queue server, `notes.jsonl`, with the HTTP `POST /notes` and `GET /notes` | none |
| N2 | `rewriter-queue note` and `notes`; MCP tools; orchestrator tools `submit_note` and `search_notes` | N1 |
| N3 | Retrieval: BM25, tag boost, age decay, status filter, token cap | N1 |
| N4 | Inject retrieved notes into every agent's first prompt, labelled as possibly stale | N3 |
| N5 | Merge near-duplicates and count hits | N1 |
| N6 | Automatic sources: gate failures, reviewer rejections, findings from events, post-run analysis | N2 |
| N7 | `sg` posts its failure ledger and blame results as notes, and reads them before a repair | N2, N3 |
| N8 | Promote repeated footguns into skills or ast-grep rules | N5 |
| N9 | Resolve and expire: mark a note resolved when a later run shows it no longer happens | N5, N6 |
| **Speculation** | | |
| S1 | Cancel flag, in-flight count, free channels, scheduler, draft watcher | done |
| S2 | Speculate the stages after the contract from its settled draft | done; confirm on job #41 |
| S3 | Record each speculation's outcome (kept, discarded, why) as a note | S2, N2 |
| S4 | Priority from data: dependents from the schema graph, unknown from V1 size and notes | N3 |
| S5 | Implementation probes from a settled schema draft, ranked by S4 | S2, S4 |
| S6 | Feed probe results back: notes for the architects and reviewers, schema feedback for failures in central functions | S5, N4 |
| S7 | Speculative review: start reviewers on a draft, keep if unchanged | S2 |
| **Guards** | | |
| W1 | A stage that is meant to produce one thing may not loop writing others: an allow-list of artifacts per stage, and a forced answer after a limit | none |
| W2 | Cancelled speculative agents are not reported as errors in the run's events | S2 |

Waves for parallel work: **1** N1, W1. **2** N2, N3, N5, S3 prep. **3** N4, N6, N7, S4. **4** S5, N8, N9. **5** S6, S7.

## Measuring it

- Wall time of a run with and without speculation, on the same mission and server; and the share of speculative results kept. A low share means the contract changes after its draft and the idea is not paying.
- Repeat rate of footguns: how often a note that was retrieved is followed by the same failure again. The notes queue is working if that falls.
- Total tokens spent on discarded work, so the price of a wrong guess is visible.

## Risks

- **Wrong notes poison later runs.** Hence the framing in the prompt, hits and resolution, expiry, and an eventual look at notes that were retrieved but did not help.
- **Speculation competes with the main path.** The reserve protects one channel; if the main path still slows, raise it. Cancel only takes effect at a turn boundary, so a wrong guess can cost about a turn.
- **The contract may change often after its draft.** Then the kept share is low and speculation should start from a later, more settled point (after the reviewer's first verdict).
- **Retrieval by keyword misses paraphrases.** Tags carry most of the weight at first.
- **A note board everyone writes to grows without bound.** Merge, expire and cap per query from the start.
