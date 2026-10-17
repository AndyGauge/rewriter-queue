use crate::manager::Manager;
use std::collections::HashMap;

const LONG_FILE_LINES: usize = 500;

/// Observations about the shape of a crate that are worth acting on once it works. Never a
/// reason to reject code.
pub(crate) fn size_advice(sections: &HashMap<String, String>) -> Option<String> {
    let mut long: Vec<(usize, &str)> = sections
        .iter()
        .filter(|(path, _)| path.ends_with(".rs"))
        .map(|(path, content)| (content.lines().count(), path.as_str()))
        .filter(|(lines, _)| *lines > LONG_FILE_LINES)
        .collect();
    if long.is_empty() {
        return None;
    }
    long.sort_by(|a, b| b.cmp(a));
    let list: Vec<String> = long.iter().map(|(lines, path)| format!("{path} ({lines} lines)")).collect();
    Some(format!(
        "These files are long enough that a reader has to hold a lot in their head at once:\n{}\n\n\
         Split them along the seams the code already has (one concern per module) where that \
         makes it easier to read and change. Move code; do not change behaviour.",
        list.join("\n")
    ))
}

impl<'a> Manager<'a> {
    /// The optimisation step: with a crate that already builds and passes its tests, ask for
    /// one refactor aimed at the advice above. The result replaces the working crate only if it
    /// passes the same gate; otherwise the working crate is kept.
    pub(crate) fn refactor_pass(&self, code: String, contract: &str, schema: &str, worker_system: &str) -> String {
        let Some(advice) = size_advice(&Self::parse_file_sections(&code)) else {
            return code;
        };
        eprintln!("  [refactor] the crate works; refactoring for readability\n{advice}");
        let task = format!(
            "# ObjectiveContract\n{contract}\n\n# Schema\n```rust\n{schema}\n```\n\n\
             The crate below builds and passes its tests. Refactor it for readability and \
             maintainability. Behaviour, public items and tests must stay exactly as they are."
        );
        let raw = self.ws.checkpoint("refactor/0", || {
            self.try_patch("TargetImplementer-Refactor", worker_system, &task, &code, &advice, "refactor", None)
        });
        let Ok(raw) = raw else {
            eprintln!("  [refactor] the refactor attempt failed; keeping the working crate");
            return code;
        };
        let (clean, errors) = self.integration_gate(&Self::format_multi_file(&Self::parse_file_sections(&raw)));
        if errors.is_empty() {
            clean
        } else {
            eprintln!("  [refactor] the refactored crate did not pass the gate; keeping the working crate");
            let _ = self.ws.log_deviation("TargetImplementer-Refactor", &format!("Refactor discarded, it failed the gate:\n{errors}"));
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(lines: usize) -> String {
        "fn f() {}\n".repeat(lines)
    }

    #[test]
    fn long_files_are_listed_longest_first() {
        let mut s = HashMap::new();
        s.insert("src/a.rs".to_string(), file(600));
        s.insert("src/b.rs".to_string(), file(900));
        s.insert("src/c.rs".to_string(), file(10));
        let advice = size_advice(&s).unwrap();
        assert!(advice.find("src/b.rs (900").unwrap() < advice.find("src/a.rs (600").unwrap());
        assert!(!advice.contains("src/c.rs"));
    }

    #[test]
    fn a_crate_of_short_files_has_nothing_to_refactor() {
        let mut s = HashMap::new();
        s.insert("src/a.rs".to_string(), file(500));
        s.insert("Cargo.toml".to_string(), file(900));
        assert!(size_advice(&s).is_none());
    }
}
