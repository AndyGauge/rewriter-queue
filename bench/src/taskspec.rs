use crate::types::TaskSpec;
use std::path::{Path, PathBuf};

pub fn load_one(dir: &Path) -> Result<TaskSpec, String> {
    let path = dir.join("task.toml");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

pub fn load_all(dir: &Path) -> Result<Vec<(TaskSpec, PathBuf)>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    let mut tasks = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.join("task.toml").is_file() {
            tasks.push((load_one(&path)?, path));
        }
    }
    tasks.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(tasks)
}
