use crate::types::{Regression, RunFile, RunRecord, TaskSummary, Thresholds};

pub fn summarize(_records: &[RunRecord]) -> Vec<TaskSummary> {
    Vec::new()
}

pub fn compare(_baseline: &RunFile, _current: &RunFile, _t: &Thresholds) -> Vec<Regression> {
    Vec::new()
}
