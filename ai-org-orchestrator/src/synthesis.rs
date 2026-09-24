use crate::manager::{Manager, CHUNK_TARGET, SPLIT_THRESHOLD};
use crate::agents::{
    FORMAL_METHODS_REVIEWER, IP_COUNSEL, MAINTENANCE_REVIEWER, MERGE_AGENT, MILESTONE_PLANNER,
    PRODUCTION_READINESS_REVIEWER, SECURITY_REVIEWER, SECURITY_REVIEWER_FINAL,
};
use std::collections::HashMap;

impl<'a> Manager<'a> {
    fn parallel_degree(&self) -> usize {
        self.registry.capable_heavy_slots().max(1)
    }

    /// Split Rust source into at most `target_count` chunks at top-level item boundaries.
    pub fn split_source_n(source: &str, target_count: usize) -> Vec<String> {
        let chunk_size = (source.len() / target_count.max(1)).max(CHUNK_TARGET);

        let is_top_level = |line: &str| {
            let t = line.trim_start();
            t.starts_with("pub fn ")
                || t.starts_with("fn ")
                || t.starts_with("pub async fn ")
                || t.starts_with("async fn ")
                || t.starts_with("impl ")
                || t.starts_with("pub impl ")
                || t.starts_with("pub struct ")
                || t.starts_with("pub enum ")
                || t.starts_with("struct ")
                || t.starts_with("enum ")
        };

        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();

        for line in source.lines() {
            if is_top_level(line) && current.len() >= chunk_size && !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            current.push_str(line);
            current.push('\n');
        }
        if !current.is_empty() {
            chunks.push(current);
        }
        if chunks.is_empty() {
            chunks.push(source.to_string());
        }
        chunks
    }

    pub fn split_source(source: &str) -> Vec<String> {
        Self::split_source_n(source, 1)
    }

    /// Run TargetImplementer with quality gate (build/clippy/test/modularity) then
    /// SecurityReviewer and MaintenanceReviewer. Quality gate runs first — LLM reviewers
    /// are skipped when the code does not compile or fails modularity.
    ///
    /// A quality-gate failure is a localized, compiler-shaped problem, so it's fixed with a
    /// SEARCH/REPLACE patch against the last output (see `patch.rs`) rather than a full
    /// rewrite — cheaper and faster, especially on a slow local model. An LLM review
    /// rejection (security/maintenance) is holistic prose critique with no fixed location,
    /// so that path still asks for a full regeneration, unchanged.
    /// `id_prefix` namespaces this call's checkpoints (`<id_prefix>/iter/<n>/...`) so multiple
    /// concurrent or nested callers — one per milestone, for instance — don't collide. `variant`
    /// selects which crate directory the quality gate builds against (see
    /// `Workspace::v2_dir`) — `None` for the default shared/sequential directory, `Some(id)`
    /// when this call is one participant in an explicit parallel fan-out wave and needs its
    /// own isolated workspace so its `cargo build` doesn't race a sibling's. Returns
    /// `(code, passed)`: `passed` is false when the iteration budget ran out before the quality
    /// gate and review both passed, so a caller can tell "this is done" from "this is a
    /// best-effort result I gave up on" and react differently (e.g. escalate for a finer split)
    /// instead of silently treating both the same way.
    pub fn run_with_dual_review(
        &self,
        id_prefix: &str,
        worker_name: &str,
        worker_system: &str,
        base_task: &str,
        max_iter: usize,
        variant: Option<&str>,
    ) -> Result<(String, bool), Box<dyn std::error::Error>> {
        enum NextCall {
            Full(String),
            Patch(String), // the quality gate's error text to patch against last_output
        }

        let mut next = NextCall::Full(base_task.to_string());
        let mut last_output = String::new();

        for iteration in 0..max_iter {
            let raw = self.ws.checkpoint(&format!("{id_prefix}/iter/{iteration}/worker"), || {
                match &next {
                    NextCall::Full(task) => self.run(worker_name, worker_system, task),
                    NextCall::Patch(errors) => self.try_patch(
                        worker_name,
                        worker_system,
                        base_task,
                        &last_output,
                        errors,
                        &format!("iteration {iteration}"),
                    ),
                }
            })?;
            let (clean, quality_errors) = self.quality_gate(&raw, variant);
            last_output = clean.clone();

            if !quality_errors.is_empty() {
                eprintln!(
                    "    [quality] iteration {} failed — feeding {} bytes of errors back",
                    iteration + 1,
                    quality_errors.len()
                );
                self.ws.log_deviation(
                    worker_name,
                    &format!("Iteration {iteration} — quality gate failed:\n{quality_errors}"),
                )?;
                next = NextCall::Patch(quality_errors);
                continue;
            }

            let review_input = format!("Review this output:\n\n{clean}");
            let security = self.ws.checkpoint(&format!("{id_prefix}/iter/{iteration}/security"), || {
                self.run("SecurityReviewer", SECURITY_REVIEWER, &review_input)
            })?;
            let maintenance = self.ws.checkpoint(&format!("{id_prefix}/iter/{iteration}/maintenance"), || {
                self.run("MaintenanceReviewer", MAINTENANCE_REVIEWER, &review_input)
            })?;

            let security_ok = security.trim_start().to_uppercase().starts_with("COMPLIANT");
            let maintenance_ok = maintenance.trim_start().to_uppercase().starts_with("APPROVE");

            if security_ok && maintenance_ok {
                return Ok((last_output, true));
            }

            let mut feedback_parts = Vec::new();
            if !security_ok {
                feedback_parts.push(format!("# SecurityReviewer\n{security}"));
            }
            if !maintenance_ok {
                feedback_parts.push(format!("# MaintenanceReviewer\n{maintenance}"));
            }
            let feedback = feedback_parts.join("\n\n");

            self.ws.log_deviation(
                worker_name,
                &format!("Iteration {iteration} — LLM review rejected:\n{feedback}"),
            )?;
            eprintln!(
                "    [review] rejected (iteration {}) security={} maintenance={}, revising...",
                iteration + 1,
                if security_ok { "ok" } else { "fail" },
                if maintenance_ok { "ok" } else { "fail" },
            );

            next = NextCall::Full(format!(
                "{base_task}\n\n# Review Feedback (iteration {iteration})\n{feedback}\n\n\
                 Address all feedback above. Record any new deviations — do not implement them."
            ));
        }

        Ok((last_output, false))
    }

    /// Ask for a minimal SEARCH/REPLACE patch against `last_output` that fixes
    /// `quality_errors`, and apply it. Falls back to a full regeneration — logged as a
    /// deviation — if the patch doesn't parse or its SEARCH text doesn't match the current
    /// file content unambiguously, so a bad patch never stalls the loop.
    fn try_patch(
        &self,
        worker_name: &str,
        worker_system: &str,
        base_task: &str,
        last_output: &str,
        quality_errors: &str,
        label: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let sections = Self::parse_file_sections(last_output);
        let patch_task = format!(
            "{base_task}\n\n\
             # Current Implementation\n{}\n\n\
             # Quality Gate Failures ({label})\n{quality_errors}\n\n\
             Fix ALL issues above with a minimal patch instead of rewriting the file(s). \
             For each fix, emit a block in exactly this form:\n\n\
             ## FILE: <path exactly as shown above>\n\
             <<<<<<< SEARCH\n\
             <exact existing text, including whitespace, that appears exactly once and \
             pinpoints the change>\n\
             =======\n\
             <replacement text>\n\
             >>>>>>> REPLACE\n\n\
             One such block per change — do not repeat unchanged code. Only emit a full \
             `// === path ===` file if you are adding a brand-new file not shown above.",
            Self::format_multi_file(&sections)
        );
        let patch_text = self.run(worker_name, worker_system, &patch_task)?;

        match crate::patch::apply_patch(&sections, Self::strip_fences(&patch_text)) {
            Ok(updated) => Ok(Self::format_multi_file(&updated)),
            Err(e) => {
                eprintln!(
                    "    [patch] {label} failed to apply ({e}) — falling back to full regeneration"
                );
                self.ws.log_deviation(
                    worker_name,
                    &format!("Patch failed to apply ({label}), fell back to full regeneration:\n{e}"),
                )?;
                let fallback_task = format!(
                    "{base_task}\n\n# Quality Gate Failures ({label})\n\
                     {quality_errors}\n\n\
                     Fix ALL issues above. The code must compile, pass `clippy -D warnings`, \
                     pass all tests, and keep every file ≤500 lines. \
                     Return only the complete corrected Rust source."
                );
                self.run(worker_name, worker_system, &fallback_task)
            }
        }
    }

    /// Build, lint, and test the *fully assembled* crate — every milestone's sections
    /// merged together, module declarations wired up, the works — and repair it against its
    /// own errors if it fails, up to `max_repair_attempts` times. Every milestone (and every
    /// chunk in the legacy fan-out path) only ever gets its own quality gate run against its
    /// own slice in an isolated or scratch directory (see `Workspace::v2_dir`); this is the
    /// first and only point where the whole crate as it will actually ship gets built and
    /// tested together, so it's the only place a cross-milestone problem — two milestones
    /// each writing their own version of the crate root, a module one file expects that
    /// another never produced — can even be seen, let alone fixed.
    ///
    /// This is a hard gate, not a deviation: a crate that still doesn't build or pass its
    /// own tests after every repair attempt is a failed run, returned as `Err` rather than
    /// silently handed back as if it were a finished V2. Delivering something that doesn't
    /// compile as a "success" is exactly the failure mode this closes.
    fn integration_check_and_repair(
        &self,
        mut sections: HashMap<String, String>,
        contract: &str,
        schema: &str,
        worker_system: &str,
        max_repair_attempts: usize,
    ) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
        let base_task = format!(
            "# ObjectiveContract\n{contract}\n\n# Schema\n```rust\n{schema}\n```\n\n\
             You are fixing build/lint/test failures in the FULLY ASSEMBLED crate, produced \
             by merging several independently-implemented pieces. A failure here can be \
             cross-file in a way no single piece's own author could have seen: two pieces may \
             each define their own version of the same type, or one module may reference \
             another that was never produced under the name it expects. Fix the actual cause \
             — prefer removing or merging a duplicate/conflicting definition over patching \
             around it."
        );

        for attempt in 0..max_repair_attempts {
            let code = Self::format_multi_file(&sections);
            let (clean, errors) = self.quality_gate(&code, None);
            if errors.is_empty() {
                return Ok(Self::parse_file_sections(&clean));
            }

            eprintln!(
                "    [integration] attempt {} — fully assembled crate failed the gate \
                 ({} bytes of errors), attempting a cross-file repair",
                attempt + 1,
                errors.len()
            );
            self.ws.log_deviation(
                "TargetImplementer-Integration",
                &format!(
                    "Integration attempt {attempt} — full-crate quality gate failed:\n{errors}"
                ),
            )?;

            let repaired_raw = self.ws.checkpoint(&format!("integration/repair/{attempt}"), || {
                self.try_patch(
                    "TargetImplementer-Integration",
                    worker_system,
                    &base_task,
                    &clean,
                    &errors,
                    &format!("integration attempt {attempt}"),
                )
            })?;
            sections = Self::parse_file_sections(&repaired_raw);
        }

        // The loop above only re-gates at the top of the *next* iteration, so the very last
        // repair attempt's result never gets checked without one final gate here.
        let code = Self::format_multi_file(&sections);
        let (clean, errors) = self.quality_gate(&code, None);
        if errors.is_empty() {
            return Ok(Self::parse_file_sections(&clean));
        }

        self.ws.log_agent_decision(
            "TargetImplementer-Integration",
            &format!(
                "Fully assembled crate still fails its quality gate after \
                 {max_repair_attempts} integration repair attempts — failing the run instead \
                 of delivering a crate that does not build:\n{errors}"
            ),
        )?;
        Err(format!(
            "V2 crate does not build/pass tests after {max_repair_attempts} integration \
             repair attempts:\n{errors}"
        )
        .into())
    }

    /// Run IP Counsel, the (hard-gated) Formal Methods soundness check, and Production
    /// Readiness. IP and production-readiness findings are always recorded as deviations —
    /// genuine judgment calls, not blocked on. Soundness is different (see
    /// `enforce_soundness`) and can repair `code`, so this returns the — possibly repaired —
    /// code the caller should actually ship, not just `()`.
    fn run_review_panel(
        &self,
        code: String,
        contract: &str,
        schema: &str,
        worker_system: &str,
        max_repair_attempts: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let review_task = |c: &str| {
            format!("# ObjectiveContract\n{contract}\n\n# V2 Implementation\n```rust\n{c}\n```")
        };

        let ip = self.ws.checkpoint("review_panel/ip_counsel", || {
            self.run("IpCounsel", IP_COUNSEL, &review_task(&code))
        })?;
        if !ip.trim_start().to_uppercase().starts_with("CLEAR") {
            self.ws
                .log_deviation("IpCounsel", &format!("IP concerns (deferred):\n{ip}"))?;
        }

        let code =
            self.enforce_soundness(code, contract, schema, worker_system, max_repair_attempts)?;

        let readiness = self.ws.checkpoint("review_panel/production_readiness", || {
            self.run("ProductionReadinessReviewer", PRODUCTION_READINESS_REVIEWER, &review_task(&code))
        })?;
        if !readiness.trim_start().to_uppercase().starts_with("READY") {
            self.ws.log_deviation(
                "ProductionReadinessReviewer",
                &format!("Production-readiness concerns (deferred):\n{readiness}"),
            )?;
        }
        Ok(code)
    }

    /// FormalMethodsReviewer is a hard gate, unlike the rest of the review panel: a program
    /// it calls UNSOUND has a real panic on valid input, or violates a totality/determinism
    /// guarantee the contract implies — that's a defect, not the kind of judgment call IP
    /// framing or production-readiness usually are (job #10's V2 panicked on double
    /// verification and returned a different timestamp for "the same" report on repeated
    /// calls — both UNSOUND, both shipped anyway, because this used to be deferred-only).
    ///
    /// Repairs the same way the integration gate does: a targeted patch against the
    /// reviewer's own finding, checkpointed so a resume doesn't redo it, re-verified against
    /// the build (a soundness fix can break compilation) before asking the reviewer again.
    /// Fails the run if it's still UNSOUND after every attempt, rather than shipping code
    /// with a known soundness violation.
    fn enforce_soundness(
        &self,
        mut code: String,
        contract: &str,
        schema: &str,
        worker_system: &str,
        max_repair_attempts: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let review_task = |c: &str| {
            format!("# ObjectiveContract\n{contract}\n\n# V2 Implementation\n```rust\n{c}\n```")
        };
        let repair_base_task = format!(
            "# ObjectiveContract\n{contract}\n\n# Schema\n```rust\n{schema}\n```\n\n\
             FormalMethodsReviewer found a soundness violation in the implementation below — a \
             real panic on valid input, or a violation of a totality/determinism guarantee the \
             contract implies. Fix the actual defect, not just the specific input the reviewer \
             happened to construct to demonstrate it."
        );

        for attempt in 0..max_repair_attempts {
            let fm = self.ws.checkpoint(&format!("review_panel/formal_methods/{attempt}"), || {
                self.run("FormalMethodsReviewer", FORMAL_METHODS_REVIEWER, &review_task(&code))
            })?;
            if fm.trim_start().to_uppercase().starts_with("SOUND") {
                return Ok(code);
            }

            eprintln!(
                "    [soundness] attempt {} — FormalMethodsReviewer found a violation, repairing",
                attempt + 1
            );
            self.ws.log_deviation(
                "FormalMethodsReviewer",
                &format!("Attempt {attempt} — soundness violation, repairing:\n{fm}"),
            )?;

            let repaired_raw =
                self.ws.checkpoint(&format!("review_panel/soundness_repair/{attempt}"), || {
                    self.try_patch(
                        "TargetImplementer-Soundness",
                        worker_system,
                        &repair_base_task,
                        &code,
                        &fm,
                        &format!("soundness attempt {attempt}"),
                    )
                })?;

            // A soundness fix can break the build -- re-verify before asking the reviewer
            // again, so it's never handed code the compiler would reject.
            let repaired_sections = self.integration_check_and_repair(
                Self::parse_file_sections(&repaired_raw),
                contract,
                schema,
                worker_system,
                max_repair_attempts,
            )?;
            code = Self::format_multi_file(&repaired_sections);
        }

        let fm = self.ws.checkpoint(&format!("review_panel/formal_methods/{max_repair_attempts}"), || {
            self.run("FormalMethodsReviewer", FORMAL_METHODS_REVIEWER, &review_task(&code))
        })?;
        if fm.trim_start().to_uppercase().starts_with("SOUND") {
            return Ok(code);
        }

        self.ws.log_agent_decision(
            "FormalMethodsReviewer",
            &format!(
                "Still UNSOUND after {max_repair_attempts} repair attempts — failing the run \
                 instead of delivering code with a known soundness violation:\n{fm}"
            ),
        )?;
        Err(format!(
            "V2 implementation still fails FormalMethodsReviewer's soundness check after \
             {max_repair_attempts} repair attempts:\n{fm}"
        )
        .into())
    }

    /// Synthesize V2: split if large (hourglass fan-out), then merge (converge).
    pub fn synthesize(
        &self,
        worker_system: &str,
        contract: &str,
        schema: &str,
        v1_source: &str,
        test_matrix: &str,
        inductive: &str,
        max_iter: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let full_task = format!(
            "# ObjectiveContract\n{contract}\n\n\
             # Inductive Analysis\n{inductive}\n\n\
             # Schema\n```rust\n{schema}\n```\n\n\
             # V1 Source\n```rust\n{v1_source}\n```\n\n\
             # Test Matrix\n{test_matrix}"
        );

        // Gated: capable_heavy_slots() reads live provider health, so re-evaluating it on
        // resume could answer differently than the original run and desync already-checkpointed
        // chunk work from a re-derived split. Every other branch below is a pure function of
        // checkpointed data and needs no gate.
        let degree = self.ws.gate_usize("fanout_degree", || self.parallel_degree())?;

        // Fan-out is only worthwhile when V1 source itself is large enough that a single
        // capable model can't handle it comfortably. The full_task includes constant context
        // (contract, schema, test matrix) that inflates its size regardless of V1 size.
        // Splitting a small V1 forces a merge step that degrades quality on weak merge models.
        if degree == 1 || v1_source.len() <= SPLIT_THRESHOLD {
            // synthesize_by_milestones already runs the fully-assembled crate through
            // integration_check_and_repair before returning, so `code` here is guaranteed to
            // build/lint/test clean — the review panel below only ever sees code that works.
            let code = self.synthesize_by_milestones(worker_system, contract, schema, &full_task, max_iter)?;
            let code = self.run_review_panel(code, contract, schema, worker_system, max_iter)?;
            return Ok(code);
        }

        let chunks = Self::split_source_n(v1_source, degree);
        let n = chunks.len();
        eprintln!("  splitting into {n} chunks ({degree} capable slots, parallel)");

        let chunk_tasks: Vec<(usize, String)> = chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| {
                let task = format!(
                    "# ObjectiveContract\n{contract}\n\n\
                     # Inductive Analysis\n{inductive}\n\n\
                     # Schema\n```rust\n{schema}\n```\n\n\
                     # V1 Chunk {}/{n}\n```rust\n{chunk}\n```\n\n\
                     Implement ONLY the items in this chunk. Other modules handled separately.",
                    i + 1,
                );
                (i, task)
            })
            .collect();

        let mut results: Vec<(usize, Result<String, String>)> = Vec::new();
        std::thread::scope(|s| {
            let handles: Vec<_> = chunk_tasks
                .iter()
                .map(|(i, task)| {
                    let agent = format!("TargetImplementer-{}", i + 1);
                    s.spawn(move || {
                        (
                            *i,
                            self.ws
                                .checkpoint(&format!("chunk/{i}/implement"), || {
                                    self.run_on_slot(&agent, worker_system, task, Some(*i))
                                })
                                .map_err(|e| e.to_string()),
                        )
                    })
                })
                .collect();
            for h in handles {
                results.push(h.join().unwrap_or_else(|_| (0, Err("thread panicked".into()))));
            }
        });

        results.sort_by_key(|(i, _)| *i);
        let mut partials: Vec<String> = Vec::new();
        for (_, result) in results {
            partials.push(result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?);
        }

        eprintln!("  merging {} partials", partials.len());
        let merge_task = format!(
            "# ObjectiveContract\n{contract}\n\n\
             # Schema\n```rust\n{schema}\n```\n\n\
             # Partial Implementations\n{}",
            partials
                .iter()
                .enumerate()
                .map(|(i, p)| format!("## Part {}\n```rust\n{p}\n```", i + 1))
                .collect::<Vec<_>>()
                .join("\n\n")
        );
        let merged_raw = self.ws.checkpoint("merge/raw", || {
            self.run("MergeAgent", MERGE_AGENT, &merge_task)
        })?;

        eprintln!("  final integration check (build + clippy + test on the merged crate)");
        let merged_sections = self.integration_check_and_repair(
            Self::parse_file_sections(Self::strip_fences(&merged_raw)),
            contract,
            schema,
            worker_system,
            max_iter,
        )?;
        let merged = Self::format_multi_file(&merged_sections);

        // Security review on merged (fan-out: record as deviation, cannot loop back)
        let review_chunks = Self::split_source(&merged);
        if review_chunks.len() == 1 {
            let review_task =
                format!("# ObjectiveContract\n{contract}\n\n# V2\n```rust\n{merged}\n```");
            let verdict = self.ws.checkpoint("security/single", || {
                self.run("SecurityReviewer", SECURITY_REVIEWER, &review_task)
            })?;
            if !verdict.trim_start().to_uppercase().starts_with("COMPLIANT") {
                self.ws.log_deviation(
                    "SecurityReviewer",
                    &format!("Non-compliant items deferred to next phase:\n{verdict}"),
                )?;
            }
        } else {
            let mut reviews: Vec<String> = Vec::new();
            for (i, chunk) in review_chunks.iter().enumerate() {
                let rt = format!(
                    "# ObjectiveContract\n{contract}\n\n# V2 Chunk {}/{}\n```rust\n{chunk}\n```",
                    i + 1,
                    review_chunks.len()
                );
                let r = self.ws.checkpoint(&format!("security/chunk/{i}"), || {
                    self.run(&format!("SecurityReviewer-{}", i + 1), SECURITY_REVIEWER, &rt)
                })?;
                reviews.push(r);
            }
            let agg = format!(
                "Aggregate these chunk reviews:\n\n{}",
                reviews.join("\n\n---\n\n")
            );
            let verdict = self.ws.checkpoint("security/aggregate", || {
                self.run("SecurityReviewer-Final", SECURITY_REVIEWER_FINAL, &agg)
            })?;
            if !verdict.trim_start().to_uppercase().starts_with("COMPLIANT") {
                self.ws.log_deviation(
                    "SecurityReviewer-Final",
                    &format!("Non-compliant items deferred:\n{verdict}"),
                )?;
            }
        }

        let maint_task = format!("Review this output:\n\n{merged}");
        let maint = self.ws.checkpoint("maintenance/verdict", || {
            self.run("MaintenanceReviewer", MAINTENANCE_REVIEWER, &maint_task)
        })?;
        if !maint.trim_start().to_uppercase().starts_with("APPROVE") {
            self.ws.log_deviation(
                "MaintenanceReviewer",
                &format!("Readability concerns on merged output (deferred):\n{maint}"),
            )?;
        }

        let merged = self.run_review_panel(merged, contract, schema, worker_system, max_iter)?;

        Ok(merged)
    }

    /// Cap on how many times a piece of work can be recursively re-split before we give up and
    /// implement it as-is. Without a cap, a MilestonePlanner that never settles could recurse
    /// forever; three levels is enough to turn "the whole crate" into pieces small enough for a
    /// single implementer attempt in practice, and a genuinely pathological milestone should
    /// surface as a deviation for a human, not loop.
    const MAX_SPLIT_DEPTH: usize = 3;

    /// Cap on how many times the plan as a whole can go back to MilestonePlanner for a second
    /// opinion on scope (`fill_scope_gaps`), distinct from `MAX_SPLIT_DEPTH` — that cap bounds
    /// re-splitting *one* milestone that's too big; this one bounds re-*planning* because the
    /// original decomposition simply didn't enumerate enough milestones in the first place
    /// (planner said 3 for an N=10 budget, the schema actually needed 6).
    const MAX_GAP_FILL_ROUNDS: usize = 3;

    /// Entry point: plan the work into milestones sized to `max_iter`, implement each one
    /// independently (checkpointed on its own, so a crash mid-way only redoes the milestone it
    /// was on), and merge their file sections into one crate. Replaces handing the whole crate
    /// to one `run_with_dual_review` call as a single all-or-nothing bet against the iteration
    /// budget — see `ai-org-orchestrator/lints` README and the root README's design notes for
    /// why that bet kept losing in practice.
    pub fn synthesize_by_milestones(
        &self,
        worker_system: &str,
        contract: &str,
        schema: &str,
        root_task: &str,
        max_iter: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut sections = self.implement_milestones(
            "root", root_task, worker_system, contract, schema, max_iter, 0, None,
        )?;
        ensure_module_declarations(&mut sections);

        let sections = self.fill_scope_gaps(sections, contract, schema, worker_system, max_iter)?;

        eprintln!(
            "==> Final integration check (build + clippy + test on the fully assembled crate)"
        );
        let sections =
            self.integration_check_and_repair(sections, contract, schema, worker_system, max_iter)?;
        Ok(Self::format_multi_file(&sections))
    }

    /// A milestone plan can be wrong in a way `implement_milestones`'s own per-milestone
    /// escalation never catches: not that one milestone was too big for its budget, but that
    /// the plan simply didn't enumerate enough milestones to begin with (three planned for an
    /// N=10 budget when the schema actually implies six — the planner under-counted the
    /// *scope*, not the difficulty of any one piece). Nothing about a single milestone
    /// converging cleanly tells you the plan as a whole was complete.
    ///
    /// So once the current plan's milestones are all implemented, go back to MilestonePlanner
    /// with what's actually been built and ask it to compare that against the contract and
    /// schema: if something required is still missing, it plans milestones for exactly the gap
    /// (reusing the same `implement_milestones` machinery — same wave scheduling, same isolated
    /// workspaces for genuine fan-out, same HARD/convergence-failure recursion), which get
    /// merged in and the question gets asked again. Stops as soon as a round finds nothing
    /// missing, or after `MAX_GAP_FILL_ROUNDS` rounds — a plan that still can't converge on
    /// "complete" that many times over gets a deviation instead of an unbounded planning loop.
    fn fill_scope_gaps(
        &self,
        mut sections: HashMap<String, String>,
        contract: &str,
        schema: &str,
        worker_system: &str,
        max_iter: usize,
    ) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
        for round in 0..Self::MAX_GAP_FILL_ROUNDS {
            let implemented = Self::format_multi_file(&sections);
            let gap_task = format!(
                "# ObjectiveContract\n{contract}\n\n# Schema\n```rust\n{schema}\n```\n\n\
                 # Already Implemented\n```rust\n{implemented}\n```\n\n\
                 Compare what's already implemented above against the ObjectiveContract and \
                 Schema. If every type, method, and behavior the schema requires already \
                 exists in the code above, emit no milestones at all — respond with exactly \
                 the single word NONE and nothing else. Otherwise, plan milestones — in the \
                 exact same format as before — covering ONLY what's still missing. Do not \
                 re-plan, re-describe, or duplicate anything already implemented above."
            );

            let id_prefix = format!("root/gap-fill-{round}");
            let added = self.implement_milestones(
                &id_prefix, &gap_task, worker_system, contract, schema, max_iter, 0, None,
            )?;

            if added.is_empty() {
                eprintln!(
                    "  [gap-fill] round {round}: scope is complete, nothing further missing"
                );
                return Ok(sections);
            }

            eprintln!(
                "  [gap-fill] round {round}: original plan under-scoped the work — planner \
                 added {} more file(s) to close the gap",
                added.len()
            );
            self.ws.log_agent_decision(
                "MilestonePlanner",
                &format!(
                    "Gap-fill round {round}: the milestone plan did not cover the full \
                     contract/schema scope on its own — added {} file(s) worth of milestones \
                     to close the gap.",
                    added.len()
                ),
            )?;

            sections.extend(added);
            ensure_module_declarations(&mut sections);
        }

        self.ws.log_deviation(
            "MilestonePlanner",
            &format!(
                "Scope may still be incomplete after {} gap-fill rounds — proceeding with what \
                 was implemented rather than looping indefinitely.",
                Self::MAX_GAP_FILL_ROUNDS
            ),
        )?;
        Ok(sections)
    }

    /// Ask MilestonePlanner to break `task` into milestones estimated to fit `max_iter` each,
    /// schedule them into dependency-respecting waves (see `schedule_waves`), and implement
    /// each wave — recursing on any milestone the planner still calls HARD, or that actually
    /// failed to converge within its budget, up to `MAX_SPLIT_DEPTH`.
    ///
    /// `variant` is the crate workspace this whole call operates in (see `Workspace::v2_dir`):
    /// `None` at the root, or `Some(id)` when this call is itself running inside one
    /// milestone's isolated workspace from an enclosing parallel wave. A wave with exactly one
    /// milestone runs inline in that same workspace — the default, and what an unmodified plan
    /// (or a planner that never writes `DEPENDS_ON`) always gets, matching the old fully
    /// sequential behavior exactly. A wave with more than one milestone is an explicit,
    /// planner-declared fan-out: those milestones have no dependency relationship in either
    /// direction, so they run at the same time, each seeded into its own fresh isolated
    /// workspace first (`Workspace::seed_variant_workspace`) so their concurrent `cargo build`
    /// runs — and the quality gate's stale-file cleanup — can't race each other or the shared
    /// directory.
    fn implement_milestones(
        &self,
        id_prefix: &str,
        task: &str,
        worker_system: &str,
        contract: &str,
        schema: &str,
        max_iter: usize,
        depth: usize,
        variant: Option<&str>,
    ) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
        let plan_task = format!("{task}\n\n# Iteration Budget\nN = {max_iter}");
        let plan_text = self.ws.checkpoint(&format!("{id_prefix}/plan"), || {
            self.run("MilestonePlanner", MILESTONE_PLANNER, &plan_task)
        })?;
        let milestones = parse_milestone_plan(&plan_text);
        let waves = schedule_waves(&milestones);

        eprintln!(
            "  [milestones] '{id_prefix}': {} milestone(s) scheduled into {} wave(s) \
             (critical path length {}): {}",
            milestones.len(),
            waves.len(),
            waves.len(),
            waves
                .iter()
                .map(|w| if w.len() == 1 {
                    milestones[w[0]].name.clone()
                } else {
                    format!(
                        "[fan-out: {}]",
                        w.iter().map(|&i| milestones[i].name.as_str()).collect::<Vec<_>>().join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join(" -> ")
        );

        let mut sections = HashMap::new();
        for wave in &waves {
            if wave.len() == 1 {
                let sub = self.implement_one_milestone(
                    id_prefix,
                    &milestones[wave[0]],
                    worker_system,
                    contract,
                    schema,
                    max_iter,
                    depth,
                    variant,
                )?;
                sections.extend(sub);
                continue;
            }

            eprintln!(
                "  [milestones] fan-out: running {} independent milestones in parallel, \
                 each in its own isolated workspace",
                wave.len()
            );
            let mut ms_variants: Vec<String> = Vec::with_capacity(wave.len());
            for &i in wave {
                let ms_variant = format!("{id_prefix}/{}", milestones[i].name);
                self.ws.seed_variant_workspace(variant, &ms_variant)?;
                ms_variants.push(ms_variant);
            }

            let results: Vec<Result<HashMap<String, String>, String>> = std::thread::scope(|s| {
                let handles: Vec<_> = wave
                    .iter()
                    .zip(ms_variants.iter())
                    .map(|(&i, ms_variant)| {
                        let milestone = &milestones[i];
                        s.spawn(move || {
                            self.implement_one_milestone(
                                id_prefix,
                                milestone,
                                worker_system,
                                contract,
                                schema,
                                max_iter,
                                depth,
                                Some(ms_variant.as_str()),
                            )
                            .map_err(|e| e.to_string())
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_else(|_| Err("thread panicked".into())))
                    .collect()
            });

            for r in results {
                sections.extend(r.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?);
            }
        }
        Ok(sections)
    }

    /// Implement (or split, or escalate) a single milestone. Factored out of
    /// `implement_milestones` so the exact same logic runs whether this milestone is the
    /// lone member of a sequential wave or one of several running concurrently in a fan-out
    /// wave — `variant` is the only thing that differs between those two calling contexts.
    fn implement_one_milestone(
        &self,
        id_prefix: &str,
        milestone: &Milestone,
        worker_system: &str,
        contract: &str,
        schema: &str,
        max_iter: usize,
        depth: usize,
        variant: Option<&str>,
    ) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
        let ms_id = format!("{id_prefix}/{}", milestone.name);

        if milestone.risk == Risk::Hard && depth < Self::MAX_SPLIT_DEPTH {
            eprintln!(
                "  [milestones] '{}' planned HARD at depth {depth} — splitting further",
                milestone.name
            );
            self.ws.log_deviation(
                "MilestonePlanner",
                &format!(
                    "Milestone '{}' estimated HARD for a {max_iter}-iteration budget — \
                     splitting into sub-milestones (depth {depth})",
                    milestone.name
                ),
            )?;
            return self.implement_milestones(
                &ms_id, &milestone.task, worker_system, contract, schema, max_iter, depth + 1,
                variant,
            );
        }

        let base_task = format!(
            "# ObjectiveContract\n{contract}\n\n# Schema\n```rust\n{schema}\n```\n\n\
             # Milestone: {}\n{}\n\n\
             # Scope\nImplement only the file(s) this milestone owns. Do not add or edit a \
             `mod` declaration for this milestone's own module in `main.rs`/`lib.rs` — that \
             wiring is added automatically after every milestone is done, specifically so \
             that milestones implemented independently never collide by both editing the \
             same shared entry file.",
            milestone.name, milestone.task
        );
        let (code, passed) = self.run_with_dual_review(
            &ms_id,
            "TargetImplementer",
            worker_system,
            &base_task,
            max_iter,
            variant,
        )?;

        if !passed && depth < Self::MAX_SPLIT_DEPTH {
            eprintln!(
                "  [milestones] '{}' did not converge in {max_iter} iterations — \
                 escalating for a finer split",
                milestone.name
            );
            self.ws.log_deviation(
                "MilestonePlanner",
                &format!(
                    "Milestone '{}' did not pass its quality gate within {max_iter} \
                     iterations as a single unit — splitting further (depth {depth})",
                    milestone.name
                ),
            )?;
            let retry_task = format!(
                "{}\n\n# Why This Needs Splitting\nA previous attempt implemented this as \
                 one unit and did not pass its quality gate within {max_iter} iterations. \
                 Split it into two or more smaller, independently-implementable pieces.",
                milestone.task
            );
            return self.implement_milestones(
                &ms_id, &retry_task, worker_system, contract, schema, max_iter, depth + 1,
                variant,
            );
        }

        if !passed {
            self.ws.log_agent_decision(
                "MilestonePlanner",
                &format!(
                    "Milestone '{}' still failing its quality gate at max split depth \
                     {depth} — accepting best effort",
                    milestone.name
                ),
            )?;
        }
        Ok(Self::parse_file_sections(&code))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Risk {
    Easy,
    Hard,
}

#[derive(Debug, Clone)]
struct Milestone {
    name: String,
    risk: Risk,
    task: String,
    /// Names of milestones (from the same plan) that must finish before this one starts.
    /// Empty means this milestone can start immediately — either because the plan explicitly
    /// said `DEPENDS_ON: none`, or (only for the very first milestone in a plan) because there
    /// was nothing before it to default to.
    depends_on: Vec<String>,
}

/// Group milestone indices into dependency-respecting waves (Kahn's algorithm topological
/// layering): every milestone in a wave has all of its dependencies satisfied by an earlier
/// wave, so same-wave milestones are guaranteed to have no dependency relationship in either
/// direction and are safe to build at the same time. The number of waves is the plan's
/// critical path length — the longest chain of milestones that must happen strictly in order,
/// regardless of how much fan-out the other waves have.
///
/// A milestone with no `DEPENDS_ON` line defaults (during parsing, see `parse_milestone_plan`)
/// to depending on the one immediately before it in plan order — so a plan that never uses the
/// field schedules into exactly one milestone per wave, in order, identical to the old
/// unconditionally-sequential behavior. Fan-out only happens where the plan explicitly says
/// two or more milestones don't depend on each other.
///
/// An unresolvable dependency name (a typo, or a genuine cycle) can never be satisfied; rather
/// than deadlock, any milestones left over once no further wave can be formed are force-
/// scheduled one at a time, in plan order, as their own single-milestone wave.
fn schedule_waves(milestones: &[Milestone]) -> Vec<Vec<usize>> {
    let name_to_idx: HashMap<&str, usize> =
        milestones.iter().enumerate().map(|(i, m)| (m.name.as_str(), i)).collect();
    let deps: Vec<Vec<usize>> = milestones
        .iter()
        .map(|m| m.depends_on.iter().filter_map(|n| name_to_idx.get(n.as_str()).copied()).collect())
        .collect();

    let mut scheduled = vec![false; milestones.len()];
    let mut waves = Vec::new();

    while scheduled.iter().any(|&s| !s) {
        let wave: Vec<usize> = (0..milestones.len())
            .filter(|&i| !scheduled[i] && deps[i].iter().all(|&d| scheduled[d]))
            .collect();

        if wave.is_empty() {
            let stuck = (0..milestones.len()).find(|&i| !scheduled[i]).unwrap();
            scheduled[stuck] = true;
            waves.push(vec![stuck]);
            continue;
        }
        for &i in &wave {
            scheduled[i] = true;
        }
        waves.push(wave);
    }
    waves
}

/// Parse MilestonePlanner's `## MILESTONE: <name>` / `RISK: EASY|HARD` / `TASK: ...` blocks.
/// Tolerant of a missing/garbled RISK line (defaults to Hard — forces a split rather than
/// silently trusting an ambiguous plan) and of TASK spanning multiple lines up to the next
/// milestone marker.
/// Register every top-level `src/*.rs` module in the crate's entry file (`src/main.rs`,
/// falling back to `src/lib.rs`), inserting only the `mod <name>;` lines that aren't already
/// present. This is deliberately mechanical, not an implementer's job: two milestones that each
/// need their own module registered are otherwise both trying to hand-edit the same shared
/// file, and since each milestone's `run_with_dual_review` loop only tracks its own in-memory
/// copy of what it last wrote, one milestone's edit to that file can silently diverge from what
/// another milestone (or this same file's own owning milestone, on a later iteration) has
/// since put on disk -- surfacing as "SEARCH text not found" patch failures with no single
/// milestone at fault. Doing this once, after every milestone's own file(s) are settled,
/// removes the shared file from contention entirely.
fn ensure_module_declarations(sections: &mut HashMap<String, String>) {
    let entry_path = if sections.contains_key("src/main.rs") {
        "src/main.rs"
    } else if sections.contains_key("src/lib.rs") {
        "src/lib.rs"
    } else {
        return; // no entry file yet -- nothing to wire up
    };

    let module_names: Vec<String> = sections
        .keys()
        .filter(|p| p.as_str() != entry_path)
        .filter_map(|p| {
            let rel = p.strip_prefix("src/")?;
            let name = rel.strip_suffix(".rs")?;
            (!name.contains('/')).then(|| name.to_string()) // direct children of src/ only
        })
        .collect();

    let entry = sections.get_mut(entry_path).unwrap();
    let mut missing: Vec<String> = module_names
        .into_iter()
        .filter(|name| {
            let declared = format!("mod {name};");
            let declared_pub = format!("pub mod {name};");
            !entry.contains(&declared) && !entry.contains(&declared_pub)
        })
        .collect();
    missing.sort();

    if !missing.is_empty() {
        let decls: String = missing.iter().map(|n| format!("mod {n};\n")).collect();
        *entry = format!("{decls}{entry}");
    }
}

/// One milestone as parsed before dependency defaults are resolved. `depends_on` is `None`
/// when the block had no `DEPENDS_ON:` line at all, distinct from `Some(vec![])` (an explicit
/// `DEPENDS_ON: none`) — only the former falls back to the "depends on the previous milestone"
/// default; the latter is a deliberate, explicit declaration of independence.
struct RawMilestone {
    name: String,
    risk: Risk,
    task: String,
    depends_on: Option<Vec<String>>,
}

fn parse_milestone_plan(text: &str) -> Vec<Milestone> {
    let mut raw: Vec<RawMilestone> = Vec::new();
    let mut name: Option<String> = None;
    let mut risk = Risk::Hard;
    let mut task = String::new();
    let mut depends_on: Option<Vec<String>> = None;

    let flush = |name: &mut Option<String>,
                 risk: &mut Risk,
                 task: &mut String,
                 depends_on: &mut Option<Vec<String>>,
                 out: &mut Vec<RawMilestone>| {
        if let Some(n) = name.take() {
            out.push(RawMilestone {
                name: n,
                risk: risk.clone(),
                task: task.trim().to_string(),
                depends_on: depends_on.take(),
            });
        }
        *risk = Risk::Hard;
        task.clear();
    };

    // Whether we're past a closing ``` fence and waiting for the next milestone marker.
    // Models often wrap the *whole* plan in one fence (closing it only after the last
    // milestone, with trailing commentary after that) rather than fencing each milestone
    // individually — without this, that trailing prose glues onto the last milestone's task.
    // Resetting on the next marker also keeps this correct if a model instead fences each
    // milestone separately.
    let mut suppressed = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("## MILESTONE:") {
            flush(&mut name, &mut risk, &mut task, &mut depends_on, &mut raw);
            name = Some(rest.trim().to_string());
            suppressed = false;
        } else if let Some(rest) = trimmed.strip_prefix("RISK:") {
            risk = if rest.trim().eq_ignore_ascii_case("EASY") { Risk::Easy } else { Risk::Hard };
        } else if let Some(rest) = trimmed.strip_prefix("DEPENDS_ON:") {
            let val = rest.trim();
            depends_on = Some(if val.is_empty() || val.eq_ignore_ascii_case("none") {
                Vec::new()
            } else {
                val.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
            });
        } else if let Some(rest) = trimmed.strip_prefix("TASK:") {
            task.push_str(rest.trim());
            task.push('\n');
        } else if trimmed == "```" {
            if name.is_some() {
                suppressed = true;
            }
        } else if name.is_some() && !suppressed && !trimmed.is_empty() {
            task.push_str(line);
            task.push('\n');
        }
    }
    flush(&mut name, &mut risk, &mut task, &mut depends_on, &mut raw);

    // Resolve the sequential default now that we know the full plan order: a milestone with
    // no DEPENDS_ON line depends on exactly the one before it, so an unmodified plan (or a
    // planner that never writes the field) schedules one-at-a-time, in order — identical to
    // the pre-fan-out behavior.
    raw.iter()
        .enumerate()
        .map(|(i, rm)| Milestone {
            name: rm.name.clone(),
            risk: rm.risk.clone(),
            task: rm.task.clone(),
            depends_on: rm.depends_on.clone().unwrap_or_else(|| {
                if i == 0 { Vec::new() } else { vec![raw[i - 1].name.clone()] }
            }),
        })
        .collect()
}

#[cfg(test)]
mod wiring_tests {
    use super::*;

    fn sections(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn adds_mod_declarations_for_every_other_top_level_file() {
        let mut s = sections(&[
            ("src/main.rs", "fn main() {}\n"),
            ("src/collector.rs", "pub struct Collector;\n"),
            ("src/database.rs", "pub struct Database;\n"),
        ]);
        ensure_module_declarations(&mut s);
        let main = &s["src/main.rs"];
        assert!(main.contains("mod collector;"));
        assert!(main.contains("mod database;"));
        assert!(main.contains("fn main() {}"));
    }

    #[test]
    fn does_not_duplicate_an_already_present_declaration() {
        let mut s = sections(&[
            ("src/main.rs", "mod collector;\nfn main() {}\n"),
            ("src/collector.rs", "pub struct Collector;\n"),
        ]);
        ensure_module_declarations(&mut s);
        assert_eq!(s["src/main.rs"].matches("mod collector;").count(), 1);
    }

    #[test]
    fn a_pub_mod_declaration_also_counts_as_already_present() {
        let mut s = sections(&[
            ("src/main.rs", "pub mod collector;\nfn main() {}\n"),
            ("src/collector.rs", "pub struct Collector;\n"),
        ]);
        ensure_module_declarations(&mut s);
        assert_eq!(s["src/main.rs"].matches("mod collector;").count(), 1);
    }

    #[test]
    fn no_entry_file_is_a_no_op() {
        let mut s = sections(&[("src/collector.rs", "pub struct Collector;\n")]);
        ensure_module_declarations(&mut s);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn falls_back_to_lib_rs_when_there_is_no_main_rs() {
        let mut s = sections(&[
            ("src/lib.rs", "pub fn hello() {}\n"),
            ("src/collector.rs", "pub struct Collector;\n"),
        ]);
        ensure_module_declarations(&mut s);
        assert!(s["src/lib.rs"].contains("mod collector;"));
    }
}

#[cfg(test)]
mod milestone_tests {
    use super::*;

    #[test]
    fn parses_multiple_milestones_with_multiline_tasks() {
        let text = "\
## MILESTONE: research-database
RISK: EASY
TASK: Implement the ResearchDatabase struct and its add/get/verify methods,
covering the edge cases in section 3.1 of the contract.

## MILESTONE: research-workflow
RISK: HARD
TASK: Implement the whole workflow orchestration.
";
        let milestones = parse_milestone_plan(text);
        assert_eq!(milestones.len(), 2);
        assert_eq!(milestones[0].name, "research-database");
        assert_eq!(milestones[0].risk, Risk::Easy);
        assert!(milestones[0].task.contains("add/get/verify"));
        assert!(milestones[0].task.contains("edge cases"));
        assert_eq!(milestones[1].name, "research-workflow");
        assert_eq!(milestones[1].risk, Risk::Hard);
    }

    #[test]
    fn missing_or_garbled_risk_line_defaults_to_hard() {
        let text = "\
## MILESTONE: mystery
TASK: No RISK line was given at all.
";
        let milestones = parse_milestone_plan(text);
        assert_eq!(milestones.len(), 1);
        assert_eq!(milestones[0].risk, Risk::Hard);
    }

    #[test]
    fn text_before_any_milestone_marker_is_discarded() {
        let text = "Some preamble the model wrote before the first marker.\n\n\
## MILESTONE: only-one\nRISK: EASY\nTASK: Just this.\n";
        let milestones = parse_milestone_plan(text);
        assert_eq!(milestones.len(), 1);
        assert_eq!(milestones[0].task, "Just this.");
    }

    /// Regression test for a real devstral response: the whole plan wrapped in one fence, with
    /// prose commentary after the closing fence -- that trailing prose used to glue onto the
    /// last milestone's task.
    #[test]
    fn trailing_prose_after_a_whole_plan_fence_does_not_leak_into_the_last_task() {
        let text = "Here's the decomposition:\n\n\
```\n\
## MILESTONE: first-one\n\
RISK: EASY\n\
TASK: Do the first thing.\n\
\n\
## MILESTONE: last-one\n\
RISK: EASY\n\
TASK: Do the last thing.\n\
```\n\
\n\
Each milestone is focused and independently implementable.\n";
        let milestones = parse_milestone_plan(text);
        assert_eq!(milestones.len(), 2);
        assert_eq!(milestones[1].name, "last-one");
        assert_eq!(milestones[1].task, "Do the last thing.");
        assert!(!milestones[1].task.contains("independently implementable"));
    }

    /// A model that fences each milestone separately (rather than the whole plan at once)
    /// must still parse all of them, not just the first.
    #[test]
    fn per_milestone_fencing_does_not_truncate_the_plan() {
        let text = "\
## MILESTONE: first-one\n\
RISK: EASY\n\
TASK: Do the first thing.\n\
```\n\
\n\
## MILESTONE: last-one\n\
RISK: EASY\n\
TASK: Do the last thing.\n\
```\n";
        let milestones = parse_milestone_plan(text);
        assert_eq!(milestones.len(), 2);
        assert_eq!(milestones[0].task, "Do the first thing.");
        assert_eq!(milestones[1].task, "Do the last thing.");
    }

    /// Not a synthetic example: a real devstral response to the actual MilestonePlanner
    /// prompt, captured while validating this feature. Locks in that the real, messier shape
    /// (whole-plan fence, bullet-list tasks, trailing summary prose) parses cleanly end to end.
    #[test]
    fn parses_a_real_captured_devstral_response() {
        let text = include_str!("testdata_real_milestone_response.txt");
        let milestones = parse_milestone_plan(text);
        assert_eq!(milestones.len(), 10);
        assert_eq!(milestones[0].name, "research-database-core");
        assert_eq!(milestones[9].name, "error-handling-types");
        for m in &milestones {
            assert_eq!(m.risk, Risk::Easy);
            assert!(!m.task.is_empty());
            assert!(!m.task.contains("```"), "fence leaked into {}: {}", m.name, m.task);
        }
        assert!(
            !milestones[9].task.contains("independently"),
            "trailing summary prose leaked into last milestone: {}",
            milestones[9].task
        );
        // No DEPENDS_ON line anywhere in this real response — must default to a strict chain.
        assert!(milestones[0].depends_on.is_empty());
        for i in 1..milestones.len() {
            assert_eq!(milestones[i].depends_on, vec![milestones[i - 1].name.clone()]);
        }
    }

    #[test]
    fn omitted_depends_on_defaults_to_the_previous_milestone() {
        let text = "\
## MILESTONE: first\nRISK: EASY\nTASK: First thing.\n\n\
## MILESTONE: second\nRISK: EASY\nTASK: Second thing.\n";
        let milestones = parse_milestone_plan(text);
        assert!(milestones[0].depends_on.is_empty());
        assert_eq!(milestones[1].depends_on, vec!["first".to_string()]);
    }

    #[test]
    fn depends_on_none_is_explicit_independence_not_the_default_chain() {
        let text = "\
## MILESTONE: first\nRISK: EASY\nTASK: First thing.\n\n\
## MILESTONE: second\nRISK: EASY\nDEPENDS_ON: none\nTASK: Second thing.\n";
        let milestones = parse_milestone_plan(text);
        assert!(milestones[1].depends_on.is_empty());
    }

    #[test]
    fn depends_on_parses_a_comma_separated_list_of_named_milestones() {
        let text = "\
## MILESTONE: a\nRISK: EASY\nDEPENDS_ON: none\nTASK: A.\n\n\
## MILESTONE: b\nRISK: EASY\nDEPENDS_ON: none\nTASK: B.\n\n\
## MILESTONE: c\nRISK: EASY\nDEPENDS_ON: a, b\nTASK: C needs both a and b.\n";
        let milestones = parse_milestone_plan(text);
        assert_eq!(milestones[2].depends_on, vec!["a".to_string(), "b".to_string()]);
    }

    fn milestone(name: &str, depends_on: &[&str]) -> Milestone {
        Milestone {
            name: name.to_string(),
            risk: Risk::Easy,
            task: String::new(),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn no_dependencies_at_all_schedules_one_wave_per_milestone_by_default() {
        // This is the shape parse_milestone_plan actually produces for an unmodified plan:
        // each milestone (after the first) explicitly depends on the one before it.
        let ms = [milestone("a", &[]), milestone("b", &["a"]), milestone("c", &["b"])];
        let waves = schedule_waves(&ms);
        assert_eq!(waves, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn independent_milestones_with_no_dependency_relationship_share_one_wave() {
        let ms = [milestone("a", &[]), milestone("b", &[]), milestone("c", &[])];
        let waves = schedule_waves(&ms);
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].len(), 3);
    }

    #[test]
    fn a_milestone_depending_on_two_independent_ones_waits_for_both() {
        // a and b are independent (wave 1, parallel); c depends on both (wave 2, alone).
        let ms = [milestone("a", &[]), milestone("b", &[]), milestone("c", &["a", "b"])];
        let waves = schedule_waves(&ms);
        assert_eq!(waves.len(), 2);
        let mut first = waves[0].clone();
        first.sort();
        assert_eq!(first, vec![0, 1]);
        assert_eq!(waves[1], vec![2]);
    }

    #[test]
    fn an_unresolvable_dependency_name_does_not_deadlock_scheduling() {
        // "typo-name" doesn't match any milestone -- must not block "a" forever.
        let ms = [milestone("a", &["typo-name"])];
        let waves = schedule_waves(&ms);
        assert_eq!(waves, vec![vec![0]]);
    }

    #[test]
    fn a_genuine_dependency_cycle_still_terminates_and_schedules_everything() {
        let ms = [milestone("a", &["b"]), milestone("b", &["a"])];
        let waves = schedule_waves(&ms);
        let scheduled: Vec<usize> = waves.iter().flatten().copied().collect();
        assert_eq!(scheduled.len(), 2);
    }
}

#[cfg(test)]
mod integration_gate_tests {
    use super::*;
    use crate::workspace::Workspace;
    use inference_providers::backends::Provider;
    use inference_providers::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
    use inference_providers::Registry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Always answers with the same canned text, regardless of the prompt -- enough to drive
    /// `integration_check_and_repair`'s control flow (gate fails -> patch -> gate again)
    /// without needing a real model.
    struct FixedReply {
        text: String,
        models: Vec<ModelInfo>,
        calls: Arc<AtomicUsize>,
    }

    impl Provider for FixedReply {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn complete(&self, _req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(InferenceResponse {
                text: self.text.clone(),
                input_tokens: 0,
                output_tokens: 0,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    fn mock_model() -> ModelInfo {
        ModelInfo {
            provider: "mock".into(),
            model: "mock".into(),
            tier: ModelTier::Heavy,
            cost_per_1k_input: 0.0,
            cost_per_1k_output: 0.0,
            context_limit: 200_000,
        }
    }

    fn temp_ws(tag: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("aoo-integration-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::new(dir).unwrap();
        ws.emit_artifact(
            "v2/Cargo.toml",
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        ws
    }

    fn sections(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn repairs_a_broken_assembled_crate_within_the_attempt_budget() {
        let ws = temp_ws("repair-ok");
        let calls = Arc::new(AtomicUsize::new(0));
        let patch = "## FILE: src/main.rs\n\
                     <<<<<<< SEARCH\n    undefined_fn();\n=======\n    println!(\"fixed\");\n\
                     >>>>>>> REPLACE\n";
        let provider: Box<dyn Provider> = Box::new(FixedReply {
            text: patch.to_string(),
            models: vec![mock_model()],
            calls: calls.clone(),
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let broken = sections(&[("src/main.rs", "fn main() {\n    undefined_fn();\n}\n")]);
        let fixed = mgr
            .integration_check_and_repair(broken, "contract", "schema", "worker system", 3)
            .expect("an assembled crate the model can actually fix should succeed");

        assert!(fixed["src/main.rs"].contains("println!(\"fixed\")"));
        // One patch call fixes it -- the gate passes on the very next check, no further calls.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fails_the_run_after_exhausting_repair_attempts_on_an_unfixable_crate() {
        let ws = temp_ws("repair-exhaust");
        // No SEARCH/REPLACE markers at all -- apply_patch can never match, so try_patch always
        // falls back to full regeneration, which this mock also answers with the same broken
        // source, so the crate can never actually get fixed.
        let broken_src = "fn main() {\n    undefined_fn();\n}\n";
        let provider: Box<dyn Provider> = Box::new(FixedReply {
            text: broken_src.to_string(),
            models: vec![mock_model()],
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let broken = sections(&[("src/main.rs", broken_src)]);
        let err = mgr
            .integration_check_and_repair(broken, "contract", "schema", "worker system", 2)
            .expect_err("a crate that can never be fixed must fail the run, not succeed");

        assert!(
            err.to_string().contains("does not build"),
            "expected a build-failure message, got: {err}"
        );
    }
}

#[cfg(test)]
mod soundness_gate_tests {
    use super::*;
    use crate::agents::FORMAL_METHODS_REVIEWER;
    use crate::workspace::Workspace;
    use inference_providers::backends::Provider;
    use inference_providers::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
    use inference_providers::Registry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Answers `FORMAL_METHODS_REVIEWER` requests distinctly from patch requests -- identified
    /// by the exact system prompt, since that's the one thing that differs between an
    /// `enforce_soundness` call to the reviewer and a call to the patcher.
    struct SequencedReply {
        formal_calls: Arc<AtomicUsize>,
        formal_replies: Vec<String>,
        patch_text: String,
        models: Vec<ModelInfo>,
    }

    impl Provider for SequencedReply {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            let text = if req.system == FORMAL_METHODS_REVIEWER {
                let n = self.formal_calls.fetch_add(1, Ordering::SeqCst);
                self.formal_replies[n.min(self.formal_replies.len() - 1)].clone()
            } else {
                self.patch_text.clone()
            };
            Ok(InferenceResponse {
                text,
                input_tokens: 0,
                output_tokens: 0,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    fn mock_model() -> ModelInfo {
        ModelInfo {
            provider: "mock".into(),
            model: "mock".into(),
            tier: ModelTier::Heavy,
            cost_per_1k_input: 0.0,
            cost_per_1k_output: 0.0,
            context_limit: 200_000,
        }
    }

    fn temp_ws(tag: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("aoo-soundness-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::new(dir).unwrap();
        ws.emit_artifact(
            "v2/Cargo.toml",
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        ws
    }

    #[test]
    fn repairs_an_unsound_program_and_confirms_soundness_before_returning() {
        let ws = temp_ws("repair-ok");
        let provider: Box<dyn Provider> = Box::new(SequencedReply {
            formal_calls: Arc::new(AtomicUsize::new(0)),
            formal_replies: vec!["UNSOUND\n\ntotality: panics".into(), "SOUND\n\nno issues".into()],
            patch_text: "## FILE: src/main.rs\n\
                         <<<<<<< SEARCH\n    println!(\"unsound\");\n=======\n    println!(\"sound\");\n\
                         >>>>>>> REPLACE\n"
                .into(),
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let code = "// === src/main.rs ===\nfn main() {\n    println!(\"unsound\");\n}\n".to_string();
        let result = mgr
            .enforce_soundness(code, "contract", "schema", "worker system", 2)
            .expect("a soundness violation the model can fix should succeed");

        assert!(result.contains("println!(\"sound\")"));
    }

    #[test]
    fn fails_the_run_when_the_program_is_still_unsound_after_every_repair_attempt() {
        let ws = temp_ws("repair-exhaust");
        let provider: Box<dyn Provider> = Box::new(SequencedReply {
            formal_calls: Arc::new(AtomicUsize::new(0)),
            // Always UNSOUND, no matter how many times it's asked.
            formal_replies: vec!["UNSOUND\n\ntotality: still panics".into()],
            patch_text: "## FILE: src/main.rs\n\
                         <<<<<<< SEARCH\n    println!(\"v1\");\n=======\n    println!(\"v2\");\n\
                         >>>>>>> REPLACE\n"
                .into(),
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let code = "// === src/main.rs ===\nfn main() {\n    println!(\"v1\");\n}\n".to_string();
        let err = mgr
            .enforce_soundness(code, "contract", "schema", "worker system", 1)
            .expect_err("a soundness violation that never resolves must fail the run");

        assert!(
            err.to_string().contains("soundness check"),
            "expected a soundness-failure message, got: {err}"
        );
    }
}

#[cfg(test)]
mod gap_fill_tests {
    use super::*;
    use crate::workspace::Workspace;
    use inference_providers::backends::Provider;
    use inference_providers::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
    use inference_providers::Registry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Dispatches by exact system prompt: MilestonePlanner gets a scripted sequence of
    /// replies (index by call count), reviewers always approve, and anything else
    /// (TargetImplementer) gets a fixed, self-contained, buildable two-file crate.
    struct RoleScripted {
        planner_calls: Arc<AtomicUsize>,
        planner_replies: Vec<String>,
        models: Vec<ModelInfo>,
    }

    impl Provider for RoleScripted {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            let text = if req.system == MILESTONE_PLANNER {
                let n = self.planner_calls.fetch_add(1, Ordering::SeqCst);
                self.planner_replies[n.min(self.planner_replies.len() - 1)].clone()
            } else if req.system == SECURITY_REVIEWER {
                "COMPLIANT".to_string()
            } else if req.system == MAINTENANCE_REVIEWER {
                "APPROVE".to_string()
            } else {
                // TargetImplementer: a complete, self-contained, buildable crate -- a real
                // milestone building alone (no prior wave's files on disk yet) must include
                // its own main.rs the same way, since Cargo needs one to build at all.
                "// === src/main.rs ===\nfn main() {}\n\
                 // === src/extra.rs ===\npub fn helper() -> i32 { 42 }\n"
                    .to_string()
            };
            Ok(InferenceResponse {
                text,
                input_tokens: 0,
                output_tokens: 0,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    fn mock_model() -> ModelInfo {
        ModelInfo {
            provider: "mock".into(),
            model: "mock".into(),
            tier: ModelTier::Heavy,
            cost_per_1k_input: 0.0,
            cost_per_1k_output: 0.0,
            context_limit: 200_000,
        }
    }

    fn temp_ws(tag: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("aoo-gap-fill-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::new(dir).unwrap();
        ws.emit_artifact(
            "v2/Cargo.toml",
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        ws
    }

    fn sections(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const MILESTONE_PLAN: &str = "\
## MILESTONE: extra-module\nRISK: EASY\nTASK: Add a missing helper module.\n";

    #[test]
    fn adds_a_missing_milestone_the_original_plan_left_out_then_stops() {
        let ws = temp_ws("adds-then-stops");
        let planner_calls = Arc::new(AtomicUsize::new(0));
        let provider: Box<dyn Provider> = Box::new(RoleScripted {
            planner_calls: planner_calls.clone(),
            planner_replies: vec![MILESTONE_PLAN.to_string(), "NONE".to_string()],
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let initial = sections(&[("src/main.rs", "fn main() {}\n")]);
        let result = mgr
            .fill_scope_gaps(initial, "contract", "schema", "worker system", 3)
            .expect("a fixable gap should resolve, not error");

        assert!(result.contains_key("src/extra.rs"));
        assert!(
            result["src/main.rs"].contains("mod extra;"),
            "gap-fill milestone's module was never wired into the entry file: {}",
            result["src/main.rs"]
        );
        // Round 0 found the gap and planned it; round 1 confirmed nothing else was missing.
        assert_eq!(planner_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn does_not_replan_at_all_when_the_first_round_already_says_nothing_is_missing() {
        let ws = temp_ws("already-complete");
        let planner_calls = Arc::new(AtomicUsize::new(0));
        let provider: Box<dyn Provider> = Box::new(RoleScripted {
            planner_calls: planner_calls.clone(),
            planner_replies: vec!["NONE".to_string()],
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let initial = sections(&[("src/main.rs", "fn main() {}\n")]);
        let result = mgr
            .fill_scope_gaps(initial.clone(), "contract", "schema", "worker system", 3)
            .expect("an already-complete plan must not error");

        assert_eq!(result, initial);
        assert_eq!(planner_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gives_up_after_max_rounds_instead_of_replanning_forever() {
        let ws = temp_ws("never-settles");
        let planner_calls = Arc::new(AtomicUsize::new(0));
        // Never says NONE, no matter how many times it's asked.
        let provider: Box<dyn Provider> = Box::new(RoleScripted {
            planner_calls: planner_calls.clone(),
            planner_replies: vec![MILESTONE_PLAN.to_string()],
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let initial = sections(&[("src/main.rs", "fn main() {}\n")]);
        let result = mgr
            .fill_scope_gaps(initial, "contract", "schema", "worker system", 3)
            .expect("exhausting gap-fill rounds must not fail the whole run");

        assert!(result.contains_key("src/extra.rs"));
        assert_eq!(planner_calls.load(Ordering::SeqCst), Manager::MAX_GAP_FILL_ROUNDS);

        let deviations = std::fs::read_to_string(ws.deviations_path()).unwrap();
        assert!(deviations.contains("may still be incomplete"));
    }
}
