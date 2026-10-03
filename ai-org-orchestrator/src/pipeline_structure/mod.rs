//! Deterministic, free tests of how the pipeline is wired. Every test drives the real `Manager`
//! over a provider that answers from canned text keyed on the agent's system prompt, then
//! asserts on which agents were called, in what order, with what in their prompts, and on the
//! emitted event stream. None of them needs a model.

mod budget;
mod contract_review;
mod debug_repair;
mod observability;
mod plan_review;

use crate::agents::{
    DEBUG_IMPLEMENTER, FORMAL_METHODS_REVIEWER, IP_COUNSEL, MAINTENANCE_REVIEWER, MILESTONE_PLANNER,
    PLAN_REVIEWER, PRODUCTION_READINESS_REVIEWER, SECURITY_REVIEWER,
};
use crate::observer::Observer;
use crate::workspace::Workspace;
use inference_providers::backends::Provider;
use inference_providers::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
use inference_providers::Registry;
use run_events::Event;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(super) const WORKER: &str = "WORKER-SYSTEM-PROMPT";

#[derive(Clone, Debug)]
pub(super) struct Call {
    pub role: &'static str,
    pub user: String,
}

pub(super) type Log = Arc<Mutex<Vec<Call>>>;

fn role_of(system: &str) -> &'static str {
    if system == PLAN_REVIEWER {
        "plan-reviewer"
    } else if system == MILESTONE_PLANNER {
        "planner"
    } else if system.starts_with(DEBUG_IMPLEMENTER) {
        "debugger"
    } else if system == crate::agents::CONTRACT_REVIEWER {
        "contract-reviewer"
    } else if system == crate::agents::MISSION_ARCHITECT {
        "mission-architect"
    } else if system == SECURITY_REVIEWER {
        "security"
    } else if system == MAINTENANCE_REVIEWER {
        "maintenance"
    } else if system == IP_COUNSEL {
        "ip-counsel"
    } else if system == FORMAL_METHODS_REVIEWER {
        "formal-methods"
    } else if system == PRODUCTION_READINESS_REVIEWER {
        "production-readiness"
    } else if system.starts_with(WORKER) {
        "worker"
    } else {
        "other"
    }
}

type Handler = Box<dyn Fn(&Call) -> Result<String, ProviderError> + Send + Sync>;

struct Scripted {
    handler: Handler,
    log: Log,
    models: Vec<ModelInfo>,
}

impl Provider for Scripted {
    fn name(&self) -> &str {
        "mock"
    }
    fn models(&self) -> &[ModelInfo] {
        &self.models
    }
    fn is_available(&self) -> bool {
        true
    }
    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        let call = Call { role: role_of(&req.system), user: req.user.clone() };
        self.log.lock().unwrap().push(call.clone());
        let text = (self.handler)(&call)?;
        Ok(InferenceResponse {
            text,
            tool_calls: Vec::new(),
            input_tokens: 10,
            output_tokens: 5,
            provider: "mock".into(),
            model: "mock".into(),
            latency_ms: 1,
        })
    }
}

pub(super) fn scripted(
    handler: impl Fn(&Call) -> Result<String, ProviderError> + Send + Sync + 'static,
) -> (Registry, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let provider: Box<dyn Provider> = Box::new(Scripted {
        handler: Box::new(handler),
        log: log.clone(),
        models: vec![ModelInfo {
            provider: "mock".into(),
            model: "mock".into(),
            tier: ModelTier::Heavy,
            cost_per_1k_input: 0.0,
            cost_per_1k_output: 0.0,
            context_limit: 200_000,
        }],
    });
    (Registry::with_providers(vec![provider], (1.0, 1.0, 1.0)), log)
}

pub(super) fn roles(log: &Log) -> Vec<&'static str> {
    log.lock().unwrap().iter().map(|c| c.role).collect()
}

pub(super) fn calls_of(log: &Log, role: &str) -> Vec<Call> {
    log.lock().unwrap().iter().filter(|c| c.role == role).cloned().collect()
}

pub(super) fn crate_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aoo-structure-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A workspace at `dir`. With `cargo`, its quality gate really builds, lints and tests `v2/`;
/// without, there is no `v2/Cargo.toml`, the gate is a no-op and no cargo process runs.
pub(super) fn ws_at(dir: &std::path::Path, cargo: bool) -> Workspace {
    let ws = Workspace::new(dir).unwrap();
    if cargo {
        ws.emit_artifact(
            "v2/Cargo.toml",
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
    }
    ws
}

pub(super) fn observed(ws: Workspace, dir: &std::path::Path) -> (Workspace, PathBuf) {
    let path = dir.join("events.jsonl");
    (ws.with_observer(Observer::new(&path, None, None)), path)
}

pub(super) fn read_events(path: &std::path::Path) -> Vec<Event> {
    run_events::parse_events(&std::fs::read_to_string(path).unwrap_or_default())
}

pub(super) struct NullToolbox;
impl crate::tools::Toolbox for NullToolbox {
    fn tool_defs(&self) -> Vec<inference_providers::ToolDef> {
        Vec::new()
    }
    fn call(&self, _name: &str, _arguments: &serde_json::Value) -> String {
        "error: no tools available".to_string()
    }
}

pub(super) fn plan(milestones: &[(&str, &str)]) -> String {
    milestones
        .iter()
        .map(|(name, risk)| format!("## MILESTONE: {name}\nRISK: {risk}\nEFFORT: 10\nTASK: build {name}\n"))
        .collect()
}

pub(super) const GOOD_MAIN: &str = "// === src/main.rs ===\nfn main() {\n    println!(\"ok\");\n}\n";
pub(super) const BROKEN_MAIN: &str = "// === src/main.rs ===\nfn main() {\n    undefined_fn();\n}\n";
