use crate::job::{Job, Queue, State};
use chrono::Utc;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(500);
const IDLE_EXIT: Duration = Duration::from_secs(300);
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Cap on automatic relaunches for a job the worker finds interrupted (crash or restart),
/// so a job whose orchestrator dies immediately every time doesn't loop forever.
const MAX_RESUMES: u32 = 5;

const PROVIDER_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "GROQ_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "REWRITER_LMSTUDIO_HOST",
    "REWRITER_LMSTUDIO_MODEL",
    "REWRITER_CLAUDE_CLI_MODEL",
    "REWRITER_USE_CLAUDE_CLI",
];

pub fn lock_worker(queue: &Queue) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(queue.lock_path())?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

pub fn is_running(queue: &Queue) -> io::Result<bool> {
    Ok(lock_worker(queue)?.is_none())
}

pub fn spawn_detached(queue: &Queue) -> io::Result<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(queue.worker_log_path())?;
    Command::new(std::env::current_exe()?)
        .arg("worker")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    Ok(())
}

pub fn ensure_running(queue: &Queue) -> io::Result<bool> {
    if is_running(queue)? {
        return Ok(false);
    }
    spawn_detached(queue)?;
    Ok(true)
}

pub fn run(queue: &Queue) -> io::Result<()> {
    loop {
        let Some(lock) = lock_worker(queue)? else {
            return Ok(());
        };
        serve(queue, Some(IDLE_EXIT))?;
        drop(lock);
        if queue.next_queued()?.is_none() {
            return Ok(());
        }
    }
}

pub fn serve(queue: &Queue, idle_exit: Option<Duration>) -> io::Result<()> {
    recover_orphans(queue)?;
    let mut idle_since = Instant::now();
    loop {
        match queue.next_queued()? {
            Some(job) => {
                run_job(queue, job)?;
                idle_since = Instant::now();
            }
            None if idle_exit.is_some_and(|limit| idle_since.elapsed() > limit) => return Ok(()),
            None => std::thread::sleep(POLL),
        }
    }
}

/// A job still marked Running when a new worker starts was interrupted by a crash or a
/// deliberate restart — its orchestrator subprocess runs in its own process group, so it
/// isn't killed by the worker exiting; it's either still going as an orphan or already dead.
/// Either way, kill it outright (avoids two orchestrators writing the same workspace at once)
/// and requeue rather than fail: `main.rs` checkpoints each pipeline stage to `artifacts/*`,
/// so relaunching against the same workspace/source skips everything already done and only
/// redoes the stage that was interrupted (V2 synthesis itself has no finer checkpoint inside
/// it, so an interruption mid-synthesis redoes that whole stage, not just the missing part).
fn recover_orphans(queue: &Queue) -> io::Result<()> {
    for mut job in queue
        .list()?
        .into_iter()
        .filter(|j| j.state == State::Running)
    {
        if let Some(pid) = job.pid {
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
        if job.resume_count >= MAX_RESUMES {
            finish(
                queue,
                &mut job,
                State::Failed,
                None,
                Some(format!(
                    "interrupted and already resumed {MAX_RESUMES} times — giving up rather than risk a crash loop"
                )),
            )?;
            continue;
        }
        job.resume_count += 1;
        job.state = State::Queued;
        job.pid = None;
        job.started_at = None;
        queue.save(&job)?;
        append_log(
            queue,
            job.id,
            &format!(
                "\n[worker] interrupted (crash or restart) — requeued for automatic resume {}/{MAX_RESUMES}, \
                 relaunching against the existing workspace so completed stages are skipped\n",
                job.resume_count
            ),
        );
    }
    Ok(())
}

fn append_log(queue: &Queue, id: u32, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(queue.log_path(id)) {
        let _ = f.write_all(text.as_bytes());
    }
}

fn find_orchestrator() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("ai-org-orchestrator")))
        .filter(|p| p.exists());
    beside.or_else(|| {
        std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .map(|d| PathBuf::from(d).join("ai-org-orchestrator"))
            .find(|p| p.exists())
    })
}

fn run_job(queue: &Queue, mut job: Job) -> io::Result<()> {
    if queue.cancel_requested(job.id) {
        return finish(queue, &mut job, State::Cancelled, None, None);
    }

    let Some(binary) = find_orchestrator() else {
        return finish(
            queue,
            &mut job,
            State::Failed,
            None,
            Some("ai-org-orchestrator not found".into()),
        );
    };

    job.state = State::Running;
    job.started_at = Some(Utc::now());
    queue.save(&job)?;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(queue.log_path(job.id))?;
    let mut cmd = Command::new(binary);
    cmd.arg("--workspace")
        .arg(&job.workspace)
        .arg("--source")
        .arg(&job.source)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    // The worker's own inherited PATH depends on how *it* was launched (interactive
    // shell vs. a bare `ssh host cmd`, which skips .bashrc/.profile on most systems) —
    // that's not reliable across restarts. quality_gate() needs cargo/clippy/rustfmt
    // (and now ast-grep) to actually be found, so guarantee cargo's bin dir is on PATH
    // for the orchestrator regardless of how the worker itself ended up running.
    if let Ok(home) = std::env::var("HOME") {
        let cargo_bin = format!("{home}/.cargo/bin");
        let existing = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{cargo_bin}:{existing}"));
    }
    if let Some(n) = job.max_iter {
        cmd.arg("--max-iter").arg(n.to_string());
    }
    if !job.inherit_env {
        PROVIDER_ENV.iter().for_each(|v| {
            cmd.env_remove(v);
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return finish(queue, &mut job, State::Failed, None, Some(e.to_string())),
    };
    job.pid = Some(child.id());
    queue.save(&job)?;

    loop {
        if let Some(status) = child.try_wait()? {
            let (state, error) = if status.success() {
                (State::Succeeded, None)
            } else {
                (State::Failed, Some(describe_exit(status)))
            };
            return finish(queue, &mut job, state, status.code(), error);
        }
        if queue.cancel_requested(job.id) {
            terminate(&mut child)?;
            return finish(queue, &mut job, State::Cancelled, None, None);
        }
        std::thread::sleep(POLL);
    }
}

fn terminate(child: &mut Child) -> io::Result<()> {
    let pgid = child.id() as i32;
    unsafe { libc::killpg(pgid, libc::SIGTERM) };
    let deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
    child.wait().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Spec;

    fn temp_queue(tag: &str) -> Queue {
        let dir = std::env::temp_dir().join(format!("rq-worker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
    fn interrupted_job_is_requeued_up_to_the_cap() {
        let q = temp_queue("resume");
        let mut job = q.submit(spec()).unwrap();
        job.state = State::Running;
        job.pid = Some(999_999); // unlikely to be a live pid; killpg on it is a harmless no-op
        q.save(&job).unwrap();

        recover_orphans(&q).unwrap();

        let after = q.get(job.id).unwrap();
        assert_eq!(after.state, State::Queued);
        assert_eq!(after.resume_count, 1);
        assert!(after.pid.is_none());
    }

    #[test]
    fn job_fails_after_exceeding_max_resumes() {
        let q = temp_queue("resume-cap");
        let mut job = q.submit(spec()).unwrap();
        job.state = State::Running;
        job.pid = Some(999_999);
        job.resume_count = MAX_RESUMES;
        q.save(&job).unwrap();

        recover_orphans(&q).unwrap();

        let after = q.get(job.id).unwrap();
        assert_eq!(after.state, State::Failed);
        assert!(after.error.unwrap().contains("giving up"));
    }
}

fn describe_exit(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with code {code}"),
        (None, Some(sig)) => format!("killed by signal {sig}"),
        _ => "exited abnormally".into(),
    }
}

fn finish(
    queue: &Queue,
    job: &mut Job,
    state: State,
    exit_code: Option<i32>,
    error: Option<String>,
) -> io::Result<()> {
    job.state = state;
    job.exit_code = exit_code;
    job.error = error;
    job.finished_at = Some(Utc::now());
    queue.save(job)
}
