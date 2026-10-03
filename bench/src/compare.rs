//! Regression policy. Per task present in the baseline with at least `min_runs` runs on both
//! sides: pass rate regresses when it falls by more than `pass_rate_drop`; median time (or tokens)
//! regresses when it exceeds baseline * ratio and the rise is larger than the baseline's own
//! spread (median absolute deviation). Tasks with too few runs are reported, never failed. A
//! baseline task absent from the current run is a regression (`missing`); a task only in the
//! current run is `new`. Improvements mirror the regression rules and are informational.

use crate::types::{Regression, RunFile, RunRecord, TaskSummary, Thresholds};
use std::collections::BTreeMap;

const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Improved,
    Regressed,
    Insufficient,
    New,
    Missing,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Improved => "improved",
            Status::Regressed => "REGRESSED",
            Status::Insufficient => "insufficient",
            Status::New => "new",
            Status::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub task: String,
    pub base: Option<TaskSummary>,
    pub current: Option<TaskSummary>,
    pub status: Status,
    pub regressions: Vec<Regression>,
}

fn median(values: &[f64]) -> f64 {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn spread(values: &[f64]) -> f64 {
    let m = median(values);
    let dev: Vec<f64> = values
        .iter()
        .filter(|x| x.is_finite())
        .map(|x| (x - m).abs())
        .collect();
    median(&dev)
}

pub fn summarize(records: &[RunRecord]) -> Vec<TaskSummary> {
    let mut by_task: BTreeMap<&str, Vec<&RunRecord>> = BTreeMap::new();
    for r in records {
        by_task.entry(r.task.as_str()).or_default().push(r);
    }
    by_task
        .into_iter()
        .map(|(task, runs)| {
            let secs: Vec<f64> = runs.iter().map(|r| r.wall_secs).collect();
            let tokens: Vec<f64> = runs
                .iter()
                .map(|r| (r.input_tokens + r.output_tokens) as f64)
                .collect();
            let passed = runs.iter().filter(|r| r.passed).count();
            TaskSummary {
                task: task.to_string(),
                runs: runs.len(),
                pass_rate: passed as f64 / runs.len() as f64,
                median_secs: median(&secs),
                median_tokens: median(&tokens),
                secs_spread: spread(&secs),
                tokens_spread: spread(&tokens),
            }
        })
        .collect()
}

fn worse(base: f64, cur: f64, base_spread: f64, ratio: f64) -> bool {
    base > 0.0 && cur > base * ratio && cur - base > base_spread
}

fn better(base: f64, cur: f64, base_spread: f64, ratio: f64) -> bool {
    base > 0.0 && ratio > 0.0 && cur < base / ratio && base - cur > base_spread
}

fn rate(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

pub fn analyze(baseline: &RunFile, current: &RunFile, t: &Thresholds) -> Vec<TaskRow> {
    let base: BTreeMap<String, TaskSummary> = summarize(&baseline.records)
        .into_iter()
        .map(|s| (s.task.clone(), s))
        .collect();
    let cur: BTreeMap<String, TaskSummary> = summarize(&current.records)
        .into_iter()
        .map(|s| (s.task.clone(), s))
        .collect();
    let mut names: Vec<&String> = base.keys().chain(cur.keys()).collect();
    names.sort();
    names.dedup();

    names
        .into_iter()
        .map(|name| {
            let b = base.get(name);
            let c = cur.get(name);
            let mut regressions = Vec::new();
            let status = match (b, c) {
                (None, _) => Status::New,
                (Some(b), None) => {
                    regressions.push(Regression {
                        task: name.clone(),
                        metric: "missing".into(),
                        baseline: b.runs as f64,
                        current: 0.0,
                        detail: format!("{} runs in the baseline, none in the current run", b.runs),
                    });
                    Status::Missing
                }
                (Some(b), Some(c)) if b.runs < t.min_runs || c.runs < t.min_runs => {
                    Status::Insufficient
                }
                (Some(b), Some(c)) => {
                    let mut improved = false;
                    if b.pass_rate - c.pass_rate > t.pass_rate_drop + EPS {
                        regressions.push(Regression {
                            task: name.clone(),
                            metric: "pass_rate".into(),
                            baseline: b.pass_rate,
                            current: c.pass_rate,
                            detail: format!(
                                "pass rate fell from {} to {} (allowed drop {})",
                                rate(b.pass_rate),
                                rate(c.pass_rate),
                                rate(t.pass_rate_drop)
                            ),
                        });
                    } else if c.pass_rate - b.pass_rate > t.pass_rate_drop + EPS {
                        improved = true;
                    }
                    for (metric, unit, bm, cm, sp, ratio) in [
                        (
                            "time",
                            "s",
                            b.median_secs,
                            c.median_secs,
                            b.secs_spread,
                            t.time_ratio,
                        ),
                        (
                            "tokens",
                            " tokens",
                            b.median_tokens,
                            c.median_tokens,
                            b.tokens_spread,
                            t.token_ratio,
                        ),
                    ] {
                        if worse(bm, cm, sp, ratio) {
                            regressions.push(Regression {
                                task: name.clone(),
                                metric: metric.into(),
                                baseline: bm,
                                current: cm,
                                detail: format!(
                                    "median {metric} rose from {bm:.1}{unit} to {cm:.1}{unit} \
                                     (x{:.2}, limit x{ratio:.2}, baseline spread {sp:.1})",
                                    cm / bm
                                ),
                            });
                        } else if better(bm, cm, sp, ratio) {
                            improved = true;
                        }
                    }
                    if !regressions.is_empty() {
                        Status::Regressed
                    } else if improved {
                        Status::Improved
                    } else {
                        Status::Ok
                    }
                }
            };
            TaskRow {
                task: name.clone(),
                base: b.cloned(),
                current: c.cloned(),
                status,
                regressions,
            }
        })
        .collect()
}

pub fn compare(baseline: &RunFile, current: &RunFile, t: &Thresholds) -> Vec<Regression> {
    analyze(baseline, current, t)
        .into_iter()
        .flat_map(|r| r.regressions)
        .collect()
}

pub fn providers_differ(baseline: &RunFile, current: &RunFile) -> bool {
    let norm = |f: &RunFile| {
        let mut p = f.env.providers.clone();
        p.sort();
        p
    };
    norm(baseline) != norm(current)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::EnvStamp;

    pub(crate) fn rec(task: &str, passed: bool, secs: f64, tokens: u64) -> RunRecord {
        RunRecord {
            task: task.into(),
            repeat: 0,
            job_id: Some(1),
            passed,
            failed_checks: vec![],
            pipeline_outcome: "succeeded".into(),
            wall_secs: secs,
            input_tokens: tokens / 2,
            output_tokens: tokens - tokens / 2,
            model_calls: 1,
            retries: 0,
            gate_failures: 0,
            integration_attempts: 1,
            escalations: 0,
            plan_revised: false,
            debugger_consulted: false,
            started: "t".into(),
        }
    }

    pub(crate) fn file(label: &str, records: Vec<RunRecord>) -> RunFile {
        RunFile {
            label: label.into(),
            env: EnvStamp {
                commit: "abc".into(),
                version: "0.9.1".into(),
                providers: vec!["m".into()],
                host: "h".into(),
                queue_binary_sha: "q".into(),
                orchestrator_binary_sha: "o".into(),
                timestamp: "t".into(),
            },
            records,
        }
    }

    fn runs(task: &str, passes: &[bool], secs: &[f64], tokens: &[u64]) -> Vec<RunRecord> {
        (0..passes.len())
            .map(|i| rec(task, passes[i], secs[i], tokens[i]))
            .collect()
    }

    fn steady(task: &str, passes: &[bool], secs: f64, tokens: u64) -> Vec<RunRecord> {
        passes.iter().map(|p| rec(task, *p, secs, tokens)).collect()
    }

    fn metrics(rs: &[Regression]) -> Vec<&str> {
        rs.iter().map(|r| r.metric.as_str()).collect()
    }

    #[test]
    fn summarize_handles_empty_input() {
        assert!(summarize(&[]).is_empty());
    }

    #[test]
    fn summarize_groups_sorts_and_computes_medians() {
        let mut recs = runs("b", &[true, false], &[10.0, 20.0], &[100, 300]);
        recs.extend(runs(
            "a",
            &[true, true, true],
            &[1.0, 2.0, 9.0],
            &[10, 20, 90],
        ));
        let s = summarize(&recs);
        assert_eq!(
            s.iter().map(|x| x.task.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(s[0].runs, 3);
        assert_eq!(s[0].pass_rate, 1.0);
        assert_eq!(s[0].median_secs, 2.0);
        assert_eq!(s[0].median_tokens, 20.0);
        assert_eq!(s[0].secs_spread, 1.0);
        assert_eq!(s[0].tokens_spread, 10.0);
        assert_eq!(s[1].pass_rate, 0.5);
        assert_eq!(s[1].median_secs, 15.0);
        assert_eq!(s[1].median_tokens, 200.0);
        assert_eq!(s[1].secs_spread, 5.0);
    }

    #[test]
    fn a_single_run_has_zero_spread() {
        let s = summarize(&[rec("a", true, 7.0, 50)]);
        assert_eq!(s[0].secs_spread, 0.0);
        assert_eq!(s[0].tokens_spread, 0.0);
        assert_eq!(s[0].median_secs, 7.0);
    }

    #[test]
    fn non_finite_times_do_not_poison_the_summary() {
        let s = summarize(&[rec("a", true, f64::NAN, 5), rec("a", true, 4.0, 5)]);
        assert_eq!(s[0].median_secs, 4.0);
        let all_nan = summarize(&[rec("a", true, f64::NAN, 5)]);
        assert_eq!(all_nan[0].median_secs, 0.0);
    }

    #[test]
    fn pass_rate_at_just_under_and_just_over_the_threshold() {
        let t = Thresholds {
            pass_rate_drop: 0.2,
            min_runs: 5,
            ..Thresholds::default()
        };
        let base = file("b", steady("a", &[true; 5], 10.0, 100));
        let at = file(
            "c",
            steady("a", &[true, true, true, true, false], 10.0, 100),
        );
        assert!(compare(&base, &at, &t).is_empty());
        let over = file(
            "c",
            steady("a", &[true, true, true, false, false], 10.0, 100),
        );
        assert_eq!(metrics(&compare(&base, &over, &t)), ["pass_rate"]);
        let loose = Thresholds {
            pass_rate_drop: 0.3,
            min_runs: 5,
            ..Thresholds::default()
        };
        assert!(compare(&base, &at, &loose).is_empty());
        let just_under = Thresholds {
            pass_rate_drop: 0.19,
            min_runs: 5,
            ..Thresholds::default()
        };
        assert_eq!(metrics(&compare(&base, &at, &just_under)), ["pass_rate"]);
    }

    #[test]
    fn pass_rate_drop_from_three_of_three_to_one_of_three_is_a_regression() {
        let t = Thresholds::default();
        let base = file("b", steady("rect", &[true, true, true], 10.0, 100));
        let cur = file("c", steady("rect", &[true, false, false], 10.0, 100));
        let r = compare(&base, &cur, &t);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].task, "rect");
        assert_eq!(r[0].metric, "pass_rate");
        assert_eq!(r[0].baseline, 1.0);
        assert!((r[0].current - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn an_unchanged_result_has_no_regressions() {
        let recs = runs(
            "a",
            &[true, true, false],
            &[10.0, 11.0, 12.0],
            &[100, 110, 120],
        );
        let f = file("x", recs);
        assert!(compare(&f, &f.clone(), &Thresholds::default()).is_empty());
    }

    #[test]
    fn time_ratio_at_just_under_and_just_over_the_threshold() {
        let t = Thresholds::default();
        let base = file("b", steady("a", &[true; 3], 10.0, 100));
        let at = file("c", steady("a", &[true; 3], 15.0, 100));
        assert!(compare(&base, &at, &t).is_empty());
        let under = file("c", steady("a", &[true; 3], 14.9, 100));
        assert!(compare(&base, &under, &t).is_empty());
        let over = file("c", steady("a", &[true; 3], 15.1, 100));
        let r = compare(&base, &over, &t);
        assert_eq!(metrics(&r), ["time"]);
        assert_eq!(r[0].baseline, 10.0);
        assert_eq!(r[0].current, 15.1);
    }

    #[test]
    fn token_ratio_at_just_under_and_just_over_the_threshold() {
        let t = Thresholds::default();
        let base = file("b", steady("a", &[true; 3], 10.0, 1000));
        let at = file("c", steady("a", &[true; 3], 10.0, 1500));
        assert!(compare(&base, &at, &t).is_empty());
        let over = file("c", steady("a", &[true; 3], 10.0, 1501));
        assert_eq!(metrics(&compare(&base, &over, &t)), ["tokens"]);
    }

    #[test]
    fn a_noisy_baseline_does_not_flag_ordinary_variation() {
        let t = Thresholds::default();
        let base = file(
            "b",
            runs("a", &[true; 3], &[2.0, 10.0, 30.0], &[100, 100, 100]),
        );
        assert_eq!(summarize(&base.records)[0].secs_spread, 8.0);
        let cur = file(
            "c",
            runs("a", &[true; 3], &[16.0, 16.0, 16.0], &[100, 100, 100]),
        );
        assert!(compare(&base, &cur, &t).is_empty());
    }

    #[test]
    fn a_rise_equal_to_the_spread_is_not_beyond_it() {
        let t = Thresholds {
            time_ratio: 1.0,
            ..Thresholds::default()
        };
        let base = file("b", runs("a", &[true; 3], &[8.0, 10.0, 12.0], &[100; 3]));
        let eq = file("c", steady("a", &[true; 3], 12.0, 100));
        assert!(compare(&base, &eq, &t).is_empty());
        let gt = file("c", steady("a", &[true; 3], 12.5, 100));
        assert_eq!(metrics(&compare(&base, &gt, &t)), ["time"]);
    }

    #[test]
    fn a_zero_baseline_median_never_regresses_time_or_tokens() {
        let t = Thresholds::default();
        let base = file("b", steady("a", &[true; 3], 0.0, 0));
        let cur = file("c", steady("a", &[true; 3], 500.0, 9_000_000));
        assert!(compare(&base, &cur, &t).is_empty());
    }

    #[test]
    fn nan_inputs_do_not_panic_or_regress() {
        let t = Thresholds::default();
        let base = file("b", steady("a", &[true; 3], f64::NAN, 100));
        let cur = file("c", steady("a", &[true; 3], f64::NAN, 100));
        assert!(compare(&base, &cur, &t).is_empty());
        let nan_t = Thresholds {
            pass_rate_drop: f64::NAN,
            time_ratio: f64::NAN,
            ..t
        };
        let ok = file("c", steady("a", &[true; 3], 10.0, 100));
        let b2 = file("b", steady("a", &[true; 3], 10.0, 100));
        assert!(compare(&b2, &ok, &nan_t).is_empty());
    }

    #[test]
    fn min_runs_is_checked_on_both_sides_and_never_regresses() {
        let t = Thresholds::default();
        let many = file("b", steady("a", &[true; 3], 10.0, 100));
        let few = file("c", steady("a", &[false; 2], 100.0, 9000));
        assert!(compare(&many, &few, &t).is_empty());
        assert!(compare(&few, &many, &t).is_empty());
        let rows = analyze(&many, &few, &t);
        assert_eq!(rows[0].status, Status::Insufficient);
        let enough = file("c", steady("a", &[false; 3], 10.0, 100));
        assert_eq!(metrics(&compare(&many, &enough, &t)), ["pass_rate"]);
    }

    #[test]
    fn a_task_missing_from_the_current_run_is_a_regression() {
        let t = Thresholds::default();
        let mut recs = steady("a", &[true; 3], 10.0, 100);
        recs.extend(steady("b", &[true; 3], 10.0, 100));
        let base = file("b", recs);
        let cur = file("c", steady("a", &[true; 3], 10.0, 100));
        let r = compare(&base, &cur, &t);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].task, "b");
        assert_eq!(r[0].metric, "missing");
        assert_eq!(r[0].current, 0.0);
    }

    #[test]
    fn a_missing_task_is_a_regression_even_with_one_baseline_run() {
        let base = file("b", steady("a", &[true], 10.0, 100));
        let cur = file("c", vec![]);
        assert_eq!(
            metrics(&compare(&base, &cur, &Thresholds::default())),
            ["missing"]
        );
    }

    #[test]
    fn a_task_only_in_the_current_run_is_new_and_not_a_regression() {
        let t = Thresholds::default();
        let base = file("b", steady("a", &[true; 3], 10.0, 100));
        let mut recs = steady("a", &[true; 3], 10.0, 100);
        recs.extend(steady("z", &[false; 3], 99.0, 999));
        let cur = file("c", recs);
        assert!(compare(&base, &cur, &t).is_empty());
        let rows = analyze(&base, &cur, &t);
        assert_eq!(rows[1].task, "z");
        assert_eq!(rows[1].status, Status::New);
    }

    #[test]
    fn improvements_are_not_regressions() {
        let t = Thresholds::default();
        let base = file("b", steady("a", &[true, false, false], 30.0, 3000));
        let cur = file("c", steady("a", &[true, true, true], 10.0, 1000));
        assert!(compare(&base, &cur, &t).is_empty());
        assert_eq!(analyze(&base, &cur, &t)[0].status, Status::Improved);
    }

    #[test]
    fn empty_inputs_are_total() {
        let t = Thresholds::default();
        let e = file("e", vec![]);
        assert!(compare(&e, &e, &t).is_empty());
        assert!(analyze(&e, &e, &t).is_empty());
        let one = file("o", steady("a", &[true; 3], 1.0, 1));
        assert!(compare(&e, &one, &t).is_empty());
        assert_eq!(metrics(&compare(&one, &e, &t)), ["missing"]);
    }

    #[test]
    fn compare_is_deterministic() {
        let mut recs = steady("b", &[true; 3], 10.0, 100);
        recs.extend(steady("a", &[true; 3], 10.0, 100));
        let base = file("b", recs);
        let cur = file("c", vec![]);
        let t = Thresholds::default();
        let r = compare(&base, &cur, &t);
        assert_eq!(r, compare(&base, &cur, &t));
        assert_eq!(r[0].task, "a");
    }

    #[test]
    fn provider_lists_are_compared_as_sets() {
        let mut a = file("a", vec![]);
        let mut b = file("b", vec![]);
        a.env.providers = vec!["x".into(), "y".into()];
        b.env.providers = vec!["y".into(), "x".into()];
        assert!(!providers_differ(&a, &b));
        b.env.providers = vec!["x".into()];
        assert!(providers_differ(&a, &b));
    }
}
