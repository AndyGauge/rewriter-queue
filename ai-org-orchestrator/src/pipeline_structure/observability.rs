use super::*;
use crate::manager::Manager;
use std::collections::HashMap;

fn run_one_milestone(ws: &Workspace) -> HashMap<String, String> {
    let (registry, _) = scripted(|call| match call.role {
        "planner" => Ok(plan(&[("only", "EASY")])),
        "plan-reviewer" => Ok("APPROVED".into()),
        "security" => Ok("COMPLIANT".into()),
        "maintenance" => Ok("APPROVE".into()),
        _ => Ok(GOOD_MAIN.into()),
    });
    Manager::new(ws, &registry)
        .implement_milestones("root", "task", WORKER, "contract", "schema", 3, 0, None, &NullToolbox, false)
        .expect("the run must succeed whatever the observer can or cannot do")
}

#[test]
fn an_unwritable_events_file_does_not_fail_the_run() {
    let dir = crate_dir("obs-unwritable");
    let ws = ws_at(&dir, false);
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, "a file where a directory is needed").unwrap();
    let ws = ws.with_observer(Observer::new(blocker.join("events.jsonl"), None, None));

    let sections = run_one_milestone(&ws);

    assert!(sections["src/main.rs"].contains("println!"));
    assert!(!blocker.join("events.jsonl").exists());
}

#[test]
fn an_unreachable_queue_server_does_not_fail_the_run_and_events_are_still_saved() {
    let dir = crate_dir("obs-unreachable");
    let ws = ws_at(&dir, false);
    let events = dir.join("events.jsonl");
    let observer = Observer::new(&events, Some(7), Some(("http://127.0.0.1:1".into(), "token".into())));
    let ws = ws.with_observer(observer);

    let sections = run_one_milestone(&ws);

    assert!(sections["src/main.rs"].contains("println!"));
    let saved = read_events(&events);
    assert!(
        saved.iter().any(|e| e.kind == run_events::EventKind::AgentCall),
        "the local events file must still record the run while the queue is down"
    );
    assert!(saved.iter().all(|e| e.job_id == Some(7)));
}

#[test]
fn a_run_with_a_healthy_observer_records_each_model_call_as_an_event() {
    let dir = crate_dir("obs-healthy");
    let (ws, events) = observed(ws_at(&dir, false), &dir);
    let (registry, log) = scripted(|call| match call.role {
        "planner" => Ok(plan(&[("only", "EASY")])),
        "plan-reviewer" => Ok("APPROVED".into()),
        "security" => Ok("COMPLIANT".into()),
        "maintenance" => Ok("APPROVE".into()),
        _ => Ok(GOOD_MAIN.into()),
    });
    Manager::new(&ws, &registry)
        .implement_milestones("root", "task", WORKER, "contract", "schema", 3, 0, None, &NullToolbox, false)
        .unwrap();

    let calls = read_events(&events).into_iter().filter(|e| e.kind == run_events::EventKind::AgentCall).count();
    assert_eq!(calls, log.lock().unwrap().len(), "every model call must be reported exactly once");
}
