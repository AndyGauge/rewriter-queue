use crate::{
    agents::{agent_hora_depth, agent_tier},
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
/// answer, and the whole run failed despite the tool loop itself working perfectly. Exploring a
/// real source tree can reasonably take several reads (and a `fan_out` call, which recurses
/// with this same budget) before there's enough context to answer at all.
pub(crate) const AGENTIC_MAX_TURNS: usize = 20;

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
            tools: Vec::new(),
            history: Vec::new(),
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

    /// Run an agent with tool-calling access instead of pasting everything it might need
    /// straight into its prompt: it reads V1 source files, other stages' artifacts, and (via
    /// `fan_out`) delegates genuinely separable sub-questions to concurrent copies of itself,
    /// all on demand through `toolbox` (see `tools::PipelineToolbox`). Loops turn by turn
    /// until the model answers with no further tool calls, or `max_turns` is exhausted.
    ///
    /// The first turn is routed normally (`Registry::complete`, scored across every eligible
    /// tool-capable provider); every following turn in the same conversation is pinned to
    /// that same provider (`Registry::complete_pinned`), since the growing tool-call/tool-
    /// result history is encoded in that provider's own wire format and can't be replayed
    /// against a different one mid-conversation.
    pub fn run_agentic(
        &self,
        agent: &str,
        system: &str,
        task: &str,
        toolbox: &(dyn Toolbox + Sync),
        max_turns: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.ws.set_agent_task(agent, task)?;
        let tool_defs = toolbox.tool_defs();
        let mut history: Vec<Turn> = Vec::new();
        let mut pinned_provider: Option<String> = None;

        for turn in 0..max_turns {
            let req = InferenceRequest {
                system: system.to_string(),
                user: task.to_string(),
                max_tokens: 16000,
                tier: agent_tier(agent),
                hora_depth: agent_hora_depth(agent),
                slot_hint: None,
                tools: tool_defs.clone(),
                history: history.clone(),
            };
            let resp = match &pinned_provider {
                None => self.registry.complete(&req)?,
                Some(name) => self.registry.complete_pinned(name, &req)?,
            };
            pinned_provider = Some(resp.provider.clone());

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
        }

        Err(format!("{agent} did not produce a final answer within {max_turns} tool-calling turns").into())
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
             # Current Output (line numbers shown for reference — not part of the document)\n{}\n\n\
             # Review Feedback ({label})\n{review_feedback}\n\n\
             Address the feedback above with a minimal patch instead of rewriting the whole \
             document. {}",
            crate::patch::format_multi_file_numbered(&sections),
            crate::patch::PATCH_FORMAT_INSTRUCTIONS,
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
    use std::sync::Arc;

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
}
