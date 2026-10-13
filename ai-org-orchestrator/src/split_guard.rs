use crate::manager::Manager;
use std::collections::BTreeSet;

/// The distinct `error...` lines of a quality-gate report, ignoring paths and line numbers.
pub(crate) fn error_signature(report: &str) -> BTreeSet<String> {
    report
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("error"))
        .map(str::to_string)
        .collect()
}

/// A split that leaves the failure essentially unchanged was not a size problem: at least
/// half of the child's errors are ones its parent already failed on.
pub(crate) fn failure_unchanged(parent: &BTreeSet<String>, child: &BTreeSet<String>) -> bool {
    !child.is_empty() && child.iter().filter(|e| parent.contains(*e)).count() * 2 >= child.len()
}

impl<'a> Manager<'a> {
    pub(crate) fn record_gate_errors(&self, id: &str, report: &str) {
        self.gate_signatures
            .lock()
            .unwrap()
            .insert(id.to_string(), error_signature(report));
    }

    /// True when `id` is a sub-milestone whose last failure matches its parent's, so
    /// splitting it again cannot help.
    pub(crate) fn split_cannot_help(&self, id: &str) -> bool {
        let Some((parent_id, _)) = id.rsplit_once('/') else { return false };
        let signatures = self.gate_signatures.lock().unwrap();
        match (signatures.get(parent_id), signatures.get(id)) {
            (Some(parent), Some(child)) => failure_unchanged(parent, child),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(report: &str) -> BTreeSet<String> {
        error_signature(report)
    }

    #[test]
    fn signature_keeps_only_error_lines_without_locations() {
        let s = sig("## cargo build\n```\nerror[E0119]: conflicting impls\n --> src/lib.rs:6:79\nwarning: unused import\n```");
        assert_eq!(s.len(), 1);
        assert!(s.contains("error[E0119]: conflicting impls"));
    }

    #[test]
    fn same_errors_after_a_split_mean_the_split_did_not_help() {
        let parent = sig("error[E0119]: a\nerror[E0308]: b");
        assert!(failure_unchanged(&parent, &sig("error[E0119]: a")));
    }

    #[test]
    fn different_errors_after_a_split_mean_progress() {
        let parent = sig("error[E0119]: a\nerror[E0308]: b");
        assert!(!failure_unchanged(&parent, &sig("error[E0425]: c\nerror[E0599]: d")));
    }

    #[test]
    fn an_empty_child_failure_is_never_unchanged() {
        assert!(!failure_unchanged(&sig("error: a"), &BTreeSet::new()));
    }
}
