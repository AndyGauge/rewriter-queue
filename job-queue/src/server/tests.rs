use super::*;
use crate::job::Spec;
use run_events::{EventKind, FeatureRequest, Priority, RequestStatus};

const TOKEN: &str = "secret";

struct Harness {
    base: String,
    queue: Queue,
}

fn harness(tag: &str) -> Harness {
    let dir = std::env::temp_dir().join(format!("rq-server-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let queue = Queue::at(dir).unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let served = queue.clone();
    std::thread::spawn(move || serve_requests(&server, &served, TOKEN.into()));
    Harness { base, queue }
}

impl Harness {
    fn submit_job(&self) -> u32 {
        self.queue
            .submit(Spec {
                name: None,
                workspace: self.queue.root().join("ws"),
                source: "/s".into(),
                max_iter: None,
                inherit_env: false,
            })
            .unwrap()
            .id
    }

    fn call(&self, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
        let mut req = ureq::request(method, &format!("{}{path}", self.base));
        if let Some(t) = token {
            req = req.set("authorization", &format!("Bearer {t}"));
        }
        let result = if method == "POST" {
            req.send_string(body)
        } else {
            req.call()
        };
        match result {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
            Err(e) => panic!("transport error: {e}"),
        }
    }

    fn authed(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        self.call(method, path, Some(TOKEN), body)
    }
}

fn seed_request(h: &Harness) {
    let request = FeatureRequest {
        id: "retry-budget".into(),
        title: "t".into(),
        rationale: "r".into(),
        evidence: vec![],
        source_job_ids: vec![1],
        priority: Priority::Low,
        status: RequestStatus::Open,
    };
    events::merge_into_store(&h.queue, vec![request]).unwrap();
}

#[test]
fn every_new_route_requires_the_bearer_token() {
    let h = harness("auth");
    let id = h.submit_job();
    let routes = [
        ("POST", format!("/jobs/{id}/events")),
        ("GET", format!("/jobs/{id}/events")),
        ("GET", format!("/jobs/{id}/analysis")),
        ("GET", "/feature-requests".to_string()),
        ("POST", "/feature-requests/x/status".to_string()),
    ];
    for (method, path) in routes {
        assert_eq!(h.call(method, &path, None, "{}").0, 401, "{method} {path}");
        assert_eq!(
            h.call(method, &path, Some("wrong"), "{}").0,
            401,
            "{method} {path}"
        );
    }
}

#[test]
fn events_for_an_unknown_or_malformed_job_are_rejected() {
    let h = harness("unknown");
    assert_eq!(h.authed("GET", "/jobs/42/events", "").0, 404);
    assert_eq!(h.authed("POST", "/jobs/42/events", &line("a")).0, 404);
    assert_eq!(h.authed("GET", "/jobs/42/analysis", "").0, 404);
    for bad in ["abc", "..", "%2e%2e", "1%2f..", "-1", "99999999999"] {
        let (code, _) = h.authed("GET", &format!("/jobs/{bad}/events"), "");
        assert!(code == 400 || code == 404, "{bad} gave {code}");
        let (code, _) = h.authed("POST", &format!("/jobs/{bad}/events"), &line("a"));
        assert!(code == 400 || code == 404, "{bad} gave {code}");
    }
}

#[test]
fn a_malformed_event_body_gets_a_400_and_stores_nothing() {
    let h = harness("badbody");
    let id = h.submit_job();
    let path = format!("/jobs/{id}/events");
    assert_eq!(h.authed("POST", &path, "not json").0, 400);
    assert_eq!(h.authed("POST", &path, "{\"ts\":1}").0, 400);
    assert_eq!(h.authed("POST", &path, "").0, 400);
    let batch = format!("[{},1]", line("a"));
    assert_eq!(h.authed("POST", &path, &batch).0, 400);
    assert_eq!(h.authed("GET", &path, "").1, "");
}

#[test]
fn an_oversized_event_body_gets_a_413() {
    let h = harness("big");
    let id = h.submit_job();
    let body = " ".repeat(MAX_EVENTS_BODY as usize + 1);
    assert_eq!(
        h.authed("POST", &format!("/jobs/{id}/events"), &body).0,
        413
    );
}

#[test]
fn posted_events_read_back_with_filters() {
    let h = harness("roundtrip");
    let id = h.submit_job();
    let path = format!("/jobs/{id}/events");
    let mut late = Event::new("b", EventKind::Error, "boom");
    late.ts = "2999-01-01T00:00:00.000Z".into();
    assert_eq!(h.authed("POST", &path, &line("a")).0, 200);
    let batch = format!("[{},{}]", late.to_json_line(), line("c"));
    assert_eq!(h.authed("POST", &path, &batch).0, 200);

    let all = run_events::parse_events(&h.authed("GET", &path, "").1);
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|e| e.job_id == Some(u64::from(id))));
    let by_kind =
        run_events::parse_events(&h.authed("GET", &format!("{path}?kind=error"), "").1);
    assert_eq!(by_kind.len(), 1);
    let by_agent = run_events::parse_events(&h.authed("GET", &format!("{path}?agent=c"), "").1);
    assert_eq!(by_agent.len(), 1);
    let since = format!("{path}?since=2998-01-01T00:00:00Z");
    assert_eq!(
        run_events::parse_events(&h.authed("GET", &since, "").1).len(),
        1
    );
    assert_eq!(h.authed("GET", &format!("{path}?kind=nope"), "").0, 400);
}

#[test]
fn a_stored_analysis_is_served_and_a_missing_one_is_a_404() {
    let h = harness("analysis");
    let id = h.submit_job();
    let path = format!("/jobs/{id}/analysis");
    assert_eq!(h.authed("GET", &path, "").0, 404);
    let analysis = run_events::Analysis {
        job_id: u64::from(id),
        outcome: "failed".into(),
        summary: "s".into(),
        root_causes: vec![],
        feature_requests: vec![],
    };
    events::write_analysis(&h.queue, &analysis).unwrap();
    let (code, body) = h.authed("GET", &path, "");
    assert_eq!(code, 200);
    assert_eq!(
        serde_json::from_str::<run_events::Analysis>(&body).unwrap(),
        analysis
    );
}

#[test]
fn a_feature_request_status_can_be_changed_over_http() {
    let h = harness("status");
    seed_request(&h);
    let (code, body) = h.authed("GET", "/feature-requests", "");
    assert_eq!(code, 200);
    assert_eq!(
        serde_json::from_str::<Vec<FeatureRequest>>(&body)
            .unwrap()
            .len(),
        1
    );

    let path = "/feature-requests/retry-budget/status";
    let (code, body) = h.authed("POST", path, "{\"status\":\"accepted\"}");
    assert_eq!(code, 200);
    let updated: FeatureRequest = serde_json::from_str(&body).unwrap();
    assert_eq!(updated.status, RequestStatus::Accepted);
    assert_eq!(
        events::load_requests(&h.queue).unwrap()[0].status,
        RequestStatus::Accepted
    );
    assert_eq!(h.authed("POST", path, "{\"status\":\"maybe\"}").0, 400);
    assert_eq!(h.authed("POST", path, "nope").0, 400);
    let missing = "/feature-requests/missing/status";
    assert_eq!(h.authed("POST", missing, "{\"status\":\"done\"}").0, 404);
    let big = format!("{{\"status\":\"{}\"}}", "a".repeat(5000));
    assert_eq!(h.authed("POST", path, &big).0, 413);
}

fn line(agent: &str) -> String {
    Event::new(agent, EventKind::Decision, "s").to_json_line()
}

#[test]
fn an_event_body_may_be_one_event_or_an_array() {
    let one = parse_event_batch(line("a").as_bytes()).unwrap();
    assert_eq!(one.len(), 1);
    let many = parse_event_batch(format!("[{},{}]", line("a"), line("b")).as_bytes()).unwrap();
    assert_eq!(many.len(), 2);
}

#[test]
fn a_malformed_event_body_is_a_client_error() {
    assert_eq!(parse_event_batch(b"not json").unwrap_err().0, 400);
    assert_eq!(parse_event_batch(b"{\"ts\":1}").unwrap_err().0, 400);
    assert_eq!(parse_event_batch(b"[1]").unwrap_err().0, 400);
}

#[test]
fn a_status_body_names_one_known_status() {
    let ok = parse_status_body(b"{\"status\":\"accepted\"}").unwrap();
    assert_eq!(ok, RequestStatus::Accepted);
    assert_eq!(
        parse_status_body(b"{\"status\":\"maybe\"}").unwrap_err().0,
        400
    );
    assert_eq!(parse_status_body(b"{}").unwrap_err().0, 400);
    assert_eq!(parse_status_body(b"").unwrap_err().0, 400);
}

#[test]
fn the_worker_reaches_its_own_server_over_loopback() {
    assert_eq!(loopback_url("0.0.0.0:8003"), "http://127.0.0.1:8003");
    assert_eq!(loopback_url("[::]:9000"), "http://127.0.0.1:9000");
    assert_eq!(loopback_url("10.0.0.5:8003"), "http://10.0.0.5:8003");
}
