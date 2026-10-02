use super::*;
use crate::job::Spec;
use run_events::Priority;

fn temp_queue(tag: &str) -> Queue {
    let dir = std::env::temp_dir().join(format!("rq-events-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    Queue::at(dir).unwrap()
}

fn job(q: &Queue) -> Job {
    q.submit(Spec {
        name: None,
        workspace: q.root().join("ws"),
        source: "/s".into(),
        max_iter: None,
        inherit_env: false,
    })
    .unwrap()
}

fn event(agent: &str, kind: EventKind, ts: &str) -> Event {
    let mut e = Event::new(agent, kind, "x");
    e.ts = ts.to_string();
    e
}

fn request(id: &str, jobs: &[u64], evidence: &[&str]) -> FeatureRequest {
    FeatureRequest {
        id: id.into(),
        title: format!("title {id}"),
        rationale: "why".into(),
        evidence: evidence.iter().map(|s| s.to_string()).collect(),
        source_job_ids: jobs.to_vec(),
        priority: Priority::Medium,
        status: RequestStatus::Open,
    }
}

#[test]
fn appended_events_read_back_in_order_with_the_job_id_filled_in() {
    let q = temp_queue("append");
    let j = job(&q);
    let first = event("a", EventKind::StageStart, "2026-01-01T00:00:00.000Z");
    let second = event("b", EventKind::Decision, "2026-01-01T00:00:01.000Z");
    append(&q, j.id, &[first]).unwrap();
    append(&q, j.id, &[second]).unwrap();
    let events = read(&q, &j, &EventFilter::default()).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].agent, "a");
    assert_eq!(events[1].agent, "b");
    assert_eq!(events[0].job_id, Some(u64::from(j.id)));
}

#[test]
fn reading_filters_by_kind_agent_and_since() {
    let q = temp_queue("filter");
    let j = job(&q);
    append(
        &q,
        j.id,
        &[
            event("a", EventKind::Finding, "2026-01-01T00:00:00.000Z"),
            event("b", EventKind::Finding, "2026-01-01T00:00:01.000Z"),
            event("a", EventKind::Error, "2026-01-01T00:00:02.000Z"),
        ],
    )
    .unwrap();
    let by_kind = EventFilter {
        kind: Some(EventKind::Finding),
        ..Default::default()
    };
    assert_eq!(read(&q, &j, &by_kind).unwrap().len(), 2);
    let by_agent = EventFilter {
        agent: Some("a".into()),
        ..Default::default()
    };
    assert_eq!(read(&q, &j, &by_agent).unwrap().len(), 2);
    let since = EventFilter {
        since: Some("2026-01-01T00:00:01.000Z".into()),
        ..Default::default()
    };
    let after = read(&q, &j, &since).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].kind, EventKind::Error);
    let all = EventFilter {
        kind: Some(EventKind::Finding),
        agent: Some("a".into()),
        since: Some("2025-12-31T00:00:00Z".into()),
    };
    assert_eq!(read(&q, &j, &all).unwrap().len(), 1);
}

#[test]
fn a_job_with_no_events_reads_as_empty() {
    let q = temp_queue("none");
    let j = job(&q);
    assert!(read(&q, &j, &EventFilter::default()).unwrap().is_empty());
}

#[test]
fn reading_falls_back_to_the_workspace_file_and_skips_torn_lines() {
    let q = temp_queue("fallback");
    let j = job(&q);
    fs::create_dir_all(&j.workspace).unwrap();
    let good = event("a", EventKind::Decision, "2026-01-01T00:00:00.000Z").to_json_line();
    fs::write(
        j.workspace.join("events.jsonl"),
        format!("{good}\n{{\"ts\":\"tr"),
    )
    .unwrap();
    assert_eq!(read(&q, &j, &EventFilter::default()).unwrap().len(), 1);
}

#[test]
fn a_partial_server_copy_is_completed_from_the_workspace_file() {
    let q = temp_queue("partial");
    let j = job(&q);
    fs::create_dir_all(&j.workspace).unwrap();
    let first = event("a", EventKind::StageStart, "2026-01-01T00:00:00.000Z");
    let second = event("b", EventKind::Decision, "2026-01-01T00:00:01.000Z");
    let third = event("c", EventKind::Error, "2026-01-01T00:00:02.000Z");
    let local: String = [&first, &second, &third]
        .iter()
        .map(|e| e.to_json_line() + "\n")
        .collect();
    fs::write(j.workspace.join("events.jsonl"), local).unwrap();
    append(&q, j.id, &[first, third]).unwrap();
    let agents: Vec<String> = read(&q, &j, &EventFilter::default())
        .unwrap()
        .into_iter()
        .map(|e| e.agent)
        .collect();
    assert_eq!(agents, ["a", "b", "c"]);
}

#[test]
fn repeated_identical_events_are_kept_but_not_double_counted_across_sources() {
    let e = event("a", EventKind::Retry, "2026-01-01T00:00:00.000Z");
    let merged = merge_event_sources(vec![e.clone(), e.clone()], vec![e.clone(), e.clone()]);
    assert_eq!(merged.len(), 2);
    let merged = merge_event_sources(vec![e.clone()], vec![e.clone(), e]);
    assert_eq!(merged.len(), 2);
}

#[test]
fn the_job_id_does_not_make_the_same_event_look_different() {
    let plain = event("a", EventKind::Retry, "2026-01-01T00:00:00.000Z");
    let tagged = plain.clone().with_job(1);
    assert_eq!(merge_event_sources(vec![tagged], vec![plain]).len(), 1);
}

#[test]
fn a_job_id_out_of_u32_range_is_rejected_when_storing_an_analysis() {
    let q = temp_queue("big-id");
    let analysis = Analysis {
        job_id: u64::from(u32::MAX) + 5,
        outcome: "failed".into(),
        summary: String::new(),
        root_causes: vec![],
        feature_requests: vec![],
    };
    assert!(write_analysis(&q, &analysis).is_err());
}

#[test]
fn a_posted_event_is_stored_under_the_job_it_was_posted_to() {
    let q = temp_queue("force-id");
    let j = job(&q);
    let stray = event("a", EventKind::Retry, "2026-01-01T00:00:00.000Z").with_job(99);
    append(&q, j.id, &[stray]).unwrap();
    let events = read(&q, &j, &EventFilter::default()).unwrap();
    assert_eq!(events[0].job_id, Some(u64::from(j.id)));
}

#[test]
fn a_request_seen_in_a_new_job_is_raised_one_step_and_never_past_high() {
    let mut low = request("a", &[1], &[]);
    low.priority = Priority::Low;
    let once = merge_requests(vec![low], vec![request("a", &[2], &[])]);
    assert_eq!(once[0].priority, Priority::Medium);
    let twice = merge_requests(once, vec![request("a", &[3], &[])]);
    assert_eq!(twice[0].priority, Priority::High);
    let capped = merge_requests(twice, vec![request("a", &[4], &[])]);
    assert_eq!(capped[0].priority, Priority::High);
}

#[test]
fn a_request_seen_again_in_the_same_job_keeps_its_priority() {
    let mut low = request("a", &[1], &[]);
    low.priority = Priority::Low;
    let same = merge_requests(vec![low], vec![request("a", &[1], &["more"])]);
    assert_eq!(same[0].priority, Priority::Low);
}

#[test]
fn a_lower_incoming_priority_never_lowers_an_existing_one() {
    let mut high = request("a", &[1], &[]);
    high.priority = Priority::High;
    let mut incoming = request("a", &[2], &[]);
    incoming.priority = Priority::Low;
    assert_eq!(
        merge_requests(vec![high], vec![incoming])[0].priority,
        Priority::High
    );
}

#[test]
fn concurrent_status_changes_and_merges_lose_nothing() {
    let q = temp_queue("concurrent");
    merge_into_store(&q, vec![request("seed", &[1], &[])]).unwrap();
    let handles: Vec<_> = (0..8u64)
        .map(|n| {
            let q = q.clone();
            std::thread::spawn(move || {
                merge_into_store(&q, vec![request(&format!("r{n}"), &[n], &[])]).unwrap();
                set_request_status(&q, "seed", RequestStatus::Accepted).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let requests = load_requests(&q).unwrap();
    assert_eq!(requests.len(), 9);
    assert_eq!(requests[0].status, RequestStatus::Accepted);
}

#[test]
fn an_analysis_round_trips() {
    let q = temp_queue("analysis");
    let analysis = Analysis {
        job_id: 3,
        outcome: "failed".into(),
        summary: "s".into(),
        root_causes: vec!["c".into()],
        feature_requests: vec![request("r", &[3], &["e"])],
    };
    assert!(read_analysis(&q, 3).is_err());
    write_analysis(&q, &analysis).unwrap();
    assert_eq!(read_analysis(&q, 3).unwrap(), analysis);
}

#[test]
fn a_new_request_is_added() {
    let q = temp_queue("new-request");
    merge_into_store(&q, vec![request("a", &[1], &["e1"])]).unwrap();
    merge_into_store(&q, vec![request("b", &[2], &["e2"])]).unwrap();
    let ids: Vec<String> = load_requests(&q)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, ["a", "b"]);
}

#[test]
fn an_existing_request_keeps_its_status_and_gains_jobs_and_evidence() {
    let q = temp_queue("merge");
    merge_into_store(&q, vec![request("a", &[1], &["e1"])]).unwrap();
    set_request_status(&q, "a", RequestStatus::Accepted).unwrap();
    let mut again = request("a", &[2], &["e2", "e1"]);
    again.title = "renamed".into();
    again.status = RequestStatus::Open;
    merge_into_store(&q, vec![again]).unwrap();
    let requests = load_requests(&q).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].status, RequestStatus::Accepted);
    assert_eq!(requests[0].title, "title a");
    assert_eq!(requests[0].source_job_ids, [1, 2]);
    assert_eq!(requests[0].evidence, ["e1", "e2"]);
}

#[test]
fn merging_the_same_job_twice_changes_nothing() {
    let first = merge_requests(vec![], vec![request("a", &[1], &["e1"])]);
    let second = merge_requests(first.clone(), vec![request("a", &[1], &["e1"])]);
    assert_eq!(first, second);
}

#[test]
fn duplicates_inside_one_batch_merge() {
    let merged = merge_requests(
        vec![],
        vec![request("a", &[1], &["e1"]), request("a", &[2], &["e2"])],
    );
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].source_job_ids, [1, 2]);
    assert_eq!(merged[0].evidence, ["e1", "e2"]);
}

#[test]
fn setting_status_persists_and_rejects_unknown_ids() {
    let q = temp_queue("status");
    merge_into_store(&q, vec![request("a", &[1], &[])]).unwrap();
    let updated = set_request_status(&q, "a", RequestStatus::Done).unwrap();
    assert_eq!(updated.status, RequestStatus::Done);
    assert_eq!(load_requests(&q).unwrap()[0].status, RequestStatus::Done);
    let err = set_request_status(&q, "missing", RequestStatus::Done).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn kinds_and_statuses_parse_from_text() {
    assert_eq!(parse_kind("gate_result").unwrap(), EventKind::GateResult);
    assert!(parse_kind("nope").is_err());
    assert_eq!(parse_status("rejected").unwrap(), RequestStatus::Rejected);
    assert!(parse_status("nope").is_err());
}
