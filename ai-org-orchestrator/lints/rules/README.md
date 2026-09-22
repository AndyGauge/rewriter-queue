# Quality-gate lint plugins

Every `.yml` file in this directory is an [ast-grep](https://ast-grep.github.io/)
rule, loaded fresh by `quality_gate()` on each synthesis run via `ast-grep scan
--config lints/sgconfig.yml`. To add a check: drop a new rule file in here. No
Rust code changes, no recompiling the orchestrator — the next run picks it up.

These are **non-blocking**: matches are logged to `deviations.md`, the same as
the LLM review panel's findings (IpCounsel, FormalMethodsReviewer). They do not
fail the build or trigger a patch/regen retry the way a `cargo build`/`clippy`/
`test` failure does. That's deliberate — a structural pattern match can't tell
a genuine problem from a justified exception the way a compiler error can, so
it's a signal for a human (or the next synthesis run's mission) to weigh, not
a hard gate.

See `reflexive-multi-mutex.yml` for a worked example and
`https://ast-grep.github.io/guide/rule-config.html` for the full rule schema.
