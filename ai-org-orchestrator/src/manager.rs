use crate::{
    agents::{agent_hora_depth, agent_tier},
    workspace::Workspace,
};
use inference_providers::{InferenceRequest, Registry};
use std::sync::atomic::{AtomicU32, Ordering};

pub(crate) const SPLIT_THRESHOLD: usize = 100_000;
pub(crate) const CHUNK_TARGET: usize = 8_000;

pub struct Manager<'a> {
    pub ws: &'a Workspace,
    pub registry: &'a Registry,
    total_input_tokens: AtomicU32,
    total_output_tokens: AtomicU32,
}

impl<'a> Manager<'a> {
    pub fn new(ws: &'a Workspace, registry: &'a Registry) -> Self {
        Self {
            ws,
            registry,
            total_input_tokens: AtomicU32::new(0),
            total_output_tokens: AtomicU32::new(0),
        }
    }

    pub fn total_input_tokens(&self) -> u32 {
        self.total_input_tokens.load(Ordering::Relaxed)
    }
    pub fn total_output_tokens(&self) -> u32 {
        self.total_output_tokens.load(Ordering::Relaxed)
    }

    /// Run a single named agent: write task, call registry, write output, log tokens.
    pub fn run(
        &self,
        agent: &str,
        system: &str,
        task: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.run_on_slot(agent, system, task, None)
    }

    /// Like `run`, but with a slot hint for parallel fan-out.
    pub fn run_on_slot(
        &self,
        agent: &str,
        system: &str,
        task: &str,
        slot_hint: Option<usize>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.ws.set_agent_task(agent, task)?;

        let req = InferenceRequest {
            system: system.to_string(),
            user: task.to_string(),
            max_tokens: 16000,
            tier: agent_tier(agent),
            hora_depth: agent_hora_depth(agent),
            slot_hint,
        };

        let resp = self.registry.complete(&req)?;

        let total_in = self
            .total_input_tokens
            .fetch_add(resp.input_tokens, Ordering::Relaxed)
            + resp.input_tokens;
        let total_out = self
            .total_output_tokens
            .fetch_add(resp.output_tokens, Ordering::Relaxed)
            + resp.output_tokens;

        self.ws.log_agent_decision(
            agent,
            &format!(
                "tokens: +{}in/{}out — running total: {}in/{}out",
                resp.input_tokens, resp.output_tokens, total_in, total_out,
            ),
        )?;
        self.ws.write_agent_output(agent, &resp.text)?;

        eprintln!(
            "    [{}] {}in {}out (total {}in {}out)",
            agent, resp.input_tokens, resp.output_tokens, total_in, total_out,
        );

        Ok(resp.text)
    }

    /// Run an agent with a single reviewer gate. Retries up to `max_iter` times.
    ///
    /// A rejection is fixed with a SEARCH/REPLACE patch against the last output rather than
    /// a full rewrite -- same reasoning as the quality-gate retry loop in synthesis.rs: most
    /// review feedback is localized ("edge case X isn't covered"), and patching is cheaper
    /// and faster than regenerating the whole document every round. Falls back to full
    /// regeneration if the patch doesn't apply. Each iteration is checkpointed under
    /// `<worker_name>/iter/<n>/...` so an interruption only redoes the round it was on, not
    /// every round back to the start.
    pub fn run_with_review(
        &self,
        worker_name: &str,
        worker_system: &str,
        reviewer_name: &str,
        reviewer_system: &str,
        base_task: &str,
        approved_prefix: &str,
        max_iter: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut last_output = String::new();
        let mut feedback: Option<String> = None;

        for iteration in 0..max_iter {
            last_output = self.ws.checkpoint(
                &format!("{worker_name}/iter/{iteration}/worker"),
                || match &feedback {
                    None => self.run(worker_name, worker_system, base_task),
                    Some(review) => self.try_patch_document(
                        worker_name,
                        worker_system,
                        base_task,
                        &last_output,
                        review,
                        &format!("iteration {iteration}"),
                    ),
                },
            )?;

            let review = self.ws.checkpoint(
                &format!("{worker_name}/iter/{iteration}/review"),
                || {
                    self.run(
                        reviewer_name,
                        reviewer_system,
                        &format!("Review this output:\n\n{last_output}"),
                    )
                },
            )?;

            if review.trim_start().to_uppercase().starts_with(approved_prefix) {
                return Ok(last_output);
            }

            self.ws.log_deviation(
                reviewer_name,
                &format!("Iteration {iteration} rejection (continuing as planned):\n{review}"),
            )?;
            eprintln!(
                "    [{reviewer_name}] rejected (iteration {}), revising...",
                iteration + 1
            );

            feedback = Some(review);
        }

        self.ws.log_agent_decision(
            reviewer_name,
            "max iterations reached — accepting last output",
        )?;
        Ok(last_output)
    }

    /// Ask for a minimal SEARCH/REPLACE patch against `last_output` addressing
    /// `review_feedback`, and apply it. Falls back to a full regeneration -- logged as a
    /// deviation -- if the patch doesn't parse or its SEARCH text doesn't match unambiguously,
    /// so a bad patch never stalls the loop. Mirrors `Manager::try_patch` in synthesis.rs, but
    /// for a single document instead of a multi-file Rust `sections` map.
    fn try_patch_document(
        &self,
        worker_name: &str,
        worker_system: &str,
        base_task: &str,
        last_output: &str,
        review_feedback: &str,
        label: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        const DOC_KEY: &str = "document";
        let mut sections = std::collections::HashMap::new();
        sections.insert(DOC_KEY.to_string(), last_output.to_string());

        let patch_task = format!(
            "{base_task}\n\n\
             # Current Output\n{last_output}\n\n\
             # Review Feedback ({label})\n{review_feedback}\n\n\
             Address the feedback above with a minimal patch instead of rewriting the whole \
             document. Emit exactly one block per change:\n\n\
             ## FILE: document\n\
             <<<<<<< SEARCH\n\
             <exact existing text, including whitespace, that appears exactly once and \
             pinpoints the change>\n\
             =======\n\
             <replacement text>\n\
             >>>>>>> REPLACE\n\n\
             Record any new deviations you observe — do not implement them."
        );
        let patch_text = self.run(worker_name, worker_system, &patch_task)?;

        match crate::patch::apply_patch(&sections, Self::strip_fences(&patch_text)) {
            Ok(updated) => Ok(updated[DOC_KEY].clone()),
            Err(e) => {
                eprintln!(
                    "    [patch] {label} failed to apply ({e}) — falling back to full regeneration"
                );
                self.ws.log_deviation(
                    worker_name,
                    &format!(
                        "Patch failed to apply ({label}), fell back to full regeneration:\n{e}"
                    ),
                )?;
                let fallback_task = format!(
                    "{base_task}\n\n# Previous Review Feedback ({label})\n{review_feedback}\n\n\
                     Address the feedback above. Record any new deviations you observe — do not \
                     implement them."
                );
                self.run(worker_name, worker_system, &fallback_task)
            }
        }
    }
}
