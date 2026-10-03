use super::*;
use crate::manager::Manager;

/// Upper bound on model calls for one easy milestone taken from plan to production review. The
/// pipeline currently makes 9: plan, plan review, implement, security, maintenance, scope-gap
/// check, IP counsel, soundness and production readiness. It is deliberately a little above
/// that, not equal, so a retry-free happy path has no slack for an extra agent per step.
const MAX_CALLS_HAPPY_PATH: usize = 10;

#[test]
fn a_one_milestone_happy_path_stays_within_its_model_call_budget() {
    let dir = crate_dir("budget");
    let (ws, events) = observed(ws_at(&dir, true), &dir);
    let (registry, log) = scripted(|call| match call.role {
        "planner" if call.user.contains("# Already Implemented") => Ok("NONE".into()),
        "planner" => Ok(plan(&[("core", "EASY")])),
        "plan-reviewer" => Ok("APPROVED".into()),
        "worker" => Ok(GOOD_MAIN.into()),
        "security" => Ok("COMPLIANT".into()),
        "maintenance" => Ok("APPROVE".into()),
        "ip-counsel" => Ok("CLEAR".into()),
        "formal-methods" => Ok("SOUND".into()),
        "production-readiness" => Ok("READY".into()),
        other => panic!("unexpected agent {other} on the happy path"),
    });
    let mgr = Manager::new(&ws, &registry);

    let code = mgr
        .synthesize(WORKER, "contract", "schema", "fn main() {}\n", "matrix", "inductive", "", 3, &NullToolbox)
        .expect("happy path");
    assert!(code.contains("println!"));

    let used = roles(&log);
    assert!(
        used.len() <= MAX_CALLS_HAPPY_PATH,
        "the one-milestone happy path used {} model calls ({used:?}), over the budget of \
         {MAX_CALLS_HAPPY_PATH}. If the extra call is intended, raise MAX_CALLS_HAPPY_PATH in \
         pipeline_structure/budget.rs and say why in the commit message; otherwise an agent was \
         added to a step that did not have one.",
        used.len(),
    );

    let reported = read_events(&events).into_iter().filter(|e| e.kind == run_events::EventKind::AgentCall).count();
    assert_eq!(reported, used.len(), "events and provider calls must agree");
    assert_eq!(mgr.total_input_tokens() as usize, 10 * used.len());
}
