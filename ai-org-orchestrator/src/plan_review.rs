use crate::agents::PLAN_REVIEWER;
use crate::manager::Manager;

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
        let task = format!("{plan_task}\n\n# Plan To Review\n{plan_text}");
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
    use super::parse_review;

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
