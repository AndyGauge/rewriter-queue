use crate::agents::{DEBUG_IMPLEMENTER, IMPLEMENTER_PATTERNS};
use crate::manager::Manager;

/// `base` with the diagnosis appended as the thing the repair must follow; `base` unchanged
/// when there is no diagnosis, so a failed diagnosis never blocks a repair.
pub(crate) fn with_diagnosis(base: &str, diagnosis: &str) -> String {
    if diagnosis.trim().is_empty() {
        return base.to_string();
    }
    format!(
        "{base}\n\n# Diagnosis Of Why Earlier Repairs Failed\n{diagnosis}\n\n\
         Make the change the diagnosis prescribes and nothing else. If you disagree with it, \
         check the quality-gate output below against it first."
    )
}

impl<'a> Manager<'a> {
    /// Ask DebugImplementer why `code` keeps failing its quality gate. Returns an empty string
    /// when the call fails, since a missing diagnosis should leave the repair loop exactly as
    /// it was rather than stop it.
    pub(crate) fn diagnose_failure(
        &self,
        id: &str,
        contract: &str,
        code: &str,
        errors: &str,
    ) -> String {
        let pitfalls = IMPLEMENTER_PATTERNS
            .iter()
            .map(|p| p.content)
            .collect::<Vec<_>>()
            .join("\n\n");
        let system = format!("{DEBUG_IMPLEMENTER}\n\n# Known Pitfalls\n{pitfalls}");
        let task = format!(
            "# ObjectiveContract\n{contract}\n\n# Current Crate\n```rust\n{code}\n```\n\n\
             # Quality Gate Output\n{errors}"
        );
        match self.ws.checkpoint(id, || self.run("DebugImplementer", &system, &task)) {
            Ok(diagnosis) => diagnosis,
            Err(e) => {
                eprintln!("    [debug] '{id}': diagnosis failed ({e}) — repairing without one");
                String::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::with_diagnosis;

    #[test]
    fn an_empty_diagnosis_leaves_the_task_unchanged() {
        assert_eq!(with_diagnosis("task", "  \n"), "task");
    }

    #[test]
    fn a_diagnosis_is_appended_to_the_task() {
        let out = with_diagnosis("task", "ROOT CAUSE: x");
        assert!(out.starts_with("task"));
        assert!(out.contains("ROOT CAUSE: x"));
    }
}
