use super::*;
use inference_providers::backends::Provider;
use inference_providers::types::{
    InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError,
};
use inference_providers::Registry;
use serde_json::json;
use std::sync::{Arc, Mutex};

fn ev(ts: &str, agent: &str, kind: EventKind, summary: &str) -> Event {
    let mut e = Event::new(agent, kind, summary);
    e.ts = ts.to_string();
    e
}

fn call(ts: &str, agent: &str, input: u64, output: u64, ms: u64, ok: bool) -> Event {
    ev(ts, agent, EventKind::AgentCall, "call").with_detail(json!({
        "provider": "p", "model": "m", "input_tokens": input,
        "output_tokens": output, "latency_ms": ms, "ok": ok,
    }))
}

fn gate(ts: &str, stage: &str, passed: bool, attempt: u64, bytes: u64) -> Event {
    ev(ts, "Gate", EventKind::GateResult, "gate")
        .with_stage(stage)
        .with_detail(json!({"passed": passed, "attempt": attempt, "error_bytes": bytes}))
}

fn sample() -> Vec<Event> {
    vec![
        ev("t01", "Pipeline", EventKind::StageStart, "start").with_stage("synthesis"),
        call("t02", "TargetImplementer", 100, 50, 1000, true),
        call("t03", "TargetImplementer", 200, 70, 3000, false),
        call("t04", "MergeAgent", 10, 5, 500, true),
        gate("t05", "synthesis", false, 1, 900),
        ev("t06", "Pipeline", EventKind::Retry, "retry").with_stage("synthesis"),
        gate("t07", "synthesis", false, 2, 400),
        ev("t08", "Pipeline", EventKind::Retry, "retry").with_stage("synthesis"),
        gate("t09", "synthesis", true, 3, 0),
        ev("t10", "Pipeline", EventKind::StageStart, "start").with_stage("review"),
        ev(
            "t11",
            "MaintenanceReviewer",
            EventKind::Finding,
            "unwrap on user input in parser",
        )
        .with_detail(
            json!({"severity": "warn", "category": "panic", "evidence": "src/parse.rs line 40"}),
        ),
    ]
}

#[test]
fn facts_sum_calls_tokens_and_latency_per_agent() {
    let f = compute_facts(&sample());
    let t = &f.agents["TargetImplementer"];
    assert_eq!(
        *t,
        AgentStats {
            calls: 2,
            failed_calls: 1,
            input_tokens: 300,
            output_tokens: 120,
            latency_ms: 4000
        }
    );
    assert_eq!(f.agents["MergeAgent"].calls, 1);
    assert_eq!(f.event_count, 11);
}

#[test]
fn facts_count_retries_per_stage_and_gate_failures_per_attempt() {
    let f = compute_facts(&sample());
    assert_eq!(f.retries_by_stage["synthesis"], 2);
    assert_eq!(f.gate_runs, 3);
    assert_eq!(
        f.gate_failures,
        vec![
            GateFailure {
                stage: "synthesis".into(),
                attempt: Some(1),
                error_bytes: 900
            },
            GateFailure {
                stage: "synthesis".into(),
                attempt: Some(2),
                error_bytes: 400
            },
        ]
    );
}

#[test]
fn stages_that_started_and_never_ended_are_reported() {
    let mut events = sample();
    events.insert(
        9,
        ev("t095", "Pipeline", EventKind::StageEnd, "end").with_stage("synthesis"),
    );
    let f = compute_facts(&events);
    assert_eq!(f.unfinished_stages, vec!["review".to_string()]);
}

#[test]
fn an_unfinished_stage_means_failed() {
    assert_eq!(compute_facts(&sample()).outcome, "failed");
}

#[test]
fn a_run_whose_stages_all_ended_without_a_trailing_error_succeeded() {
    let events = vec![
        ev("t1", "P", EventKind::StageStart, "s").with_stage("a"),
        ev("t2", "P", EventKind::StageEnd, "e")
            .with_stage("a")
            .with_detail(json!({"ok": true})),
    ];
    assert_eq!(compute_facts(&events).outcome, "succeeded");
}

#[test]
fn a_trailing_error_means_failed_even_when_stages_ended() {
    let events = vec![
        ev("t1", "P", EventKind::StageStart, "s").with_stage("a"),
        ev("t2", "P", EventKind::StageEnd, "e").with_stage("a"),
        ev("t3", "P", EventKind::Error, "synthesis failed"),
    ];
    assert_eq!(compute_facts(&events).outcome, "failed");
}

#[test]
fn a_failed_final_stage_end_means_failed() {
    let events = vec![
        ev("t1", "P", EventKind::StageStart, "s").with_stage("a"),
        ev("t2", "P", EventKind::StageEnd, "e")
            .with_stage("a")
            .with_detail(json!({"ok": false})),
    ];
    assert_eq!(compute_facts(&events).outcome, "failed");
}

#[test]
fn a_cancel_message_at_the_end_means_cancelled() {
    let events = vec![
        ev("t1", "P", EventKind::StageStart, "s").with_stage("a"),
        ev("t2", "Worker", EventKind::Error, "job cancelled by user"),
    ];
    assert_eq!(compute_facts(&events).outcome, "cancelled");
}

#[test]
fn an_empty_log_is_failed_with_no_facts() {
    let f = compute_facts(&[]);
    assert_eq!(f.outcome, "failed");
    assert!(f.agents.is_empty() && f.gate_failures.is_empty() && f.unfinished_stages.is_empty());
}

#[test]
fn slugs_are_stable_kebab_case() {
    assert_eq!(
        slugify("Report gate error text, on retry!"),
        "report-gate-error-text-on-retry"
    );
    assert_eq!(slugify("  --Already--kebab--  "), "already-kebab");
    assert_eq!(slugify("!!!"), "");
}

#[test]
fn the_prompt_lists_events_with_citable_refs_and_the_facts() {
    let events = sample();
    let prompt = render_prompt(7, &events, &compute_facts(&events));
    assert!(prompt.contains("# Job 7"));
    assert!(prompt.contains("TargetImplementer: 2 calls (1 failed)"));
    assert!(prompt.contains("[finding] MaintenanceReviewer@t11"));
    assert!(prompt.contains("evidence=src/parse.rs line 40"));
    assert!(prompt.contains("[retry] Pipeline@t06"));
    assert!(
        !prompt.contains("MergeAgent@t04"),
        "successful calls are summarised, not listed"
    );
}

fn reply_json() -> String {
    json!({
        "summary": "The build gate failed twice.",
        "root_causes": [
            {"text": "Parser panics", "evidence": ["MaintenanceReviewer@t11"]},
            {"text": "Invented cause", "evidence": ["Ghost@t99"]},
            {"text": "No evidence", "evidence": []},
        ],
        "feature_requests": [
            {"title": "Ban unwrap in parsers", "rationale": "r", "priority": "high",
             "evidence": ["Ghost@t99", "unwrap on user input"]},
            {"title": "Fabricated request", "rationale": "r", "priority": "low",
             "evidence": ["nothing like this happened anywhere"]},
            {"title": "Ban unwrap in parsers!", "rationale": "dup", "priority": "low",
             "evidence": ["MaintenanceReviewer@t11"]},
            {"title": "???", "rationale": "r", "priority": "low", "evidence": ["MaintenanceReviewer@t11"]},
        ],
    })
    .to_string()
}

#[test]
fn claims_citing_nonexistent_events_are_dropped() {
    let events = sample();
    let facts = compute_facts(&events);
    let a = build_analysis(7, &events, &facts, parse_reply(&reply_json()));

    assert_eq!(a.summary, "The build gate failed twice.");
    assert_eq!(a.root_causes.len(), 1);
    assert!(a.root_causes[0].starts_with("Parser panics"));

    assert_eq!(a.feature_requests.len(), 1);
    let r = &a.feature_requests[0];
    assert_eq!(r.id, "ban-unwrap-in-parsers");
    assert_eq!(r.evidence, vec!["unwrap on user input".to_string()]);
    assert_eq!(r.source_job_ids, vec![7]);
    assert_eq!(r.priority, Priority::High);
    assert_eq!(r.status, RequestStatus::Open);
}

#[test]
fn a_short_quote_does_not_count_as_evidence() {
    let events = sample();
    let index = EvidenceIndex::new(&events);
    assert!(!index.matches("unwrap"));
    assert!(index.matches("src/parse.rs line 40"));
    assert!(index.matches("\"UNWRAP ON USER INPUT\""));
}

#[test]
fn a_reply_with_prose_and_fences_around_the_json_still_parses() {
    let wrapped = format!("Here you go:\n```json\n{}\n```", reply_json());
    assert!(parse_reply(&wrapped).is_some());
    assert!(parse_reply("no json here").is_none());
    assert!(parse_reply("{ broken").is_none());
}

#[test]
fn no_reply_yields_mechanical_facts_and_no_requests() {
    let events = sample();
    let facts = compute_facts(&events);
    let a = build_analysis(3, &events, &facts, None);
    assert_eq!(a.job_id, 3);
    assert_eq!(a.outcome, "failed");
    assert!(a.summary.contains("3 model calls"));
    assert!(a.summary.contains("never ended: synthesis, review"));
    assert!(a.root_causes.is_empty() && a.feature_requests.is_empty());
}

struct Scripted {
    reply: Option<String>,
    seen: Arc<Mutex<Option<InferenceRequest>>>,
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
        *self.seen.lock().unwrap() = Some(req.clone());
        match &self.reply {
            Some(text) => Ok(InferenceResponse {
                text: text.clone(),
                tool_calls: Vec::new(),
                input_tokens: 1,
                output_tokens: 1,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            }),
            None => Err(ProviderError::Unavailable("boom".into())),
        }
    }
}

fn run_with(reply: Option<String>, tag: &str) -> (Analysis, Option<InferenceRequest>) {
    let dir = std::env::temp_dir().join(format!("aoo-analysis-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ws = Workspace::new(&dir).unwrap();
    let seen = Arc::new(Mutex::new(None));
    let models = vec![ModelInfo {
        provider: "mock".into(),
        model: "mock".into(),
        tier: ModelTier::Heavy,
        cost_per_1k_input: 0.0,
        cost_per_1k_output: 0.0,
        context_limit: 200_000,
    }];
    let provider: Box<dyn Provider> = Box::new(Scripted {
        reply,
        seen: seen.clone(),
        models,
    });
    let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
    let mgr = Manager::new(&ws, &registry);
    let analysis = analyze(7, &sample(), &mgr);
    let req = seen.lock().unwrap().clone();
    (analysis, req)
}

#[test]
fn analyze_sends_the_skill_and_facts_and_validates_the_reply() {
    let (a, req) = run_with(Some(reply_json()), "ok");
    let req = req.unwrap();
    assert!(req.system.contains("PostRunAnalyst"));
    assert!(req.user.contains("MaintenanceReviewer@t11"));
    assert_eq!(a.feature_requests.len(), 1);
    assert_eq!(a.feature_requests[0].id, "ban-unwrap-in-parsers");
}

#[test]
fn analyze_survives_a_model_failure() {
    let (a, _) = run_with(None, "fail");
    assert_eq!(a.outcome, "failed");
    assert!(a.feature_requests.is_empty());
    assert!(a.summary.contains("model calls"));
}

#[test]
fn analyze_survives_an_unparseable_reply() {
    let (a, _) = run_with(Some("I could not decide.".into()), "garbage");
    assert!(a.feature_requests.is_empty() && a.root_causes.is_empty());
    assert!(a.summary.contains("model calls"));
}

#[test]
fn the_analyst_runs_at_the_light_tier() {
    assert_eq!(
        crate::agents::agent_tier("PostRunAnalyst"),
        ModelTier::Light
    );
}

#[test]
fn cli_args_require_all_three_flags() {
    let ok: Vec<String> = ["--job-id", "5", "--events", "e.jsonl", "--out", "o.json"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let c = parse_cli(&ok).unwrap();
    assert_eq!(
        (c.job_id, c.events.as_str(), c.out.as_str()),
        (5, "e.jsonl", "o.json")
    );
    assert!(parse_cli(&ok[..4]).is_err());
    assert!(parse_cli(&["--job-id".to_string(), "x".to_string()]).is_err());
    assert!(parse_cli(&["--bogus".to_string(), "1".to_string()]).is_err());
}

#[test]
fn write_analysis_creates_parent_dirs_and_round_trips() {
    let dir = std::env::temp_dir().join(format!("aoo-analysis-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = dir.join("nested/7.json");
    let events = sample();
    let a = build_analysis(7, &events, &compute_facts(&events), None);
    assert_eq!(write_analysis(&a, &out), 0);
    let back: Analysis = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(back, a);
}

#[test]
fn run_cli_with_a_missing_events_file_fails_without_writing() {
    let dir = std::env::temp_dir().join(format!("aoo-analysis-missing-{}", std::process::id()));
    let args: Vec<String> = [
        "--job-id",
        "1",
        "--events",
        dir.join("nope.jsonl").to_str().unwrap(),
        "--out",
        dir.join("o.json").to_str().unwrap(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(run_cli(&args), 1);
    assert!(!dir.join("o.json").exists());
}
