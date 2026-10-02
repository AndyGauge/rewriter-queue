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
