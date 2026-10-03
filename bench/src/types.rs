use serde::{Deserialize, Serialize};

/// `bench/tasks/<name>/task.toml`. The task directory also holds `source/` (the V1 input, possibly
/// a stub), `mission.md`, and `acceptance/` (hidden from the pipeline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub name: String,
    pub description: String,
    #[serde(default = "default_max_iter")]
    pub max_iter: usize,
    /// A run that exceeds this is a failure, not a hang.
    #[serde(default = "default_time_budget_secs")]
    pub time_budget_secs: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Name of the binary the finished crate must produce, when checks run it.
    #[serde(default)]
    pub binary: Option<String>,
    /// Run `acceptance/tests/*.rs` as cargo integration tests against the finished crate.
    #[serde(default)]
    pub cargo_tests: bool,
    #[serde(default, rename = "check")]
    pub checks: Vec<Check>,
}

fn default_max_iter() -> usize {
    10
}

fn default_time_budget_secs() -> u64 {
    3600
}

/// Run the finished binary once and compare. Unset `expect_*` fields are not checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub expect_stdout: Option<String>,
    #[serde(default)]
    pub expect_stderr: Option<String>,
    #[serde(default)]
    pub expect_exit: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub name: String,
    pub passed: bool,
    /// What differed, in one or two lines, when `passed` is false.
    pub detail: String,
}

/// The measurable result of one task run (one repeat).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub task: String,
    pub repeat: usize,
    pub job_id: Option<u64>,
    /// Passed the task's acceptance checks within its time budget. The pipeline's own
    /// `succeeded` never decides this.
    pub passed: bool,
    pub failed_checks: Vec<String>,
    /// `succeeded`, `failed`, `cancelled` or `timeout` as the pipeline reported it.
    pub pipeline_outcome: String,
    pub wall_secs: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model_calls: u64,
    pub retries: u64,
    pub gate_failures: u64,
    pub integration_attempts: u64,
    pub escalations: u64,
    pub plan_revised: bool,
    pub debugger_consulted: bool,
    pub started: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvStamp {
    pub commit: String,
    pub version: String,
    pub providers: Vec<String>,
    pub host: String,
    pub queue_binary_sha: String,
    pub orchestrator_binary_sha: String,
    pub timestamp: String,
}

/// One invocation of `rewriter-bench run`: stored as one JSON file under `bench/results/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFile {
    pub label: String,
    pub env: EnvStamp,
    pub records: Vec<RunRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task: String,
    pub runs: usize,
    pub pass_rate: f64,
    pub median_secs: f64,
    pub median_tokens: f64,
    pub secs_spread: f64,
    pub tokens_spread: f64,
}

/// How much worse than the baseline counts as a regression.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// Absolute drop in pass rate (0.0-1.0) that fails the comparison.
    pub pass_rate_drop: f64,
    /// Median time may rise by this ratio over the baseline median, beyond the baseline's spread.
    pub time_ratio: f64,
    /// Same, for tokens.
    pub token_ratio: f64,
    /// Fewer runs than this per task is reported but never fails the comparison.
    pub min_runs: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { pass_rate_drop: 0.2, time_ratio: 1.5, token_ratio: 1.5, min_runs: 3 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Regression {
    pub task: String,
    /// `pass_rate`, `time` or `tokens`.
    pub metric: String,
    pub baseline: f64,
    pub current: f64,
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_spec_parses_with_defaults() {
        let spec: TaskSpec = toml::from_str(
            r#"
name = "rect"
description = "draw a rectangle"
binary = "rect"

[[check]]
name = "basic"
args = ["0", "0", "4", "2"]
expect_stdout = "+---+\n"
expect_exit = 0
"#,
        )
        .unwrap();
        assert_eq!(spec.max_iter, 10);
        assert_eq!(spec.time_budget_secs, 3600);
        assert_eq!(spec.checks.len(), 1);
        assert_eq!(spec.checks[0].expect_exit, Some(0));
        assert!(!spec.cargo_tests);
    }

    #[test]
    fn a_run_file_round_trips_through_json() {
        let f = RunFile {
            label: "t".into(),
            env: EnvStamp {
                commit: "abc".into(),
                version: "0.9.1".into(),
                providers: vec!["p".into()],
                host: "h".into(),
                queue_binary_sha: "q".into(),
                orchestrator_binary_sha: "o".into(),
                timestamp: "2026-10-03T00:00:00Z".into(),
            },
            records: vec![RunRecord {
                task: "rect".into(),
                repeat: 0,
                job_id: Some(1),
                passed: true,
                failed_checks: vec![],
                pipeline_outcome: "succeeded".into(),
                wall_secs: 1.5,
                input_tokens: 10,
                output_tokens: 5,
                model_calls: 3,
                retries: 0,
                gate_failures: 0,
                integration_attempts: 1,
                escalations: 0,
                plan_revised: false,
                debugger_consulted: false,
                started: "2026-10-03T00:00:00Z".into(),
            }],
        };
        let back: RunFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(f, back);
    }
}
