mod acceptance;
mod compare;
mod env;
mod metrics;
mod proc;
mod report;
mod results;
mod runner;
mod taskspec;
mod types;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use types::{RunFile, Thresholds};

const USAGE: &str = "Usage: rewriter-bench <command>

Commands:
  run [--tasks a,b] [--repeat N] [--label L] [--direct] [--queue-bin P]
      [--orchestrator-bin P] [--tasks-dir D] [--results-dir D] [--keep]
                    Run tasks through the queue (or the orchestrator with --direct),
                    check each result against the task's hidden acceptance checks and
                    save a results file. Defaults: all tasks, 3 repeats.
  compare <baseline> <current> [--pass-rate-drop X] [--time-ratio X]
      [--token-ratio X] [--min-runs N]
                    Exit 1 when the current run regressed against the baseline.
  promote <run-file>
                    Copy a results file to bench/baseline.json.
  validate-tasks [--tasks-dir D]
                    Check that every task's acceptance checks pass solutions/good
                    and fail solutions/bad.
  list [--tasks-dir D] [--results-dir D]
                    Show the tasks and saved results files.";

struct Args {
    values: HashMap<String, String>,
    switches: Vec<String>,
    positional: Vec<String>,
}

impl Args {
    fn parse(raw: &[String], value_flags: &[&str], switch_flags: &[&str]) -> Result<Args, String> {
        let mut args = Args {
            values: HashMap::new(),
            switches: Vec::new(),
            positional: Vec::new(),
        };
        let mut iter = raw.iter();
        while let Some(arg) = iter.next() {
            if value_flags.contains(&arg.as_str()) {
                let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                args.values.insert(arg.clone(), value.clone());
            } else if switch_flags.contains(&arg.as_str()) {
                args.switches.push(arg.clone());
            } else if arg.starts_with("--") {
                return Err(format!("unknown option {arg}"));
            } else {
                args.positional.push(arg.clone());
            }
        }
        Ok(args)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn switch(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        self.value(name)
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("{name} needs a number, got '{v}'"))
            })
            .transpose()
    }

    fn path(&self, name: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
        self.value(name).map(PathBuf::from).unwrap_or_else(default)
    }
}

fn default_tasks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks")
}

fn select_tasks(
    all: Vec<(types::TaskSpec, PathBuf)>,
    wanted: Option<&str>,
) -> Result<Vec<(types::TaskSpec, PathBuf)>, String> {
    let Some(wanted) = wanted else {
        return Ok(all);
    };
    let names: Vec<&str> = wanted
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    if let Some(unknown) = names
        .iter()
        .find(|n| !all.iter().any(|(s, _)| s.name == **n))
    {
        return Err(format!("no task named '{unknown}'"));
    }
    Ok(all
        .into_iter()
        .filter(|(s, _)| names.contains(&s.name.as_str()))
        .collect())
}

fn cmd_run(raw: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(
        raw,
        &[
            "--tasks",
            "--repeat",
            "--label",
            "--queue-bin",
            "--orchestrator-bin",
            "--tasks-dir",
            "--results-dir",
        ],
        &["--direct", "--keep"],
    )?;
    let repeat = a.number::<usize>("--repeat")?.unwrap_or(3);
    if repeat == 0 {
        return Err("--repeat must be at least 1".into());
    }
    let tasks_dir = a.path("--tasks-dir", default_tasks_dir);
    let tasks = select_tasks(taskspec::load_all(&tasks_dir)?, a.value("--tasks"))?;
    if tasks.is_empty() {
        return Err(format!("no tasks found in {}", tasks_dir.display()));
    }
    let queue_bin = a.value("--queue-bin").map(PathBuf::from);
    let orchestrator_bin = a.value("--orchestrator-bin").map(PathBuf::from);
    let mode = if a.switch("--direct") {
        let bin = orchestrator_bin
            .clone()
            .or_else(|| env::find_in_path("ai-org-orchestrator"))
            .ok_or("ai-org-orchestrator not found: pass --orchestrator-bin")?;
        runner::Mode::Direct {
            orchestrator_bin: bin,
        }
    } else {
        runner::Mode::Queue {
            queue_bin: queue_bin
                .clone()
                .or_else(|| env::find_in_path("rewriter-queue"))
                .unwrap_or_else(|| "rewriter-queue".into()),
        }
    };
    let results_dir = std::path::absolute(a.path("--results-dir", results::default_results_dir))
        .map_err(|e| format!("results dir: {e}"))?;
    let label = a.value("--label").unwrap_or("run").to_string();
    let opts = runner::Options {
        results_dir: results_dir.clone(),
        label: label.clone(),
        mode,
        repeat,
        keep: a.switch("--keep"),
        poll_interval: Duration::from_secs(5),
        command_timeout: Duration::from_secs(120),
    };
    let records = runner::run_all(&tasks, &opts, &acceptance::run_checks);
    println!("{}", runner::summary_table(&records));
    let file = RunFile {
        label,
        env: env::stamp(queue_bin.as_deref(), orchestrator_bin.as_deref()),
        records,
    };
    let path = results::save_run_file(&results_dir, &file)?;
    println!("results: {}", path.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_compare(raw: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(
        raw,
        &[
            "--pass-rate-drop",
            "--time-ratio",
            "--token-ratio",
            "--min-runs",
        ],
        &[],
    )?;
    let [baseline, current] = a.positional.as_slice() else {
        return Err("compare needs <baseline> <current>".into());
    };
    let defaults = Thresholds::default();
    let thresholds = Thresholds {
        pass_rate_drop: a
            .number("--pass-rate-drop")?
            .unwrap_or(defaults.pass_rate_drop),
        time_ratio: a.number("--time-ratio")?.unwrap_or(defaults.time_ratio),
        token_ratio: a.number("--token-ratio")?.unwrap_or(defaults.token_ratio),
        min_runs: a.number("--min-runs")?.unwrap_or(defaults.min_runs),
    };
    let baseline = results::load_run_file(Path::new(baseline))?;
    let current = results::load_run_file(Path::new(current))?;
    let regressions = compare::compare(&baseline, &current, &thresholds);
    print!("{}", report::render_with(&baseline, &current, &regressions, &thresholds));
    Ok(if regressions.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn cmd_promote(raw: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(raw, &[], &[])?;
    let [path] = a.positional.as_slice() else {
        return Err("promote needs <run-file>".into());
    };
    let baseline = results::promote(Path::new(path))?;
    println!("baseline: {}", baseline.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_validate(raw: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(raw, &["--tasks-dir"], &[])?;
    let tasks = taskspec::load_all(&a.path("--tasks-dir", default_tasks_dir))?;
    let mut failures = 0;
    for (name, problems) in runner::validate_tasks(&tasks, &acceptance::run_checks) {
        if problems.is_empty() {
            println!("ok    {name}");
        } else {
            failures += 1;
            println!("FAIL  {name}: {}", problems.join("; "));
        }
    }
    println!("{} task(s), {failures} failing", tasks.len());
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn cmd_list(raw: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(raw, &["--tasks-dir", "--results-dir"], &[])?;
    let tasks = taskspec::load_all(&a.path("--tasks-dir", default_tasks_dir))?;
    println!("tasks:");
    for (spec, _) in &tasks {
        println!(
            "  {:<24} max_iter {:<3} budget {:>5}s  [{}]  {}",
            spec.name,
            spec.max_iter,
            spec.time_budget_secs,
            spec.tags.join(","),
            spec.description
        );
    }
    let files = results::list_run_files(&a.path("--results-dir", results::default_results_dir))?;
    println!("results:");
    for file in files {
        println!("  {}", file.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn dispatch(args: &[String]) -> Result<ExitCode, String> {
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(rest),
        Some("compare") => cmd_compare(rest),
        Some("promote") => cmd_promote(rest),
        Some("validate-tasks") => cmd_validate(rest),
        Some("list") => cmd_list(rest),
        _ => Err("missing or unknown command".into()),
    }
}

fn is_usage_error(message: &str) -> bool {
    message.starts_with("unknown option")
        || message.contains("needs a value")
        || message.contains("needs a number")
        || message.contains("needs <")
        || message.starts_with("missing or unknown command")
        || message.starts_with("no task named")
        || message.starts_with("--repeat must")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(code) => code,
        Err(message) if is_usage_error(&message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments_split_into_values_switches_and_positionals() {
        let a = Args::parse(
            &strings(&["x", "--repeat", "2", "--keep", "y"]),
            &["--repeat"],
            &["--keep"],
        )
        .unwrap();
        assert_eq!(a.value("--repeat"), Some("2"));
        assert!(a.switch("--keep") && !a.switch("--direct"));
        assert_eq!(a.positional, vec!["x", "y"]);
        assert_eq!(a.number::<usize>("--repeat").unwrap(), Some(2));
    }

    #[test]
    fn unknown_options_missing_values_and_bad_numbers_are_usage_errors() {
        let unknown = Args::parse(&strings(&["--nope"]), &[], &[]).err().unwrap();
        assert!(is_usage_error(&unknown));
        let missing = Args::parse(&strings(&["--repeat"]), &["--repeat"], &[])
            .err()
            .unwrap();
        assert!(is_usage_error(&missing));
        let a = Args::parse(&strings(&["--repeat", "x"]), &["--repeat"], &[]).unwrap();
        assert!(is_usage_error(&a.number::<usize>("--repeat").unwrap_err()));
    }

    #[test]
    fn task_selection_filters_by_name_and_rejects_unknown_names() {
        let spec = |n: &str| {
            (
                toml::from_str::<types::TaskSpec>(&format!("name = \"{n}\"\ndescription = \"d\""))
                    .unwrap(),
                PathBuf::from(n),
            )
        };
        let all = vec![spec("a"), spec("b"), spec("c")];
        let picked = select_tasks(all.clone(), Some("c, a")).unwrap();
        assert_eq!(
            picked
                .iter()
                .map(|(s, _)| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert_eq!(select_tasks(all.clone(), None).unwrap().len(), 3);
        assert!(is_usage_error(&select_tasks(all, Some("zzz")).unwrap_err()));
    }

    #[test]
    fn the_command_dispatch_rejects_a_missing_command() {
        assert!(dispatch(&[]).is_err());
        assert!(dispatch(&strings(&["frobnicate"])).is_err());
    }
}
