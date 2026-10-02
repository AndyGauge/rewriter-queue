use crate::{
    agents::{agent_hora_depth, agent_tier},
    observer::AgentCall,
    tools::Toolbox,
    workspace::Workspace,
};
use inference_providers::{InferenceRequest, Registry, Turn};
use std::sync::atomic::{AtomicU32, Ordering};

pub(crate) const SPLIT_THRESHOLD: usize = 100_000;
pub(crate) const CHUNK_TARGET: usize = 8_000;

/// Turn budget for `run_agentic` calls, deliberately separate from `max_iter` (the
/// review/convergence-round budget). The two measure different things: `max_iter` bounds how
/// many times a worker gets to revise after a rejection; this bounds how many tool calls an
/// agentic call can make before it must produce a final answer. A real run found this the hard
/// way -- with `max_turns` wired to a `max_iter` of 2, MissionArchitect's first two turns were
/// both legitimate exploration (`list_files` then `read_file`), leaving zero turns to actually
/// answer, and the whole run failed despite the tool loop itself working perfectly.
///
/// 20 turned out to still be too tight once decoupled: a second real run against a 14-file
/// source tree burned all 20 without ever answering, and reading the actual tool-call log
/// showed why -- `objective_contract.md` read three times, `schema.rs` three times, one source
/// file three times, `list_files` five times, one call per turn throughout even though a turn
/// can request several at once. The turn-batching reminder in `run_agentic` (see
/// `agents::AGENTIC_BATCHING_REMINDER`) targets that waste directly; this higher ceiling is the
/// safety margin for whatever a model still doesn't follow -- exploring a real source tree can
/// legitimately take a couple dozen reads (and a `fan_out` call, which recurses with this same
/// budget) before there's enough context to answer at all.
///
/// It's a safety margin, not a guarantee: a third real run hit the exact same one-call-per-turn
/// pattern in a different role (TestEngineer, not MilestonePlanner this time) and still
/// exhausted all 40 turns -- re-reading one file six separate times across nine of them. The
/// reminder is a real mitigation, demonstrably not a fix; raising this constant further buys
/// more margin against a model that still won't batch, not a cure for it not batching.
pub(crate) const AGENTIC_MAX_TURNS: usize = 40;

/// Standard OpenAI sampling params (honored by llama.cpp's server too, confirmed live against
/// gx10) applied to every request, agentic or not: a fourth real run degenerated into calling
/// `read_artifact({"name":"schema.rs"})` 39 times verbatim in a row before the circuit breaker
/// even tripped, then kept calling it after the breaker's directive too -- a genuine stuck-
/// decoding repetition loop, not an ordinary reasoning mistake, and not something a prompt-level
/// reminder can fix (the model isn't choosing to repeat, it's stuck). These are a first attempt
/// at the values, not a tuned optimum -- frequency_penalty in particular grows with repeat
/// count, directly targeting "the same call 39 times" rather than a flat one-time nudge.
const FREQUENCY_PENALTY: f64 = 0.3;
const PRESENCE_PENALTY: f64 = 0.1;

/// How many extra turns `run_agentic`'s circuit breaker grants, once, when a call reaches
/// `AGENTIC_MAX_TURNS` without answering -- see the breaker's own doc comment on
/// `run_agentic`. Deliberately not "keep bumping forever": one extension, paired with a
/// fresh-context directive telling the agent specifically what it's already gathered and what
/// (if anything) is still missing, should be enough for a call that was making real progress;
/// a call that still can't converge after that is failing for a reason more turns won't fix.
const CIRCUIT_BREAKER_TURN_BUMP: usize = 15;

#[derive(Default)]
struct AgenticStats {
    provider: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    latency_ms: u64,
    turns: usize,
}

impl AgenticStats {
    fn record(&mut self, resp: &inference_providers::InferenceResponse) {
        self.provider = resp.provider.clone();
        self.model = resp.model.clone();
        self.input_tokens += u64::from(resp.input_tokens);
        self.output_tokens += u64::from(resp.output_tokens);
        self.latency_ms += resp.latency_ms;
        self.turns += 1;
    }
}

pub struct Manager<'a> {
    pub ws: &'a Workspace,
    pub registry: &'a Registry,
    total_input_tokens: AtomicU32,
    total_output_tokens: AtomicU32,
    /// Where per-milestone effort estimates get calibrated against actual outcomes (see
    /// `estimation.rs`). `None` (the default) disables estimation entirely rather than
    /// falling back to some machine-wide path picked implicitly -- every existing test
    /// constructs a `Manager` with `new()` and never opts in, so none of them read or write
    /// real calibration state; only `main.rs` calls `with_estimation_history` explicitly, and
    /// tests of the estimation feature itself opt in with their own temp path.
    pub(crate) estimation_history: Option<std::path::PathBuf>,
}

impl<'a> Manager<'a> {
    pub fn new(ws: &'a Workspace, registry: &'a Registry) -> Self {
        Self {
            ws,
            registry,
            total_input_tokens: AtomicU32::new(0),
            total_output_tokens: AtomicU32::new(0),
            estimation_history: None,
        }
    }

    pub fn with_estimation_history(mut self, path: std::path::PathBuf) -> Self {
        self.estimation_history = Some(path);
        self
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
            tools: Vec::new(),
            history: Vec::new(),
            frequency_penalty: Some(FREQUENCY_PENALTY),
            presence_penalty: Some(PRESENCE_PENALTY),
        };

        let started = std::time::Instant::now();
        let resp = match self.registry.complete(&req) {
            Ok(r) => r,
            Err(e) => {
                self.observe_failed_call(agent, started, &e.to_string());
                return Err(e.into());
            }
        };
        self.observe_call(agent, &resp);

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

    fn observe_call(&self, agent: &str, resp: &inference_providers::InferenceResponse) {
        if let Some(o) = self.ws.observer() {
            o.agent_call(&AgentCall {
                agent: agent.to_string(),
                provider: resp.provider.clone(),
                model: resp.model.clone(),
                input_tokens: resp.input_tokens.into(),
                output_tokens: resp.output_tokens.into(),
                latency_ms: resp.latency_ms,
                ok: true,
                note: String::new(),
            });
        }
    }

    fn observe_failed_call(&self, agent: &str, started: std::time::Instant, error: &str) {
        if let Some(o) = self.ws.observer() {
            o.agent_call(&AgentCall {
                agent: agent.to_string(),
                latency_ms: started.elapsed().as_millis() as u64,
                note: format!("call failed: {error}"),
                ..AgentCall::default()
            });
            o.error(agent, error);
        }
    }

    /// Run an agent with tool-calling access instead of pasting everything it might need
    /// straight into its prompt: it reads V1 source files, other stages' artifacts, and (via
    /// `fan_out`) delegates genuinely separable sub-questions to concurrent copies of itself,
    /// all on demand through `toolbox` (see `tools::PipelineToolbox`). Loops turn by turn
    /// until the model answers with no further tool calls, or the turn budget (`max_turns`,
    /// extended once by the circuit breaker below) is exhausted.
    ///
    /// The first turn is routed normally (`Registry::complete`, scored across every eligible
    /// tool-capable provider); every following turn in the same conversation is pinned to
    /// that same provider (`Registry::complete_pinned`), since the growing tool-call/tool-
    /// result history is encoded in that provider's own wire format and can't be replayed
    /// against a different one mid-conversation.
    ///
    /// **Turn-budget circuit breaker.** Reaching `max_turns` without an answer used to fail
    /// the call outright — but a real run (job #17, TestEngineer) burned 39 of 40 turns on
    /// genuine, if badly redundant, file exploration and then lost every bit of that work to a
    /// hard failure one turn short of ever trying to answer. `AGENTIC_BATCHING_REMINDER` is
    /// sent up front and evidently isn't always enough on its own. Instead of failing or just
    /// silently granting more turns, the breaker trips exactly once: a fresh-context call
    /// (`TurnBudgetSupervisor`, no conversation clutter of its own) is given the original task
    /// and a plain digest of every tool call made so far — including exact repeats, the
    /// clearest signal of the runaway pattern this exists to catch — and asked for one
    /// concrete directive. That directive replaces the passive reminder for the rest of the
    /// call, and the budget is extended by `CIRCUIT_BREAKER_TURN_BUMP`. If it still hasn't
    /// answered after that, it fails for real — one extension, not an unbounded one.
    pub fn run_agentic(
        &self,
        agent: &str,
        system: &str,
        task: &str,
        toolbox: &(dyn Toolbox + Sync),
        max_turns: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let started = std::time::Instant::now();
        let mut stats = AgenticStats::default();
        let result = self.run_agentic_inner(agent, system, task, toolbox, max_turns, &mut stats);
        if let Some(o) = self.ws.observer() {
            let err = result.as_ref().err().map(|e| e.to_string());
            let turns = stats.turns;
            o.agent_call(&AgentCall {
                agent: agent.to_string(),
                provider: stats.provider,
                model: stats.model,
                input_tokens: stats.input_tokens,
                output_tokens: stats.output_tokens,
                latency_ms: if stats.latency_ms > 0 {
                    stats.latency_ms
                } else {
                    started.elapsed().as_millis() as u64
                },
                ok: err.is_none(),
                note: match &err {
                    None => format!(
                        "{turns} turn(s), {}in/{}out",
                        stats.input_tokens, stats.output_tokens
                    ),
                    Some(e) => format!("failed after {turns} turn(s): {e}"),
                },
            });
            if let Some(e) = err {
                o.error(agent, &e);
            }
        }
        result
    }

    fn run_agentic_inner(
        &self,
        agent: &str,
        system: &str,
        task: &str,
        toolbox: &(dyn Toolbox + Sync),
        max_turns: usize,
        stats: &mut AgenticStats,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.ws.set_agent_task(agent, task)?;
        let tool_defs = toolbox.tool_defs();

        // Shared across every agentic role, not specific to whichever one first hit this --
        // see `agents::AGENTIC_BATCHING_REMINDER`'s own doc comment and skill file for why.
        let mut user = if tool_defs.is_empty() {
            task.to_string()
        } else {
            format!(
                "{task}\n\n{}\n\n{}",
                crate::agents::AGENTIC_BATCHING_REMINDER,
                crate::agents::AGENTIC_REPORTING_REMINDER
            )
        };
        let mut history: Vec<Turn> = Vec::new();
        let mut pinned_provider: Option<String> = None;
        let mut effective_max_turns = max_turns;
        let mut breaker_tripped = false;
        let mut turn = 0;

        loop {
            if turn >= effective_max_turns {
                if breaker_tripped {
                    return Err(format!(
                        "{agent} did not produce a final answer within {effective_max_turns} \
                         tool-calling turns (after one circuit-breaker extension)"
                    )
                    .into());
                }
                breaker_tripped = true;
                let digest = Self::summarize_tool_call_history(&history);
                let supervisor_task = format!(
                    "# Original Task\n{task}\n\n\
                     # Tool Calls Made So Far ({effective_max_turns} turns used)\n{digest}"
                );
                let directive = self
                    .run("TurnBudgetSupervisor", crate::agents::TURN_BUDGET_SUPERVISOR, &supervisor_task)
                    .unwrap_or_else(|e| {
                        eprintln!(
                            "    [{agent}] turn-budget supervisor call failed ({e}) -- falling \
                             back to a generic directive"
                        );
                        "You are almost out of turns. Stop reading and answer now with what \
                         you already have."
                            .to_string()
                    });
                eprintln!(
                    "    [{agent}] circuit breaker tripped at turn {effective_max_turns} -- \
                     extending by {CIRCUIT_BREAKER_TURN_BUMP} turns; directive: {directive}"
                );
                if let Some(o) = self.ws.observer() {
                    o.retry(
                        agent,
                        &format!("turn budget circuit breaker tripped after {effective_max_turns} turns"),
                    );
                }
                self.ws.log_deviation(
                    agent,
                    &format!(
                        "Turn budget circuit breaker tripped after {effective_max_turns} turns \
                         -- extended by {CIRCUIT_BREAKER_TURN_BUMP} with this supervisor \
                         directive:\n{directive}"
                    ),
                )?;
                user = format!("{task}\n\n{directive}");
                effective_max_turns += CIRCUIT_BREAKER_TURN_BUMP;
            }

            let req = InferenceRequest {
                system: system.to_string(),
                user: user.clone(),
                max_tokens: 16000,
                tier: agent_tier(agent),
                hora_depth: agent_hora_depth(agent),
                slot_hint: None,
                tools: tool_defs.clone(),
                history: history.clone(),
                frequency_penalty: Some(FREQUENCY_PENALTY),
                presence_penalty: Some(PRESENCE_PENALTY),
            };
            let resp = match &pinned_provider {
                None => self.registry.complete(&req)?,
                Some(name) => self.registry.complete_pinned(name, &req)?,
            };
            pinned_provider = Some(resp.provider.clone());
            stats.record(&resp);

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
                    "turn {turn}: tokens: +{}in/{}out — running total: {total_in}in/{total_out}out",
                    resp.input_tokens, resp.output_tokens,
                ),
            )?;

            if resp.tool_calls.is_empty() {
                eprintln!(
                    "    [{agent}] turn {turn}: {}in {}out (total {total_in}in {total_out}out), final answer",
                    resp.input_tokens, resp.output_tokens,
                );
                self.ws.write_agent_output(agent, &resp.text)?;
                return Ok(resp.text);
            }

            eprintln!(
                "    [{agent}] turn {turn}: {}in {}out, {} tool call(s)",
                resp.input_tokens,
                resp.output_tokens,
                resp.tool_calls.len()
            );
            let mut results = Vec::with_capacity(resp.tool_calls.len());
            for call in &resp.tool_calls {
                eprintln!("    [{agent}]   {}({})", call.name, call.arguments);
                let content = toolbox.call(&call.name, &call.arguments);
                results.push(inference_providers::ToolResult { id: call.id.clone(), content });
            }
            history.push(Turn::Assistant { text: resp.text, tool_calls: resp.tool_calls });
            history.push(Turn::ToolResults(results));
            turn += 1;
        }
    }

    /// Condense an agentic conversation's tool-call history into a plain-text digest for
    /// `TurnBudgetSupervisor`: every call made, and how many were exact repeats (same name and
    /// arguments) of an earlier one -- the clearest single signal of the runaway pattern the
    /// turn-budget circuit breaker exists to catch.
    fn summarize_tool_call_history(history: &[Turn]) -> String {
        let calls: Vec<String> = history
            .iter()
            .filter_map(|t| match t {
                Turn::Assistant { tool_calls, .. } => Some(tool_calls),
                Turn::ToolResults(_) => None,
            })
            .flatten()
            .map(|c| format!("{}({})", c.name, c.arguments))
            .collect();

        if calls.is_empty() {
            return "(no tool calls made yet)".to_string();
        }

        let mut seen = std::collections::HashSet::new();
        let repeats = calls.iter().filter(|c| !seen.insert(c.as_str())).count();
        format!(
            "{} total call(s), {repeats} of them an exact repeat of an earlier call:\n{}",
            calls.len(),
            calls.join("\n"),
        )
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
        toolbox: &(dyn Toolbox + Sync),
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut last_output = String::new();
        let mut feedback: Option<String> = None;

        for iteration in 0..max_iter {
            last_output = self.ws.checkpoint(
                &format!("{worker_name}/iter/{iteration}/worker"),
                || match &feedback {
                    None => self.run_agentic(worker_name, worker_system, base_task, toolbox, AGENTIC_MAX_TURNS),
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
            if let Some(o) = self.ws.observer() {
                o.retry(
                    worker_name,
                    &format!("{reviewer_name} rejected iteration {iteration}, revising"),
                );
            }

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
             # Current Output\n{}\n\n\
             # Review Feedback ({label})\n{review_feedback}\n\n\
             Address the feedback above with a minimal patch instead of rewriting the whole \
             document. {}",
            Self::format_multi_file(&sections),
            crate::patch::PATCH_FORMAT_INSTRUCTIONS,
        );
        let patch_text = self.run(worker_name, worker_system, &patch_task)?;

        match crate::patch::apply_patch(&sections, Self::strip_fences(&patch_text)) {
            Ok(updated) => Ok(updated[DOC_KEY].clone()),
            Err(e) => {
                eprintln!(
                    "    [patch] {label} failed to apply ({e}) — falling back to full regeneration"
                );
                if let Some(o) = self.ws.observer() {
                    o.retry(
                        worker_name,
                        &format!("patch failed to apply ({label}), regenerating in full"),
                    );
                }
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

#[cfg(test)]
mod agentic_tests;
