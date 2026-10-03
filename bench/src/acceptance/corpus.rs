use super::*;
use crate::taskspec;

fn cargo_available() -> bool {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn solution_dirs(task: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(task.join("solutions"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
        .collect();
    dirs.sort();
    dirs
}

#[test]
fn every_task_accepts_its_good_solution_and_rejects_its_bad_ones() {
    if !cargo_available() {
        eprintln!("skipping: cargo is not available");
        return;
    }
    let tasks_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tasks");
    let tasks = taskspec::load_all(&tasks_dir).unwrap();
    assert!(
        tasks.len() >= 5,
        "expected the seed corpus, found {}",
        tasks.len()
    );

    let mut jobs: Vec<(String, &TaskSpec, &Path, PathBuf, bool)> = Vec::new();
    for (spec, dir) in &tasks {
        for good in solution_dirs(dir, "good") {
            jobs.push((spec.name.clone(), spec, dir, good, true));
        }
        let bad = solution_dirs(dir, "bad");
        assert!(!bad.is_empty(), "task '{}' has no bad solution", spec.name);
        for b in bad {
            jobs.push((spec.name.clone(), spec, dir, b, false));
        }
    }

    let problems: Vec<String> = thread::scope(|s| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(name, spec, dir, sol, good)| {
                s.spawn(move || {
                    let outcomes = run_checks(spec, dir, sol);
                    let label = format!("{name}/{}", sol.file_name().unwrap().to_string_lossy());
                    let failed: Vec<&CheckOutcome> =
                        outcomes.iter().filter(|o| !o.passed).collect();
                    eprintln!(
                        "{label}: {} of {} checks failed{}",
                        failed.len(),
                        outcomes.len(),
                        failed
                            .first()
                            .map(|f| format!(" (first: {}: {})", f.name, f.detail))
                            .unwrap_or_default()
                    );
                    if *good {
                        failed.first().map(|f| {
                            format!("{label}: good solution failed '{}': {}", f.name, f.detail)
                        })
                    } else if failed.is_empty() {
                        Some(format!("{label}: bad solution passed every check"))
                    } else if failed
                        .iter()
                        .any(|f| f.name == "build" || f.name == "setup")
                    {
                        Some(format!(
                            "{label}: bad solution does not build: {}",
                            failed[0].detail
                        ))
                    } else {
                        None
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect()
    });
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
