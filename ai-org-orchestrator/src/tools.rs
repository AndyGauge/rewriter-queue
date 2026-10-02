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

/// Wraps another `Toolbox`, hiding tools that don't apply to the wrapped role -- a real run
/// found the model using `write_artifact` to try to "deliver" its own milestone implementation
/// instead of returning it as its final answer (the only thing `parse_file_sections`/the patch
/// machinery actually reads), which `write_artifact`'s own description invites ("instead of
/// only returning it as your final answer"). Removing the tool is the fix, not a warning
/// competing against that description: AgenticImplementer's actual job (real/leave milestone
/// notes, read source/artifacts) never needed `write_artifact` or `fan_out` in the first place.
pub struct RestrictedToolbox<'a> {
    pub inner: &'a (dyn Toolbox + Sync),
    pub hidden: &'static [&'static str],
}

impl<'a> Toolbox for RestrictedToolbox<'a> {
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.inner.tool_defs().into_iter().filter(|t| !self.hidden.contains(&t.name.as_str())).collect()
    }

    fn call(&self, name: &str, arguments: &Value) -> String {
        if self.hidden.contains(&name) {
            return format!("error: \"{name}\" is not available to this role");
        }
        self.inner.call(name, arguments)
    }
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

    /// Announce what a milestone is building so concurrently-running siblings (each in their
    /// own isolated workspace, no visibility into each other otherwise) don't redefine it --
    /// see `Workspace::log_milestone_note` for why this is a single atomic append, not a tool
    /// wrapper around `write_artifact` (which overwrites, and would let two milestones' notes
    /// stomp each other).
    fn leave_note(&self, arguments: &Value) -> String {
        let (Some(milestone), Some(note)) =
            (arguments["milestone"].as_str(), arguments["note"].as_str())
        else {
            return "error: \"milestone\" and \"note\" arguments are both required".to_string();
        };
        match self.ws.log_milestone_note(milestone, note) {
            Ok(()) => "note recorded".to_string(),
            Err(e) => format!("error recording note: {e}"),
        }
    }

    fn report_finding(&self, arguments: &Value) -> String {
        let severity = arguments["severity"].as_str().unwrap_or("");
        if !matches!(severity, "info" | "warn" | "error") {
            return "error: \"severity\" must be one of \"info\", \"warn\", \"error\"".to_string();
        }
        let category = arguments["category"].as_str().unwrap_or("").trim();
        let kebab = !category.is_empty()
            && category.len() <= 60
            && category.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !category.starts_with('-')
            && !category.ends_with('-');
        if !kebab {
            return "error: \"category\" must be a short kebab-case label such as \"missing-tool\"".to_string();
        }
        let text = arguments["text"].as_str().unwrap_or("").trim();
        if text.is_empty() {
            return "error: \"text\" is required and must say what you noticed".to_string();
        }
        let evidence = arguments["evidence"].as_str().unwrap_or("").trim();
        if let Some(o) = self.ws.observer() {
            o.finding(&self.agent, severity, category, text, evidence);
        }
        "finding noted".to_string()
    }

    fn read_notes(&self) -> String {
        self.ws.read_milestone_notes()
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
                description: "Save a named pipeline artifact (e.g. schema.rs, \
                    objective_contract.md) that a LATER stage will read back with \
                    read_artifact. This is for artifacts other stages consume -- it is never \
                    an alternate way to deliver this call's own expected output. Your actual \
                    answer to this task always goes in your final response text, never here."
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
                name: "leave_note".into(),
                description: "Announce what you're building (a type/module/trait name and \
                    which file it lives in) so concurrently-running sibling milestones don't \
                    independently redefine it. Call this as soon as you know your names, before \
                    or while implementing — not only after you finish."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "milestone": {"type": "string", "description": "Your own milestone name, exactly as given in your task."},
                        "note": {"type": "string", "description": "What you're building, e.g. \"implementing FetchConfig struct in src/fetch_config.rs\"."},
                    },
                    "required": ["milestone", "note"],
                }),
            },
            ToolDef {
                name: "read_notes".into(),
                description: "Read every note left so far by concurrently-running or \
                    already-finished sibling milestones. Check this before defining a new \
                    top-level type/module/trait to avoid duplicating one a sibling already owns."
                    .into(),
                parameters: json!({"type": "object", "properties": {}}),
            },
            ToolDef {
                name: "report_finding".into(),
                description: "Report something you noticed that the people running this \
                    system would want to know -- a missing or misleading tool or skill, \
                    malformed input, a pattern that wasted effort, a risk in the source. It \
                    is recorded for later analysis and does not affect your task or answer."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "severity": {"type": "string", "enum": ["info", "warn", "error"]},
                        "category": {"type": "string", "description": "Short kebab-case label, e.g. \"missing-tool\"."},
                        "text": {"type": "string", "description": "What you noticed, in one or two sentences."},
                        "evidence": {"type": "string", "description": "A quote or file/line reference that shows it."},
                    },
                    "required": ["severity", "category", "text"],
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
            "leave_note" => self.leave_note(arguments),
            "read_notes" => self.read_notes(),
            "report_finding" => self.report_finding(arguments),
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
mod tests;
