use super::*;
use crate::agents::{CONTRACT_REVIEWER, MISSION_ARCHITECT};
use crate::manager::Manager;
use std::sync::atomic::{AtomicUsize, Ordering};

const MISSION: &str = "Port the rectangle printer and keep the trailing newline exactly.";

fn contract_task() -> String {
    format!("# Mission\n{MISSION}\n\nProduce the ObjectiveContract. V1 source has not been pasted in.")
}

fn run_contract_phase(tag: &str, verdicts: Vec<&'static str>) -> (String, Log) {
    let ws = ws_at(&crate_dir(tag), false);
    let verdicts = Mutex::new(verdicts.into_iter());
    let authored = AtomicUsize::new(0);
    let (registry, log) = scripted(move |call| match call.role {
        "mission-architect" => {
            let n = authored.fetch_add(1, Ordering::SeqCst);
            Ok(if n == 0 { "CONTRACT v1".into() } else { "CONTRACT v2".into() })
        }
        "contract-reviewer" => Ok(verdicts.lock().unwrap().next().expect("verdict").into()),
        other => panic!("unexpected agent {other}"),
    });
    let mgr = Manager::new(&ws, &registry);
    let out = mgr
        .run_with_review(
            "MissionArchitect",
            MISSION_ARCHITECT,
            "ContractReviewer",
            CONTRACT_REVIEWER,
            &contract_task(),
            "APPROVED",
            3,
            &NullToolbox,
        )
        .expect("contract phase");
    (out, log)
}

#[test]
fn the_contract_reviewer_is_given_the_mission() {
    let (out, log) = run_contract_phase("contract-approved", vec!["APPROVED"]);

    assert_eq!(out, "CONTRACT v1");
    let reviews = calls_of(&log, "contract-reviewer");
    assert_eq!(reviews.len(), 1);
    assert!(reviews[0].user.contains(MISSION), "reviewer prompt:\n{}", reviews[0].user);
    assert!(reviews[0].user.contains("CONTRACT v1"), "the reviewer must see the contract it reviews");
}

#[test]
fn a_rejected_contract_is_revised_with_the_reviewers_feedback_and_reviewed_again() {
    let (out, log) = run_contract_phase(
        "contract-rejected",
        vec!["REJECTED: the trailing newline requirement is missing", "APPROVED"],
    );

    assert_eq!(out, "CONTRACT v2", "the revision, not the rejected draft, must be returned");
    let reviews = calls_of(&log, "contract-reviewer");
    assert_eq!(reviews.len(), 2, "a revision must be reviewed again");
    assert!(reviews[1].user.contains("CONTRACT v2"));
    let revisions: Vec<_> = calls_of(&log, "mission-architect").into_iter().skip(1).collect();
    assert!(!revisions.is_empty(), "a rejection must trigger a revision call");
    assert!(
        revisions.iter().all(|c| c.user.contains("the trailing newline requirement is missing")),
        "the reviewer's reasons must reach the reviser"
    );
}

/// `main()` builds the contract task inline, so the behaviour above can only be pinned to it by
/// reading its source: this fails if the mission or the reviewer is dropped from that call.
#[test]
fn the_entry_point_hands_the_mission_to_the_contract_author_and_reviewer() {
    let main_rs = include_str!("../main.rs");
    let at = main_rs.find("let contract_task = format!(").expect("contract task construction");
    let task_def = &main_rs[at..at + 200];
    assert!(task_def.contains("# Mission\\n{mission}"), "the contract task must carry the mission:\n{task_def}");

    let call = &main_rs[main_rs.find(".run_with_review(").expect("run_with_review call")..];
    let call = &call[..call.find(".expect(\"contract phase failed\")").expect("end of call")];
    for needle in ["CONTRACT_REVIEWER", "&contract_task", "\"APPROVED\""] {
        assert!(call.contains(needle), "run_with_review in main.rs lost `{needle}`:\n{call}");
    }
}
