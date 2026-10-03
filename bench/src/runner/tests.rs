use super::*;
use run_events::{Event, EventKind};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bench-runner-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn spec(name: &str, budget: u64) -> TaskSpec {
    TaskSpec {
        name: name.into(),
        description: "d".into(),
        max_iter: 4,
        time_budget_secs: budget,
        tags: vec![],
        binary: None,
        cargo_tests: false,
        checks: vec![],
    }
}

fn task_dir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join("tasks").join(name);
    std::fs::create_dir_all(dir.join("source")).unwrap();
    std::fs::write(dir.join("source/seed.txt"), "seed").unwrap();
    std::fs::write(dir.join("mission.md"), "Build the thing").unwrap();
    dir
}

fn canned_events() -> String {
    let events = [
        Event::new("SchemaArchitect", EventKind::AgentCall, "call")
            .with_detail(json!({"input_tokens": 100, "output_tokens": 40, "ok": true})),
        Event::new("PlanReviewer", EventKind::Decision, "Revised the plan"),
        Event::new(
            "TargetImplementer",
            EventKind::Retry,
            "reviewer rejected iteration 1, revising",
        ),
        Event::new("QualityGate", EventKind::GateResult, "gate")
            .with_detail(json!({"passed": false, "error_bytes": 3, "attempt": 1})),
        Event::new("DebugImplementer", EventKind::AgentCall, "call")
            .with_detail(json!({"input_tokens": 10, "output_tokens": 5, "ok": true})),
    ];
    events.iter().map(|e| e.to_json_line() + "\n").collect()
}

struct Fake {
    state: PathBuf,
    bin: PathBuf,
}

const CRATE_SNIPPET: &str = r#"
make_crate() {
  mkdir -p "$1/artifacts/v2/src"
  printf '[package]\nname = "x"\n' > "$1/artifacts/v2/Cargo.toml"
  printf '%s' "MARKER" > "$1/artifacts/v2/src/main.rs"
}
"#;

fn fake_queue(root: &Path, marker: &str, crate_in_workspace: bool, forever: bool) -> Fake {
    let state = root.join("qstate");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("events.jsonl"), canned_events()).unwrap();
    let bin = root.join("fake-rewriter-queue");
    let submit_crate = if crate_in_workspace {
        "make_crate \"$ws\""
    } else {
        ":"
    };
    let body = format!(
        r#"STATE="{state}"
{snippet}
cmd="$1"; shift
case "$cmd" in
submit)
  while [ $# -gt 0 ]; do
    case "$1" in --workspace) ws="$2";; --max-iter) echo "$2" > "$STATE/max_iter";; esac
    shift
  done
  echo "$ws" > "$STATE/ws"
  {submit_crate}
  echo 0 > "$STATE/n"
  echo "queued job #7 (0 ahead) — worker started"
  ;;
status)
  n=$(cat "$STATE/n"); echo $((n+1)) > "$STATE/n"
  if [ "$n" -lt 1 ]; then echo "job #7 — queued"
  elif [ "{forever}" = yes ] || [ "$n" -lt 3 ]; then echo "job #7 — running"
  else echo "job #7 — succeeded"; fi
  echo
  echo "log tail"
  ;;
cancel) touch "$STATE/cancelled"; echo "cancel requested for job #7" ;;
events) cat "$STATE/events.jsonl" ;;
fetch)
  while [ $# -gt 0 ]; do case "$1" in --out) out="$2";; esac; shift; done
  make_crate "$out"
  echo "workspace for job #7 written to $out"
  ;;
*) echo "unknown $cmd" >&2; exit 2 ;;
esac
"#,
        state = state.display(),
        snippet = CRATE_SNIPPET.replace("MARKER", marker),
        forever = if forever { "yes" } else { "no" },
    );
    script(&bin, &body);
    Fake { state, bin }
}

fn fake_orchestrator(root: &Path, behaviour: &str, marker: &str) -> PathBuf {
    let bin = root.join("fake-orchestrator");
    let events = root.join("orch-events.jsonl");
    std::fs::write(&events, canned_events()).unwrap();
    let body = format!(
        r#"{snippet}
while [ $# -gt 0 ]; do
  case "$1" in --workspace) ws="$2";; --source) src="$2";; --max-iter) echo "$2" > "$ws/max_iter";; esac
  shift
done
test -f "$src/seed.txt" || exit 3
test -f "$ws/artifacts/mission.md" || exit 4
case "{behaviour}" in
  sleep) sleep 30; exit 0 ;;
  fail) exit 1 ;;
esac
make_crate "$ws"
cp "{events}" "$ws/events.jsonl"
"#,
        snippet = CRATE_SNIPPET.replace("MARKER", marker),
        events = events.display(),
    );
    script(&bin, &body);
    bin
}

fn opts(root: &Path, mode: Mode, keep: bool) -> Options {
    Options {
        results_dir: root.join("results"),
        label: "t".into(),
        mode,
        repeat: 1,
        keep,
        poll_interval: Duration::from_millis(30),
        command_timeout: Duration::from_secs(20),
    }
}

fn marker_checker(spec: &TaskSpec, _dir: &Path, crate_dir: &Path) -> Vec<CheckOutcome> {
    let main = std::fs::read_to_string(crate_dir.join("src/main.rs")).unwrap_or_default();
    vec![
        CheckOutcome {
            name: "build".into(),
            passed: true,
            detail: String::new(),
        },
        CheckOutcome {
            name: format!("{}-output", spec.name),
            passed: main == "GOOD",
            detail: format!("expected GOOD, got {main}"),
        },
    ]
}

fn assert_metrics_from_canned_events(r: &RunRecord) {
    assert_eq!(
        (r.input_tokens, r.output_tokens, r.model_calls),
        (110, 45, 2)
    );
    assert_eq!(
        (r.retries, r.gate_failures, r.integration_attempts),
        (1, 1, 1)
    );
    assert!(r.plan_revised && r.debugger_consulted);
    assert_eq!(r.escalations, 0);
}

#[test]
fn queue_mode_runs_a_task_end_to_end_and_keeps_the_workspace_when_asked() {
    let root = temp("queue-ok");
    let td = task_dir(&root, "rect");
    let fake = fake_queue(&root, "GOOD", true, false);
    let o = opts(
        &root,
        Mode::Queue {
            queue_bin: fake.bin.clone(),
        },
        true,
    );
    let r = run_one(&spec("rect", 30), &td, 2, &o, &marker_checker);
    assert!(r.passed, "{r:?}");
    assert_eq!((r.task.as_str(), r.repeat, r.job_id), ("rect", 2, Some(7)));
    assert_eq!(r.pipeline_outcome, "succeeded");
    assert!(r.failed_checks.is_empty());
    assert_metrics_from_canned_events(&r);
    assert_eq!(
        std::fs::read_to_string(fake.state.join("max_iter"))
            .unwrap()
            .trim(),
        "4"
    );
    let work = root.join("results/work/t/rect-2");
    assert_eq!(
        std::fs::read_to_string(work.join("workspace/artifacts/mission.md")).unwrap(),
        "Build the thing"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("source/seed.txt")).unwrap(),
        "seed"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn queue_mode_removes_the_work_directory_unless_kept() {
    let root = temp("queue-clean");
    let td = task_dir(&root, "rect");
    let fake = fake_queue(&root, "GOOD", true, false);
    let o = opts(
        &root,
        Mode::Queue {
            queue_bin: fake.bin,
        },
        false,
    );
    let records = run_all(&[(spec("rect", 30), td)], &o, &marker_checker);
    assert_eq!(records.len(), 1);
    assert!(!root.join("results/work").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn queue_mode_fails_the_run_when_acceptance_fails_even_though_the_pipeline_succeeded() {
    let root = temp("queue-bad");
    let td = task_dir(&root, "rect");
    let fake = fake_queue(&root, "BAD", true, false);
    let o = opts(
        &root,
        Mode::Queue {
            queue_bin: fake.bin,
        },
        false,
    );
    let r = run_one(&spec("rect", 30), &td, 0, &o, &marker_checker);
    assert_eq!(r.pipeline_outcome, "succeeded");
    assert!(!r.passed);
    assert_eq!(r.failed_checks, vec!["rect-output"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn queue_mode_times_out_cancels_the_job_and_fails_the_run() {
    let root = temp("queue-timeout");
    let td = task_dir(&root, "slow");
    let fake = fake_queue(&root, "GOOD", true, true);
    let o = opts(
        &root,
        Mode::Queue {
            queue_bin: fake.bin,
        },
        false,
    );
    let start = Instant::now();
    let r = run_one(&spec("slow", 1), &td, 0, &o, &marker_checker);
    assert!(start.elapsed() < Duration::from_secs(15));
    assert_eq!(r.pipeline_outcome, "timeout");
    assert!(!r.passed);
    assert!(fake.state.join("cancelled").exists());
    assert_eq!(r.job_id, Some(7));
    assert_metrics_from_canned_events(&r);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn queue_mode_fetches_the_workspace_when_the_crate_is_not_local() {
    let root = temp("queue-fetch");
    let td = task_dir(&root, "rect");
    let fake = fake_queue(&root, "GOOD", false, false);
    let o = opts(
        &root,
        Mode::Queue {
            queue_bin: fake.bin,
        },
        false,
    );
    let r = run_one(&spec("rect", 30), &td, 0, &o, &marker_checker);
    assert!(r.passed, "{r:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_queue_binary_is_a_failed_run_not_a_panic() {
    let root = temp("queue-missing");
    let td = task_dir(&root, "rect");
    let o = opts(
        &root,
        Mode::Queue {
            queue_bin: root.join("no-such-queue"),
        },
        false,
    );
    let r = run_one(&spec("rect", 30), &td, 0, &o, &marker_checker);
    assert!(!r.passed);
    assert_eq!(r.pipeline_outcome, "failed");
    assert_eq!(r.failed_checks, vec!["no-crate"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn direct_mode_runs_the_orchestrator_and_reads_the_workspace_events() {
    let root = temp("direct-ok");
    let td = task_dir(&root, "rect");
    let bin = fake_orchestrator(&root, "ok", "GOOD");
    let o = opts(
        &root,
        Mode::Direct {
            orchestrator_bin: bin,
        },
        true,
    );
    let r = run_one(&spec("rect", 30), &td, 0, &o, &marker_checker);
    assert!(r.passed, "{r:?}");
    assert_eq!((r.pipeline_outcome.as_str(), r.job_id), ("succeeded", None));
    assert_metrics_from_canned_events(&r);
    let max_iter =
        std::fs::read_to_string(root.join("results/work/t/rect-0/workspace/max_iter")).unwrap();
    assert_eq!(max_iter.trim(), "4");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn direct_mode_with_failing_acceptance_is_not_a_pass() {
    let root = temp("direct-bad");
    let td = task_dir(&root, "rect");
    let bin = fake_orchestrator(&root, "ok", "BAD");
    let o = opts(
        &root,
        Mode::Direct {
            orchestrator_bin: bin,
        },
        false,
    );
    let r = run_one(&spec("rect", 30), &td, 0, &o, &marker_checker);
    assert!(!r.passed);
    assert_eq!(r.failed_checks, vec!["rect-output"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn direct_mode_kills_an_orchestrator_that_overruns_its_budget() {
    let root = temp("direct-timeout");
    let td = task_dir(&root, "slow");
    let bin = fake_orchestrator(&root, "sleep", "GOOD");
    let o = opts(
        &root,
        Mode::Direct {
            orchestrator_bin: bin,
        },
        false,
    );
    let start = Instant::now();
    let r = run_one(&spec("slow", 1), &td, 0, &o, &marker_checker);
    assert!(start.elapsed() < Duration::from_secs(15));
    assert_eq!(r.pipeline_outcome, "timeout");
    assert!(!r.passed);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn direct_mode_reports_a_crashed_orchestrator_as_failed_with_no_crate() {
    let root = temp("direct-fail");
    let td = task_dir(&root, "rect");
    let bin = fake_orchestrator(&root, "fail", "GOOD");
    let o = opts(
        &root,
        Mode::Direct {
            orchestrator_bin: bin,
        },
        false,
    );
    let r = run_one(&spec("rect", 30), &td, 0, &o, &marker_checker);
    assert_eq!(r.pipeline_outcome, "failed");
    assert!(!r.passed);
    assert_eq!(r.failed_checks, vec!["no-crate"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_task_without_a_mission_fails_in_setup() {
    let root = temp("no-mission");
    let td = task_dir(&root, "rect");
    std::fs::remove_file(td.join("mission.md")).unwrap();
    let o = opts(
        &root,
        Mode::Direct {
            orchestrator_bin: root.join("x"),
        },
        false,
    );
    let r = run_one(&spec("rect", 30), &td, 0, &o, &marker_checker);
    assert!(!r.passed);
    assert_eq!(r.failed_checks, vec!["setup"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn run_all_runs_every_task_for_every_repeat() {
    let root = temp("run-all");
    let a = task_dir(&root, "a");
    let b = task_dir(&root, "b");
    let bin = fake_orchestrator(&root, "ok", "GOOD");
    let mut o = opts(
        &root,
        Mode::Direct {
            orchestrator_bin: bin,
        },
        false,
    );
    o.repeat = 2;
    let records = run_all(
        &[(spec("a", 30), a), (spec("b", 30), b)],
        &o,
        &marker_checker,
    );
    let ids: Vec<(String, usize)> = records.iter().map(|r| (r.task.clone(), r.repeat)).collect();
    assert_eq!(
        ids,
        vec![
            ("a".into(), 0),
            ("a".into(), 1),
            ("b".into(), 0),
            ("b".into(), 1)
        ]
    );
    assert!(records.iter().all(|r| r.passed));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn exec_times_out_and_kills_the_process_group() {
    let root = temp("exec");
    let bin = root.join("hang");
    script(&bin, "sleep 30 &\nsleep 30\n");
    let start = Instant::now();
    let r = exec(&bin, &[], None, Duration::from_millis(300)).unwrap();
    assert!(r.timed_out);
    assert!(start.elapsed() < Duration::from_secs(10));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn exec_captures_output_and_exit_code() {
    let root = temp("exec-out");
    let bin = root.join("talk");
    script(&bin, "echo out; echo err >&2; exit 3\n");
    let r = exec(&bin, &[], None, Duration::from_secs(10)).unwrap();
    assert_eq!(
        (r.stdout.trim(), r.stderr.trim(), r.code, r.timed_out),
        ("out", "err", Some(3), false)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn job_ids_and_states_are_parsed_from_queue_output() {
    assert_eq!(
        parse_job_id("queued job #12 (3 ahead) — worker started"),
        Some(12)
    );
    assert_eq!(parse_job_id("no id here"), None);
    assert_eq!(parse_state("job #12 — running\nsource: x"), Some("running"));
    assert_eq!(
        parse_state("job #12 — succeeded\n\nlog mentions failed"),
        Some("succeeded")
    );
    assert_eq!(parse_state("job #3 — cancelled"), Some("cancelled"));
    assert_eq!(parse_state("garbage"), None);
}

#[test]
fn the_summary_table_lists_each_run_and_a_pass_count() {
    let root = temp("table");
    let td = task_dir(&root, "rect");
    let bin = fake_orchestrator(&root, "ok", "BAD");
    let o = opts(
        &root,
        Mode::Direct {
            orchestrator_bin: bin,
        },
        false,
    );
    let r = run_one(&spec("rect", 30), &td, 0, &o, &marker_checker);
    let table = summary_table(&[r]);
    assert!(table.contains("rect") && table.contains("FAIL") && table.contains("rect-output"));
    assert!(table.contains("0/1 runs passed"));
    let _ = std::fs::remove_dir_all(&root);
}

fn solution(task_dir: &Path, kind: &str, marker: &str) {
    let dir = task_dir.join("solutions").join(kind).join("src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), marker).unwrap();
}

#[test]
fn validation_accepts_a_good_passing_and_bad_failing_pair() {
    let root = temp("validate-ok");
    let td = task_dir(&root, "rect");
    solution(&td, "good", "GOOD");
    solution(&td, "bad", "BAD");
    let results = validate_tasks(&[(spec("rect", 30), td)], &marker_checker);
    assert_eq!(results, vec![("rect".to_string(), Vec::<String>::new())]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn validation_reports_a_bad_solution_that_passes_a_good_one_that_fails_and_missing_directories() {
    let root = temp("validate-bad");
    let both_wrong = task_dir(&root, "wrong");
    solution(&both_wrong, "good", "BAD");
    solution(&both_wrong, "bad", "GOOD");
    let missing = task_dir(&root, "missing");
    let results = validate_tasks(
        &[
            (spec("wrong", 30), both_wrong),
            (spec("missing", 30), missing),
        ],
        &marker_checker,
    );
    assert_eq!(results[0].1.len(), 2);
    assert!(results[0].1[0].contains("good solution fails"));
    assert!(results[0].1[1].contains("bad solution passes"));
    assert_eq!(
        results[1].1,
        vec!["missing solutions/good", "missing solutions/bad"]
    );
    let _ = std::fs::remove_dir_all(&root);
}
