use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    StageStart,
    StageEnd,
    /// One model call. `detail`: provider, model, input_tokens, output_tokens, latency_ms, ok.
    AgentCall,
    Decision,
    Deviation,
    /// Something an agent noticed and chose to report. `detail`: severity, category, evidence.
    Finding,
    /// A quality-gate or integration-gate run. `detail`: passed, error_bytes, attempt.
    GateResult,
    Retry,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub ts: String,
    pub job_id: Option<u64>,
    pub stage: Option<String>,
    pub agent: String,
    pub kind: EventKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl Event {
    pub fn new(agent: &str, kind: EventKind, summary: &str) -> Self {
        Event {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            job_id: None,
            stage: None,
            agent: agent.to_string(),
            kind,
            summary: summary.to_string(),
            detail: None,
        }
    }

    pub fn with_job(mut self, job_id: u64) -> Self {
        self.job_id = Some(job_id);
        self
    }

    pub fn with_stage(mut self, stage: &str) -> Self {
        self.stage = Some(stage.to_string());
        self
    }

    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).expect("Event always serializes")
    }

    pub fn from_json_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line.trim())
    }
}

/// Parse a JSONL stream of events, skipping blank lines and any line that is not a valid
/// event (a truncated final line from a killed process must not hide the rest).
pub fn parse_events(text: &str) -> Vec<Event> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| Event::from_json_line(l).ok())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Open,
    Accepted,
    Done,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureRequest {
    pub id: String,
    pub title: String,
    pub rationale: String,
    /// Short quotes or `agent@ts` references to the events that motivated the request.
    pub evidence: Vec<String>,
    pub source_job_ids: Vec<u64>,
    pub priority: Priority,
    pub status: RequestStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    pub job_id: u64,
    /// `succeeded`, `failed` or `cancelled`.
    pub outcome: String,
    pub summary: String,
    pub root_causes: Vec<String>,
    pub feature_requests: Vec<FeatureRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_event_round_trips_through_a_json_line() {
        let e = Event::new("MilestonePlanner", EventKind::AgentCall, "planned")
            .with_job(7)
            .with_stage("V2 synthesis")
            .with_detail(json!({"input_tokens": 10, "ok": true}));
        let back = Event::from_json_line(&e.to_json_line()).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn a_json_line_has_no_embedded_newline() {
        let e = Event::new("a", EventKind::Decision, "line one\nline two");
        assert!(!e.to_json_line().contains('\n'));
    }

    #[test]
    fn parse_events_skips_blank_and_truncated_lines() {
        let good = Event::new("a", EventKind::Finding, "x").to_json_line();
        let text = format!("{good}\n\n{{\"ts\":\"trunc\n{good}\n");
        assert_eq!(parse_events(&text).len(), 2);
    }

    #[test]
    fn kinds_serialize_as_snake_case() {
        let e = Event::new("a", EventKind::StageStart, "s");
        assert!(e.to_json_line().contains("\"stage_start\""));
    }
}
