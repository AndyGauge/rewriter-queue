use crate::manager::Manager;
use crate::workspace::Workspace;
use inference_providers::ToolDef;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Directories a source-tree tool should never walk into or read from, regardless of
/// language -- mirrors `workspace::IGNORED_DIRS`.
const IGNORED_DIRS: &[&str] =
    &["target", "node_modules", "dist", "build", "vendor", "venv", ".venv", "__pycache__", ".next", "coverage"];

const MAX_LISTING_ENTRIES: usize = 2000;
const MAX_READ_BYTES: usize = 60_000;

/// Executes whatever tools an agentic call is offered. `tool_defs()` is the JSON-schema
/// catalog sent to the model (see `Manager::run_agentic`); `call()` executes one invocation
/// by name and returns its result as text -- every tool result is text, even for tools that
/// don't "read" anything, since that's the one format every backend's wire protocol accepts
/// back for a tool result. `Sync` because `fan_out` can call `call()`'s implementer
/// concurrently from multiple threads.
pub trait Toolbox: Sync {
    fn tool_defs(&self) -> Vec<ToolDef>;
    fn call(&self, name: &str, arguments: &Value) -> String;
}

/// The standard toolbox every uplifted pipeline stage gets: read the V1 source tree instead
/// of having it pasted in whole, read/write named pipeline artifacts instead of having every
/// upstream stage's output pasted into every downstream prompt, and fan sub-questions out to
/// concurrent copies of itself instead of the orchestrator pre-splitting the source by byte
/// count (see `Manager::run_agentic` and `synthesis::synthesize`'s `SPLIT_THRESHOLD` path,
/// which this is meant to eventually make unnecessary for the analysis stages).
pub struct PipelineToolbox<'a> {
    pub source_dir: PathBuf,
    pub ws: &'a Workspace,
    pub manager: &'a Manager<'a>,
    /// Role name and system prompt used for `fan_out` sub-calls -- a spawned sub-task is
    /// answered by "the same kind of agent" that asked for it, just given a narrower task.
    pub agent: String,
    pub system: String,
    pub max_turns: usize,
}

impl<'a> PipelineToolbox<'a> {
    pub fn new(
        source_dir: impl Into<PathBuf>,
        ws: &'a Workspace,
        manager: &'a Manager<'a>,
        agent: impl Into<String>,
        system: impl Into<String>,
        max_turns: usize,
    ) -> Self {
        Self {
            source_dir: source_dir.into(),
            ws,
            manager,
            agent: agent.into(),
            system: system.into(),
            max_turns,
        }
    }

    /// Resolve a path the model gave us against `source_dir`, refusing anything that would
    /// escape it (`../../etc/passwd`) -- the model's input is untrusted the same way any tool
    /// argument is.
    fn resolve_in_source(&self, rel: &str) -> Result<PathBuf, String> {
        let root = self
            .source_dir
            .canonicalize()
            .map_err(|e| format!("cannot resolve source directory: {e}"))?;
        let candidate = root.join(rel.trim_start_matches('/'));
        let canonical = candidate
            .canonicalize()
            .map_err(|e| format!("\"{rel}\" not found under the source directory: {e}"))?;
        if !canonical.starts_with(&root) {
            return Err(format!("\"{rel}\" escapes the source directory — refusing to read it"));
        }
        Ok(canonical)
    }

    fn list_files(&self, arguments: &Value) -> String {
        // Strip prefixes against the CANONICAL root consistently, whether `start` is the root
        // itself or a canonicalized subpath from `resolve_in_source` -- comparing a canonical
        // path against the original (possibly symlinked, e.g. /tmp -> /private/tmp on macOS)
        // `source_dir` would silently strip nothing and return an empty listing.
        let root = match self.source_dir.canonicalize() {
            Ok(p) => p,
            Err(e) => return format!("cannot resolve source directory: {e}"),
        };
        let subpath = arguments["path"].as_str().unwrap_or("");
        let start = if subpath.is_empty() {
            root.clone()
        } else {
            match self.resolve_in_source(subpath) {
                Ok(p) => p,
                Err(e) => return e,
            }
        };

        let mut entries: Vec<String> = walkdir::WalkDir::new(&start)
            .into_iter()
            .filter_entry(|e| {
                let n = e.file_name().to_string_lossy();
                !n.starts_with('.') && !IGNORED_DIRS.contains(&n.as_ref())
            })
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.path().strip_prefix(&root).ok().map(|p| p.display().to_string()))
            .collect();
        entries.sort();

        if entries.is_empty() {
            return "(no files found)".to_string();
        }
        if entries.len() > MAX_LISTING_ENTRIES {
            let omitted = entries.len() - MAX_LISTING_ENTRIES;
            entries.truncate(MAX_LISTING_ENTRIES);
            entries.push(format!("... [{omitted} more file(s) omitted — list a subdirectory to narrow this down]"));
        }
        entries.join("\n")
    }

    fn read_file(&self, arguments: &Value) -> String {
        let Some(rel) = arguments["path"].as_str() else {
            return "error: \"path\" argument is required".to_string();
        };
        let path = match self.resolve_in_source(rel) {
            Ok(p) => p,
            Err(e) => return e,
        };
        match std::fs::read_to_string(&path) {
            Ok(content) if content.len() > MAX_READ_BYTES => {
                format!(
                    "{}\n... [truncated at {MAX_READ_BYTES} bytes — read a narrower range or a \
                     different file if you need more]",
                    &content[..MAX_READ_BYTES]
                )
            }
            Ok(content) => content,
            Err(e) => format!("error reading \"{rel}\": {e} (binary or non-UTF-8 files can't be read this way)"),
        }
    }

    fn read_artifact(&self, arguments: &Value) -> String {
        let Some(name) = arguments["name"].as_str() else {
            return "error: \"name\" argument is required".to_string();
        };
        self.ws
            .get_artifact(name)
            .unwrap_or_else(|_| format!("no artifact named \"{name}\" exists yet"))
    }

    fn write_artifact(&self, arguments: &Value) -> String {
        let (Some(name), Some(content)) = (arguments["name"].as_str(), arguments["content"].as_str()) else {
            return "error: \"name\" and \"content\" arguments are both required".to_string();
        };
        match self.ws.emit_artifact(name, content) {
            Ok(()) => format!("wrote {} byte(s) to artifact \"{name}\"", content.len()),
            Err(e) => format!("error writing artifact \"{name}\": {e}"),
        }
    }

    fn fan_out(&self, arguments: &Value) -> String {
        let Some(tasks) = arguments["tasks"].as_array() else {
            return "error: \"tasks\" argument (an array of task strings) is required".to_string();
        };
        let tasks: Vec<String> = tasks.iter().filter_map(|t| t.as_str().map(str::to_string)).collect();
        if tasks.is_empty() {
            return "error: \"tasks\" must contain at least one task string".to_string();
        }

        let results: Vec<Result<String, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = tasks
                .iter()
                .map(|task| {
                    s.spawn(move || {
                        self.manager
                            .run_agentic(&self.agent, &self.system, task, self, self.max_turns)
                            .map_err(|e| e.to_string())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err("sub-task thread panicked".into())))
                .collect()
        });

        results
            .into_iter()
            .enumerate()
            .map(|(i, r)| match r {
                Ok(text) => format!("## Result {}\n{text}", i + 1),
                Err(e) => format!("## Result {} (failed)\n{e}", i + 1),
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

impl<'a> Toolbox for PipelineToolbox<'a> {
    fn tool_defs(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "list_files".into(),
                description: "List file paths under the V1 source directory, optionally \
                    scoped to a subdirectory. Use this to see the project's shape before \
                    deciding what to read."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Subdirectory to list, relative to the source root. Omit to list the whole tree.",
                        }
                    },
                }),
            },
            ToolDef {
                name: "read_file".into(),
                description: "Read one file's full text content from the V1 source directory.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to the source root."},
                    },
                    "required": ["path"],
                }),
            },
            ToolDef {
                name: "read_artifact".into(),
                description: "Read a pipeline artifact another stage already produced — e.g. \
                    objective_contract.md, schema.rs, inductive_analysis.md, \
                    refined_test_matrix.json — instead of needing it pasted into your prompt."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Artifact name, e.g. \"schema.rs\"."},
                    },
                    "required": ["name"],
                }),
            },
            ToolDef {
                name: "write_artifact".into(),
                description: "Write (or overwrite) a named pipeline artifact directly, instead \
                    of only returning it as your final answer."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "content": {"type": "string"},
                    },
                    "required": ["name", "content"],
                }),
            },
            ToolDef {
                name: "fan_out".into(),
                description: "Delegate two or more genuinely independent sub-tasks (e.g. \
                    analyzing unrelated subsystems of a large source tree) to concurrent \
                    copies of yourself, each with the same role, and get their answers back. \
                    Not for a single sequential train of thought — only for work that's \
                    actually separable."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "tasks": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Independent sub-task descriptions to run in parallel.",
                        },
                    },
                    "required": ["tasks"],
                }),
            },
        ]
    }

    fn call(&self, name: &str, arguments: &Value) -> String {
        match name {
            "list_files" => self.list_files(arguments),
            "read_file" => self.read_file(arguments),
            "read_artifact" => self.read_artifact(arguments),
            "write_artifact" => self.write_artifact(arguments),
            "fan_out" => self.fan_out(arguments),
            other => format!("error: unknown tool \"{other}\""),
        }
    }
}

/// True if `dir` contains at least one readable, non-ignored file — a cheap existence check
/// that replaces eagerly reading and concatenating the whole tree just to confirm it isn't
/// empty (see `workspace::read_source_dir`, still used for the one-shot legacy fan-out path).
pub fn source_dir_has_readable_files(dir: &Path) -> bool {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !n.starts_with('.') && !IGNORED_DIRS.contains(&n.as_ref())
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .any(|e| std::fs::read_to_string(e.path()).is_ok())
}

#[cfg(test)]
mod tests {
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
            vec!["list_files", "read_file", "read_artifact", "write_artifact", "fan_out"]
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
}
