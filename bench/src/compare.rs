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
pub(crate) mod tests;
