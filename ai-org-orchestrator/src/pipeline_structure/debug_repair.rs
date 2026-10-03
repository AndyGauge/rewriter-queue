use super::*;
use crate::manager::Manager;
use std::collections::HashMap;

const DIAGNOSIS: &str = "ROOT-CAUSE-MARKER: undefined_fn is never defined";
const DIAGNOSIS_HEADING: &str = "# Diagnosis Of Why Earlier Repairs Failed";

fn broken_sections() -> HashMap<String, String> {
    Manager::parse_file_sections(BROKEN_MAIN)
}

fn repair_attempt(user: &str) -> Option<usize> {
    let rest = user.split("integration attempt ").nth(1)?;
    rest.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
}

/// Repairs succeed only from `fixed_from` onward; earlier integration repairs return still-broken
/// code. Everything the reviewers are asked is approved.
fn repair_script(
    fixed_from: usize,
    debugger: fn() -> Result<String, ProviderError>,
    escalation_plan: Option<String>,
) -> (Registry, Log) {
    scripted(move |call| match call.role {
        "debugger" => debugger(),
        "worker" if call.user.contains("# Milestone:") => Ok(GOOD_MAIN.into()),
        "worker" => match repair_attempt(&call.user) {
            Some(n) if n >= fixed_from => Ok(GOOD_MAIN.into()),
            _ => Ok(BROKEN_MAIN.into()),
        },
        "planner" => Ok(escalation_plan.clone().unwrap_or_else(|| "NONE".into())),
        "plan-reviewer" => Ok("APPROVED".into()),
        "security" => Ok("COMPLIANT".into()),
        "maintenance" => Ok("APPROVE".into()),
        other => panic!("unexpected agent {other}"),
    })
}

fn diagnoses() -> Result<String, ProviderError> {
    Ok(DIAGNOSIS.into())
}

fn debugger_down() -> Result<String, ProviderError> {
    Err(ProviderError::BadResponse("debugger unavailable".into()))
}

fn index_of(log: &Log, pred: impl Fn(&Call) -> bool) -> Option<usize> {
    log.lock().unwrap().iter().position(pred)
}

const NO_ESCALATION_LEFT: usize = 2;

#[test]
fn the_first_failed_repair_does_not_consult_the_debugger_but_later_ones_do_and_the_diagnosis_reaches_the_repair() {
    let ws = ws_at(&crate_dir("debug-from-second"), true);
    let (registry, log) = repair_script(2, diagnoses, None);
    let mgr = Manager::new(&ws, &registry);

    let fixed = mgr
        .integration_check_and_repair(broken_sections(), "contract", "schema", WORKER, 3, &NullToolbox, NO_ESCALATION_LEFT)
        .expect("the third repair attempt fixes the crate");
    assert!(fixed["src/main.rs"].contains("println!"));

    let first_repair = index_of(&log, |c| c.role == "worker").expect("a repair call");
    let first_debugger = index_of(&log, |c| c.role == "debugger").expect("the debugger must be consulted from the second failed attempt");
    assert!(first_repair < first_debugger, "the debugger must not run before the first repair");
    assert_eq!(calls_of(&log, "debugger").len(), 2, "attempts 1 and 2 are diagnosed, attempt 0 is not");

    let repairs = calls_of(&log, "worker");
    let attempt_0: Vec<_> = repairs.iter().filter(|c| repair_attempt(&c.user) == Some(0)).collect();
    assert!(!attempt_0.is_empty());
    assert!(
        attempt_0.iter().all(|c| !c.user.contains(DIAGNOSIS_HEADING)),
        "the first repair must be attempted without a diagnosis"
    );
    for n in [1, 2] {
        let attempt: Vec<_> = repairs.iter().filter(|c| repair_attempt(&c.user) == Some(n)).collect();
        assert!(!attempt.is_empty(), "attempt {n} must have repaired");
        assert!(
            attempt.iter().all(|c| c.user.contains(DIAGNOSIS_HEADING) && c.user.contains(DIAGNOSIS)),
            "the diagnosis must reach repair attempt {n}"
        );
    }
    assert!(
        calls_of(&log, "debugger")[0].user.contains("undefined_fn"),
        "the debugger is shown the failing crate and its gate output"
    );
    assert!(calls_of(&log, "planner").is_empty(), "no escalation while the repair budget suffices");
}

#[test]
fn a_failing_debugger_call_does_not_stop_the_repair_loop() {
    let ws = ws_at(&crate_dir("debug-down"), true);
    let (registry, log) = repair_script(2, debugger_down, None);
    let mgr = Manager::new(&ws, &registry);

    let fixed = mgr
        .integration_check_and_repair(broken_sections(), "contract", "schema", WORKER, 3, &NullToolbox, NO_ESCALATION_LEFT)
        .expect("a missing diagnosis must leave the repair loop running");

    assert!(fixed["src/main.rs"].contains("println!"));
    assert!(!calls_of(&log, "debugger").is_empty(), "the debugger must have been tried");
    assert!(
        calls_of(&log, "worker").iter().all(|c| !c.user.contains(DIAGNOSIS_HEADING)),
        "with no diagnosis the repair prompt carries no diagnosis section"
    );
}

#[test]
fn escalation_to_the_milestone_planner_happens_only_after_the_repair_budget_and_carries_the_diagnosis() {
    let dir = crate_dir("escalation");
    let (ws, events) = observed(ws_at(&dir, true), &dir);
    let (registry, log) = repair_script(usize::MAX, diagnoses, Some(plan(&[("fix-it", "EASY")])));
    let mgr = Manager::new(&ws, &registry);

    let fixed = mgr
        .integration_check_and_repair(broken_sections(), "contract", "schema", WORKER, 2, &NullToolbox, 1)
        .expect("the escalated fix plan repairs the crate");
    assert!(fixed["src/main.rs"].contains("println!"));

    let planner_at = index_of(&log, |c| c.role == "planner" && c.user.contains("Repeated Patch Attempts Could Not Fix"))
        .expect("the integration failure must escalate to the milestone planner");
    let last_repair = {
        let calls = log.lock().unwrap();
        calls.iter().rposition(|c| c.role == "worker" && repair_attempt(&c.user).is_some()).expect("repairs")
    };
    assert!(last_repair < planner_at, "escalation must come after every repair attempt, not before");
    let attempts_seen: std::collections::BTreeSet<_> = log
        .lock()
        .unwrap()
        .iter()
        .take(planner_at)
        .filter_map(|c| repair_attempt(&c.user))
        .collect();
    assert_eq!(attempts_seen.len(), 2, "the whole repair budget (2 attempts) is spent first: {attempts_seen:?}");

    let planner = log.lock().unwrap()[planner_at].clone();
    assert!(planner.user.contains(DIAGNOSIS), "the escalation prompt must contain the debugger's diagnosis");
    assert!(planner.user.contains(DIAGNOSIS_HEADING));

    let escalated = read_events(&events)
        .into_iter()
        .any(|e| e.agent == "MilestonePlanner" && e.summary.contains("integration escalation"));
    assert!(escalated, "the escalation must be visible in the event stream as a retry");

    let marked = read_events(&events)
        .into_iter()
        .filter(|e| e.kind == run_events::EventKind::GateResult)
        .filter(|e| e.detail.as_ref().and_then(|d| d.get("integration")).is_some())
        .count();
    assert!(marked >= 3, "each integration gate run must be marked as such, found {marked}");
}
