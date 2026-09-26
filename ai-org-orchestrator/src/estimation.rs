//! Cross-run milestone effort estimation. MilestonePlanner rates each milestone's difficulty
//! on a 1-255 scale (`EFFORT:`, see `parse_milestone_plan`); once a milestone finishes (whether
//! it converged or not), what it actually cost -- iterations used, wall-clock time, tokens --
//! is appended to a persisted history file. Every future estimate is a least-squares fit of
//! effort against that growing history, so the same effort rating maps to a tighter, more
//! accurate prediction as more real milestones complete, on this machine, across every job.
//!
//! Deliberately not a trained model: the user asked for "improves its estimation over time",
//! not a retraining pipeline (this project already has one of those, for the actual reviewer
//! model -- see the top-level `reviewer-train` crate elsewhere in this workspace's family of
//! repos). A recalibrated linear fit over a few dozen data points is the right amount of
//! machinery for "is 200 minutes still a good guess for an effort-180 milestone".

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// One milestone's (estimate, actual) pair. Recorded even when `converged` is false -- a
/// milestone that burned its whole iteration budget without passing is real evidence that
/// milestones rated at this effort level can cost that much, and hiding it would make the
/// calibration systematically over-optimistic.
#[derive(Debug, Clone, PartialEq)]
pub struct EstimationRecord {
    pub milestone_name: String,
    pub estimated_effort: u8,
    pub actual_iterations: usize,
    pub max_iter: usize,
    pub converged: bool,
    pub wall_clock_secs: f64,
    /// Tokens attributable to this milestone's own attempt. Milestones in the same fan-out
    /// wave run concurrently against one shared `Manager` token counter (see
    /// `implement_milestones`), so a token delta taken during a fan-out wave can include a
    /// sibling milestone's traffic too -- a known noise source, not a precision guarantee.
    /// `actual_iterations` and `wall_clock_secs` are unaffected: both are purely local to this
    /// call, never shared across threads.
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub timestamp: String,
}

impl EstimationRecord {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "milestone_name": self.milestone_name,
            "estimated_effort": self.estimated_effort,
            "actual_iterations": self.actual_iterations,
            "max_iter": self.max_iter,
            "converged": self.converged,
            "wall_clock_secs": self.wall_clock_secs,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "timestamp": self.timestamp,
        })
    }

    fn from_json_line(line: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        Some(Self {
            milestone_name: v.get("milestone_name")?.as_str()?.to_string(),
            estimated_effort: v.get("estimated_effort")?.as_u64()? as u8,
            actual_iterations: v.get("actual_iterations")?.as_u64()? as usize,
            max_iter: v.get("max_iter")?.as_u64()? as usize,
            converged: v.get("converged")?.as_bool()?,
            wall_clock_secs: v.get("wall_clock_secs")?.as_f64()?,
            tokens_in: v.get("tokens_in")?.as_u64()? as u32,
            tokens_out: v.get("tokens_out")?.as_u64()? as u32,
            timestamp: v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        })
    }
}

/// Where calibration history persists by default, independent of any single job's workspace
/// (which is ephemeral) -- so estimates keep improving job over job instead of resetting every
/// run. `REWRITER_ESTIMATION_HISTORY` overrides it, matching the `REWRITER_QUEUE_DIR`
/// convention `job-queue` already uses. Callers decide whether to use this at all: see
/// `Manager::with_estimation_history` -- nothing in this module reads it implicitly.
pub fn history_path() -> PathBuf {
    if let Some(p) = std::env::var_os("REWRITER_ESTIMATION_HISTORY") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/rewriter/estimation-history.jsonl")
}

/// A missing file (nothing recorded yet) is just an empty history, not an error.
pub fn load_history(path: &Path) -> Vec<EstimationRecord> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter_map(EstimationRecord::from_json_line).collect()
}

/// Append one record as a single line, one `write_all` call so concurrent fan-out siblings
/// appending to the same file (each with its own `O_APPEND` file handle) can't interleave
/// mid-line the way a read-modify-write would.
pub fn record(path: &Path, rec: &EstimationRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = format!("{}\n", rec.to_json());
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

/// Below this many past records, there isn't enough data to fit a trustworthy line -- three
/// points is little enough that a single fluke milestone would otherwise dominate the fit.
pub const MIN_SAMPLES: usize = 3;

/// Cold-start assumption before any history exists: a deliberately rough per-iteration time
/// guess, replaced by a real fit as soon as `MIN_SAMPLES` milestones have completed.
const COLD_START_SECS_PER_ITERATION: f64 = 90.0;
const COLD_START_TOKENS_PER_ITERATION: f64 = 8_000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Prediction {
    pub predicted_iterations: f64,
    pub predicted_secs: f64,
    pub predicted_tokens: f64,
    /// False for the cold-start fallback (fewer than `MIN_SAMPLES` past records) -- callers
    /// should say so rather than presenting a guess as a track record.
    pub calibrated: bool,
    pub sample_size: usize,
}

/// Predict iterations/time/tokens for a milestone rated `effort` (1-255), from every past
/// (estimate, actual) record on file. Uses a separate ordinary-least-squares fit of each metric
/// against effort -- iterations, seconds, and tokens don't move together (a milestone can burn
/// many iterations cheaply or few iterations expensively), so one shared fit would blur all
/// three.
pub fn predict(effort: u8, max_iter: usize, history: &[EstimationRecord]) -> Prediction {
    if history.len() < MIN_SAMPLES {
        let frac = effort as f64 / 255.0;
        let iters = (frac * max_iter as f64).max(1.0);
        return Prediction {
            predicted_iterations: iters,
            predicted_secs: iters * COLD_START_SECS_PER_ITERATION,
            predicted_tokens: iters * COLD_START_TOKENS_PER_ITERATION,
            calibrated: false,
            sample_size: 0,
        };
    }

    let xs: Vec<f64> = history.iter().map(|r| r.estimated_effort as f64).collect();
    let fit_against = |extract: &dyn Fn(&EstimationRecord) -> f64| -> (f64, f64) {
        let ys: Vec<f64> = history.iter().map(|r| extract(r)).collect();
        linear_regression(&xs, &ys)
    };

    let (a_iter, b_iter) = fit_against(&|r| r.actual_iterations as f64);
    let (a_secs, b_secs) = fit_against(&|r| r.wall_clock_secs);
    let (a_tok, b_tok) = fit_against(&|r| (r.tokens_in + r.tokens_out) as f64);

    let x = effort as f64;
    Prediction {
        predicted_iterations: (a_iter + b_iter * x).max(0.0),
        predicted_secs: (a_secs + b_secs * x).max(0.0),
        predicted_tokens: (a_tok + b_tok * x).max(0.0),
        calibrated: true,
        sample_size: history.len(),
    }
}

/// Ordinary least squares intercept/slope for `y = a + b*x`. Falls back to a flat line at the
/// mean of `ys` when every `x` is identical -- a slope isn't definable from one x-value, and a
/// flat prediction is a more honest degenerate case than an arbitrary one.
fn linear_regression(xs: &[f64], ys: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let var_x: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
    if var_x == 0.0 {
        return (mean_y, 0.0);
    }
    let cov: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mean_x) * (y - mean_y)).sum();
    let b = cov / var_x;
    let a = mean_y - b * mean_x;
    (a, b)
}

pub fn format_prediction(p: &Prediction) -> String {
    let confidence = if p.calibrated {
        format!("calibrated from {} past milestone(s)", p.sample_size)
    } else {
        "cold start, no history yet — rough guess".to_string()
    };
    format!(
        "~{:.1} iteration(s), ~{:.1}min, ~{:.0} tokens ({confidence})",
        p.predicted_iterations,
        p.predicted_secs / 60.0,
        p.predicted_tokens,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(effort: u8, iterations: usize, secs: f64, tokens: u32) -> EstimationRecord {
        EstimationRecord {
            milestone_name: "m".to_string(),
            estimated_effort: effort,
            actual_iterations: iterations,
            max_iter: 10,
            converged: true,
            wall_clock_secs: secs,
            tokens_in: tokens,
            tokens_out: tokens,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn linear_regression_recovers_an_exact_line() {
        // y = 2 + 3x exactly -- a fit over exact data should recover the coefficients closely.
        let xs = vec![1.0, 2.0, 3.0, 4.0];
        let ys = vec![5.0, 8.0, 11.0, 14.0];
        let (a, b) = linear_regression(&xs, &ys);
        assert!((a - 2.0).abs() < 1e-9, "a = {a}");
        assert!((b - 3.0).abs() < 1e-9, "b = {b}");
    }

    #[test]
    fn linear_regression_falls_back_to_the_mean_when_x_never_varies() {
        let xs = vec![5.0, 5.0, 5.0];
        let ys = vec![10.0, 20.0, 30.0];
        let (a, b) = linear_regression(&xs, &ys);
        assert_eq!(a, 20.0);
        assert_eq!(b, 0.0);
    }

    #[test]
    fn cold_start_prediction_scales_with_effort_and_is_flagged_uncalibrated() {
        let low = predict(25, 10, &[]);
        let high = predict(250, 10, &[]);
        assert!(!low.calibrated);
        assert!(!high.calibrated);
        assert!(high.predicted_iterations > low.predicted_iterations);
        assert!(high.predicted_secs > low.predicted_secs);
    }

    #[test]
    fn a_handful_of_records_is_not_enough_to_calibrate() {
        let history = vec![rec(100, 3, 300.0, 10_000), rec(200, 6, 600.0, 20_000)];
        let p = predict(150, 10, &history);
        assert!(!p.calibrated, "MIN_SAMPLES is 3 -- two records must not calibrate");
    }

    #[test]
    fn enough_records_produce_a_calibrated_prediction_between_the_observed_range() {
        let history = vec![
            rec(50, 2, 120.0, 5_000),
            rec(150, 5, 400.0, 15_000),
            rec(250, 9, 800.0, 30_000),
        ];
        let p = predict(150, 10, &history);
        assert!(p.calibrated);
        assert_eq!(p.sample_size, 3);
        // An effort right in the middle of the observed range should land near the middle
        // record's actuals, not at either extreme.
        assert!(p.predicted_iterations > 3.0 && p.predicted_iterations < 7.0, "{}", p.predicted_iterations);
        assert!(p.predicted_secs > 200.0 && p.predicted_secs < 600.0, "{}", p.predicted_secs);
    }

    #[test]
    fn record_and_load_round_trip_through_a_real_file() {
        let path = std::env::temp_dir()
            .join(format!("aoo-estimation-roundtrip-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let a = rec(80, 3, 210.5, 12_345);
        let b = rec(200, 8, 650.25, 40_000);
        record(&path, &a).unwrap();
        record(&path, &b).unwrap();

        let loaded = load_history(&path);
        assert_eq!(loaded, vec![a, b]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_history_of_a_missing_file_is_an_empty_history_not_an_error() {
        let path = std::env::temp_dir().join("aoo-estimation-definitely-does-not-exist.jsonl");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_history(&path), Vec::new());
    }

    #[test]
    fn history_path_honors_the_env_override() {
        let path = std::env::temp_dir().join("aoo-estimation-override-test.jsonl");
        unsafe {
            std::env::set_var("REWRITER_ESTIMATION_HISTORY", &path);
        }
        assert_eq!(history_path(), path);
        unsafe {
            std::env::remove_var("REWRITER_ESTIMATION_HISTORY");
        }
    }
}
