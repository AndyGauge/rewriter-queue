use super::*;
use run_events::parse_events;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aoo-observer-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn events_are_appended_as_parseable_lines_with_job_and_stage() {
    let dir = temp_dir("append");
    let obs = Observer::new(dir.join("events.jsonl"), Some(7), None);
    obs.stage("Schema");
    obs.decision("SchemaArchitect", "chose a flat module layout\nsecond line");
    obs.end_stage();

    let events = parse_events(&std::fs::read_to_string(dir.join("events.jsonl")).unwrap());
    let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::StageStart,
            EventKind::Decision,
            EventKind::StageEnd
        ]
    );
    assert!(events.iter().all(|e| e.job_id == Some(7)));
    assert!(events.iter().all(|e| e.stage.as_deref() == Some("Schema")));
    assert_eq!(events[1].summary, "chose a flat module layout");
}

#[test]
fn fail_records_an_error_then_ends_the_open_stage_as_not_ok() {
    let dir = temp_dir("fail");
    let obs = Observer::new(dir.join("events.jsonl"), None, None);
    obs.stage("Schema");
    obs.fail("orchestrator", "schema phase failed");
    obs.fail("orchestrator", "again");
    let events = parse_events(&std::fs::read_to_string(dir.join("events.jsonl")).unwrap());
    let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::StageStart,
            EventKind::Error,
            EventKind::StageEnd,
            EventKind::Error
        ]
    );
    assert_eq!(events[1].stage.as_deref(), Some("Schema"));
    assert_eq!(events[2].detail.as_ref().unwrap()["ok"], false);
}

#[test]
fn a_normally_ended_stage_is_marked_ok() {
    let dir = temp_dir("ok");
    let obs = Observer::new(dir.join("events.jsonl"), None, None);
    obs.stage("A");
    obs.end_stage();
    let events = parse_events(&std::fs::read_to_string(dir.join("events.jsonl")).unwrap());
    assert_eq!(events[1].detail.as_ref().unwrap()["ok"], true);
}

#[test]
fn starting_a_stage_ends_the_previous_one() {
    let dir = temp_dir("stages");
    let obs = Observer::new(dir.join("events.jsonl"), None, None);
    obs.stage("A");
    obs.stage("B");
    let events = parse_events(&std::fs::read_to_string(dir.join("events.jsonl")).unwrap());
    let kinds: Vec<_> = events
        .iter()
        .map(|e| (e.kind, e.summary.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (EventKind::StageStart, "A"),
            (EventKind::StageEnd, "A"),
            (EventKind::StageStart, "B"),
        ]
    );
}

#[test]
fn an_unreachable_queue_never_fails_the_emit_and_events_are_still_written() {
    let dir = temp_dir("unreachable");
    let obs = Observer::new(
        dir.join("events.jsonl"),
        Some(1),
        Some(("http://127.0.0.1:1".into(), "tok".into())),
    );
    for i in 0..5 {
        obs.decision("a", &format!("d{i}"));
    }
    let events = parse_events(&std::fs::read_to_string(dir.join("events.jsonl")).unwrap());
    assert_eq!(events.len(), 5);
}

#[test]
fn an_unwritable_workspace_never_panics() {
    let dir = temp_dir("unwritable");
    let blocker = dir.join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let obs = Observer::new(blocker.join("events.jsonl"), Some(1), None);
    obs.decision("a", "still fine");
    obs.stage("S");
    obs.end_stage();
    obs.gate_result("", false, 10);
}

#[test]
fn concurrent_emits_produce_whole_lines() {
    let dir = temp_dir("concurrent");
    let obs = Observer::new(dir.join("events.jsonl"), Some(1), None);
    let big = "x".repeat(20_000);
    std::thread::scope(|s| {
        for t in 0..8 {
            let obs = &obs;
            let big = &big;
            s.spawn(move || {
                for i in 0..25 {
                    obs.deviation(&format!("agent-{t}"), &format!("{i} {big}"));
                }
            });
        }
    });
    let text = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 200);
    assert_eq!(parse_events(&text).len(), 200);
}

#[test]
fn gate_attempts_count_failures_until_a_pass_then_reset() {
    let dir = temp_dir("gate");
    let obs = Observer::new(dir.join("events.jsonl"), None, None);
    obs.gate_result("", false, 100);
    obs.gate_result("", false, 50);
    obs.gate_result("", true, 0);
    obs.gate_result("", false, 10);
    obs.gate_result("m1", false, 10);
    let events = parse_events(&std::fs::read_to_string(dir.join("events.jsonl")).unwrap());
    let attempts: Vec<u64> = events
        .iter()
        .map(|e| e.detail.as_ref().unwrap()["attempt"].as_u64().unwrap())
        .collect();
    assert_eq!(attempts, vec![1, 2, 3, 1, 1]);
    let detail = events[1].detail.as_ref().unwrap();
    assert_eq!(detail["passed"], false);
    assert_eq!(detail["error_bytes"], 50);
    assert_eq!(events[4].detail.as_ref().unwrap()["variant"], "m1");
}

#[test]
fn truncate_chars_respects_char_boundaries() {
    assert_eq!(truncate_chars("héllo", 2), "hé...");
    assert_eq!(truncate_chars("hi", 5), "hi");
}
