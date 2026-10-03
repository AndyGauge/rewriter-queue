use crate::types::{Regression, RunFile};

pub fn render(baseline: &RunFile, current: &RunFile, regressions: &[Regression]) -> String {
    format!(
        "{} -> {}: {} regression(s)\n",
        baseline.label,
        current.label,
        regressions.len()
    )
}
