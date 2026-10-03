use crate::types::{CheckOutcome, TaskSpec};
use std::path::Path;

pub fn run_checks(_spec: &TaskSpec, _task_dir: &Path, _crate_dir: &Path) -> Vec<CheckOutcome> {
    vec![CheckOutcome {
        name: "build".into(),
        passed: false,
        detail: "acceptance checks are not implemented in this checkout".into(),
    }]
}
