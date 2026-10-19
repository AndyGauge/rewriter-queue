use crate::assemble::assemble;
use crate::model::Graph;
use crate::version::{Store, View};
use crate::view::to_graph;
use std::path::{Component, Path};
use std::process::Command;

const MANIFEST: &str =
    "[package]\nname = \"exported\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n";

/// Render `view` to `dir` as a buildable crate (assembled sources only). When
/// `commit` is set and `dir` is a git repository, commit the result with the
/// view and run ids in the message.
pub fn export_view(store: &dyn Store, view: &View, dir: &Path, commit: bool) -> Result<(), String> {
    let graph = to_graph(store, view).map_err(|e| e.to_string())?;
    export_graph(&graph, view, dir, commit)
}

fn export_graph(graph: &Graph, view: &View, dir: &Path, commit: bool) -> Result<(), String> {
    if commit && !dir.join(".git").exists() {
        return Err(format!("{} is not a git repository", dir.display()));
    }
    write_files(graph, dir)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let manifest = dir.join("Cargo.toml");
    if !manifest.exists() {
        std::fs::write(&manifest, MANIFEST).map_err(|e| format!("{}: {e}", manifest.display()))?;
    }
    if commit {
        git(dir, &["add", "-A"])?;
        let message = format!("view {} (run {})", view.id.0, view.run.0);
        git(dir, &["commit", "-m", &message])?;
    }
    Ok(())
}

pub(crate) fn write_files(graph: &Graph, dir: &Path) -> Result<(), String> {
    let assembled = assemble(graph);
    for (rel, text) in &assembled.files {
        if rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(format!(
                "refusing to write outside export dir: {}",
                rel.display()
            ));
        }
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Commit everything staged in `dir`, if anything changed. Returns whether a
/// commit was made.
pub(crate) fn commit_all(dir: &Path, message: &str) -> Result<bool, String> {
    git(dir, &["add", "-A"])?;
    let clean = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(dir)
        .status()
        .map_err(|e| format!("git diff: {e}"))?
        .success();
    if clean {
        return Ok(false);
    }
    git(
        dir,
        &[
            "-c",
            "user.name=sg",
            "-c",
            "user.email=sg@localhost",
            "commit",
            "-m",
            message,
        ],
    )?;
    Ok(true)
}

pub(crate) fn init_repo(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if dir.join(".git").exists() {
        return Ok(());
    }
    git(dir, &["init", "-q"])
}

fn git(dir: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git {}: {e}", args[0]))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed: {}{}",
            args[0],
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, NodeId, NodeKind, Placement};
    use crate::version::RunId;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn graph() -> Graph {
        let id = NodeId("f".into());
        let node = Node {
            id: id.clone(),
            kind: NodeKind::Function,
            signature: "pub fn f() -> u32".into(),
            body: Some("{ 1 }".into()),
            impl_header: None,
            placement: Placement {
                file: "src/lib.rs".into(),
                index: 0,
            },
        };
        let mut g = Graph::default();
        g.nodes.insert(id, node);
        g
    }

    fn view() -> View {
        View::new(
            RunId("r1".into()),
            None,
            BTreeMap::new(),
            BTreeMap::new(),
            0,
        )
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sg-export-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]).unwrap();
        git(dir, &["config", "user.name", "t"]).unwrap();
        git(dir, &["config", "user.email", "t@example.com"]).unwrap();
        git(dir, &["config", "commit.gpgsign", "false"]).unwrap();
    }

    fn log(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["log", "--format=%s"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn writes_files_and_manifest() {
        let dir = tmp("write");
        export_graph(&graph(), &view(), &dir, false).unwrap();
        let src = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(src.contains("pub fn f() -> u32"));
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"exported\""));
    }

    #[test]
    fn keeps_existing_manifest() {
        let dir = tmp("manifest");
        std::fs::write(dir.join("Cargo.toml"), "custom").unwrap();
        export_graph(&graph(), &view(), &dir, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("Cargo.toml")).unwrap(),
            "custom"
        );
    }

    #[test]
    fn commits_with_view_and_run() {
        let dir = tmp("commit");
        init_repo(&dir);
        let v = view();
        export_graph(&graph(), &v, &dir, true).unwrap();
        assert_eq!(log(&dir), format!("view {} (run r1)", v.id.0));
    }

    #[test]
    fn no_commit_when_not_requested() {
        let dir = tmp("nocommit");
        init_repo(&dir);
        export_graph(&graph(), &view(), &dir, false).unwrap();
        assert_eq!(log(&dir), "");
    }

    #[test]
    fn commit_outside_repo_is_error() {
        let dir = tmp("norepo");
        assert!(export_graph(&graph(), &view(), &dir, true).is_err());
    }
}
