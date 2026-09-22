use crate::job::{Queue, Spec};
use crate::{archive, backend, worker};
use std::io::{self, Read};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response};

const MAX_UPLOAD: u64 = 256 * 1024 * 1024;

enum Reply {
    Text(String),
    Bytes(Vec<u8>),
}

type Failure = (u16, String);

pub fn run(queue: Queue, bind: &str, token: String) -> io::Result<()> {
    let _lock = worker::lock_worker(&queue)?
        .ok_or_else(|| io::Error::other("another process is already running this queue"))?;
    let server = tiny_http::Server::http(bind).map_err(io::Error::other)?;

    let worker_queue = queue.clone();
    std::thread::spawn(move || {
        if let Err(e) = worker::serve(&worker_queue, None) {
            eprintln!("worker stopped: {e}");
        }
        std::process::exit(1);
    });

    eprintln!("rewriter-queue listening on http://{bind}");
    let token = Arc::new(token);
    for request in server.incoming_requests() {
        let queue = queue.clone();
        let token = Arc::clone(&token);
        std::thread::spawn(move || handle(&queue, &token, request));
    }
    Ok(())
}

fn handle(queue: &Queue, token: &str, mut request: Request) {
    let response = match route(queue, token, &mut request) {
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

fn route(queue: &Queue, token: &str, request: &mut Request) -> Result<Reply, Failure> {
    let expected = format!("Bearer {token}");
    let authorized = request
        .headers()
        .iter()
        .any(|h| h.field.equiv("authorization") && h.value.as_str() == expected);
    if !authorized {
        return Err((401, "unauthorized".into()));
    }

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
        (Method::Get, ["watch"]) => backend::watch_text(queue).map(Reply::Text).map_err(fail),
        _ => Err((404, "not found".into())),
    }
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
