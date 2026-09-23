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
    /// concurrent or nested callers — one per milestone, for instance — don't collide. Returns
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
            let code = self.synthesize_by_milestones(worker_system, contract, schema, &full_task, max_iter)?;
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

    /// Cap on how many times a piece of work can be recursively re-split before we give up and
    /// implement it as-is. Without a cap, a MilestonePlanner that never settles could recurse
    /// forever; three levels is enough to turn "the whole crate" into pieces small enough for a
    /// single implementer attempt in practice, and a genuinely pathological milestone should
    /// surface as a deviation for a human, not loop.
    const MAX_SPLIT_DEPTH: usize = 3;

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
        let mut sections =
            self.implement_milestones("root", root_task, worker_system, contract, schema, max_iter, 0)?;
        ensure_module_declarations(&mut sections);
        Ok(Self::format_multi_file(&sections))
    }

    /// Ask MilestonePlanner to break `task` into milestones estimated to fit `max_iter` each,
    /// then implement every milestone — recursing on any the planner still calls HARD, or that
    /// actually failed to converge within its budget, up to `MAX_SPLIT_DEPTH`.
    fn implement_milestones(
        &self,
        id_prefix: &str,
        task: &str,
        worker_system: &str,
        contract: &str,
        schema: &str,
        max_iter: usize,
        depth: usize,
    ) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
        let plan_task = format!("{task}\n\n# Iteration Budget\nN = {max_iter}");
        let plan_text = self.ws.checkpoint(&format!("{id_prefix}/plan"), || {
            self.run("MilestonePlanner", MILESTONE_PLANNER, &plan_task)
        })?;
        let milestones = parse_milestone_plan(&plan_text);

        let mut sections = HashMap::new();
        for milestone in milestones {
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
                let sub = self.implement_milestones(
                    &ms_id,
                    &milestone.task,
                    worker_system,
                    contract,
                    schema,
                    max_iter,
                    depth + 1,
                )?;
                sections.extend(sub);
                continue;
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
                let sub = self.implement_milestones(
                    &ms_id,
                    &retry_task,
                    worker_system,
                    contract,
                    schema,
                    max_iter,
                    depth + 1,
                )?;
                sections.extend(sub);
                continue;
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
            sections.extend(Self::parse_file_sections(&code));
        }
        Ok(sections)
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

fn parse_milestone_plan(text: &str) -> Vec<Milestone> {
    let mut milestones = Vec::new();
    let mut name: Option<String> = None;
    let mut risk = Risk::Hard;
    let mut task = String::new();

    let flush = |name: &mut Option<String>, risk: &mut Risk, task: &mut String, out: &mut Vec<Milestone>| {
        if let Some(n) = name.take() {
            out.push(Milestone { name: n, risk: risk.clone(), task: task.trim().to_string() });
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
            flush(&mut name, &mut risk, &mut task, &mut milestones);
            name = Some(rest.trim().to_string());
            suppressed = false;
        } else if let Some(rest) = trimmed.strip_prefix("RISK:") {
            risk = if rest.trim().eq_ignore_ascii_case("EASY") { Risk::Easy } else { Risk::Hard };
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
    flush(&mut name, &mut risk, &mut task, &mut milestones);
    milestones
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
    }
}
