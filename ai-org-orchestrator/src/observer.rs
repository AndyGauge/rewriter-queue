use run_events::{Event, EventKind};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const POST_TIMEOUT: Duration = Duration::from_secs(2);
const POST_BACKOFF: Duration = Duration::from_secs(30);
const MAX_SUMMARY_CHARS: usize = 300;
const MAX_DETAIL_TEXT_CHARS: usize = 4000;

#[derive(Default)]
pub struct AgentCall {
    pub agent: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    pub ok: bool,
    pub note: String,
}

#[derive(Clone)]
struct QueueSink {
    url: String,
    token: String,
}

struct Inner {
    path: PathBuf,
    job_id: Option<u64>,
    queue: Option<QueueSink>,
    write: Mutex<()>,
    stage: Mutex<Option<String>>,
    gate_attempts: Mutex<HashMap<String, u64>>,
    post_paused_until: Mutex<Option<Instant>>,
    warned_write: AtomicBool,
    warned_post: AtomicBool,
}

/// Records run events to `events.jsonl` and, when a queue server is configured, posts each one
/// there too. Reporting is strictly best effort: no method panics or returns an error, and a
/// failing sink only costs one log line.
#[derive(Clone)]
pub struct Observer {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}

fn first_line(s: &str) -> &str {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

impl Observer {
    pub fn new(
        path: impl Into<PathBuf>,
        job_id: Option<u64>,
        queue: Option<(String, String)>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                path: path.into(),
                job_id,
                queue: queue.map(|(url, token)| QueueSink {
                    url: url.trim_end_matches('/').to_string(),
                    token,
                }),
                write: Mutex::new(()),
                stage: Mutex::new(None),
                gate_attempts: Mutex::new(HashMap::new()),
                post_paused_until: Mutex::new(None),
                warned_write: AtomicBool::new(false),
                warned_post: AtomicBool::new(false),
            }),
        }
    }

    /// Reads `REWRITER_JOB_ID`, `REWRITER_QUEUE_URL` and `REWRITER_QUEUE_TOKEN`. The queue sink
    /// is only enabled when both the URL and token are present and non-empty.
    pub fn from_env(workspace_root: &Path) -> Self {
        let job_id = std::env::var("REWRITER_JOB_ID")
            .ok()
            .and_then(|v| v.trim().parse().ok());
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let queue = var("REWRITER_QUEUE_URL").zip(var("REWRITER_QUEUE_TOKEN"));
        Self::new(workspace_root.join("events.jsonl"), job_id, queue)
    }

    pub fn emit(&self, mut event: Event) {
        let inner = &self.inner;
        if event.job_id.is_none() {
            event.job_id = inner.job_id;
        }
        if event.stage.is_none() {
            event.stage = lock(&inner.stage).clone();
        }
        let line = event.to_json_line();
        self.append(&line);
        self.post(&event, &line);
    }

    fn append(&self, line: &str) {
        let _guard = lock(&self.inner.write);
        let result = (|| {
            if let Some(parent) = self.inner.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.inner.path)?;
            f.write_all(format!("{line}\n").as_bytes())
        })();
        if let Err(e) = result {
            if !self.inner.warned_write.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "    [observer] cannot write {}: {e} -- events will not be saved",
                    self.inner.path.display()
                );
            }
        }
    }

    fn post(&self, event: &Event, line: &str) {
        let (Some(queue), Some(job_id)) = (&self.inner.queue, event.job_id) else {
            return;
        };
        {
            let mut paused = lock(&self.inner.post_paused_until);
            match *paused {
                Some(until) if Instant::now() < until => return,
                Some(_) => *paused = None,
                None => {}
            }
        }
        let result = ureq::post(&format!("{}/jobs/{job_id}/events", queue.url))
            .timeout(POST_TIMEOUT)
            .set("authorization", &format!("Bearer {}", queue.token))
            .set("content-type", "application/json")
            .send_string(line);
        match result {
            Ok(_) => {
                self.inner.warned_post.store(false, Ordering::Relaxed);
            }
            Err(e) => {
                *lock(&self.inner.post_paused_until) = Some(Instant::now() + POST_BACKOFF);
                if !self.inner.warned_post.swap(true, Ordering::Relaxed) {
                    eprintln!("    [observer] cannot post events to the queue server: {e} -- pausing posts, events.jsonl still written");
                }
            }
        }
    }

    pub fn stage(&self, name: &str) {
        self.end_stage();
        *lock(&self.inner.stage) = Some(name.to_string());
        self.emit(Event::new("orchestrator", EventKind::StageStart, name));
    }

    pub fn end_stage(&self) {
        self.close_stage(true);
    }

    /// Records a failure that ends the run: an error event, then the open stage (if any) ended
    /// with `ok: false`, so the outcome is unambiguous even when nothing else follows.
    pub fn fail(&self, agent: &str, text: &str) {
        self.error(agent, text);
        self.close_stage(false);
    }

    fn close_stage(&self, ok: bool) {
        let previous = lock(&self.inner.stage).take();
        if let Some(name) = previous {
            self.emit(
                Event::new("orchestrator", EventKind::StageEnd, &name)
                    .with_stage(&name)
                    .with_detail(json!({"ok": ok})),
            );
        }
    }

    pub fn agent_call(&self, call: &AgentCall) {
        let summary = if call.note.is_empty() {
            format!(
                "{}: {}in/{}out in {}ms",
                call.agent, call.input_tokens, call.output_tokens, call.latency_ms
            )
        } else {
            format!("{}: {}", call.agent, call.note)
        };
        self.emit(
            Event::new(&call.agent, EventKind::AgentCall, &summary).with_detail(json!({
                "provider": call.provider,
                "model": call.model,
                "input_tokens": call.input_tokens,
                "output_tokens": call.output_tokens,
                "latency_ms": call.latency_ms,
                "ok": call.ok,
            })),
        );
    }

    pub fn decision(&self, agent: &str, text: &str) {
        self.text_event(agent, EventKind::Decision, text);
    }

    pub fn deviation(&self, agent: &str, text: &str) {
        self.text_event(agent, EventKind::Deviation, text);
    }

    pub fn retry(&self, agent: &str, text: &str) {
        self.text_event(agent, EventKind::Retry, text);
    }

    pub fn error(&self, agent: &str, text: &str) {
        self.text_event(agent, EventKind::Error, text);
    }

    fn text_event(&self, agent: &str, kind: EventKind, text: &str) {
        let summary = truncate_chars(first_line(text), MAX_SUMMARY_CHARS);
        self.emit(
            Event::new(agent, kind, &summary)
                .with_detail(json!({"text": truncate_chars(text, MAX_DETAIL_TEXT_CHARS)})),
        );
    }

    pub fn finding(&self, agent: &str, severity: &str, category: &str, text: &str, evidence: &str) {
        self.emit(
            Event::new(
                agent,
                EventKind::Finding,
                &truncate_chars(text, MAX_DETAIL_TEXT_CHARS),
            )
            .with_detail(json!({
                "severity": severity,
                "category": category,
                "evidence": truncate_chars(evidence, MAX_DETAIL_TEXT_CHARS),
            })),
        );
    }

    /// `key` identifies one build directory (a milestone variant, or "" for the shared one);
    /// `attempt` counts consecutive runs against it since it last passed.
    pub fn gate_result(&self, key: &str, passed: bool, error_bytes: usize) {
        let attempt = {
            let mut attempts = lock(&self.inner.gate_attempts);
            let n = attempts.entry(key.to_string()).or_insert(0);
            *n += 1;
            let attempt = *n;
            if passed {
                *n = 0;
            }
            attempt
        };
        let summary = if passed {
            format!("quality gate passed (attempt {attempt})")
        } else {
            format!("quality gate failed (attempt {attempt}, {error_bytes} bytes of errors)")
        };
        let mut detail = json!({"passed": passed, "error_bytes": error_bytes, "attempt": attempt});
        if !key.is_empty() {
            detail["variant"] = Value::String(key.to_string());
        }
        self.emit(Event::new("QualityGate", EventKind::GateResult, &summary).with_detail(detail));
    }

    pub fn install_panic_hook(&self) {
        let observer = self.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = info.payload();
            let reason = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            let message = match info.location() {
                Some(l) => format!("panic: {reason} (at {}:{})", l.file(), l.line()),
                None => format!("panic: {reason}"),
            };
            if std::thread::current().name() == Some("main") {
                observer.fail("orchestrator", &message);
            } else {
                observer.error("orchestrator", &message);
            }
            previous(info);
        }));
    }
}

#[cfg(test)]
mod tests;
