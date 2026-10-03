use crate::metrics;
use crate::types::{CheckOutcome, RunRecord, TaskSpec};
use run_events::Event;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub type Checker<'a> = dyn Fn(&TaskSpec, &Path, &Path) -> Vec<CheckOutcome> + 'a;

#[derive(Debug, Clone)]
pub enum Mode {
    Queue { queue_bin: PathBuf },
    Direct { orchestrator_bin: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Options {
    pub results_dir: PathBuf,
    pub label: String,
    pub mode: Mode,
    pub repeat: usize,
    pub keep: bool,
    pub poll_interval: Duration,
    pub command_timeout: Duration,
}

pub struct Exec {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub timed_out: bool,
}

fn spawn_with_retry(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(26) && tries < 40 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn drain<R: Read + Send + 'static>(mut reader: R) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Runs in its own process group so a timeout also kills what the command spawned.
pub fn exec(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Exec, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child =
        spawn_with_retry(&mut cmd).map_err(|e| format!("run {}: {e}", program.display()))?;
    let out_rx = drain(child.stdout.take().expect("stdout is piped"));
    let err_rx = drain(child.stderr.take().expect("stderr is piped"));
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() >= timeout => {
                timed_out = true;
                kill_group(child.id());
                let _ = child.kill();
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("wait for {}: {e}", program.display())),
        }
    };
    let grace = Duration::from_secs(2);
    let collect = |rx: mpsc::Receiver<Vec<u8>>| {
        String::from_utf8_lossy(&rx.recv_timeout(grace).unwrap_or_default()).into_owned()
    };
    Ok(Exec {
        stdout: collect(out_rx),
        stderr: collect(err_rx),
        code: status.and_then(|s| s.code()),
        timed_out,
    })
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

pub fn parse_job_id(text: &str) -> Option<u64> {
    let rest = &text[text.find('#')? + 1..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The first line of `rewriter-queue status` is `job #<id> — <state>`.
pub fn parse_state(text: &str) -> Option<&'static str> {
    let first = text.lines().next()?;
    ["queued", "running", "succeeded", "failed", "cancelled"]
        .into_iter()
        .find(|state| {
            first
                .split(|c: char| !c.is_alphabetic())
                .any(|w| w == *state)
        })
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "succeeded" | "failed" | "cancelled")
}

pub fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            if entry.file_name() != "target" {
                copy_dir(&entry.path(), &target)?;
            }
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

struct Pipeline {
    job_id: Option<u64>,
    outcome: String,
    timed_out: bool,
    events: Vec<Event>,
    crate_dir: Option<PathBuf>,
}

fn read_workspace_events(workspace: &Path) -> Vec<Event> {
    std::fs::read_to_string(workspace.join("events.jsonl"))
        .map(|t| run_events::parse_events(&t))
        .unwrap_or_default()
}

fn crate_in(root: &Path) -> Option<PathBuf> {
    ["artifacts/v2", "ws/artifacts/v2"]
        .iter()
        .map(|rel| root.join(rel))
        .find(|dir| dir.join("Cargo.toml").is_file())
}

fn run_direct(spec: &TaskSpec, orchestrator: &Path, work: &Path) -> Pipeline {
    let workspace = work.join("workspace");
    let source = work.join("source");
    let argv = vec![
        "--workspace".into(),
        workspace.to_string_lossy().into_owned(),
        "--source".into(),
        source.to_string_lossy().into_owned(),
        "--max-iter".into(),
        spec.max_iter.to_string(),
    ];
    let result = exec(
        orchestrator,
        &argv,
        Some(work),
        Duration::from_secs(spec.time_budget_secs),
    );
    let (outcome, timed_out) = match &result {
        Ok(r) if r.timed_out => ("timeout", true),
        Ok(r) if r.code == Some(0) => ("succeeded", false),
        Ok(_) => ("failed", false),
        Err(e) => {
            eprintln!("  {e}");
            ("failed", false)
        }
    };
    if let Ok(r) = &result {
        let _ = std::fs::write(
            work.join("orchestrator.log"),
            format!("{}\n{}", r.stdout, r.stderr),
        );
    }
    Pipeline {
        job_id: None,
        outcome: outcome.into(),
        timed_out,
        events: read_workspace_events(&workspace),
        crate_dir: crate_in(&workspace),
    }
}

struct Queue<'a> {
    bin: &'a Path,
    timeout: Duration,
}

impl Queue<'_> {
    fn call(&self, argv: &[String]) -> Result<Exec, String> {
        exec(self.bin, argv, None, self.timeout)
    }

    fn ok_stdout(&self, argv: &[String]) -> Result<String, String> {
        let r = self.call(argv)?;
        if r.timed_out {
            return Err(format!("`{}` timed out", argv.join(" ")));
        }
        if r.code != Some(0) {
            return Err(format!("`{}` failed: {}", argv.join(" "), r.stderr.trim()));
        }
        Ok(r.stdout)
    }
}

fn run_queue(spec: &TaskSpec, queue_bin: &Path, work: &Path, opts: &Options) -> Pipeline {
    let workspace = work.join("workspace");
    let source = work.join("source");
    let queue = Queue {
        bin: queue_bin,
        timeout: opts.command_timeout,
    };
    let failed = |job_id| Pipeline {
        job_id,
        outcome: "failed".into(),
        timed_out: false,
        events: Vec::new(),
        crate_dir: None,
    };

    let submitted = queue.ok_stdout(&[
        "submit".into(),
        "--source".into(),
        source.to_string_lossy().into_owned(),
        "--workspace".into(),
        workspace.to_string_lossy().into_owned(),
        "--max-iter".into(),
        spec.max_iter.to_string(),
    ]);
    let id = match submitted {
        Ok(text) => match parse_job_id(&text) {
            Some(id) => id,
            None => {
                eprintln!(
                    "  could not find a job id in submit output: {}",
                    text.trim()
                );
                return failed(None);
            }
        },
        Err(e) => {
            eprintln!("  {e}");
            return failed(None);
        }
    };
    let id_arg = id.to_string();

    let budget = Duration::from_secs(spec.time_budget_secs);
    let waiting_since = Instant::now();
    let mut running_since: Option<Instant> = None;
    let mut status_errors = 0;
    let (outcome, timed_out) = loop {
        match queue.ok_stdout(&args(&["status", &id_arg])) {
            Ok(text) => {
                status_errors = 0;
                match parse_state(&text) {
                    Some(state) if is_terminal(state) => break (state.to_string(), false),
                    Some("queued") | None => {}
                    Some(_) => {
                        running_since.get_or_insert_with(Instant::now);
                    }
                }
            }
            Err(e) => {
                status_errors += 1;
                eprintln!("  {e}");
                if status_errors >= 5 {
                    break ("failed".into(), false);
                }
            }
        }
        let elapsed = running_since.map_or_else(|| waiting_since.elapsed(), |t| t.elapsed());
        if elapsed >= budget {
            if let Err(e) = queue.ok_stdout(&args(&["cancel", &id_arg])) {
                eprintln!("  {e}");
            }
            break ("timeout".into(), true);
        }
        std::thread::sleep(opts.poll_interval);
    };

    let mut events = queue
        .ok_stdout(&args(&["events", &id_arg, "--json"]))
        .map(|t| run_events::parse_events(&t))
        .unwrap_or_default();
    if events.is_empty() {
        events = read_workspace_events(&workspace);
    }
    let mut crate_dir = crate_in(&workspace);
    if crate_dir.is_none() && !timed_out {
        let fetched = work.join("fetched");
        let out = fetched.to_string_lossy().into_owned();
        if queue
            .ok_stdout(&["fetch".into(), id_arg.clone(), "--out".into(), out])
            .is_ok()
        {
            crate_dir = crate_in(&fetched);
        }
    }
    Pipeline {
        job_id: Some(id),
        outcome,
        timed_out,
        events,
        crate_dir,
    }
}

fn setup(spec: &TaskSpec, task_dir: &Path, work: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_dir_all(work);
    let source = task_dir.join("source");
    if source.is_dir() {
        copy_dir(&source, &work.join("source"))?;
    } else {
        std::fs::create_dir_all(work.join("source"))?;
    }
    let artifacts = work.join("workspace/artifacts");
    std::fs::create_dir_all(&artifacts)?;
    let mission = std::fs::read_to_string(task_dir.join("mission.md"))
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: mission.md: {e}", spec.name)))?;
    std::fs::write(artifacts.join("mission.md"), mission)
}

fn work_dir(opts: &Options, spec: &TaskSpec, repeat: usize) -> PathBuf {
    let label = crate::results::sanitize_label(&opts.label);
    opts.results_dir
        .join("work")
        .join(label)
        .join(format!("{}-{repeat}", spec.name))
}

pub fn run_one(
    spec: &TaskSpec,
    task_dir: &Path,
    repeat: usize,
    opts: &Options,
    check: &Checker,
) -> RunRecord {
    let work = work_dir(opts, spec, repeat);
    let started = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let clock = Instant::now();
    let mut record = RunRecord {
        task: spec.name.clone(),
        repeat,
        job_id: None,
        passed: false,
        failed_checks: Vec::new(),
        pipeline_outcome: "failed".into(),
        wall_secs: 0.0,
        input_tokens: 0,
        output_tokens: 0,
        model_calls: 0,
        retries: 0,
        gate_failures: 0,
        integration_attempts: 0,
        escalations: 0,
        plan_revised: false,
        debugger_consulted: false,
        started,
    };
    if let Err(e) = setup(spec, task_dir, &work) {
        eprintln!("  setup failed: {e}");
        record.failed_checks.push("setup".into());
        return record;
    }
    let pipeline = match &opts.mode {
        Mode::Queue { queue_bin } => run_queue(spec, queue_bin, &work, opts),
        Mode::Direct { orchestrator_bin } => run_direct(spec, orchestrator_bin, &work),
    };
    record.wall_secs = clock.elapsed().as_secs_f64();
    record.job_id = pipeline.job_id;
    record.pipeline_outcome = pipeline.outcome;

    let m = metrics::from_events(&pipeline.events);
    record.input_tokens = m.input_tokens;
    record.output_tokens = m.output_tokens;
    record.model_calls = m.model_calls;
    record.retries = m.retries;
    record.gate_failures = m.gate_failures;
    record.integration_attempts = m.integration_attempts;
    record.escalations = m.escalations;
    record.plan_revised = m.plan_revised;
    record.debugger_consulted = m.debugger_consulted;

    match &pipeline.crate_dir {
        Some(crate_dir) if !pipeline.timed_out => {
            let outcomes = check(spec, task_dir, crate_dir);
            for o in outcomes.iter().filter(|o| !o.passed) {
                eprintln!("  check '{}' failed: {}", o.name, o.detail);
                record.failed_checks.push(o.name.clone());
            }
            record.passed = !outcomes.is_empty() && record.failed_checks.is_empty();
        }
        Some(_) => record.failed_checks.push("timeout".into()),
        None => record.failed_checks.push("no-crate".into()),
    }
    if !opts.keep {
        let _ = std::fs::remove_dir_all(&work);
    }
    record
}

pub fn run_all(tasks: &[(TaskSpec, PathBuf)], opts: &Options, check: &Checker) -> Vec<RunRecord> {
    let mut records = Vec::new();
    for (spec, dir) in tasks {
        for repeat in 0..opts.repeat {
            eprintln!("==> {} (repeat {}/{})", spec.name, repeat + 1, opts.repeat);
            let record = run_one(spec, dir, repeat, opts, check);
            eprintln!(
                "    {} in {:.1}s ({})",
                if record.passed { "passed" } else { "FAILED" },
                record.wall_secs,
                record.pipeline_outcome
            );
            records.push(record);
        }
    }
    if !opts.keep {
        let work_root = opts
            .results_dir
            .join("work")
            .join(crate::results::sanitize_label(&opts.label));
        let _ = std::fs::remove_dir(&work_root);
        let _ = std::fs::remove_dir(opts.results_dir.join("work"));
    }
    records
}

pub fn summary_table(records: &[RunRecord]) -> String {
    let mut out = format!(
        "{:<24} {:>3} {:<6} {:<10} {:>8} {:>10} {:>6}  {}\n",
        "TASK", "RUN", "RESULT", "OUTCOME", "SECS", "TOKENS", "CALLS", "FAILED CHECKS"
    );
    for r in records {
        out.push_str(&format!(
            "{:<24} {:>3} {:<6} {:<10} {:>8.1} {:>10} {:>6}  {}\n",
            r.task,
            r.repeat,
            if r.passed { "pass" } else { "FAIL" },
            r.pipeline_outcome,
            r.wall_secs,
            r.input_tokens + r.output_tokens,
            r.model_calls,
            r.failed_checks.join(",")
        ));
    }
    let passed = records.iter().filter(|r| r.passed).count();
    out.push_str(&format!("{passed}/{} runs passed\n", records.len()));
    out
}

fn solution_problem(
    spec: &TaskSpec,
    task_dir: &Path,
    kind: &str,
    scratch: &Path,
    check: &Checker,
) -> Option<String> {
    let solution = task_dir.join("solutions").join(kind);
    if !solution.is_dir() {
        return Some(format!("missing solutions/{kind}"));
    }
    let copy = scratch.join(format!("{}-{kind}", spec.name));
    let _ = std::fs::remove_dir_all(&copy);
    if let Err(e) = copy_dir(&solution, &copy) {
        return Some(format!("copy solutions/{kind}: {e}"));
    }
    let outcomes = check(spec, task_dir, &copy);
    let _ = std::fs::remove_dir_all(&copy);
    let failed: Vec<String> = outcomes
        .iter()
        .filter(|o| !o.passed)
        .map(|o| format!("{} ({})", o.name, o.detail))
        .collect();
    match kind {
        "good" if outcomes.is_empty() => Some("good solution ran no checks".into()),
        "good" if !failed.is_empty() => Some(format!("good solution fails: {}", failed.join("; "))),
        "bad" if outcomes.is_empty() => Some("bad solution ran no checks".into()),
        "bad" if failed.is_empty() => Some("bad solution passes every check".into()),
        _ => None,
    }
}

/// Each result is a task name with the problems found; no problems means the checks pass the
/// good solution and fail the bad one.
pub fn validate_tasks(
    tasks: &[(TaskSpec, PathBuf)],
    check: &Checker,
) -> Vec<(String, Vec<String>)> {
    let scratch = std::env::temp_dir().join(format!("bench-validate-{}", std::process::id()));
    let results = tasks
        .iter()
        .map(|(spec, dir)| {
            let problems = ["good", "bad"]
                .iter()
                .filter_map(|kind| solution_problem(spec, dir, kind, &scratch, check))
                .collect();
            (spec.name.clone(), problems)
        })
        .collect();
    let _ = std::fs::remove_dir_all(&scratch);
    results
}

#[cfg(test)]
mod tests;
