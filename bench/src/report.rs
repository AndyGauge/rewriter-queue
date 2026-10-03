use crate::compare::{analyze, providers_differ, Status};
use crate::types::{Regression, RunFile, RunRecord, TaskSummary, Thresholds};
use std::fmt::Write;

const MAX_TASK_WIDTH: usize = 24;

type Fmt = fn(&Option<TaskSummary>) -> String;

fn fmt_rate(s: &Option<TaskSummary>) -> String {
    s.as_ref()
        .map_or("-".into(), |s| format!("{:.0}%", s.pass_rate * 100.0))
}

fn fmt_secs(s: &Option<TaskSummary>) -> String {
    s.as_ref()
        .map_or("-".into(), |s| format!("{:.1}s", s.median_secs))
}

fn fmt_tokens(s: &Option<TaskSummary>) -> String {
    s.as_ref().map_or("-".into(), |s| {
        let t = s.median_tokens;
        if t >= 1_000_000.0 {
            format!("{:.2}M", t / 1_000_000.0)
        } else if t >= 1000.0 {
            format!("{:.1}k", t / 1000.0)
        } else {
            format!("{t:.0}")
        }
    })
}

fn pair(f: Fmt, a: &Option<TaskSummary>, b: &Option<TaskSummary>) -> String {
    format!("{} -> {}", f(a), f(b))
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let head: String = s.chars().take(width - 1).collect();
        format!("{head}~")
    }
}

fn header(out: &mut String, role: &str, f: &RunFile) {
    let _ = writeln!(
        out,
        "{role:<9}{} (commit {}, version {}, {} runs)",
        f.label,
        f.env.commit,
        f.env.version,
        f.records.len()
    );
}

fn job_ids(records: &[RunRecord], task: &str) -> String {
    let ids: Vec<String> = records
        .iter()
        .filter(|r| r.task == task)
        .filter_map(|r| r.job_id)
        .map(|j| j.to_string())
        .collect();
    if ids.is_empty() {
        "none".into()
    } else {
        ids.join(", ")
    }
}

#[cfg(test)]
pub fn render(baseline: &RunFile, current: &RunFile, regressions: &[Regression]) -> String {
    render_with(baseline, current, regressions, &Thresholds::default())
}

/// `render` classifies insufficient/improved rows with default thresholds; callers that compared
/// with their own thresholds should use this so the table agrees with the regressions.
pub fn render_with(
    baseline: &RunFile,
    current: &RunFile,
    regressions: &[Regression],
    t: &Thresholds,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Benchmark comparison");
    header(&mut out, "baseline", baseline);
    header(&mut out, "current", current);
    if providers_differ(baseline, current) {
        let _ = writeln!(
            out,
            "Model note: providers differ (baseline: {}; current: {}). Results across different \
             models are not directly comparable.",
            baseline.env.providers.join(", "),
            current.env.providers.join(", ")
        );
    } else {
        let _ = writeln!(
            out,
            "Model note: same providers ({}).",
            baseline.env.providers.join(", ")
        );
    }
    out.push('\n');

    let rows = analyze(baseline, current, t);
    let width = rows
        .iter()
        .map(|r| r.task.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, MAX_TASK_WIDTH);
    let _ = writeln!(
        out,
        "{:<width$}  {:<11}  {:<17}  {:<15}  status",
        "task", "pass", "median time", "median tokens"
    );
    for r in &rows {
        let status = if regressions.iter().any(|g| g.task == r.task) {
            Status::Regressed.label()
        } else {
            r.status.label()
        };
        let _ = writeln!(
            out,
            "{:<width$}  {:<11}  {:<17}  {:<15}  {}",
            clip(&r.task, width),
            pair(fmt_rate, &r.base, &r.current),
            pair(fmt_secs, &r.base, &r.current),
            pair(fmt_tokens, &r.base, &r.current),
            status
        );
    }
    if rows.is_empty() {
        let _ = writeln!(out, "(no tasks in either run)");
    }
    if rows.iter().any(|r| r.status == Status::Insufficient) {
        let _ = writeln!(
            out,
            "\ninsufficient: fewer than {} runs on one side; reported only, never a regression.",
            t.min_runs
        );
    }

    out.push('\n');
    if regressions.is_empty() {
        let _ = writeln!(out, "No regressions.");
    } else {
        let _ = writeln!(out, "Regressions ({}):", regressions.len());
        for g in regressions {
            let _ = writeln!(out, "  {} [{}]: {}", g.task, g.metric, g.detail);
            let _ = writeln!(
                out,
                "    baseline jobs: {}; current jobs: {}",
                job_ids(&baseline.records, &g.task),
                job_ids(&current.records, &g.task)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::compare;
    use crate::compare::tests::{file, rec};

    fn steady(task: &str, passes: &[bool], secs: f64, tokens: u64) -> Vec<RunRecord> {
        passes.iter().map(|p| rec(task, *p, secs, tokens)).collect()
    }

    #[test]
    fn a_regression_report_names_the_task_and_marks_it() {
        let base = file("good", steady("rect", &[true; 3], 10.0, 1000));
        let cur = file("bad", steady("rect", &[true, false, false], 10.0, 1000));
        let reg = compare(&base, &cur, &Thresholds::default());
        let text = render(&base, &cur, &reg);
        assert!(text.contains("rect"));
        assert!(text.contains("REGRESSED"));
        assert!(text.contains("Regressions (1):"));
        assert!(text.contains("100% -> 33%"));
        assert!(text.contains("current jobs: 1, 1, 1"));
        assert!(!text.contains("No regressions."));
    }

    #[test]
    fn an_unchanged_report_says_no_regressions() {
        let f = file("same", steady("rect", &[true; 3], 10.0, 1000));
        let text = render(&f, &f.clone(), &[]);
        assert!(text.contains("No regressions."));
        assert!(!text.contains("REGRESSED"));
        assert!(text.contains("same providers"));
        assert!(text.contains("commit abc"));
    }

    #[test]
    fn statuses_cover_new_missing_insufficient_and_improved() {
        let mut b = steady("gone", &[true; 3], 10.0, 100);
        b.extend(steady("few", &[true; 3], 10.0, 100));
        b.extend(steady("fast", &[true; 3], 30.0, 100));
        let mut c = steady("fresh", &[true; 3], 10.0, 100);
        c.extend(steady("few", &[true], 10.0, 100));
        c.extend(steady("fast", &[true; 3], 10.0, 100));
        let (base, cur) = (file("b", b), file("c", c));
        let reg = compare(&base, &cur, &Thresholds::default());
        let text = render(&base, &cur, &reg);
        for word in ["missing", "insufficient", "improved", "new"] {
            assert!(text.contains(word), "{word} in\n{text}");
        }
        assert!(text.contains("gone [missing]"));
        assert!(text.contains("30.0s -> 10.0s"));
    }

    #[test]
    fn a_provider_change_is_noted_but_not_a_regression() {
        let base = file("b", steady("a", &[true; 3], 10.0, 100));
        let mut cur = file("c", steady("a", &[true; 3], 10.0, 100));
        cur.env.providers = vec!["other-model".into()];
        assert!(compare(&base, &cur, &Thresholds::default()).is_empty());
        let text = render(&base, &cur, &[]);
        assert!(text.contains("providers differ"));
        assert!(text.contains("other-model"));
    }

    #[test]
    fn empty_inputs_render_without_panicking() {
        let e = file("e", vec![]);
        let text = render(&e, &e, &[]);
        assert!(text.contains("No regressions."));
        assert!(text.contains("no tasks"));
    }

    #[test]
    fn table_lines_stay_within_100_columns_for_long_task_names() {
        let name = "a_very_long_task_name_that_goes_on_and_on_forever";
        let base = file("b", steady(name, &[true; 3], 1234.5, 12_345_678));
        let cur = file("c", steady(name, &[true; 3], 2000.0, 1_000));
        let text = render(&base, &cur, &compare(&base, &cur, &Thresholds::default()));
        for l in text.lines().skip(5).take(2) {
            assert!(l.chars().count() <= 100, "{l}");
        }
        assert!(text.contains("12.35M -> 1.0k"));
    }
}
