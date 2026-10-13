use super::*;

fn temp_queue(tag: &str) -> Queue {
    let dir = std::env::temp_dir().join(format!("rq-chain-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    Queue::at(dir).unwrap()
}

/// A finished job whose workspace holds `files` (relative path, content).
fn finished_job(q: &Queue, tag: &str, files: &[(&str, &str)]) -> Job {
    let root = q.new_work_dir().unwrap();
    let workspace = root.join("ws");
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("Orders.bas"), "Sub A()").unwrap();
    for (rel, content) in files {
        let path = workspace.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let mut job = q
        .submit(Spec { name: Some(tag.into()), workspace, source, max_iter: Some(10), inherit_env: false })
        .unwrap();
    job.state = State::Succeeded;
    q.save(&job).unwrap();
    job
}

const HANDOFF: &str = r#"{"schema_lines":764,"line_budget":500,"increments":[
  {"slug":"core","mission":"Build the core."},
  {"slug":"orders","mission":"Add orders."},
  {"slug":"reports","mission":"Add reports."}]}"#;

#[test]
fn a_split_job_submits_the_first_increment_with_its_own_mission_and_the_original_source() {
    let q = temp_queue("first");
    let parent = finished_job(&q, "cairn", &[("artifacts/increments.json", HANDOFF)]);

    follow_up(&q, parent.id).unwrap();

    let jobs = q.list().unwrap();
    assert_eq!(jobs.len(), 2);
    let inc = &jobs[1];
    assert_eq!(inc.state, State::Queued);
    assert_eq!(inc.source, parent.source);
    assert_eq!(inc.max_iter, Some(10));
    assert_eq!(inc.name.as_deref(), Some("cairn · increment 1/3: core"));
    let mission = fs::read_to_string(inc.workspace.join("artifacts/mission.md")).unwrap();
    assert!(mission.contains("increment 1 of 3") && mission.contains("Build the core."), "{mission}");
    assert!(!mission.contains("v2-so-far"), "the first increment extends nothing");
    let chain: Chain = serde_json::from_slice(&fs::read(inc.workspace.join("chain.json")).unwrap()).unwrap();
    assert_eq!((chain.index, chain.total, chain.remaining.len()), (1, 3, 2));
    assert!(q.get(parent.id).unwrap().note.unwrap().contains("increment 1 is job #2"));
}

#[test]
fn a_succeeded_increment_submits_the_next_with_the_crate_it_built_added_to_the_source() {
    let q = temp_queue("next");
    let parent = finished_job(&q, "cairn", &[("artifacts/increments.json", HANDOFF)]);
    follow_up(&q, parent.id).unwrap();
    let first = q.list().unwrap().pop().unwrap();
    fs::create_dir_all(first.workspace.join("artifacts/v2/src")).unwrap();
    fs::create_dir_all(first.workspace.join("artifacts/v2/target/debug")).unwrap();
    fs::write(first.workspace.join("artifacts/v2/src/lib.rs"), "pub fn core() {}").unwrap();
    fs::write(first.workspace.join("artifacts/v2/target/debug/junk"), "x").unwrap();
    let mut done = q.get(first.id).unwrap();
    done.state = State::Succeeded;
    q.save(&done).unwrap();

    follow_up(&q, first.id).unwrap();

    let second = q.list().unwrap().pop().unwrap();
    assert_eq!(second.name.as_deref(), Some("cairn · increment 2/3: orders"));
    assert_eq!(fs::read_to_string(second.source.join("Orders.bas")).unwrap(), "Sub A()");
    assert_eq!(fs::read_to_string(second.source.join("v2-so-far/src/lib.rs")).unwrap(), "pub fn core() {}");
    assert!(!second.source.join("v2-so-far/target").exists(), "build output is not source");
    let mission = fs::read_to_string(second.workspace.join("artifacts/mission.md")).unwrap();
    assert!(mission.contains("v2-so-far") && mission.contains("Add orders."), "{mission}");
}

#[test]
fn the_last_increment_submits_nothing() {
    let q = temp_queue("last");
    let parent = finished_job(&q, "cairn", &[("artifacts/increments.json", HANDOFF)]);
    let mut current = parent.clone();
    for _ in 0..3 {
        follow_up(&q, current.id).unwrap();
        current = q.list().unwrap().pop().unwrap();
        fs::create_dir_all(current.workspace.join("artifacts/v2/src")).unwrap();
        current.state = State::Succeeded;
        q.save(&current).unwrap();
    }
    let before = q.list().unwrap().len();

    follow_up(&q, current.id).unwrap();

    assert_eq!(q.list().unwrap().len(), before);
    assert_eq!(before, 4, "the original job plus all three increments");
}

#[test]
fn follow_up_is_idempotent_and_ignores_failed_and_ordinary_jobs() {
    let q = temp_queue("idem");
    let split = finished_job(&q, "a", &[("artifacts/increments.json", HANDOFF)]);
    follow_up(&q, split.id).unwrap();
    follow_up(&q, split.id).unwrap();
    assert_eq!(q.list().unwrap().len(), 2, "a second call must not submit again");

    let ordinary = finished_job(&q, "b", &[("artifacts/v2/src/lib.rs", "")]);
    follow_up(&q, ordinary.id).unwrap();
    let mut failed = finished_job(&q, "c", &[("artifacts/increments.json", HANDOFF)]);
    failed.state = State::Failed;
    q.save(&failed).unwrap();
    follow_up(&q, failed.id).unwrap();
    assert_eq!(q.list().unwrap().len(), 4, "only the two jobs and one increment exist");
}
