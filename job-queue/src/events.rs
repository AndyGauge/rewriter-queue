use crate::job::{Job, Queue};
use run_events::{parse_events, Analysis, Event, EventKind, FeatureRequest, RequestStatus};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

static REQUESTS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Default, Clone)]
pub struct EventFilter {
    pub kind: Option<EventKind>,
    pub agent: Option<String>,
    pub since: Option<String>,
}

impl EventFilter {
    fn matches(&self, event: &Event) -> bool {
        self.kind.is_none_or(|k| k == event.kind)
            && self.agent.as_ref().is_none_or(|a| *a == event.agent)
            && self
                .since
                .as_ref()
                .is_none_or(|since| is_after(&event.ts, since))
    }
}

fn is_after(ts: &str, since: &str) -> bool {
    match (
        chrono::DateTime::parse_from_rfc3339(ts),
        chrono::DateTime::parse_from_rfc3339(since),
    ) {
        (Ok(a), Ok(b)) => a > b,
        _ => ts > since,
    }
}

fn parse_label<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(text.to_string())).ok()
}

pub fn parse_kind(text: &str) -> io::Result<EventKind> {
    parse_label(text).ok_or_else(|| io::Error::other(format!("unknown event kind: {text}")))
}

pub fn parse_status(text: &str) -> io::Result<RequestStatus> {
    parse_label(text).ok_or_else(|| {
        io::Error::other(format!(
            "unknown status: {text} (use open, accepted, done or rejected)"
        ))
    })
}

fn events_path(queue: &Queue, id: u32) -> PathBuf {
    queue.root().join("events").join(format!("{id:04}.jsonl"))
}

fn analysis_path(queue: &Queue, id: u32) -> PathBuf {
    queue.root().join("analysis").join(format!("{id:04}.json"))
}

fn requests_path(queue: &Queue) -> PathBuf {
    queue.root().join("feature-requests.json")
}

pub fn append(queue: &Queue, id: u32, events: &[Event]) -> io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let path = events_path(queue, id);
    fs::create_dir_all(path.parent().expect("events path has a parent"))?;
    let mut buf = String::new();
    for event in events {
        let mut event = event.clone();
        event.job_id.get_or_insert(u64::from(id));
        buf.push_str(&event.to_json_line());
        buf.push('\n');
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(buf.as_bytes())
}

/// The server's copy when it has one; otherwise the workspace file the orchestrator always
/// writes, so local queues and unreachable servers still have events to show.
pub fn source_path(queue: &Queue, job: &Job) -> Option<PathBuf> {
    [
        events_path(queue, job.id),
        job.workspace.join("events.jsonl"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

pub fn read(queue: &Queue, job: &Job, filter: &EventFilter) -> io::Result<Vec<Event>> {
    let Some(path) = source_path(queue, job) else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
    Ok(parse_events(&text)
        .into_iter()
        .filter(|e| filter.matches(e))
        .collect())
}

pub fn to_jsonl(events: &[Event]) -> String {
    events.iter().map(|e| e.to_json_line() + "\n").collect()
}

pub fn write_analysis(queue: &Queue, analysis: &Analysis) -> io::Result<()> {
    let path = analysis_path(queue, analysis.job_id as u32);
    fs::create_dir_all(path.parent().expect("analysis path has a parent"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(analysis)?)?;
    fs::rename(tmp, path)
}

pub fn read_analysis(queue: &Queue, id: u32) -> io::Result<Analysis> {
    let text = fs::read_to_string(analysis_path(queue, id))
        .map_err(|e| io::Error::new(e.kind(), format!("no analysis for job {id}: {e}")))?;
    serde_json::from_str(&text).map_err(io::Error::other)
}

pub fn analysis_scratch_path(queue: &Queue, id: u32) -> PathBuf {
    analysis_path(queue, id).with_extension("pending")
}

pub fn load_requests(queue: &Queue) -> io::Result<Vec<FeatureRequest>> {
    match fs::read_to_string(requests_path(queue)) {
        Ok(text) => serde_json::from_str(&text).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn save_requests(queue: &Queue, requests: &[FeatureRequest]) -> io::Result<()> {
    let path = requests_path(queue);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(requests)?)?;
    fs::rename(tmp, path)
}

fn extend_unique<T: PartialEq>(into: &mut Vec<T>, from: Vec<T>) {
    for item in from {
        if !into.contains(&item) {
            into.push(item);
        }
    }
}

pub fn merge_requests(
    existing: Vec<FeatureRequest>,
    incoming: Vec<FeatureRequest>,
) -> Vec<FeatureRequest> {
    let mut merged = existing;
    for request in incoming {
        match merged.iter_mut().find(|r| r.id == request.id) {
            Some(current) => {
                extend_unique(&mut current.source_job_ids, request.source_job_ids);
                extend_unique(&mut current.evidence, request.evidence);
            }
            None => merged.push(request),
        }
    }
    merged
}

pub fn merge_into_store(queue: &Queue, incoming: Vec<FeatureRequest>) -> io::Result<()> {
    let _guard = REQUESTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let merged = merge_requests(load_requests(queue)?, incoming);
    save_requests(queue, &merged)
}

pub fn set_request_status(
    queue: &Queue,
    id: &str,
    status: RequestStatus,
) -> io::Result<FeatureRequest> {
    let _guard = REQUESTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut requests = load_requests(queue)?;
    let request = requests.iter_mut().find(|r| r.id == id).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("no feature request {id}"))
    })?;
    request.status = status;
    let updated = request.clone();
    save_requests(queue, &requests)?;
    Ok(updated)
}

fn label<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn kind_label(kind: EventKind) -> String {
    label(&kind)
}

pub fn render_events(events: &[Event]) -> String {
    if events.is_empty() {
        return "(no events)".into();
    }
    events
        .iter()
        .map(|e| {
            let stage = e
                .stage
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            format!(
                "{} {}{stage} {} {}",
                e.ts,
                label(&e.kind),
                e.agent,
                e.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_analysis(analysis: &Analysis) -> String {
    let mut out = format!(
        "job #{} {}\n\n{}",
        analysis.job_id, analysis.outcome, analysis.summary
    );
    if !analysis.root_causes.is_empty() {
        out.push_str("\n\nRoot causes:");
        for cause in &analysis.root_causes {
            out.push_str(&format!("\n- {cause}"));
        }
    }
    if !analysis.feature_requests.is_empty() {
        out.push_str("\n\nFeature requests:");
        for r in &analysis.feature_requests {
            out.push_str(&format!("\n- {} ({})", r.id, r.title));
        }
    }
    out
}

pub fn render_requests(requests: &[FeatureRequest]) -> String {
    if requests.is_empty() {
        return "(no feature requests)".into();
    }
    requests
        .iter()
        .map(|r| {
            let jobs: Vec<String> = r.source_job_ids.iter().map(|j| format!("#{j}")).collect();
            format!(
                "{} [{} / {}] {}\n    jobs: {}\n    {}",
                r.id,
                label(&r.status),
                label(&r.priority),
                r.title,
                jobs.join(", "),
                r.rationale
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Spec;
    use run_events::Priority;

    fn temp_queue(tag: &str) -> Queue {
        let dir = std::env::temp_dir().join(format!("rq-events-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Queue::at(dir).unwrap()
    }

    fn job(q: &Queue) -> Job {
        q.submit(Spec {
            name: None,
            workspace: q.root().join("ws"),
            source: "/s".into(),
            max_iter: None,
            inherit_env: false,
        })
        .unwrap()
    }

    fn event(agent: &str, kind: EventKind, ts: &str) -> Event {
        let mut e = Event::new(agent, kind, "x");
        e.ts = ts.to_string();
        e
    }

    fn request(id: &str, jobs: &[u64], evidence: &[&str]) -> FeatureRequest {
        FeatureRequest {
            id: id.into(),
            title: format!("title {id}"),
            rationale: "why".into(),
            evidence: evidence.iter().map(|s| s.to_string()).collect(),
            source_job_ids: jobs.to_vec(),
            priority: Priority::Medium,
            status: RequestStatus::Open,
        }
    }

    #[test]
    fn appended_events_read_back_in_order_with_the_job_id_filled_in() {
        let q = temp_queue("append");
        let j = job(&q);
        let first = event("a", EventKind::StageStart, "2026-01-01T00:00:00.000Z");
        let second = event("b", EventKind::Decision, "2026-01-01T00:00:01.000Z");
        append(&q, j.id, &[first]).unwrap();
        append(&q, j.id, &[second]).unwrap();
        let events = read(&q, &j, &EventFilter::default()).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].agent, "a");
        assert_eq!(events[1].agent, "b");
        assert_eq!(events[0].job_id, Some(u64::from(j.id)));
    }

    #[test]
    fn reading_filters_by_kind_agent_and_since() {
        let q = temp_queue("filter");
        let j = job(&q);
        append(
            &q,
            j.id,
            &[
                event("a", EventKind::Finding, "2026-01-01T00:00:00.000Z"),
                event("b", EventKind::Finding, "2026-01-01T00:00:01.000Z"),
                event("a", EventKind::Error, "2026-01-01T00:00:02.000Z"),
            ],
        )
        .unwrap();
        let by_kind = EventFilter {
            kind: Some(EventKind::Finding),
            ..Default::default()
        };
        assert_eq!(read(&q, &j, &by_kind).unwrap().len(), 2);
        let by_agent = EventFilter {
            agent: Some("a".into()),
            ..Default::default()
        };
        assert_eq!(read(&q, &j, &by_agent).unwrap().len(), 2);
        let since = EventFilter {
            since: Some("2026-01-01T00:00:01.000Z".into()),
            ..Default::default()
        };
        let after = read(&q, &j, &since).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].kind, EventKind::Error);
        let all = EventFilter {
            kind: Some(EventKind::Finding),
            agent: Some("a".into()),
            since: Some("2025-12-31T00:00:00Z".into()),
        };
        assert_eq!(read(&q, &j, &all).unwrap().len(), 1);
    }

    #[test]
    fn a_job_with_no_events_reads_as_empty() {
        let q = temp_queue("none");
        let j = job(&q);
        assert!(read(&q, &j, &EventFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn reading_falls_back_to_the_workspace_file_and_skips_torn_lines() {
        let q = temp_queue("fallback");
        let j = job(&q);
        fs::create_dir_all(&j.workspace).unwrap();
        let good = event("a", EventKind::Decision, "2026-01-01T00:00:00.000Z").to_json_line();
        fs::write(
            j.workspace.join("events.jsonl"),
            format!("{good}\n{{\"ts\":\"tr"),
        )
        .unwrap();
        assert_eq!(read(&q, &j, &EventFilter::default()).unwrap().len(), 1);
        let served = event("b", EventKind::Decision, "2026-01-01T00:00:00.000Z");
        append(&q, j.id, &[served]).unwrap();
        let events = read(&q, &j, &EventFilter::default()).unwrap();
        assert_eq!(events[0].agent, "b");
    }

    #[test]
    fn an_analysis_round_trips() {
        let q = temp_queue("analysis");
        let analysis = Analysis {
            job_id: 3,
            outcome: "failed".into(),
            summary: "s".into(),
            root_causes: vec!["c".into()],
            feature_requests: vec![request("r", &[3], &["e"])],
        };
        assert!(read_analysis(&q, 3).is_err());
        write_analysis(&q, &analysis).unwrap();
        assert_eq!(read_analysis(&q, 3).unwrap(), analysis);
    }

    #[test]
    fn a_new_request_is_added() {
        let q = temp_queue("new-request");
        merge_into_store(&q, vec![request("a", &[1], &["e1"])]).unwrap();
        merge_into_store(&q, vec![request("b", &[2], &["e2"])]).unwrap();
        let ids: Vec<String> = load_requests(&q)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn an_existing_request_keeps_its_status_and_gains_jobs_and_evidence() {
        let q = temp_queue("merge");
        merge_into_store(&q, vec![request("a", &[1], &["e1"])]).unwrap();
        set_request_status(&q, "a", RequestStatus::Accepted).unwrap();
        let mut again = request("a", &[2], &["e2", "e1"]);
        again.title = "renamed".into();
        again.status = RequestStatus::Open;
        merge_into_store(&q, vec![again]).unwrap();
        let requests = load_requests(&q).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].status, RequestStatus::Accepted);
        assert_eq!(requests[0].title, "title a");
        assert_eq!(requests[0].source_job_ids, [1, 2]);
        assert_eq!(requests[0].evidence, ["e1", "e2"]);
    }

    #[test]
    fn merging_the_same_job_twice_changes_nothing() {
        let first = merge_requests(vec![], vec![request("a", &[1], &["e1"])]);
        let second = merge_requests(first.clone(), vec![request("a", &[1], &["e1"])]);
        assert_eq!(first, second);
    }

    #[test]
    fn duplicates_inside_one_batch_merge() {
        let merged = merge_requests(
            vec![],
            vec![request("a", &[1], &["e1"]), request("a", &[2], &["e2"])],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source_job_ids, [1, 2]);
        assert_eq!(merged[0].evidence, ["e1", "e2"]);
    }

    #[test]
    fn setting_status_persists_and_rejects_unknown_ids() {
        let q = temp_queue("status");
        merge_into_store(&q, vec![request("a", &[1], &[])]).unwrap();
        let updated = set_request_status(&q, "a", RequestStatus::Done).unwrap();
        assert_eq!(updated.status, RequestStatus::Done);
        assert_eq!(load_requests(&q).unwrap()[0].status, RequestStatus::Done);
        let err = set_request_status(&q, "missing", RequestStatus::Done).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn kinds_and_statuses_parse_from_text() {
        assert_eq!(parse_kind("gate_result").unwrap(), EventKind::GateResult);
        assert!(parse_kind("nope").is_err());
        assert_eq!(parse_status("rejected").unwrap(), RequestStatus::Rejected);
        assert!(parse_status("nope").is_err());
    }
}
