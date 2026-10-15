/// What the LLM reviewers are shown for one milestone. The code alone makes a reviewer judge
/// the whole contract: every `todo!()` stub that belongs to another milestone reads as a
/// missing feature in this one (job #33: 16 milestones rejected for that, none converged).
pub(crate) fn review_input(clean: &str, scope: Option<&str>) -> String {
    match scope {
        Some(scope) => format!(
            "# Scope of this milestone\nReview only the work described here. Functions and \
             modules owned by other milestones are expected to be stubs (`todo!()`) at this \
             point; do not count them against this milestone.\n\n{scope}\n\n\
             Review this output:\n\n{clean}"
        ),
        None => format!("Review this output:\n\n{clean}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_scope_the_input_is_just_the_code() {
        assert_eq!(review_input("fn a() {}", None), "Review this output:\n\nfn a() {}");
    }

    #[test]
    fn a_scope_comes_before_the_code_and_excuses_other_milestones_stubs() {
        let input = review_input("fn a() {}", Some("# Milestone: readers"));
        let scope_at = input.find("# Milestone: readers").unwrap();
        let code_at = input.find("fn a() {}").unwrap();
        assert!(scope_at < code_at);
        assert!(input.contains("other milestones"));
        assert!(input.contains("todo!()"));
    }
}
