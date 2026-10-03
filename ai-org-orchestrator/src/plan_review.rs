use crate::agents::PLAN_REVIEWER;
use crate::manager::Manager;
use crate::workspace::Workspace;

const MILESTONE_MARKER: &str = "## MILESTONE:";

/// Split a PlanReviewer response into its findings and the corrected plan. `None` when the
/// reviewer approved the plan, or produced no parseable milestones to replace it with.
fn parse_review(response: &str) -> Option<(String, String)> {
    let trimmed = response.trim();
    if trimmed.eq_ignore_ascii_case("APPROVED") {
        return None;
    }
    let at = trimmed.find(MILESTONE_MARKER)?;
    let findings = trimmed[..at]
        .trim()
        .trim_start_matches("## FINDINGS")
        .trim()
        .to_string();
    Some((findings, trimmed[at..].to_string()))
}

/// Plan ids above `id_prefix`, outermost first: `root/a/b` has ancestors `root` and `root/a`.
fn ancestor_prefixes(id_prefix: &str) -> Vec<String> {
    let parts: Vec<&str> = id_prefix.split('/').collect();
    (1..parts.len()).map(|n| parts[..n].join("/")).collect()
}

/// The plan that was actually implemented at one level: the reviewer's correction when it made
/// one, otherwise the planner's original.
fn effective_plan(plan: &str, review: &str) -> String {
    match parse_review(review) {
        Some((_, revised)) => revised,
        None => plan.to_string(),
    }
}

/// The plans above `id_prefix`, as context for reviewing a nested plan. A nested plan splits one
/// milestone of its parent, so without this the reviewer sees only that milestone and will
/// "fill gaps" by adding work the parent's other milestones already own.
fn plan_context(ws: &Workspace, id_prefix: &str) -> String {
    let sections: Vec<String> = ancestor_prefixes(id_prefix)
        .iter()
        .filter_map(|a| {
            let plan = ws.get_artifact(&format!("synthesis/{a}/plan.md")).ok()?;
            let review = ws.get_artifact(&format!("synthesis/{a}/plan-review.md")).unwrap_or_default();
            Some(format!("## {a}\n{}", effective_plan(&plan, &review)))
        })
        .collect();
    if sections.is_empty() {
        return String::new();
    }
    format!(
        "# Plans Above This One\nThis plan splits one milestone of the plan(s) below. Every \
         other milestone in them is separate work with its own implementation step: do not add \
         milestones that duplicate them, do not report them as missing, and keep this plan \
         inside the milestone being split.\n\n{}\n\n",
        sections.join("\n\n")
    )
}

impl<'a> Manager<'a> {
    /// Have PlanReviewer check a freshly produced milestone plan before anything is
    /// implemented, and return the plan to use: the corrected one when the reviewer found
    /// contradictions, redundant steps or gaps, otherwise the original. A review that errors
    /// or returns no usable plan falls back to the original rather than failing the run.
    pub(crate) fn review_plan(
        &self,
        id_prefix: &str,
        plan_task: &str,
        plan_text: &str,
        toolbox: &(dyn crate::tools::Toolbox + Sync),
    ) -> Result<String, Box<dyn std::error::Error>> {
        if !plan_text.contains(MILESTONE_MARKER) {
            return Ok(plan_text.to_string());
        }
        let context = plan_context(self.ws, id_prefix);
        let task = format!("{plan_task}\n\n{context}# Plan To Review\n{plan_text}");
        let response = match self.ws.checkpoint(&format!("{id_prefix}/plan-review"), || {
            self.run_agentic(
                "PlanReviewer",
                PLAN_REVIEWER,
                &task,
                toolbox,
                crate::manager::AGENTIC_MAX_TURNS,
            )
        }) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  [plan-review] '{id_prefix}': review failed ({e}) — using the plan as written");
                return Ok(plan_text.to_string());
            }
        };

        let Some((findings, revised)) = parse_review(&response) else {
            eprintln!("  [plan-review] '{id_prefix}': plan approved as written");
            return Ok(plan_text.to_string());
        };

        eprintln!("  [plan-review] '{id_prefix}': plan revised before implementation:");
        for line in findings.lines().filter(|l| !l.trim().is_empty()) {
            eprintln!("    {line}");
        }
        self.ws.log_agent_decision(
            "PlanReviewer",
            &format!("Revised the plan for '{id_prefix}':\n{findings}"),
        )?;
        Ok(revised)
    }
}

#[cfg(test)]
mod tests {
    use super::{ancestor_prefixes, effective_plan, parse_review, plan_context};
    use crate::workspace::Workspace;

    fn temp_ws(tag: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("aoo-plan-review-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Workspace::new(dir).unwrap()
    }

    #[test]
    fn ancestors_are_listed_outermost_first() {
        assert_eq!(ancestor_prefixes("root/a/b"), vec!["root", "root/a"]);
        assert!(ancestor_prefixes("root").is_empty());
    }

    #[test]
    fn the_effective_plan_is_the_revision_when_there_is_one() {
        let review = "## FINDINGS\n- merged\n\n## MILESTONE: ab\nTASK: x";
        assert!(effective_plan("## MILESTONE: a\n", review).starts_with("## MILESTONE: ab"));
        assert_eq!(effective_plan("## MILESTONE: a\n", "APPROVED"), "## MILESTONE: a\n");
    }

    #[test]
    fn a_nested_plan_is_given_the_plans_above_it() {
        let ws = temp_ws("context");
        ws.emit_artifact("synthesis/root/plan.md", "## MILESTONE: draw\n## MILESTONE: parse-args\n").unwrap();
        ws.emit_artifact("synthesis/root/plan-review.md", "APPROVED").unwrap();
        let ctx = plan_context(&ws, "root/draw");
        assert!(ctx.contains("## root"));
        assert!(ctx.contains("parse-args"));
        assert!(ctx.contains("do not add milestones that duplicate them"));
    }

    #[test]
    fn a_top_level_plan_has_no_context() {
        let ws = temp_ws("top");
        ws.emit_artifact("synthesis/root/plan.md", "## MILESTONE: draw\n").unwrap();
        assert!(plan_context(&ws, "root").is_empty());
    }

    #[test]
    fn approved_keeps_the_original_plan() {
        assert!(parse_review("  APPROVED\n").is_none());
    }

    #[test]
    fn a_response_with_no_milestones_keeps_the_original_plan() {
        assert!(parse_review("## FINDINGS\n- something is wrong").is_none());
    }

    #[test]
    fn findings_are_separated_from_the_corrected_plan() {
        let (findings, plan) = parse_review(
            "## FINDINGS\n- merged a and b\n\n## MILESTONE: ab\nRISK: EASY\nTASK: do it",
        )
        .unwrap();
        assert_eq!(findings, "- merged a and b");
        assert!(plan.starts_with("## MILESTONE: ab"));
    }
}
