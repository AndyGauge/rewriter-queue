use chrono::Utc;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn write(&self, rel: &str, content: &str) -> std::io::Result<()> {
        let path = self.root.join(rel);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(path, content)
    }

    fn read(&self, rel: &str) -> std::io::Result<String> {
        fs::read_to_string(self.root.join(rel))
    }

    fn append(&self, rel: &str, content: &str) -> std::io::Result<()> {
        let path = self.root.join(rel);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let existing = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            String::new()
        };
        fs::write(path, format!("{existing}{content}"))
    }

    pub fn get_mission(&self) -> String {
        self.read("mission.md")
            .unwrap_or_else(|_| "*(no mission set)*".into())
    }

    pub fn set_agent_task(&self, agent: &str, task: &str) -> std::io::Result<()> {
        self.write(&format!("agents/{agent}/task.md"), task)
    }

    pub fn write_agent_output(&self, agent: &str, output: &str) -> std::io::Result<()> {
        self.write(&format!("agents/{agent}/output.md"), output)
    }

    pub fn log_agent_decision(&self, agent: &str, decision: &str) -> std::io::Result<()> {
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        self.append(
            &format!("agents/{agent}/decisions.md"),
            &format!("\n---\n**[{ts}]** {decision}\n"),
        )
    }

    /// Record a deviation without acting on it — deferred to the next phase.
    pub fn log_deviation(&self, agent: &str, description: &str) -> std::io::Result<()> {
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        self.append(
            "deviations.md",
            &format!("\n---\n**[{ts}] {agent}**\n\n{description}\n\n*Status: deferred — continue as planned*\n"),
        )
    }

    pub fn emit_artifact(&self, name: &str, content: &str) -> std::io::Result<()> {
        self.write(&format!("artifacts/{name}"), content)
    }

    pub fn get_artifact(&self, name: &str) -> std::io::Result<String> {
        std::fs::read_to_string(self.root.join("artifacts").join(name))
    }

    /// Memoize one unit of expensive work (an agent call) under `artifacts/synthesis/<id>`.
    /// A stage whose single call is too large/costly to redo wholesale on resume should be
    /// split into several small, uniquely-`id`'d checkpoints instead — a chain of steps
    /// (e.g. `dual_review/iter/{n}/worker`) or independent siblings off a shared context
    /// (e.g. `chunk/{i}/implement`) both resume for free this way: each id is looked up
    /// independently, so already-done ones return instantly and only the first missing one
    /// does real work. The id's own path IS the prev/root relationship — no separate lineage
    /// data structure is needed. Cheap, deterministic Rust-side work (formatting, splitting,
    /// the quality gate) is not checkpointed here — only real agent calls are worth the cost.
    pub fn checkpoint(
        &self,
        id: &str,
        f: impl FnOnce() -> Result<String, Box<dyn std::error::Error>>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let rel = format!("synthesis/{id}.md");
        if let Ok(cached) = self.get_artifact(&rel) {
            eprintln!("    [checkpoint] {id} (resuming from checkpoint)");
            return Ok(cached);
        }
        let out = f()?;
        self.emit_artifact(&rel, &out)?;
        Ok(out)
    }

    /// Memoize a decision that is NOT a pure function of already-checkpointed data — e.g.
    /// `parallel_degree()`, which reads live provider health and could legitimately answer
    /// differently on resume than it did originally, silently desyncing already-checkpointed
    /// chunk work from a re-derived split. Ordinary `checkpoint()` calls make every other
    /// branch in synthesis replay deterministically for free (same cached inputs -> same
    /// decision), so this "gate" primitive is reserved for that one kind of place — introduce
    /// it only where resuming could otherwise take a different branch than the original run.
    pub fn gate_usize(&self, id: &str, f: impl FnOnce() -> usize) -> std::io::Result<usize> {
        let rel = format!("synthesis/{id}.gate");
        if let Ok(cached) = self.get_artifact(&rel) {
            if let Ok(n) = cached.trim().parse() {
                eprintln!("    [gate] {id} = {n} (resuming from checkpoint)");
                return Ok(n);
            }
        }
        let n = f();
        self.emit_artifact(&rel, &n.to_string())?;
        Ok(n)
    }

    pub fn log_retrospective(&self, entry: &str) -> std::io::Result<()> {
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        self.append(
            "retrospective.md",
            &format!("\n---\n**[{ts}]**\n\n{entry}\n"),
        )
    }

    /// Read test matrix files, up to MAX_TOTAL bytes total.
    pub fn read_test_matrix(&self) -> std::io::Result<String> {
        const MAX_TOTAL: usize = 50_000;
        let dir = self.root.join("test-matrix");
        if !dir.exists() {
            return Ok("*(no test matrix)*".into());
        }

        let mut cases: Vec<(String, String)> = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().map_or(false, |e| e == "md") {
                let name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let content = fs::read_to_string(&path)?;
                cases.push((name, content));
            }
        }
        cases.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = String::new();
        let mut total = 0usize;
        for (_, content) in cases {
            if total + content.len() > MAX_TOTAL {
                out.push_str("\n[... remaining test cases omitted — size limit]\n");
                break;
            }
            out.push_str(&content);
            out.push_str("\n---\n\n");
            total += content.len();
        }
        Ok(out)
    }

    pub fn deviations_path(&self) -> PathBuf {
        self.root.join("deviations.md")
    }

    /// Root directory for a synthesis run's V2 crate draft. `variant` isolates one
    /// milestone's build from every other's -- used only when an explicit, non-default
    /// parallel fan-out wave puts two or more milestones' `cargo build`/`clippy`/`test`
    /// runs (and the quality gate's stale-file cleanup) in flight at the same time,
    /// where sharing one directory would race. `None` is the shared, default-sequential
    /// crate directory that the rest of the pipeline (and every milestone that only ever
    /// runs one-at-a-time) uses.
    pub fn v2_dir(&self, variant: Option<&str>) -> PathBuf {
        match variant {
            Some(v) => self.root.join("artifacts/fanout").join(v),
            None => self.root.join("artifacts/v2"),
        }
    }

    /// Seed a freshly-isolated milestone workspace with the current crate manifest
    /// before it starts its own quality-gate loop. Only the manifest is copied: like the
    /// shared sequential crate directory, an isolated one is scratch space the quality
    /// gate repopulates from that one milestone's own output every iteration, not a
    /// snapshot of sibling milestones' code -- see the scheduling comment in
    /// `synthesis.rs` for why a milestone never needs to see sibling code to build.
    pub fn seed_variant_workspace(&self, from: Option<&str>, variant: &str) -> std::io::Result<()> {
        let dst = self.v2_dir(Some(variant));
        fs::create_dir_all(&dst)?;
        let src_manifest = self.v2_dir(from).join("Cargo.toml");
        if src_manifest.exists() {
            fs::copy(&src_manifest, dst.join("Cargo.toml"))?;
        }
        Ok(())
    }
}

/// Directories that are never source, regardless of language.
const IGNORED_DIRS: &[&str] = &[
    "target", "node_modules", "dist", "build", "vendor", "venv", ".venv",
    "__pycache__", ".next", "coverage",
];

/// Walk a source directory and return concatenated source with file headers.
/// Language-agnostic: every non-ignored file is included as long as it's
/// readable as UTF-8 text. Binary files (images, lockfile blobs, etc.) are
/// skipped rather than failing the whole read.
pub fn read_source_dir(dir: &Path) -> std::io::Result<String> {
    const MAX_BYTES: usize = 200_000;
    let mut out = String::new();
    let mut total = 0usize;

    let mut files: Vec<_> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !n.starts_with('.') && !IGNORED_DIRS.contains(&n.as_ref())
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();
    files.sort_by_key(|e| e.path().to_path_buf());

    for entry in files {
        let path = entry.path();
        let rel = path.strip_prefix(dir).unwrap_or(path);
        let Ok(content) = fs::read_to_string(path) else {
            continue; // binary or non-UTF-8 file — not source we can feed a model
        };
        out.push_str(&format!("// === {} ===\n", rel.display()));
        if total + content.len() <= MAX_BYTES {
            out.push_str(&content);
            total += content.len();
        } else {
            out.push_str("// [omitted: combined source size limit reached]\n");
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use std::cell::Cell;

    fn temp_ws(tag: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("aoo-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Workspace::new(dir).unwrap()
    }

    #[test]
    fn checkpoint_runs_once_then_replays_from_cache() {
        let ws = temp_ws("checkpoint");
        let calls = Cell::new(0);

        let a = ws
            .checkpoint("step", || {
                calls.set(calls.get() + 1);
                Ok("first".into())
            })
            .unwrap();
        assert_eq!(a, "first");
        assert_eq!(calls.get(), 1);

        // Second call must not invoke f again — this is the resume path.
        let b = ws
            .checkpoint("step", || {
                calls.set(calls.get() + 1);
                Ok("second".into())
            })
            .unwrap();
        assert_eq!(b, "first");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn distinct_ids_do_not_collide() {
        let ws = temp_ws("checkpoint-distinct");
        let a = ws.checkpoint("chunk/0/implement", || Ok("a".into())).unwrap();
        let b = ws.checkpoint("chunk/1/implement", || Ok("b".into())).unwrap();
        assert_eq!(a, "a");
        assert_eq!(b, "b");
    }

    #[test]
    fn gate_pins_the_first_answer_despite_a_different_later_closure() {
        let ws = temp_ws("gate");
        let a = ws.gate_usize("fanout_degree", || 4).unwrap();
        assert_eq!(a, 4);

        // Simulates a resume where live state (e.g. provider health) would now answer
        // differently — the gate must still return the originally recorded value.
        let b = ws.gate_usize("fanout_degree", || 99).unwrap();
        assert_eq!(b, 4);
    }
}
