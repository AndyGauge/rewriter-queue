use super::*;
use crate::manager::Manager;
use std::collections::HashMap;

const SIMPLE_WORKER_REPLY: &str = "fn main() {}\n";

fn run_plan(
    tag: &str,
    reviewer_reply: Result<String, ProviderError>,
    original_plan: String,
) -> (Result<HashMap<String, String>, String>, Log, PathBuf) {
    let dir = crate_dir(tag);
    let (ws, events) = observed(ws_at(&dir, false), &dir);
    let reviewer_reply = Mutex::new(Some(reviewer_reply));
    let (registry, log) = scripted(move |call| match call.role {
        "planner" => Ok(original_plan.clone()),
        "plan-reviewer" => reviewer_reply.lock().unwrap().take().expect("one review call"),
        "security" => Ok("COMPLIANT".into()),
        "maintenance" => Ok("APPROVE".into()),
        _ => Ok(SIMPLE_WORKER_REPLY.into()),
    });
    let mgr = Manager::new(&ws, &registry);
    let result = mgr
        .implement_milestones("root", "task", WORKER, "contract", "schema", 3, 0, None, &NullToolbox, false)
        .map_err(|e| e.to_string());
    (result, log, events)
}

fn implemented(log: &Log) -> Vec<String> {
    calls_of(log, "worker")
        .into_iter()
        .filter_map(|c| {
            let rest = c.user.split("# Milestone: ").nth(1)?;
            Some(rest.lines().next().unwrap_or("").to_string())
        })
        .collect()
}

#[test]
fn plan_review_runs_before_implementation_and_its_corrected_plan_is_what_gets_implemented() {
    let corrected = format!("## FINDINGS\n- alpha is redundant, merged into beta\n\n{}", plan(&[("beta", "EASY")]));
    let (result, log, events) = run_plan("review-revises", Ok(corrected), plan(&[("alpha", "EASY")]));
    result.expect("run");

    let order = roles(&log);
    let review_at = order.iter().position(|r| *r == "plan-reviewer").expect("the plan reviewer must be called");
    let first_worker = order.iter().position(|r| *r == "worker").expect("something must be implemented");
    assert!(review_at < first_worker, "plan review must precede implementation, got {order:?}");
    assert_eq!(implemented(&log), vec!["beta"], "only the corrected plan may be implemented");

    let decided = read_events(&events)
        .into_iter()
        .any(|e| e.agent == "PlanReviewer" && e.summary.contains("Revised the plan"));
    assert!(decided, "a revised plan must be recorded as a PlanReviewer decision event");
}

#[test]
fn an_approved_review_keeps_the_original_plan() {
    let (result, log, _) = run_plan("review-approves", Ok("APPROVED".into()), plan(&[("alpha", "EASY")]));
    result.expect("run");

    assert_eq!(calls_of(&log, "plan-reviewer").len(), 1);
    assert_eq!(implemented(&log), vec!["alpha"]);
}

#[test]
fn a_review_call_that_errors_falls_back_to_the_original_plan_and_the_run_continues() {
    let (result, log, _) = run_plan(
        "review-errors",
        Err(ProviderError::BadResponse("reviewer exploded".into())),
        plan(&[("alpha", "EASY")]),
    );
    result.expect("a failed review must not fail the run");

    assert_eq!(calls_of(&log, "plan-reviewer").len(), 1, "the review must have been attempted");
    assert_eq!(implemented(&log), vec!["alpha"]);
}

#[test]
fn a_nested_plan_review_is_given_the_plans_above_it() {
    let dir = crate_dir("review-nested");
    let ws = ws_at(&dir, false);
    let (registry, log) = scripted(|call| match call.role {
        "planner" if call.user.starts_with("build draw") => Ok(plan(&[("draw-circle", "EASY")])),
        "planner" => Ok(plan(&[("draw", "HARD"), ("parse-args", "EASY")])),
        "plan-reviewer" => Ok("APPROVED".into()),
        "security" => Ok("COMPLIANT".into()),
        "maintenance" => Ok("APPROVE".into()),
        _ => Ok(SIMPLE_WORKER_REPLY.into()),
    });
    let mgr = Manager::new(&ws, &registry);
    mgr.implement_milestones("root", "task", WORKER, "contract", "schema", 3, 0, None, &NullToolbox, false)
        .expect("run");

    let reviews = calls_of(&log, "plan-reviewer");
    assert_eq!(reviews.len(), 2, "one review for the root plan and one for the nested plan");
    assert!(
        !reviews[0].user.contains("# Plans Above This One"),
        "the top-level plan has nothing above it"
    );
    let nested = &reviews[1].user;
    assert!(nested.contains("# Plans Above This One"), "nested review prompt:\n{nested}");
    assert!(nested.contains("## root"));
    assert!(nested.contains("parse-args"), "the sibling milestone must be visible so it is not re-added");
    assert!(nested.contains("# Plan To Review"));
}
