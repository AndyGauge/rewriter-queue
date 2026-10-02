use crate::job::{Job, Queue};
use run_events::{
    parse_events, Analysis, Event, EventKind, FeatureRequest, Priority, RequestStatus,
};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

static APPEND_LOCK: Mutex<()> = Mutex::new(());

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
        event.job_id = Some(u64::from(id));
        buf.push_str(&event.to_json_line());
        buf.push('\n');
    }
    let _guard = APPEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(buf.as_bytes())
}

fn read_file_events(path: PathBuf) -> io::Result<Vec<Event>> {
    match fs::read(path) {
        Ok(bytes) => Ok(parse_events(&String::from_utf8_lossy(&bytes))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn identity(event: &Event) -> String {
    let mut event = event.clone();
    event.job_id = None;
    event.to_json_line()
}

/// The server's copy and the workspace file the orchestrator always writes can each be missing
/// events the other has (a post that failed, a run before the server was reachable). Taking the
/// union, keeping each distinct event as many times as the file that has it most often, never
/// drops anything and never double-counts events present in both.
pub fn merge_event_sources(served: Vec<Event>, local: Vec<Event>) -> Vec<Event> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for event in &served {
        *seen.entry(identity(event)).or_default() += 1;
    }
    let mut merged = served;
    let mut local_counts: HashMap<String, usize> = HashMap::new();
    for event in local {
        let key = identity(&event);
        let count = local_counts.entry(key.clone()).or_default();
        *count += 1;
        if *count > seen.get(&key).copied().unwrap_or(0) {
            merged.push(event);
        }
    }
    merged.sort_by_cached_key(|e| {
        chrono::DateTime::parse_from_rfc3339(&e.ts)
            .map(|t| t.timestamp_millis())
            .unwrap_or(i64::MIN)
    });
    merged
}

pub fn all_events(queue: &Queue, job: &Job) -> io::Result<Vec<Event>> {
    Ok(merge_event_sources(
        read_file_events(events_path(queue, job.id))?,
        read_file_events(job.workspace.join("events.jsonl"))?,
    ))
}

pub fn read(queue: &Queue, job: &Job, filter: &EventFilter) -> io::Result<Vec<Event>> {
    Ok(all_events(queue, job)?
        .into_iter()
        .filter(|e| filter.matches(e))
        .collect())
}

pub fn analysis_events_path(queue: &Queue, id: u32) -> PathBuf {
    analysis_path(queue, id).with_extension("events.tmp")
}

pub fn to_jsonl(events: &[Event]) -> String {
    events.iter().map(|e| e.to_json_line() + "\n").collect()
}

pub fn write_analysis(queue: &Queue, analysis: &Analysis) -> io::Result<()> {
    let id = u32::try_from(analysis.job_id)
        .map_err(|_| io::Error::other(format!("job id out of range: {}", analysis.job_id)))?;
    let path = analysis_path(queue, id);
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

fn raise(priority: Priority) -> Priority {
    match priority {
        Priority::Low => Priority::Medium,
        Priority::Medium | Priority::High => Priority::High,
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
                let from_new_job = request
                    .source_job_ids
                    .iter()
                    .any(|j| !current.source_job_ids.contains(j));
                if from_new_job {
                    current.priority = raise(current.priority);
                }
                extend_unique(&mut current.source_job_ids, request.source_job_ids);
                extend_unique(&mut current.evidence, request.evidence);
            }
            None => merged.push(request),
        }
    }
    merged
}

/// An OS-level lock keeps the worker's ingest and a CLI `request` in another process from
/// overwriting each other's read-modify-write. It is per open file description, so it also
/// serialises threads of one process.
fn with_requests_lock<T>(queue: &Queue, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(queue.root().join("feature-requests.lock"))?;
    lock.lock()?;
    f()
}

pub fn merge_into_store(queue: &Queue, incoming: Vec<FeatureRequest>) -> io::Result<()> {
    with_requests_lock(queue, || {
        let merged = merge_requests(load_requests(queue)?, incoming);
        save_requests(queue, &merged)
    })
}

pub fn set_request_status(
    queue: &Queue,
    id: &str,
    status: RequestStatus,
) -> io::Result<FeatureRequest> {
    with_requests_lock(queue, || {
        let mut requests = load_requests(queue)?;
        let request = requests.iter_mut().find(|r| r.id == id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no feature request {id}"))
        })?;
        request.status = status;
        let updated = request.clone();
        save_requests(queue, &requests)?;
        Ok(updated)
    })
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
mod tests;
