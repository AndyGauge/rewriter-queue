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
mod agentic_tests {
    use super::*;
    use crate::tools::Toolbox;
    use crate::workspace::Workspace;
    use inference_providers::backends::Provider;
    use inference_providers::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
    use inference_providers::{ToolCall, ToolDef};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct EchoToolbox;
    impl Toolbox for EchoToolbox {
        fn tool_defs(&self) -> Vec<ToolDef> {
            vec![ToolDef {
                name: "echo".into(),
                description: "echoes its input back".into(),
                parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            }]
        }
        fn call(&self, name: &str, arguments: &Value) -> String {
            assert_eq!(name, "echo");
            format!("echoed: {}", arguments["text"].as_str().unwrap_or(""))
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
        let dir = std::env::temp_dir().join(format!("aoo-manager-agentic-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Workspace::new(dir).unwrap()
    }

    /// Turn 0: makes an `echo` tool call. Every later turn: answers with final text that
    /// says whether the growing history actually contains that tool's result, proving the
    /// result made it back into the conversation rather than being dropped.
    struct ScriptedToolCaller {
        calls: Arc<AtomicUsize>,
        models: Vec<ModelInfo>,
    }
    impl Provider for ScriptedToolCaller {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn supports_tools(&self) -> bool { true }
        fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return Ok(InferenceResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "echo".into(),
                        arguments: json!({"text": "hello"}),
                    }],
                    input_tokens: 1,
                    output_tokens: 1,
                    provider: "mock".into(),
                    model: "mock".into(),
                    latency_ms: 1,
                });
            }
            let saw_result = req.history.iter().any(|t| {
                matches!(t, Turn::ToolResults(rs) if rs.iter().any(|r| r.content == "echoed: hello"))
            });
            Ok(InferenceResponse {
                text: if saw_result { "done".into() } else { "MISSING RESULT".into() },
                tool_calls: Vec::new(),
                input_tokens: 1,
                output_tokens: 1,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    #[test]
    fn run_agentic_executes_a_tool_call_and_feeds_the_result_back_into_the_conversation() {
        let ws = temp_ws("roundtrip");
        let provider: Box<dyn Provider> = Box::new(ScriptedToolCaller {
            calls: Arc::new(AtomicUsize::new(0)),
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let result = mgr.run_agentic("TestAgent", "system", "task", &EchoToolbox, 5).unwrap();
        assert_eq!(result, "done");
    }

    /// Always makes a tool call, never finishes -- used to prove `run_agentic` gives up
    /// cleanly after `max_turns` instead of looping forever.
    struct NeverFinishes {
        models: Vec<ModelInfo>,
    }
    impl Provider for NeverFinishes {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn supports_tools(&self) -> bool { true }
        fn complete(&self, _req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            Ok(InferenceResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "x".into(),
                    name: "echo".into(),
                    arguments: json!({"text": "again"}),
                }],
                input_tokens: 1,
                output_tokens: 1,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    #[test]
    fn run_agentic_gives_up_after_max_turns_instead_of_looping_forever() {
        let ws = temp_ws("exhausted");
        let provider: Box<dyn Provider> = Box::new(NeverFinishes { models: vec![mock_model()] });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let err = mgr.run_agentic("TestAgent", "system", "task", &EchoToolbox, 3).unwrap_err();
        assert!(err.to_string().contains("did not produce a final answer"));
    }

    /// Exhausts a deliberately tiny original budget with real tool calls, answers the
    /// `TurnBudgetSupervisor` call with a plausible directive, then finally produces a real
    /// final answer once the circuit breaker's extension is in effect -- proving a call that
    /// would have failed outright under the old hard cutoff now gets rescued.
    struct EventuallyFinishesAfterBreaker {
        agentic_turn: Arc<AtomicUsize>,
        models: Vec<ModelInfo>,
    }
    impl Provider for EventuallyFinishesAfterBreaker {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn supports_tools(&self) -> bool { true }
        fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            if req.system == crate::agents::TURN_BUDGET_SUPERVISOR {
                return Ok(InferenceResponse {
                    text: "You already have everything you need -- answer now.".into(),
                    tool_calls: Vec::new(),
                    input_tokens: 1,
                    output_tokens: 1,
                    provider: "mock".into(),
                    model: "mock".into(),
                    latency_ms: 1,
                });
            }
            let n = self.agentic_turn.fetch_add(1, Ordering::SeqCst);
            let (text, tool_calls) = if n < 3 {
                (
                    String::new(),
                    vec![ToolCall {
                        id: format!("call-{n}"),
                        name: "echo".into(),
                        arguments: json!({"text": format!("turn-{n}")}),
                    }],
                )
            } else {
                ("done after breaker".to_string(), Vec::new())
            };
            Ok(InferenceResponse {
                text,
                tool_calls,
                input_tokens: 1,
                output_tokens: 1,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    #[test]
    fn circuit_breaker_rescues_a_call_that_would_have_failed_under_the_old_hard_cutoff() {
        let ws = temp_ws("breaker-rescue");
        let provider: Box<dyn Provider> = Box::new(EventuallyFinishesAfterBreaker {
            agentic_turn: Arc::new(AtomicUsize::new(0)),
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        // Original budget of 3 is exhausted by real tool calls, exactly like the test above --
        // the only difference is this mock answers once the breaker grants more room.
        let result = mgr
            .run_agentic("TestAgent", "system", "task", &EchoToolbox, 3)
            .expect("the circuit breaker should rescue this call instead of failing it");
        assert_eq!(result, "done after breaker");

        let deviations = std::fs::read_to_string(ws.deviations_path()).unwrap();
        assert!(deviations.contains("circuit breaker tripped after 3 turns"));
        assert!(deviations.contains("You already have everything you need"));
    }

    #[test]
    fn circuit_breaker_supervisor_sees_a_digest_flagging_duplicate_calls() {
        let ws = temp_ws("breaker-digest");
        let seen_supervisor_task = Arc::new(Mutex::new(None));
        struct CapturesSupervisorTask {
            seen: Arc<Mutex<Option<String>>>,
            agentic_turn: Arc<AtomicUsize>,
            models: Vec<ModelInfo>,
        }
        impl Provider for CapturesSupervisorTask {
            fn name(&self) -> &str { "mock" }
            fn models(&self) -> &[ModelInfo] { &self.models }
            fn is_available(&self) -> bool { true }
            fn supports_tools(&self) -> bool { true }
            fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
                if req.system == crate::agents::TURN_BUDGET_SUPERVISOR {
                    *self.seen.lock().unwrap() = Some(req.user.clone());
                    return Ok(InferenceResponse {
                        text: "answer now".into(),
                        tool_calls: Vec::new(),
                        input_tokens: 1,
                        output_tokens: 1,
                        provider: "mock".into(),
                        model: "mock".into(),
                        latency_ms: 1,
                    });
                }
                let n = self.agentic_turn.fetch_add(1, Ordering::SeqCst);
                // Two turns both make the exact same call -- the duplicate the digest exists to
                // surface.
                let (text, tool_calls) = if n < 2 {
                    (
                        String::new(),
                        vec![ToolCall {
                            id: format!("call-{n}"),
                            name: "echo".into(),
                            arguments: json!({"text": "same-every-time"}),
                        }],
                    )
                } else {
                    ("done".to_string(), Vec::new())
                };
                Ok(InferenceResponse {
                    text,
                    tool_calls,
                    input_tokens: 1,
                    output_tokens: 1,
                    provider: "mock".into(),
                    model: "mock".into(),
                    latency_ms: 1,
                })
            }
        }
        let provider: Box<dyn Provider> = Box::new(CapturesSupervisorTask {
            seen: seen_supervisor_task.clone(),
            agentic_turn: Arc::new(AtomicUsize::new(0)),
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        mgr.run_agentic("TestAgent", "system", "task", &EchoToolbox, 2)
            .expect("should be rescued by the breaker");

        let supervisor_task = seen_supervisor_task.lock().unwrap().clone().unwrap();
        assert!(supervisor_task.contains("echo"));
        assert!(
            supervisor_task.contains("1 of them an exact repeat"),
            "got: {supervisor_task}"
        );
    }

    /// Provider "first" is the only one that ever answers correctly; "second" always answers
    /// "WRONG" and would only ever be reached if a later turn in the SAME conversation got
    /// routed away from the provider that served turn 1 -- proving `run_agentic` actually
    /// pins subsequent turns rather than re-scoring every time.
    struct Named {
        name: &'static str,
        calls: Arc<AtomicUsize>,
        models: Vec<ModelInfo>,
        wrong: bool,
    }
    impl Provider for Named {
        fn name(&self) -> &str { self.name }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn supports_tools(&self) -> bool { true }
        fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.wrong {
                return Ok(InferenceResponse {
                    text: "WRONG PROVIDER".into(),
                    tool_calls: Vec::new(),
                    input_tokens: 1,
                    output_tokens: 1,
                    provider: self.name.into(),
                    model: "mock".into(),
                    latency_ms: 1,
                });
            }
            if n == 0 {
                return Ok(InferenceResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "echo".into(),
                        arguments: json!({"text": "hello"}),
                    }],
                    input_tokens: 1,
                    output_tokens: 1,
                    provider: self.name.into(),
                    model: "mock".into(),
                    latency_ms: 1,
                });
            }
            let _ = req;
            Ok(InferenceResponse {
                text: "done-from-first".into(),
                tool_calls: Vec::new(),
                input_tokens: 1,
                output_tokens: 1,
                provider: self.name.into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    #[test]
    fn run_agentic_pins_every_turn_after_the_first_to_the_same_provider() {
        let ws = temp_ws("pinned");
        let first: Box<dyn Provider> = Box::new(Named {
            name: "first",
            calls: Arc::new(AtomicUsize::new(0)),
            models: vec![mock_model()],
            wrong: false,
        });
        let second: Box<dyn Provider> = Box::new(Named {
            name: "second",
            calls: Arc::new(AtomicUsize::new(0)),
            models: vec![mock_model()],
            wrong: true,
        });
        let registry = Registry::with_providers(vec![first, second], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        let result = mgr.run_agentic("TestAgent", "system", "task", &EchoToolbox, 5).unwrap();
        assert_eq!(result, "done-from-first");
    }

    struct NoTools;
    impl Toolbox for NoTools {
        fn tool_defs(&self) -> Vec<ToolDef> { Vec::new() }
        fn call(&self, _name: &str, _arguments: &Value) -> String {
            "error: no tools available".to_string()
        }
    }

    /// Finishes immediately (no tool calls) and records the exact `user` text it was sent, so
    /// tests can check what `run_agentic` actually put in the request.
    struct CapturesUser {
        seen: Arc<std::sync::Mutex<Option<String>>>,
        models: Vec<ModelInfo>,
    }
    impl Provider for CapturesUser {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn supports_tools(&self) -> bool { true }
        fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            *self.seen.lock().unwrap() = Some(req.user.clone());
            Ok(InferenceResponse {
                text: "done".into(),
                tool_calls: Vec::new(),
                input_tokens: 1,
                output_tokens: 1,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    #[test]
    fn a_tool_capable_call_gets_the_batching_and_no_re_read_reminder_appended() {
        let ws = temp_ws("reminder-with-tools");
        let seen = Arc::new(std::sync::Mutex::new(None));
        let provider: Box<dyn Provider> = Box::new(CapturesUser { seen: seen.clone(), models: vec![mock_model()] });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        mgr.run_agentic("TestAgent", "system", "the actual task", &EchoToolbox, 5).unwrap();

        let user = seen.lock().unwrap().clone().unwrap();
        assert!(user.starts_with("the actual task"), "got: {user}");
        assert!(user.contains("same turn"), "expected the batching reminder, got: {user}");
        assert!(user.contains("Never re-request"), "expected the no-re-read reminder, got: {user}");
    }

    #[test]
    fn a_call_with_no_tools_gets_the_task_verbatim_with_no_reminder() {
        let ws = temp_ws("reminder-no-tools");
        let seen = Arc::new(std::sync::Mutex::new(None));
        let provider: Box<dyn Provider> = Box::new(CapturesUser { seen: seen.clone(), models: vec![mock_model()] });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        mgr.run_agentic("TestAgent", "system", "the actual task", &NoTools, 5).unwrap();

        assert_eq!(seen.lock().unwrap().clone().unwrap(), "the actual task");
    }

    fn observed_ws(tag: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("aoo-manager-observed-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let observer = crate::observer::Observer::new(dir.join("events.jsonl"), Some(3), None);
        Workspace::new(dir).unwrap().with_observer(observer)
    }

    fn events_of(ws: &Workspace) -> Vec<run_events::Event> {
        run_events::parse_events(&std::fs::read_to_string(ws.root.join("events.jsonl")).unwrap())
    }

    #[test]
    fn a_plain_call_emits_an_agent_call_event_with_token_and_latency_numbers() {
        let ws = observed_ws("plain");
        let provider: Box<dyn Provider> = Box::new(CapturesUser {
            seen: Arc::new(std::sync::Mutex::new(None)),
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        mgr.run("TestAgent", "system", "task").unwrap();

        let calls: Vec<_> = events_of(&ws)
            .into_iter()
            .filter(|e| e.kind == run_events::EventKind::AgentCall)
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].agent, "TestAgent");
        assert_eq!(calls[0].job_id, Some(3));
        let d = calls[0].detail.as_ref().unwrap();
        assert_eq!(d["provider"], "mock");
        assert_eq!(d["input_tokens"], 1);
        assert_eq!(d["output_tokens"], 1);
        assert_eq!(d["latency_ms"], 1);
        assert_eq!(d["ok"], true);
    }

    #[test]
    fn an_agentic_call_emits_one_summarising_agent_call_event_across_its_turns() {
        let ws = observed_ws("agentic");
        let provider: Box<dyn Provider> = Box::new(ScriptedToolCaller {
            calls: Arc::new(AtomicUsize::new(0)),
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        mgr.run_agentic("TestAgent", "system", "task", &EchoToolbox, 5).unwrap();

        let calls: Vec<_> = events_of(&ws)
            .into_iter()
            .filter(|e| e.kind == run_events::EventKind::AgentCall)
            .collect();
        assert_eq!(calls.len(), 1);
        let d = calls[0].detail.as_ref().unwrap();
        assert_eq!(d["input_tokens"], 2);
        assert_eq!(d["output_tokens"], 2);
        assert_eq!(d["latency_ms"], 2);
        assert_eq!(d["ok"], true);
        assert!(calls[0].summary.contains("2 turn(s)"), "got: {}", calls[0].summary);
    }

    #[test]
    fn a_failed_agentic_call_emits_a_failed_agent_call_and_an_error_event() {
        let ws = observed_ws("failed");
        let provider: Box<dyn Provider> = Box::new(NeverFinishes { models: vec![mock_model()] });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        let mgr = Manager::new(&ws, &registry);

        assert!(mgr.run_agentic("TestAgent", "system", "task", &EchoToolbox, 1).is_err());

        let events = events_of(&ws);
        let call = events.iter().rfind(|e| e.kind == run_events::EventKind::AgentCall).unwrap();
        assert_eq!(call.detail.as_ref().unwrap()["ok"], false);
        assert!(events.iter().any(|e| e.kind == run_events::EventKind::Error));
        assert!(events.iter().any(|e| e.kind == run_events::EventKind::Retry), "breaker should log a retry");
    }

    #[test]
    fn deviations_and_decisions_become_events_but_token_bookkeeping_does_not() {
        let ws = observed_ws("logs");
        ws.log_deviation("Reviewer", "something drifted\nmore detail").unwrap();
        ws.log_agent_decision("Planner", "chose option b").unwrap();
        ws.log_agent_decision("Planner", "tokens: +1in/1out").unwrap();

        let events = events_of(&ws);
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![run_events::EventKind::Deviation, run_events::EventKind::Decision]);
        assert_eq!(events[0].summary, "something drifted");
        assert_eq!(events[0].agent, "Reviewer");
    }

    #[test]
    fn without_an_observer_nothing_is_written_to_events_jsonl() {
        let ws = temp_ws("no-observer-events");
        let provider: Box<dyn Provider> = Box::new(CapturesUser {
            seen: Arc::new(std::sync::Mutex::new(None)),
            models: vec![mock_model()],
        });
        let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
        Manager::new(&ws, &registry).run("TestAgent", "system", "task").unwrap();
        ws.log_deviation("a", "b").unwrap();
        assert!(!ws.root.join("events.jsonl").exists());
    }
}
