use crate::job::{Job, Queue, Spec, State};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Increment {
    slug: String,
    mission: String,
}

/// `artifacts/increments.json`, written by an orchestrator that found the mission too big.
#[derive(Deserialize)]
struct Handoff {
    increments: Vec<Increment>,
}

/// `chain.json` in the workspace of each increment's job: where it sits in the series and
/// what is still to build.
#[derive(Serialize, Deserialize, Debug)]
struct Chain {
    index: usize,
    total: usize,
    original_source: PathBuf,
    remaining: Vec<Increment>,
}

const SUBMITTED: &str = "follow_up.submitted";

/// After a job ends: if it split its mission into increments, submit the first as a job; if it
/// was itself an increment that succeeded, submit the next, with the crate it just built added
/// to that job's source. Idempotent, so a worker restart cannot submit an increment twice.
pub(super) fn follow_up(queue: &Queue, id: u32) -> io::Result<()> {
    let mut job = queue.get(id)?;
    if job.state != State::Succeeded || job.workspace.join(SUBMITTED).exists() {
        return Ok(());
    }
    let chain_path = job.workspace.join("chain.json");
    let handoff_path = job.workspace.join("artifacts/increments.json");

    let next = if chain_path.exists() {
        let chain: Chain = serde_json::from_slice(&fs::read(&chain_path)?)?;
        next_increment(queue, &job, chain)?
    } else if handoff_path.exists() {
        let handoff: Handoff = serde_json::from_slice(&fs::read(&handoff_path)?)?;
        first_increment(queue, &job, handoff.increments)?
    } else {
        return Ok(());
    };

    if let Some((next_job, note)) = next {
        fs::write(job.workspace.join(SUBMITTED), next_job.id.to_string())?;
        job.note = Some(note);
        queue.save(&job)?;
    }
    Ok(())
}

fn first_increment(
    queue: &Queue,
    parent: &Job,
    mut increments: Vec<Increment>,
) -> io::Result<Option<(Job, String)>> {
    if increments.is_empty() {
        return Ok(None);
    }
    let total = increments.len();
    let first = increments.remove(0);
    let chain = Chain { index: 1, total, original_source: parent.source.clone(), remaining: increments };
    let job = submit_increment(queue, parent, &first, &chain, parent.source.clone())?;
    let note = format!("mission too big for one run: split into {total} increments, increment 1 is job #{}", job.id);
    Ok(Some((job, note)))
}

fn next_increment(queue: &Queue, finished: &Job, mut chain: Chain) -> io::Result<Option<(Job, String)>> {
    if chain.remaining.is_empty() {
        return Ok(None);
    }
    let built_crate = finished.workspace.join("artifacts/v2");
    let next = chain.remaining.remove(0);
    let source = queue.new_work_dir()?.join("source");
    copy_tree(&chain.original_source, &source)?;
    copy_tree(&built_crate, &source.join("v2-so-far"))?;

    let done = chain.index;
    let upcoming = Chain {
        index: done + 1,
        total: chain.total,
        original_source: chain.original_source.clone(),
        remaining: chain.remaining,
    };
    let job = submit_increment(queue, finished, &next, &upcoming, source)?;
    let note = format!("increment {done} of {} built; increment {} is job #{}", chain.total, done + 1, job.id);
    Ok(Some((job, note)))
}

fn submit_increment(
    queue: &Queue,
    from: &Job,
    increment: &Increment,
    chain: &Chain,
    source: PathBuf,
) -> io::Result<Job> {
    let workspace = queue.new_work_dir()?.join("ws");
    fs::create_dir_all(workspace.join("artifacts"))?;
    fs::write(workspace.join("artifacts/mission.md"), mission_text(increment, chain))?;
    fs::write(workspace.join("chain.json"), serde_json::to_vec_pretty(chain)?)?;
    let base = from.name.as_deref().unwrap_or("job").split(" · increment").next().unwrap_or("job").to_string();
    queue.submit(Spec {
        name: Some(format!("{base} · increment {}/{}: {}", chain.index, chain.total, increment.slug)),
        workspace,
        source,
        max_iter: from.max_iter,
        inherit_env: from.inherit_env,
    })
}

fn mission_text(increment: &Increment, chain: &Chain) -> String {
    let mut text = format!(
        "# Mission: increment {} of {} — {}\n\nThis is one increment of a larger rewrite that was split \
         because the whole was too big to build in one run. Build only what this increment describes; \
         later increments cover the rest.\n",
        chain.index, chain.total, increment.slug
    );
    if chain.index > 1 {
        text.push_str(&format!(
            "\nThe Rust crate in `v2-so-far/` (part of the source) already implements increments 1 to {}. \
             Extend it; do not rewrite it. Reuse its types, tables and functions as they are, add only \
             what this increment needs in new modules, and keep everything it provides working and its \
             tests passing.\n",
            chain.index - 1
        ));
    }
    text.push('\n');
    text.push_str(&increment.mission);
    text.push('\n');
    text
}

/// Copy a directory tree, leaving out build output and version control.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    for entry in walkdir::WalkDir::new(from).into_iter().filter_entry(|e| {
        !matches!(e.file_name().to_str(), Some("target") | Some(".git"))
    }) {
        let entry = entry.map_err(io::Error::other)?;
        let rel = entry.path().strip_prefix(from).map_err(io::Error::other)?;
        let dest = to.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&dest)?;
        } else if entry.file_type().is_file() {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
