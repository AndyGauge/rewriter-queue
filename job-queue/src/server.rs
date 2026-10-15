use crate::events::{self, EventFilter};
use crate::job::{Queue, Spec};
use crate::{archive, backend, worker};
use run_events::Event;
use std::io::{self, Read};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response};

const MAX_UPLOAD: u64 = 256 * 1024 * 1024;
const MAX_EVENTS_BODY: u64 = 8 * 1024 * 1024;

enum Reply {
    Text(String),
    Bytes(Vec<u8>),
}

type Failure = (u16, String);

/// The full-access token, plus an optional read-only one that can look but not change anything.
#[derive(Clone)]
pub struct Tokens {
    pub full: String,
    pub read_only: Option<String>,
}

#[derive(PartialEq)]
enum Access {
    Full,
    ReadOnly,
}

impl Tokens {
    fn access(&self, header: &str) -> Option<Access> {
        if header == format!("Bearer {}", self.full) {
            return Some(Access::Full);
        }
        match &self.read_only {
            Some(t) if header == format!("Bearer {t}") => Some(Access::ReadOnly),
            _ => None,
        }
    }
}

pub fn run(queue: Queue, bind: &str, tokens: Tokens) -> io::Result<()> {
    let _lock = worker::lock_worker(&queue)?
        .ok_or_else(|| io::Error::other("another process is already running this queue"))?;
    let server = tiny_http::Server::http(bind).map_err(io::Error::other)?;

    let worker_queue = queue.clone();
    let reporting = worker::Reporting {
        url: loopback_url(bind),
        token: tokens.full.clone(),
    };
    std::thread::spawn(move || {
        if let Err(e) = worker::serve(&worker_queue, None, Some(&reporting)) {
            eprintln!("worker stopped: {e}");
        }
        std::process::exit(1);
    });

    eprintln!("rewriter-queue listening on http://{bind}");
    serve_requests(&server, &queue, tokens);
    Ok(())
}

fn serve_requests(server: &tiny_http::Server, queue: &Queue, tokens: Tokens) {
    let tokens = Arc::new(tokens);
    for request in server.incoming_requests() {
        let queue = queue.clone();
        let tokens = Arc::clone(&tokens);
        std::thread::spawn(move || handle(&queue, &tokens, request));
    }
}

fn loopback_url(bind: &str) -> String {
    let (host, port) = bind.rsplit_once(':').unwrap_or((bind, ""));
    let host = match host {
        "" | "0.0.0.0" | "[::]" => "127.0.0.1",
        other => other,
    };
    format!("http://{host}:{port}")
}

fn handle(queue: &Queue, tokens: &Tokens, mut request: Request) {
    let response = match route(queue, tokens, &mut request) {
        Ok(Reply::Text(text)) => Response::from_string(text).boxed(),
        Ok(Reply::Bytes(bytes)) => Response::from_data(bytes)
            .with_header(Header::from_bytes("content-type", "application/gzip").unwrap())
            .boxed(),
        Err((code, message)) => Response::from_string(message)
            .with_status_code(code)
            .boxed(),
    };
    let _ = request.respond(response);
}

fn fail(e: io::Error) -> Failure {
    let code = if e.kind() == io::ErrorKind::NotFound {
        404
    } else {
        400
    };
    (code, e.to_string())
}

fn route(queue: &Queue, tokens: &Tokens, request: &mut Request) -> Result<Reply, Failure> {
    let access = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("authorization"))
        .and_then(|h| tokens.access(h.value.as_str()));
    let Some(access) = access else {
        return Err((401, "unauthorized".into()));
    };

    let url = url::Url::parse(&format!("http://queue{}", request.url()))
        .map_err(|e| (400, e.to_string()))?;
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    let job_id = |segment: &str| {
        segment
            .parse::<u32>()
            .map_err(|_| (400, format!("bad job id: {segment}")))
    };
    let segments: Vec<&str> = url.path().trim_matches('/').split('/').collect();

    // The read-only token may look at progress and artifacts. It may not change anything, and
    // it may not pull the whole uploaded workspace, which can hold secrets the run never wrote.
    if access == Access::ReadOnly {
        let whole_workspace = matches!(segments.as_slice(), ["jobs", _, "result"]);
        if request.method() != &Method::Get || whole_workspace {
            return Err((403, "this token is read-only".into()));
        }
    }

    match (request.method().clone(), segments.as_slice()) {
        (Method::Post, ["jobs"]) => {
            let mut body = Vec::new();
            request
                .as_reader()
                .take(MAX_UPLOAD)
                .read_to_end(&mut body)
                .map_err(fail)?;
            let name = query("name").filter(|n| !n.is_empty());
            let max_iter = query("max_iter").and_then(|n| n.parse().ok());
            let inherit_env = query("inherit_env").as_deref() == Some("true");
            submit(queue, &body, name, max_iter, inherit_env).map(Reply::Text)
        }
        (Method::Get, ["jobs"]) => backend::list_text(queue).map(Reply::Text).map_err(fail),
        (Method::Get, ["jobs", id]) => {
            let lines = query("lines").and_then(|n| n.parse().ok()).unwrap_or(20);
            backend::status_text(queue, job_id(id)?, lines)
                .map(Reply::Text)
                .map_err(fail)
        }
        (Method::Post, ["jobs", id, "cancel"]) => backend::cancel_text(queue, job_id(id)?)
            .map(Reply::Text)
            .map_err(fail),
        (Method::Get, ["jobs", id, "result"]) => {
            let job = queue.get(job_id(id)?).map_err(fail)?;
            archive::pack(&[(&job.workspace, "")])
                .map(Reply::Bytes)
                .map_err(fail)
        }
        (Method::Get, ["jobs", id, "artifacts.tar.gz"]) => {
            let job = queue.get(job_id(id)?).map_err(fail)?;
            archive::pack(&[(&job.workspace.join("artifacts"), "")])
                .map(Reply::Bytes)
                .map_err(fail)
        }
        (Method::Get, ["jobs", id, "artifacts"]) => {
            let job = queue.get(job_id(id)?).map_err(fail)?;
            backend::list_artifacts_at(&job.workspace)
                .map(Reply::Text)
                .map_err(fail)
        }
        (Method::Get, ["jobs", id, "artifact"]) => {
            let job = queue.get(job_id(id)?).map_err(fail)?;
            let path = query("path").ok_or((400, "missing query param: path".into()))?;
            backend::read_artifact_at(&job.workspace, &path)
                .map(Reply::Text)
                .map_err(fail)
        }
        (Method::Post, ["jobs", id, "events"]) => {
            let id = job_id(id)?;
            queue.get(id).map_err(fail)?;
            let body = read_body(request, MAX_EVENTS_BODY)?;
            let batch = parse_event_batch(&body)?;
            events::append(queue, id, &batch).map_err(fail)?;
            Ok(Reply::Text(format!("{} events stored", batch.len())))
        }
        (Method::Get, ["jobs", id, "events"]) => {
            let job = queue.get(job_id(id)?).map_err(fail)?;
            let filter = EventFilter {
                kind: query("kind")
                    .map(|k| events::parse_kind(&k))
                    .transpose()
                    .map_err(|e| (400, e.to_string()))?,
                agent: query("agent"),
                since: query("since"),
            };
            let found = events::read(queue, &job, &filter).map_err(fail)?;
            Ok(Reply::Text(events::to_jsonl(&found)))
        }
        (Method::Get, ["jobs", id, "analysis"]) => {
            let id = job_id(id)?;
            queue.get(id).map_err(fail)?;
            let analysis = events::read_analysis(queue, id).map_err(fail)?;
            json_reply(&analysis)
        }
        (Method::Get, ["feature-requests"]) => {
            json_reply(&events::load_requests(queue).map_err(fail)?)
        }
        (Method::Post, ["feature-requests", id, "status"]) => {
            let body = read_body(request, 4096)?;
            let status = parse_status_body(&body)?;
            let updated = events::set_request_status(queue, id, status).map_err(fail)?;
            json_reply(&updated)
        }
        (Method::Get, ["watch"]) => backend::watch_text(queue).map(Reply::Text).map_err(fail),
        _ => Err((404, "not found".into())),
    }
}

fn read_body(request: &mut Request, limit: u64) -> Result<Vec<u8>, Failure> {
    let mut body = Vec::new();
    request
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut body)
        .map_err(fail)?;
    if body.len() as u64 > limit {
        return Err((413, format!("body larger than {limit} bytes")));
    }
    Ok(body)
}

fn json_reply<T: serde::Serialize>(value: &T) -> Result<Reply, Failure> {
    serde_json::to_string_pretty(value)
        .map(Reply::Text)
        .map_err(|e| (500, e.to_string()))
}

fn parse_event_batch(body: &[u8]) -> Result<Vec<Event>, Failure> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| (400, format!("bad event JSON: {e}")))?;
    let parsed = match value {
        serde_json::Value::Array(_) => serde_json::from_value(value),
        single => serde_json::from_value(single).map(|e| vec![e]),
    };
    parsed.map_err(|e| (400, format!("bad event: {e}")))
}

fn parse_status_body(body: &[u8]) -> Result<run_events::RequestStatus, Failure> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| (400, "body must be {\"status\": \"...\"}".to_string()))?;
    let text = value["status"]
        .as_str()
        .ok_or((400, "body must be {\"status\": \"...\"}".to_string()))?;
    events::parse_status(text).map_err(|e| (400, e.to_string()))
}

/// Unpacks fully before the job is queued so the worker never sees a half-written upload.
fn submit(
    queue: &Queue,
    body: &[u8],
    name: Option<String>,
    max_iter: Option<u32>,
    inherit_env: bool,
) -> Result<String, Failure> {
    let dir = queue.new_work_dir().map_err(fail)?;
    archive::unpack(body, &dir).map_err(fail)?;
    let source = dir.join("source");
    if !source.is_dir() {
        return Err((400, "upload has no source directory".into()));
    }
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).map_err(fail)?;

    let job = queue
        .submit(Spec {
            name,
            source,
            workspace,
            max_iter,
            inherit_env,
        })
        .map_err(fail)?;
    backend::queued_text(queue, &job).map_err(fail)
}

#[cfg(test)]
mod tests;
