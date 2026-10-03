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
