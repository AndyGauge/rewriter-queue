use crate::assemble::assemble;
use crate::model::Graph;
use crate::skeleton;
use std::path::{Path, PathBuf};

/// Parse every `.rs` file under `crate_dir/src` into one graph, keeping bodies,
/// with placements relative to the crate root.
pub fn import_crate(crate_dir: &Path) -> Result<Graph, String> {
    let mut files = Vec::new();
    collect(&crate_dir.join("src"), &mut files).map_err(|e| e.to_string())?;
    files.sort();
    let mut graph = Graph::default();
    for path in files {
        let source =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let rel = path.strip_prefix(crate_dir).map_err(|e| e.to_string())?;
        let parsed = skeleton::parse(rel, &source).map_err(|e| e.to_string())?;
        skeleton::merge(&mut graph, parsed);
    }
    Ok(graph)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Write the assembled graph back, changing only files whose rendered text
/// differs from what is on disk.
pub fn write_back(crate_dir: &Path, graph: &Graph) -> std::io::Result<()> {
    for (rel, text) in assemble(graph).files {
        let path = crate_dir.join(&rel);
        if std::fs::read_to_string(&path).is_ok_and(|disk| disk == text) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeId;
    use std::time::Duration;

    const A: &str = "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    one() + 1\n}\n";
    const B: &str = "pub struct S;\n\npub fn seven() -> u32 {\n    7\n}\n";

    fn temp_crate(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sg_import_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        dir
    }

    fn setup(name: &str) -> PathBuf {
        let dir = temp_crate(name);
        let g = skeleton::parse(Path::new("src/a.rs"), A).unwrap();
        let assembled_a = assemble(&g).files.remove(Path::new("src/a.rs")).unwrap();
        let g = skeleton::parse(Path::new("src/b.rs"), B).unwrap();
        let assembled_b = assemble(&g).files.remove(Path::new("src/b.rs")).unwrap();
        std::fs::write(dir.join("src/a.rs"), assembled_a).unwrap();
        std::fs::write(dir.join("src/b.rs"), assembled_b).unwrap();
        dir
    }

    fn read(dir: &Path, f: &str) -> String {
        std::fs::read_to_string(dir.join(f)).unwrap()
    }

    #[test]
    fn import_round_trip_keeps_assembled_files_byte_identical() {
        let dir = setup("roundtrip");
        let (a, b) = (read(&dir, "src/a.rs"), read(&dir, "src/b.rs"));
        let graph = import_crate(&dir).unwrap();
        assert!(graph.nodes.values().all(|n| n.is_filled()));
        write_back(&dir, &graph).unwrap();
        assert_eq!(read(&dir, "src/a.rs"), a);
        assert_eq!(read(&dir, "src/b.rs"), b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn import_changing_one_body_changes_only_that_file() {
        let dir = setup("onebody");
        let b = read(&dir, "src/b.rs");
        let mut graph = import_crate(&dir).unwrap();
        let id = graph
            .nodes
            .keys()
            .find(|k| k.0.ends_with("one"))
            .cloned()
            .unwrap_or_else(|| NodeId("one".into()));
        graph.nodes.get_mut(&id).unwrap().body = Some("{\n    100\n}".into());
        write_back(&dir, &graph).unwrap();
        assert!(read(&dir, "src/a.rs").contains("100"));
        assert_eq!(read(&dir, "src/b.rs"), b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn import_write_back_skips_unchanged_files() {
        let dir = setup("mtime");
        let before = std::fs::metadata(dir.join("src/b.rs"))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let graph = import_crate(&dir).unwrap();
        write_back(&dir, &graph).unwrap();
        let after = std::fs::metadata(dir.join("src/b.rs"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn import_write_back_reformats_files_not_in_assembled_form() {
        let dir = temp_crate("reformat");
        let messy = "pub fn   one() -> u32 {   1   }\n// note\n";
        std::fs::write(dir.join("src/a.rs"), messy).unwrap();
        let graph = import_crate(&dir).unwrap();
        write_back(&dir, &graph).unwrap();
        assert_ne!(read(&dir, "src/a.rs"), messy);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
