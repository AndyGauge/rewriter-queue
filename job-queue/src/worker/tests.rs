use super::*;
use crate::job::Spec;

fn temp_queue(tag: &str) -> Queue {
    let dir = std::env::temp_dir().join(format!("rq-worker-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Queue::at(dir).unwrap()
}

fn spec() -> Spec {
    Spec {
        name: None,
        workspace: "/w".into(),
        source: "/s".into(),
        max_iter: None,
        inherit_env: false,
    }
}

#[test]
fn interrupted_job_is_requeued_up_to_the_cap() {
    let q = temp_queue("resume");
    let mut job = q.submit(spec()).unwrap();
    job.state = State::Running;
    job.pid = Some(999_999); // unlikely to be a live pid; killpg on it is a harmless no-op
    q.save(&job).unwrap();

    recover_orphans(&q).unwrap();

    let after = q.get(job.id).unwrap();
    assert_eq!(after.state, State::Queued);
    assert_eq!(after.resume_count, 1);
    assert!(after.pid.is_none());
}

fn env_of(cmd: &Command, key: &str) -> Option<Option<String>> {
    cmd.get_envs()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
}

#[test]
fn the_orchestrator_is_told_where_to_report_when_a_server_hosts_the_worker() {
    let mut cmd = Command::new("true");
    let reporting = Reporting {
        url: "http://127.0.0.1:8003".into(),
        token: "t".into(),
    };
    apply_reporting(&mut cmd, 7, Some(&reporting));
    assert_eq!(env_of(&cmd, "REWRITER_JOB_ID"), Some(Some("7".into())));
    assert_eq!(
        env_of(&cmd, "REWRITER_QUEUE_URL"),
        Some(Some("http://127.0.0.1:8003".into()))
    );
    assert_eq!(env_of(&cmd, "REWRITER_QUEUE_TOKEN"), Some(Some("t".into())));
}

#[test]
fn a_local_worker_clears_any_inherited_queue_endpoint() {
    let mut cmd = Command::new("true");
    apply_reporting(&mut cmd, 7, None);
    assert_eq!(env_of(&cmd, "REWRITER_JOB_ID"), Some(Some("7".into())));
    assert_eq!(env_of(&cmd, "REWRITER_QUEUE_URL"), Some(None));
    assert_eq!(env_of(&cmd, "REWRITER_QUEUE_TOKEN"), Some(None));
}

#[test]
fn ingesting_an_analysis_stores_it_and_merges_its_requests_as_open() {
    use run_events::{FeatureRequest, Priority};
    let q = temp_queue("ingest");
    let job = q.submit(spec()).unwrap();
    let analysis = Analysis {
        job_id: 999,
        outcome: "failed".into(),
        summary: "s".into(),
        root_causes: vec![],
        feature_requests: vec![FeatureRequest {
            id: "retry-budget".into(),
            title: "t".into(),
            rationale: "r".into(),
            evidence: vec!["e".into()],
            source_job_ids: vec![],
            priority: Priority::High,
            status: RequestStatus::Done,
        }],
    };
    let scratch = events::analysis_scratch_path(&q, job.id);
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
    std::fs::write(&scratch, serde_json::to_vec(&analysis).unwrap()).unwrap();

    ingest_analysis(&q, job.id).unwrap();

    let stored = events::read_analysis(&q, job.id).unwrap();
    assert_eq!(stored.job_id, u64::from(job.id));
    let requests = events::load_requests(&q).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].status, RequestStatus::Open);
    assert_eq!(requests[0].source_job_ids, [u64::from(job.id)]);
    assert!(!scratch.exists());
}

#[test]
fn a_missing_or_garbled_analysis_file_is_an_error_not_a_panic() {
    let q = temp_queue("ingest-bad");
    let job = q.submit(spec()).unwrap();
    assert!(ingest_analysis(&q, job.id).is_err());
    let scratch = events::analysis_scratch_path(&q, job.id);
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
    std::fs::write(&scratch, "{").unwrap();
    assert!(ingest_analysis(&q, job.id).is_err());
    assert!(events::load_requests(&q).unwrap().is_empty());
}

#[test]
fn job_fails_after_exceeding_max_resumes() {
    let q = temp_queue("resume-cap");
    let mut job = q.submit(spec()).unwrap();
    job.state = State::Running;
    job.pid = Some(999_999);
    job.resume_count = MAX_RESUMES;
    q.save(&job).unwrap();

    recover_orphans(&q).unwrap();

    let after = q.get(job.id).unwrap();
    assert_eq!(after.state, State::Failed);
    assert!(after.error.unwrap().contains("giving up"));
}
