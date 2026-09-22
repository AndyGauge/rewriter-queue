use crate::job::{Job, Queue, State};
use chrono::Utc;
use std::fmt::Write;

pub fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn elapsed(job: &Job, state: State) -> String {
    match (job.started_at, job.finished_at, state) {
        (Some(start), Some(end), _) => duration((end - start).num_seconds()),
        (Some(start), None, State::Running) => duration((Utc::now() - start).num_seconds()),
        (None, _, State::Queued) => format!(
            "waiting {}",
            duration((Utc::now() - job.submitted_at).num_seconds())
        ),
        _ => "-".into(),
    }
}

fn source_name(job: &Job) -> String {
    if let Some(name) = &job.name {
        return name.clone();
    }
    job.source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| job.source.display().to_string())
}

pub fn list(queue: &Queue, jobs: &[Job]) -> String {
    if jobs.is_empty() {
        return "queue is empty".into();
    }
    let mut out = format!(
        "{:<5} {:<10} {:<24} {:<14} {}\n",
        "ID", "STATE", "STAGE", "TIME", "SOURCE"
    );
    for job in jobs {
        let state = queue.effective_state(job);
        let stage = match state {
            State::Running => queue.stage(job.id).unwrap_or_else(|| "starting".into()),
            _ => "-".into(),
        };
        let _ = writeln!(
            out,
            "{:<5} {:<10} {:<24} {:<14} {}",
            format!("#{}", job.id),
            state.label(),
            truncate(&stage, 24),
            elapsed(job, state),
            source_name(job),
        );
    }
    out.trim_end().to_string()
}

pub fn detail(queue: &Queue, job: &Job) -> String {
    let state = queue.effective_state(job);
    let mut out = format!("job #{} — {}\n", job.id, state.label());
    let _ = writeln!(out, "source:    {}", job.source.display());
    let _ = writeln!(out, "workspace: {}", job.workspace.display());
    let _ = writeln!(
        out,
        "providers: {}",
        if job.inherit_env {
            "config + shell environment"
        } else {
            "config file only"
        }
    );
    if let Some(n) = job.max_iter {
        let _ = writeln!(out, "max iter:  {n}");
    }
    let _ = writeln!(out, "time:      {}", elapsed(job, state));
    if state == State::Running {
        let _ = writeln!(
            out,
            "stage:     {}",
            queue.stage(job.id).unwrap_or_else(|| "starting".into())
        );
    }
    if job.resume_count > 0 {
        let _ = writeln!(out, "resumed:   {} time(s) after interruption", job.resume_count);
    }
    if let Some(err) = &job.error {
        let _ = writeln!(out, "error:     {err}");
    }
    let _ = write!(out, "log:       {}", queue.log_path(job.id).display());
    out
}

pub fn watch_frame(queue: &Queue, jobs: &[Job]) -> String {
    let mut out = format!(
        "rewriter queue — {}\n\n{}\n",
        Utc::now().format("%H:%M:%S"),
        list(queue, jobs)
    );
    if let Some(running) = jobs.iter().find(|j| j.state == State::Running) {
        let _ = write!(
            out,
            "\n── #{} log ──\n{}\n",
            running.id,
            queue.log_tail(running.id, 12)
        );
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
