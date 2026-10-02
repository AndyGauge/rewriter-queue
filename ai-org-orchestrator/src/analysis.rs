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
mod tests;
