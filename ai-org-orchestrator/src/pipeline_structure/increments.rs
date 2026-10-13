use super::*;
use crate::manager::Manager;
use std::sync::atomic::{AtomicUsize, Ordering};

const MISSION: &str = "Rewrite the orders database: customers, quotes, orders, payments.";
const TWO_INCREMENTS: &str = "## INCREMENT: core\nCustomers and quotes.\n\n## INCREMENT: orders\nOrders and payments.\n";

fn seeded_workspace(tag: &str) -> Workspace {
    let ws = ws_at(&crate_dir(tag), false);
    ws.emit_artifact("mission.md", MISSION).unwrap();
    ws.emit_artifact("objective_contract.md", "THE CONTRACT").unwrap();
    ws.emit_artifact("schema.rs", "pub struct Customer;").unwrap();
    ws
}

#[test]
fn the_planner_is_shown_the_mission_the_schema_and_how_far_over_budget_it_is() {
    let ws = seeded_workspace("increments-prompt");
    let (registry, log) = scripted(|_| Ok(TWO_INCREMENTS.into()));
    let mgr = Manager::new(&ws, &registry);

    let plan = mgr.plan_increments(764).expect("a usable plan");

    assert_eq!(plan.len(), 2);
    assert_eq!(plan[0].slug, "core");
    let calls = calls_of(&log, "increment-planner");
    assert_eq!(calls.len(), 1);
    for expected in [MISSION, "THE CONTRACT", "pub struct Customer;", "764 lines of Rust against a budget of 500"] {
        assert!(calls[0].user.contains(expected), "planner prompt lacks {expected:?}:\n{}", calls[0].user);
    }
}

#[test]
fn a_single_increment_is_not_a_split_and_the_planner_is_asked_again() {
    let ws = seeded_workspace("increments-retry");
    let attempts = AtomicUsize::new(0);
    let (registry, log) = scripted(move |_| {
        Ok(if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            "## INCREMENT: everything\nAll of it.\n".into()
        } else {
            TWO_INCREMENTS.into()
        })
    });
    let mgr = Manager::new(&ws, &registry);

    assert_eq!(mgr.plan_increments(764).unwrap().len(), 2);
    let calls = calls_of(&log, "increment-planner");
    assert_eq!(calls.len(), 2);
    assert!(calls[1].user.contains("contained 1 increment(s)"), "{}", calls[1].user);
}

#[test]
fn a_planner_that_never_splits_the_work_is_an_error() {
    let ws = seeded_workspace("increments-fail");
    let (registry, _) = scripted(|_| Ok("I think it is fine as one piece.".into()));
    let mgr = Manager::new(&ws, &registry);

    let err = mgr.plan_increments(764).unwrap_err().to_string();
    assert!(err.contains("could not produce a usable increment plan"), "{err}");
}
