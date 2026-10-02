use super::*;
use crate::workspace::Workspace;
use inference_providers::Registry;

fn temp_source(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aoo-tools-src-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (rel, content) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    dir
}

fn temp_ws(tag: &str) -> Workspace {
    let dir = std::env::temp_dir().join(format!("aoo-tools-ws-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Workspace::new(dir).unwrap()
}

fn toolbox<'a>(source: &Path, ws: &'a Workspace, manager: &'a Manager<'a>) -> PipelineToolbox<'a> {
    PipelineToolbox::new(source, ws, manager, "TestAgent", "test system", 5)
}

fn empty_registry() -> Registry {
    Registry::with_providers(Vec::new(), (1.0, 1.0, 1.0))
}

#[test]
fn list_files_finds_every_file_and_skips_ignored_dirs() {
    let src = temp_source(
        "list",
        &[
            ("src/main.rs", "fn main() {}"),
            ("target/debug/foo", "binary junk"),
            (".git/HEAD", "ref: refs/heads/main"),
        ],
    );
    let ws = temp_ws("list");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    let out = tb.call("list_files", &json!({}));
    assert!(out.contains("src/main.rs"), "got: {out}");
    assert!(!out.contains("target/"), "got: {out}");
    assert!(!out.contains(".git"), "got: {out}");
}

#[test]
fn list_files_can_be_scoped_to_a_subdirectory() {
    let src = temp_source("scoped", &[("src/main.rs", "a"), ("docs/readme.md", "b")]);
    let ws = temp_ws("scoped");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    let out = tb.call("list_files", &json!({"path": "src"}));
    assert!(out.contains("src/main.rs"), "got: {out}");
    assert!(!out.contains("docs/readme.md"), "got: {out}");
}

#[test]
fn read_file_returns_content() {
    let src = temp_source("read", &[("src/main.rs", "fn main() {}\n")]);
    let ws = temp_ws("read");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    assert_eq!(tb.call("read_file", &json!({"path": "src/main.rs"})), "fn main() {}\n");
}

#[test]
fn read_file_rejects_path_traversal_outside_the_source_dir() {
    let src = temp_source("traversal", &[("src/main.rs", "fn main() {}\n")]);
    let ws = temp_ws("traversal");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    let out = tb.call("read_file", &json!({"path": "../../etc/passwd"}));
    assert!(
        out.contains("escapes") || out.contains("not found"),
        "expected a refusal, got: {out}"
    );
}

#[test]
fn read_file_errors_clearly_for_a_missing_file() {
    let src = temp_source("missing", &[]);
    let ws = temp_ws("missing");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    assert!(tb.call("read_file", &json!({"path": "nope.rs"})).contains("not found"));
}

#[test]
fn write_then_read_artifact_round_trips() {
    let src = temp_source("artifact", &[]);
    let ws = temp_ws("artifact");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    let write_result =
        tb.call("write_artifact", &json!({"name": "schema.rs", "content": "pub struct X;"}));
    assert!(write_result.contains("wrote"), "got: {write_result}");
    assert_eq!(tb.call("read_artifact", &json!({"name": "schema.rs"})), "pub struct X;");
}

#[test]
fn read_artifact_reports_a_missing_artifact_clearly() {
    let src = temp_source("no-artifact", &[]);
    let ws = temp_ws("no-artifact");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    assert!(tb.call("read_artifact", &json!({"name": "nope.md"})).contains("no artifact"));
}

#[test]
fn unknown_tool_name_is_a_clear_error() {
    let src = temp_source("unknown", &[]);
    let ws = temp_ws("unknown");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    assert!(tb.call("frobnicate", &json!({})).contains("unknown tool"));
}

#[test]
fn tool_defs_names_match_what_call_dispatches_on() {
    let src = temp_source("defs", &[]);
    let ws = temp_ws("defs");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    let defs = tb.tool_defs();
    let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "list_files", "read_file", "read_artifact", "write_artifact", "leave_note",
            "read_notes", "report_finding", "fan_out"
        ]
    );
}

#[test]
fn fan_out_runs_tasks_concurrently_and_labels_each_result() {
    use inference_providers::backends::Provider;
    use inference_providers::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    struct Echo {
        calls: Arc<AtomicUsize>,
        models: Vec<ModelInfo>,
    }
    impl Provider for Echo {
        fn name(&self) -> &str { "mock" }
        fn models(&self) -> &[ModelInfo] { &self.models }
        fn is_available(&self) -> bool { true }
        fn supports_tools(&self) -> bool { true }
        fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(InferenceResponse {
                text: format!("answer to: {}", req.user),
                tool_calls: Vec::new(),
                input_tokens: 0,
                output_tokens: 0,
                provider: "mock".into(),
                model: "mock".into(),
                latency_ms: 1,
            })
        }
    }

    let src = temp_source("fanout", &[]);
    let ws = temp_ws("fanout");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Box<dyn Provider> = Box::new(Echo {
        calls: calls.clone(),
        models: vec![ModelInfo {
            provider: "mock".into(),
            model: "mock".into(),
            tier: ModelTier::Heavy,
            cost_per_1k_input: 0.0,
            cost_per_1k_output: 0.0,
            context_limit: 200_000,
        }],
    });
    let registry = Registry::with_providers(vec![provider], (1.0, 1.0, 1.0));
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    let out = tb.call("fan_out", &json!({"tasks": ["task A", "task B"]}));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(out.contains("## Result 1"), "got: {out}");
    assert!(out.contains("## Result 2"), "got: {out}");
    assert!(out.contains("answer to: task"), "got: {out}");
}

#[test]
fn restricted_toolbox_hides_named_tools_from_the_catalog() {
    let src = temp_source("restricted-defs", &[]);
    let ws = temp_ws("restricted-defs");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let inner = toolbox(&src, &ws, &mgr);
    let restricted = RestrictedToolbox { inner: &inner, hidden: &["write_artifact", "fan_out"] };

    let defs = restricted.tool_defs();
    let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["list_files", "read_file", "read_artifact", "leave_note", "read_notes", "report_finding"]
    );
}

#[test]
fn restricted_toolbox_rejects_a_call_to_a_hidden_tool_without_delegating() {
    let src = temp_source("restricted-call", &[]);
    let ws = temp_ws("restricted-call");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let inner = toolbox(&src, &ws, &mgr);
    let restricted = RestrictedToolbox { inner: &inner, hidden: &["write_artifact"] };

    let out = restricted.call("write_artifact", &json!({"name": "x", "content": "y"}));
    assert!(out.contains("not available"), "got: {out}");
    assert!(
        ws.get_artifact("x").is_err(),
        "a hidden tool's call must never reach the inner toolbox"
    );
}

#[test]
fn restricted_toolbox_still_delegates_calls_to_tools_it_does_not_hide() {
    let src = temp_source("restricted-passthrough", &[("src/main.rs", "fn main() {}")]);
    let ws = temp_ws("restricted-passthrough");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let inner = toolbox(&src, &ws, &mgr);
    let restricted = RestrictedToolbox { inner: &inner, hidden: &["write_artifact", "fan_out"] };

    let out = restricted.call("read_file", &json!({"path": "src/main.rs"}));
    assert_eq!(out, "fn main() {}");
}

fn observed_ws(tag: &str) -> Workspace {
    let dir = std::env::temp_dir().join(format!("aoo-tools-observed-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let observer = crate::observer::Observer::new(dir.join("events.jsonl"), Some(9), None);
    Workspace::new(dir).unwrap().with_observer(observer)
}

#[test]
fn report_finding_is_offered_and_emits_a_finding_event() {
    let src = temp_source("finding", &[("a.txt", "x")]);
    let ws = observed_ws("finding");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    assert!(tb.tool_defs().iter().any(|t| t.name == "report_finding"));
    let out = tb.call(
        "report_finding",
        &json!({"severity": "warn", "category": "missing-tool", "text": "no way to grep", "evidence": "list_files only"}),
    );
    assert_eq!(out, "finding noted");

    let events = run_events::parse_events(&std::fs::read_to_string(ws.root.join("events.jsonl")).unwrap());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, run_events::EventKind::Finding);
    assert_eq!(events[0].agent, "TestAgent");
    assert_eq!(events[0].summary, "no way to grep");
    let d = events[0].detail.as_ref().unwrap();
    assert_eq!(d["severity"], "warn");
    assert_eq!(d["category"], "missing-tool");
    assert_eq!(d["evidence"], "list_files only");
}

#[test]
fn report_finding_rejects_bad_arguments_and_emits_nothing() {
    let src = temp_source("finding-bad", &[("a.txt", "x")]);
    let ws = observed_ws("finding-bad");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    let bad = [
        json!({"severity": "loud", "category": "ok-cat", "text": "t"}),
        json!({"category": "ok-cat", "text": "t"}),
        json!({"severity": "info", "category": "Not Kebab", "text": "t"}),
        json!({"severity": "info", "category": "", "text": "t"}),
        json!({"severity": "info", "category": "ok-cat", "text": "   "}),
        json!({"severity": "info", "category": "ok-cat"}),
    ];
    for args in &bad {
        let out = tb.call("report_finding", args);
        assert!(out.starts_with("error:"), "{args} -> {out}");
    }
    assert!(!ws.root.join("events.jsonl").exists());
}

#[test]
fn report_finding_without_an_observer_still_succeeds() {
    let src = temp_source("finding-none", &[("a.txt", "x")]);
    let ws = temp_ws("finding-none");
    let registry = empty_registry();
    let mgr = Manager::new(&ws, &registry);
    let tb = toolbox(&src, &ws, &mgr);

    let out = tb.call("report_finding", &json!({"severity": "info", "category": "c", "text": "t"}));
    assert_eq!(out, "finding noted");
}
