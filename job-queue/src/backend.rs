use crate::client::Client;
use crate::job::{Job, Queue, Spec, State};
use crate::{config, view, worker};
use std::io;
use std::path::{Component, Path, PathBuf};

pub struct Submission {
    pub source: PathBuf,
    pub workspace: PathBuf,
    pub max_iter: Option<u32>,
    pub inherit_env: bool,
}

pub enum Backend {
    Local(Queue),
    Remote(Client),
}

impl Backend {
    pub fn open() -> io::Result<Self> {
        match config::server_url() {
            Some(url) => Ok(Backend::Remote(Client::new(url, config::token()?))),
            None => Ok(Backend::Local(Queue::open()?)),
        }
    }

    pub fn submit(&self, sub: Submission) -> io::Result<String> {
        let source = sub.source.canonicalize()?;
        match self {
            Backend::Local(queue) => {
                let job = queue.submit(Spec {
                    name: None,
                    source,
                    workspace: std::path::absolute(&sub.workspace)?,
                    max_iter: sub.max_iter,
                    inherit_env: sub.inherit_env,
                })?;
                let mut text = queued_text(queue, &job)?;
                if worker::ensure_running(queue)? {
                    text.push_str(" — worker started");
                }
                Ok(text)
            }
            Backend::Remote(client) => {
                client.submit(&source, &sub.workspace, sub.max_iter, sub.inherit_env)
            }
        }
    }

    pub fn list(&self) -> io::Result<String> {
        match self {
            Backend::Local(queue) => list_text(queue),
            Backend::Remote(client) => client.get("/jobs"),
        }
    }

    pub fn status(&self, id: u32, log_lines: usize) -> io::Result<String> {
        match self {
            Backend::Local(queue) => status_text(queue, id, log_lines),
            Backend::Remote(client) => client.get(&format!("/jobs/{id}?lines={log_lines}")),
        }
    }

    pub fn cancel(&self, id: u32) -> io::Result<String> {
        match self {
            Backend::Local(queue) => cancel_text(queue, id),
            Backend::Remote(client) => client.post(&format!("/jobs/{id}/cancel")),
        }
    }

    pub fn watch_frame(&self) -> io::Result<String> {
        match self {
            Backend::Local(queue) => watch_text(queue),
            Backend::Remote(client) => client.get("/watch"),
        }
    }

    pub fn fetch(&self, id: u32, out: &Path) -> io::Result<String> {
        match self {
            Backend::Local(_) => Err(io::Error::other(
                "local jobs write straight to their workspace; there is nothing to fetch",
            )),
            Backend::Remote(client) => client.fetch(id, out),
        }
    }

    /// Download everything a job has produced in one shot — the whole `artifacts/` tree
    /// (contract, test matrix, inductive analysis, schema, v2/ crate, synthesis/ checkpoints),
    /// not just one file. Lighter than `fetch`, which also drags in the originally-uploaded
    /// project tree (node_modules, .env, build output, ...).
    pub fn download_artifacts(&self, id: u32, out: &Path) -> io::Result<String> {
        match self {
            Backend::Local(queue) => {
                let ws = queue.get(id)?.workspace;
                Ok(format!(
                    "local job — artifacts already on disk at {}",
                    ws.join("artifacts").display()
                ))
            }
            Backend::Remote(client) => client.download_artifacts(id, out),
        }
    }

    /// List artifact files emitted so far — works on a running job, not just a finished one.
    /// Unlike `fetch`, this only looks at `<workspace>/artifacts`, so it stays fast and never
    /// pulls in the uploaded project tree (node_modules, .env, build output, ...).
    pub fn list_artifacts(&self, id: u32) -> io::Result<String> {
        match self {
            Backend::Local(queue) => list_artifacts_at(&queue.get(id)?.workspace),
            Backend::Remote(client) => client.get(&format!("/jobs/{id}/artifacts")),
        }
    }

    /// Read one artifact's raw content by its path relative to `artifacts/` (as `list_artifacts`
    /// prints it), e.g. "objective_contract.md" or "v2/Cargo.toml".
    pub fn read_artifact(&self, id: u32, rel_path: &str) -> io::Result<String> {
        match self {
            Backend::Local(queue) => read_artifact_at(&queue.get(id)?.workspace, rel_path),
            Backend::Remote(client) => {
                client.get_query(&format!("/jobs/{id}/artifact"), &[("path", rel_path)])
            }
        }
    }
}

pub fn list_artifacts_at(workspace: &Path) -> io::Result<String> {
    let dir = workspace.join("artifacts");
    if !dir.exists() {
        return Ok("(no artifacts yet)".into());
    }
    let mut paths: Vec<String> = walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(&dir)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .collect();
    paths.sort();
    if paths.is_empty() {
        Ok("(no artifacts yet)".into())
    } else {
        Ok(paths.join("\n"))
    }
}

pub fn read_artifact_at(workspace: &Path, rel_path: &str) -> io::Result<String> {
    let dir = workspace.join("artifacts");
    let has_illegal_component = Path::new(rel_path)
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)));
    if rel_path.is_empty() || has_illegal_component {
        return Err(io::Error::other(format!("invalid artifact path: {rel_path}")));
    }
    std::fs::read_to_string(dir.join(rel_path))
}

pub fn queued_text(queue: &Queue, job: &Job) -> io::Result<String> {
    let ahead = queue
        .list()?
        .iter()
        .filter(|j| j.id < job.id)
        .filter(|j| matches!(queue.effective_state(j), State::Queued | State::Running))
        .count();
    Ok(format!("queued job #{} ({ahead} ahead)", job.id))
}

pub fn list_text(queue: &Queue) -> io::Result<String> {
    Ok(view::list(queue, &queue.list()?))
}

pub fn status_text(queue: &Queue, id: u32, log_lines: usize) -> io::Result<String> {
    let job = queue.get(id)?;
    Ok(format!(
        "{}\n\n{}",
        view::detail(queue, &job),
        queue.log_tail(id, log_lines)
    ))
}

pub fn cancel_text(queue: &Queue, id: u32) -> io::Result<String> {
    let job = queue.request_cancel(id)?;
    Ok(format!("cancel requested for job #{}", job.id))
}

pub fn watch_text(queue: &Queue) -> io::Result<String> {
    Ok(view::watch_frame(queue, &queue.list()?))
}
