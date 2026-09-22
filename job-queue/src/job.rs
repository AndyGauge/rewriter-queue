use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Succeeded => "succeeded",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u32,
    #[serde(default)]
    pub name: Option<String>,
    pub workspace: PathBuf,
    pub source: PathBuf,
    pub max_iter: Option<u32>,
    pub inherit_env: bool,
    pub state: State,
    pub submitted_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// How many times the worker has relaunched this job after finding it interrupted
    /// (worker crash or restart) rather than cleanly finished. Capped by the worker to
    /// avoid crash-looping a job whose orchestrator dies immediately every time.
    #[serde(default)]
    pub resume_count: u32,
}

pub struct Spec {
    pub name: Option<String>,
    pub workspace: PathBuf,
    pub source: PathBuf,
    pub max_iter: Option<u32>,
    pub inherit_env: bool,
}

const STAGE_SCAN_BYTES: u64 = 256 * 1024;

#[derive(Clone)]
pub struct Queue {
    root: PathBuf,
}

impl Queue {
    pub fn open() -> io::Result<Self> {
        let root = match std::env::var_os("REWRITER_QUEUE_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => {
                let home =
                    std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
                PathBuf::from(home).join(".local/share/rewriter/queue")
            }
        };
        Self::at(root)
    }

    pub fn at(root: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(root.join("jobs"))?;
        fs::create_dir_all(root.join("logs"))?;
        Ok(Self { root })
    }

    fn job_path(&self, id: u32) -> PathBuf {
        self.root.join("jobs").join(format!("{id:04}.json"))
    }

    fn cancel_path(&self, id: u32) -> PathBuf {
        self.root.join("jobs").join(format!("{id:04}.cancel"))
    }

    pub fn log_path(&self, id: u32) -> PathBuf {
        self.root.join("logs").join(format!("{id:04}.log"))
    }

    pub fn new_work_dir(&self) -> io::Result<PathBuf> {
        let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let dir = self
            .root
            .join("work")
            .join(format!("{nanos}-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("worker.lock")
    }

    pub fn worker_log_path(&self) -> PathBuf {
        self.root.join("worker.log")
    }

    pub fn submit(&self, spec: Spec) -> io::Result<Job> {
        let tmp = self.root.join(format!("submit-{}.tmp", std::process::id()));
        let mut id = self.list()?.last().map_or(1, |j| j.id + 1);
        loop {
            let job = Job {
                id,
                name: spec.name.clone(),
                workspace: spec.workspace.clone(),
                source: spec.source.clone(),
                max_iter: spec.max_iter,
                inherit_env: spec.inherit_env,
                state: State::Queued,
                submitted_at: Utc::now(),
                started_at: None,
                finished_at: None,
                pid: None,
                exit_code: None,
                error: None,
                resume_count: 0,
            };
            fs::write(&tmp, serde_json::to_vec_pretty(&job)?)?;
            match fs::hard_link(&tmp, self.job_path(id)) {
                Ok(()) => {
                    fs::remove_file(&tmp)?;
                    return Ok(job);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => id += 1,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn save(&self, job: &Job) -> io::Result<()> {
        let tmp = self.root.join(format!("save-{}.tmp", job.id));
        fs::write(&tmp, serde_json::to_vec_pretty(job)?)?;
        fs::rename(&tmp, self.job_path(job.id))
    }

    pub fn get(&self, id: u32) -> io::Result<Job> {
        let text = fs::read_to_string(self.job_path(id))
            .map_err(|e| io::Error::new(e.kind(), format!("no job {id}: {e}")))?;
        serde_json::from_str(&text).map_err(io::Error::other)
    }

    pub fn list(&self) -> io::Result<Vec<Job>> {
        let mut jobs: Vec<Job> = fs::read_dir(self.root.join("jobs"))?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        jobs.sort_by_key(|j| j.id);
        Ok(jobs)
    }

    pub fn next_queued(&self) -> io::Result<Option<Job>> {
        Ok(self.list()?.into_iter().find(|j| j.state == State::Queued))
    }

    pub fn request_cancel(&self, id: u32) -> io::Result<Job> {
        let job = self.get(id)?;
        if self.effective_state(&job) != State::Queued && job.state != State::Running {
            return Err(io::Error::other(format!(
                "job {id} is already {}",
                self.effective_state(&job).label()
            )));
        }
        fs::write(self.cancel_path(id), b"")?;
        Ok(job)
    }

    pub fn cancel_requested(&self, id: u32) -> bool {
        self.cancel_path(id).exists()
    }

    pub fn effective_state(&self, job: &Job) -> State {
        if job.state == State::Queued && self.cancel_requested(job.id) {
            State::Cancelled
        } else {
            job.state
        }
    }

    pub fn log_tail(&self, id: u32, lines: usize) -> String {
        let tail = read_tail(&self.log_path(id), STAGE_SCAN_BYTES);
        let all: Vec<&str> = tail.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

    pub fn stage(&self, id: u32) -> Option<String> {
        read_tail(&self.log_path(id), STAGE_SCAN_BYTES)
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("==> ").map(|s| s.trim().to_string()))
    }
}

fn read_tail(path: &Path, max_bytes: u64) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(max_bytes)));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_queue(tag: &str) -> Queue {
        let dir = std::env::temp_dir().join(format!("rq-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Queue::at(dir).unwrap()
    }

    fn spec() -> Spec {
        Spec {
            name: None,
            workspace: "/w".into(),
            source: "/s".into(),
            max_iter: None,
            inherit_env: false,
        }
    }

    #[test]
    fn ids_are_sequential_and_ordered() {
        let q = temp_queue("ids");
        let a = q.submit(spec()).unwrap();
        let b = q.submit(spec()).unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        assert_eq!(q.next_queued().unwrap().unwrap().id, 1);
    }

    #[test]
    fn cancelling_a_queued_job_is_effective_immediately() {
        let q = temp_queue("cancel");
        let job = q.submit(spec()).unwrap();
        q.request_cancel(job.id).unwrap();
        assert_eq!(q.effective_state(&q.get(job.id).unwrap()), State::Cancelled);
    }

    #[test]
    fn finished_jobs_cannot_be_cancelled() {
        let q = temp_queue("done");
        let mut job = q.submit(spec()).unwrap();
        job.state = State::Succeeded;
        q.save(&job).unwrap();
        assert!(q.request_cancel(job.id).is_err());
    }

    #[test]
    fn stage_is_the_last_arrow_line() {
        let q = temp_queue("stage");
        let job = q.submit(spec()).unwrap();
        fs::write(
            q.log_path(job.id),
            "==> Schema\nnoise\n==> V2 synthesis\nmore\n",
        )
        .unwrap();
        assert_eq!(q.stage(job.id).as_deref(), Some("V2 synthesis"));
        assert_eq!(q.log_tail(job.id, 2), "==> V2 synthesis\nmore");
    }
}
