use crate::manager::{Manager, CHUNK_TARGET, SPLIT_THRESHOLD};
use crate::agents::{
    FORMAL_METHODS_REVIEWER, IP_COUNSEL, MAINTENANCE_REVIEWER, MERGE_AGENT,
    PRODUCTION_READINESS_REVIEWER, SECURITY_REVIEWER, SECURITY_REVIEWER_FINAL,
};

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
    pub fn run_with_dual_review(
        &self,
        worker_name: &str,
        worker_system: &str,
        base_task: &str,
        max_iter: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        enum NextCall {
            Full(String),
            Patch(String), // the quality gate's error text to patch against last_output
        }

        let mut next = NextCall::Full(base_task.to_string());
        let mut last_output = String::new();

        for iteration in 0..max_iter {
            let raw = self.ws.checkpoint(&format!("dual_review/iter/{iteration}/worker"), || {
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
            let (clean, quality_errors) = self.quality_gate(&raw);
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
            let security = self.ws.checkpoint(&format!("dual_review/iter/{iteration}/security"), || {
                self.run("SecurityReviewer", SECURITY_REVIEWER, &review_input)
            })?;
            let maintenance = self.ws.checkpoint(&format!("dual_review/iter/{iteration}/maintenance"), || {
                self.run("MaintenanceReviewer", MAINTENANCE_REVIEWER, &review_input)
            })?;

            let security_ok = security.trim_start().to_uppercase().starts_with("COMPLIANT");
            let maintenance_ok = maintenance.trim_start().to_uppercase().starts_with("APPROVE");

            if security_ok && maintenance_ok {
                return Ok(last_output);
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

        self.ws.log_agent_decision(
            worker_name,
            "max iterations reached — accepting last output",
        )?;
        Ok(last_output)
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

    /// Run IP Counsel and Formal Methods Reviewer. Findings are always recorded as
    /// deviations — neither agent blocks synthesis.
    fn run_review_panel(
        &self,
        code: &str,
        contract: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let task = format!(
            "# ObjectiveContract\n{contract}\n\n# V2 Implementation\n```rust\n{code}\n```"
        );
        let ip = self.ws.checkpoint("review_panel/ip_counsel", || {
            self.run("IpCounsel", IP_COUNSEL, &task)
        })?;
        if !ip.trim_start().to_uppercase().starts_with("CLEAR") {
            self.ws
                .log_deviation("IpCounsel", &format!("IP concerns (deferred):\n{ip}"))?;
        }
        let fm = self.ws.checkpoint("review_panel/formal_methods", || {
            self.run("FormalMethodsReviewer", FORMAL_METHODS_REVIEWER, &task)
        })?;
        if !fm.trim_start().to_uppercase().starts_with("SOUND") {
            self.ws.log_deviation(
                "FormalMethodsReviewer",
                &format!("Soundness concerns (deferred):\n{fm}"),
            )?;
        }
        let readiness = self.ws.checkpoint("review_panel/production_readiness", || {
            self.run("ProductionReadinessReviewer", PRODUCTION_READINESS_REVIEWER, &task)
        })?;
        if !readiness.trim_start().to_uppercase().starts_with("READY") {
            self.ws.log_deviation(
                "ProductionReadinessReviewer",
                &format!("Production-readiness concerns (deferred):\n{readiness}"),
            )?;
        }
        Ok(())
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
            let code = self.run_with_dual_review(
                "TargetImplementer",
                worker_system,
                &full_task,
                max_iter,
            )?;
            self.run_review_panel(&code, contract)?;
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

        // Quality gate on merged output — one repair pass if it fails
        let (merged_clean, merge_quality_errors) = self.quality_gate(&merged_raw);
        let merged = if merge_quality_errors.is_empty() {
            merged_clean
        } else {
            eprintln!(
                "  [quality] merged output failed gate — running repair pass ({} bytes of errors)",
                merge_quality_errors.len()
            );
            self.ws.log_deviation(
                "MergeAgent",
                &format!("Merged output failed quality gate:\n{merge_quality_errors}"),
            )?;
            let repair_base_task = format!("# ObjectiveContract\n{contract}");
            let repaired_raw = self.ws.checkpoint("merge/repair", || {
                self.try_patch(
                    "TargetImplementer-Repair",
                    worker_system,
                    &repair_base_task,
                    &merged_clean,
                    &merge_quality_errors,
                    "merge repair",
                )
            })?;
            let (repaired_clean, repair_errors) = self.quality_gate(&repaired_raw);
            if !repair_errors.is_empty() {
                self.ws.log_deviation(
                    "TargetImplementer-Repair",
                    &format!("Repair pass still has quality issues (deferred):\n{repair_errors}"),
                )?;
            }
            repaired_clean
        };

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

        self.run_review_panel(&merged, contract)?;

        Ok(merged)
    }
}
