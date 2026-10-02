use crate::agents::POST_RUN_ANALYST;
use crate::manager::Manager;
use crate::provider_setup;
use crate::workspace::Workspace;
use run_events::{
    parse_events, Analysis, Event, EventKind, FeatureRequest, Priority, RequestStatus,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

const MAX_LISTED_EVENTS: usize = 400;
const MAX_SUMMARY_CHARS: usize = 300;
const MIN_QUOTE_CHARS: usize = 8;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct AgentStats {
    pub calls: u32,
    pub failed_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateFailure {
    pub stage: String,
    pub attempt: Option<u64>,
    pub error_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub outcome: String,
    pub event_count: usize,
    pub agents: BTreeMap<String, AgentStats>,
    pub retries_by_stage: BTreeMap<String, u32>,
    pub gate_runs: u32,
    pub gate_failures: Vec<GateFailure>,
    pub unfinished_stages: Vec<String>,
}

fn stage_name(e: &Event) -> String {
    e.stage.clone().unwrap_or_else(|| "(no stage)".to_string())
}

fn detail_u64(e: &Event, key: &str) -> Option<u64> {
    e.detail.as_ref()?.get(key)?.as_u64()
}

fn detail_bool(e: &Event, key: &str) -> Option<bool> {
    e.detail.as_ref()?.get(key)?.as_bool()
}

fn unfinished_stages(events: &[Event]) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut open: BTreeMap<String, u32> = BTreeMap::new();
    for e in events {
        match e.kind {
            EventKind::StageStart => {
                let key = e.stage.clone().unwrap_or_else(|| e.summary.clone());
                if !order.contains(&key) {
                    order.push(key.clone());
                }
                *open.entry(key).or_insert(0) += 1;
            }
            EventKind::StageEnd => {
                let key = e.stage.clone().unwrap_or_else(|| e.summary.clone());
                if let Some(n) = open.get_mut(&key) {
                    *n = n.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .filter(|k| open.get(k).copied().unwrap_or(0) > 0)
        .collect()
}

fn infer_outcome(events: &[Event], unfinished: &[String]) -> String {
    let Some(last) = events.last() else {
        return "failed".to_string();
    };
    if last.summary.to_lowercase().contains("cancel") {
        return "cancelled".to_string();
    }
    let last_stage_end_failed = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::StageEnd)
        .and_then(|e| detail_bool(e, "ok"))
        == Some(false);
    if last.kind == EventKind::Error || !unfinished.is_empty() || last_stage_end_failed {
        "failed".to_string()
    } else {
        "succeeded".to_string()
    }
}

pub fn compute_facts(events: &[Event]) -> Facts {
    let mut agents: BTreeMap<String, AgentStats> = BTreeMap::new();
    let mut retries_by_stage: BTreeMap<String, u32> = BTreeMap::new();
    let mut gate_runs = 0;
    let mut gate_failures = Vec::new();

    for e in events {
        match e.kind {
            EventKind::AgentCall => {
                let s = agents.entry(e.agent.clone()).or_default();
                s.calls += 1;
                if detail_bool(e, "ok") == Some(false) {
                    s.failed_calls += 1;
                }
                s.input_tokens += detail_u64(e, "input_tokens").unwrap_or(0);
                s.output_tokens += detail_u64(e, "output_tokens").unwrap_or(0);
                s.latency_ms += detail_u64(e, "latency_ms").unwrap_or(0);
            }
            EventKind::Retry => *retries_by_stage.entry(stage_name(e)).or_insert(0) += 1,
            EventKind::GateResult => {
                gate_runs += 1;
                if detail_bool(e, "passed") == Some(false) {
                    gate_failures.push(GateFailure {
                        stage: stage_name(e),
                        attempt: detail_u64(e, "attempt"),
                        error_bytes: detail_u64(e, "error_bytes").unwrap_or(0),
                    });
                }
            }
            _ => {}
        }
    }

    let unfinished = unfinished_stages(events);
    Facts {
        outcome: infer_outcome(events, &unfinished),
        event_count: events.len(),
        agents,
        retries_by_stage,
        gate_runs,
        gate_failures,
        unfinished_stages: unfinished,
    }
}

fn render_facts(f: &Facts) -> String {
    let mut out = format!(
        "Outcome: {}\nEvents: {}\n\nPer-agent model calls:\n",
        f.outcome, f.event_count
    );
    if f.agents.is_empty() {
        out.push_str("- none\n");
    }
    for (name, s) in &f.agents {
        out.push_str(&format!(
            "- {name}: {} calls ({} failed), {}in/{}out tokens, {}ms total latency\n",
            s.calls, s.failed_calls, s.input_tokens, s.output_tokens, s.latency_ms
        ));
    }
    out.push_str("\nRetries per stage:\n");
    if f.retries_by_stage.is_empty() {
        out.push_str("- none\n");
    }
    for (stage, n) in &f.retries_by_stage {
        out.push_str(&format!("- {stage}: {n}\n"));
    }
    out.push_str(&format!(
        "\nGate runs: {}, failed: {}\n",
        f.gate_runs,
        f.gate_failures.len()
    ));
    for g in &f.gate_failures {
        let attempt = g
            .attempt
            .map(|a| a.to_string())
            .unwrap_or_else(|| "?".into());
        out.push_str(&format!(
            "- {} attempt {attempt}: {} bytes of errors\n",
            g.stage, g.error_bytes
        ));
    }
    out.push_str("\nStages that never ended:\n");
    if f.unfinished_stages.is_empty() {
        out.push_str("- none\n");
    }
    for s in &f.unfinished_stages {
        out.push_str(&format!("- {s}\n"));
    }
    out
}

fn mechanical_summary(f: &Facts) -> String {
    let calls: u32 = f.agents.values().map(|s| s.calls).sum();
    let retries: u32 = f.retries_by_stage.values().sum();
    let mut s = format!(
        "Job {}: {} model calls, {} retries, {} of {} gate runs failed",
        f.outcome,
        calls,
        retries,
        f.gate_failures.len(),
        f.gate_runs
    );
    if !f.unfinished_stages.is_empty() {
        s.push_str(&format!(
            "; never ended: {}",
            f.unfinished_stages.join(", ")
        ));
    }
    s.push('.');
    s
}

fn event_ref(e: &Event) -> String {
    format!("{}@{}", e.agent, e.ts)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}...")
    }
}

fn is_listed(e: &Event) -> bool {
    match e.kind {
        EventKind::Error
        | EventKind::Finding
        | EventKind::Deviation
        | EventKind::Retry
        | EventKind::StageStart
        | EventKind::StageEnd => true,
        EventKind::GateResult => detail_bool(e, "passed") == Some(false),
        EventKind::AgentCall => detail_bool(e, "ok") == Some(false),
        EventKind::Decision => false,
    }
}

fn render_event(e: &Event) -> String {
    let kind = serde_json::to_value(e.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let mut line = format!("[{kind}] {}", event_ref(e));
    if let Some(stage) = &e.stage {
        line.push_str(&format!(" stage={stage}"));
    }
    line.push_str(&format!(" {}", truncate(&e.summary, MAX_SUMMARY_CHARS)));
    if let Some(detail) = &e.detail {
        let mut parts = Vec::new();
        if let Some(obj) = detail.as_object() {
            for (k, v) in obj {
                let text = match v {
                    Value::String(s) => truncate(s, MAX_SUMMARY_CHARS),
                    other => other.to_string(),
                };
                parts.push(format!("{k}={text}"));
            }
        }
        if !parts.is_empty() {
            line.push_str(&format!(" ({})", parts.join(", ")));
        }
    }
    line
}

fn render_prompt(job_id: u64, events: &[Event], facts: &Facts) -> String {
    let listed: Vec<&Event> = events.iter().filter(|e| is_listed(e)).collect();
    let mut out = format!(
        "# Job {job_id}\n\n## Mechanical facts\n{}\n## Events\n",
        render_facts(facts)
    );
    if listed.is_empty() {
        out.push_str("(none)\n");
    }
    for e in listed.iter().take(MAX_LISTED_EVENTS) {
        out.push_str(&render_event(e));
        out.push('\n');
    }
    if listed.len() > MAX_LISTED_EVENTS {
        out.push_str(&format!(
            "({} more events omitted)\n",
            listed.len() - MAX_LISTED_EVENTS
        ));
    }
    out
}

fn collect_strings(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push(' ');
            out.push_str(&s.to_lowercase());
        }
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

struct EvidenceIndex {
    refs: HashSet<(String, String)>,
    haystacks: Vec<String>,
}

impl EvidenceIndex {
    fn new(events: &[Event]) -> Self {
        let refs = events
            .iter()
            .map(|e| (e.agent.clone(), e.ts.clone()))
            .collect();
        let haystacks = events
            .iter()
            .map(|e| {
                let mut h = e.summary.to_lowercase();
                if let Some(d) = &e.detail {
                    collect_strings(d, &mut h);
                }
                h
            })
            .collect();
        Self { refs, haystacks }
    }

    fn matches(&self, cite: &str) -> bool {
        let cite = cite.trim();
        if let Some((agent, ts)) = cite.split_once('@') {
            if self
                .refs
                .contains(&(agent.trim().to_string(), ts.trim().to_string()))
            {
                return true;
            }
        }
        let quote = cite
            .trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .to_lowercase();
        quote.chars().count() >= MIN_QUOTE_CHARS
            && self.haystacks.iter().any(|h| h.contains(&quote))
    }

    fn valid(&self, cites: &[String]) -> Vec<String> {
        cites.iter().filter(|c| self.matches(c)).cloned().collect()
    }
}

struct RawCause {
    text: String,
    evidence: Vec<String>,
}

struct RawRequest {
    title: String,
    rationale: String,
    priority: Priority,
    evidence: Vec<String>,
}

#[derive(Default)]
struct Reply {
    summary: String,
    causes: Vec<RawCause>,
    requests: Vec<RawRequest>,
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_priority(s: &str) -> Priority {
    match s.trim().to_lowercase().as_str() {
        "high" => Priority::High,
        "low" => Priority::Low,
        _ => Priority::Medium,
    }
}

fn parse_reply(text: &str) -> Option<Reply> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let v: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let obj = v.as_object()?;
    let causes = obj
        .get("root_causes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    Some(RawCause {
                        text: c.get("text")?.as_str()?.trim().to_string(),
                        evidence: str_list(c.get("evidence")),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let requests = obj
        .get("feature_requests")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(RawRequest {
                        title: r.get("title")?.as_str()?.trim().to_string(),
                        rationale: r
                            .get("rationale")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string(),
                        priority: parse_priority(
                            r.get("priority").and_then(Value::as_str).unwrap_or(""),
                        ),
                        evidence: str_list(r.get("evidence")),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Reply {
        summary: obj
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        causes,
        requests,
    })
}

pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn build_analysis(job_id: u64, events: &[Event], facts: &Facts, reply: Option<Reply>) -> Analysis {
    let mut analysis = Analysis {
        job_id,
        outcome: facts.outcome.clone(),
        summary: mechanical_summary(facts),
        root_causes: Vec::new(),
        feature_requests: Vec::new(),
    };
    let Some(reply) = reply else {
        return analysis;
    };
    let index = EvidenceIndex::new(events);

    if !reply.summary.is_empty() {
        analysis.summary = reply.summary;
    }
    for cause in reply.causes {
        let evidence = index.valid(&cause.evidence);
        if cause.text.is_empty() || evidence.is_empty() {
            continue;
        }
        analysis.root_causes.push(format!(
            "{} (evidence: {})",
            cause.text,
            evidence.join("; ")
        ));
    }
    let mut seen = HashSet::new();
    for req in reply.requests {
        let id = slugify(&req.title);
        let evidence = index.valid(&req.evidence);
        if id.is_empty() || evidence.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        analysis.feature_requests.push(FeatureRequest {
            id,
            title: req.title,
            rationale: req.rationale,
            evidence,
            source_job_ids: vec![job_id],
            priority: req.priority,
            status: RequestStatus::Open,
        });
    }
    analysis
}

pub fn analyze(job_id: u64, events: &[Event], mgr: &Manager) -> Analysis {
    let facts = compute_facts(events);
    let prompt = render_prompt(job_id, events, &facts);
    let reply = match mgr.run("PostRunAnalyst", POST_RUN_ANALYST, &prompt) {
        Ok(text) => {
            let parsed = parse_reply(&text);
            if parsed.is_none() {
                eprintln!("warn: analyst reply was not valid JSON; writing mechanical facts only");
            }
            parsed
        }
        Err(e) => {
            eprintln!("warn: analyst call failed ({e}); writing mechanical facts only");
            None
        }
    };
    build_analysis(job_id, events, &facts, reply)
}

struct CliArgs {
    job_id: u64,
    events: String,
    out: String,
}

fn parse_cli(args: &[String]) -> Result<CliArgs, String> {
    let mut job_id = None;
    let mut events = None;
    let mut out = None;
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1);
        match args[i].as_str() {
            "--job-id" => {
                let v = value.ok_or("--job-id needs a value")?;
                job_id = Some(v.parse::<u64>().map_err(|_| format!("bad --job-id: {v}"))?);
            }
            "--events" => events = Some(value.ok_or("--events needs a value")?.clone()),
            "--out" => out = Some(value.ok_or("--out needs a value")?.clone()),
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 2;
    }
    Ok(CliArgs {
        job_id: job_id.ok_or("--job-id is required")?,
        events: events.ok_or("--events is required")?,
        out: out.ok_or("--out is required")?,
    })
}

pub fn run_cli(args: &[String]) -> i32 {
    let cli = match parse_cli(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\nusage: ai-org-orchestrator analyze --job-id N --events FILE --out FILE");
            return 2;
        }
    };
    let text = match std::fs::read_to_string(&cli.events) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", cli.events);
            return 1;
        }
    };
    let events = parse_events(&text);

    let work = std::env::temp_dir().join(format!(
        "rewriter-analyze-{}-{}",
        cli.job_id,
        std::process::id()
    ));
    let analysis = match Workspace::new(&work) {
        Ok(ws) => {
            let registry = provider_setup::build_registry(
                &inference_providers::config::Config::load(),
                provider_setup::ProviderOptions::from_env(),
            );
            let mgr = Manager::new(&ws, &registry);
            analyze(cli.job_id, &events, &mgr)
        }
        Err(e) => {
            eprintln!("warn: cannot create scratch workspace ({e}); writing mechanical facts only");
            let facts = compute_facts(&events);
            build_analysis(cli.job_id, &events, &facts, None)
        }
    };
    let _ = std::fs::remove_dir_all(&work);

    write_analysis(&analysis, Path::new(&cli.out))
}

fn write_analysis(analysis: &Analysis, out: &Path) -> i32 {
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("error: cannot create {}: {e}", parent.display());
            return 1;
        }
    }
    let json = serde_json::to_string_pretty(analysis).expect("Analysis always serializes");
    match std::fs::write(out, json) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: cannot write {}: {e}", out.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inference_providers::backends::Provider;
    use inference_providers::Registry;
    use inference_providers::types::{
        InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError,
    };
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
            ev("t11", "MaintenanceReviewer", EventKind::Finding, "unwrap on user input in parser")
                .with_detail(json!({"severity": "warn", "category": "panic", "evidence": "src/parse.rs line 40"})),
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
        assert!(
            f.agents.is_empty() && f.gate_failures.is_empty() && f.unfinished_stages.is_empty()
        );
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
}
