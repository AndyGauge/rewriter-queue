use crate::types::RunFile;
use std::path::{Path, PathBuf};

pub fn default_results_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("results")
}

pub fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("baseline.json")
}

pub fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.chars().all(|c| c == '.') {
        "run".into()
    } else {
        cleaned
    }
}

fn write_json(path: &Path, file: &RunFile) -> Result<(), String> {
    let text =
        serde_json::to_string_pretty(file).map_err(|e| format!("serialize run file: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text + "\n").map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn save_run_file(dir: &Path, file: &RunFile) -> Result<PathBuf, String> {
    let stamp = file.env.timestamp.replace(':', "-");
    let path = dir.join(format!("{stamp}-{}.json", sanitize_label(&file.label)));
    write_json(&path, file)?;
    Ok(path)
}

pub fn load_run_file(path: &Path) -> Result<RunFile, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

pub fn list_run_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    Ok(files)
}

pub fn promote_to(run_file_path: &Path, baseline: &Path) -> Result<PathBuf, String> {
    let file = load_run_file(run_file_path)?;
    write_json(baseline, &file)?;
    Ok(baseline.to_path_buf())
}

pub fn promote(run_file_path: &Path) -> Result<PathBuf, String> {
    promote_to(run_file_path, &baseline_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EnvStamp;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bench-results-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn file(label: &str, timestamp: &str) -> RunFile {
        RunFile {
            label: label.into(),
            env: EnvStamp {
                commit: "c".into(),
                version: "v".into(),
                providers: vec![],
                host: "h".into(),
                queue_binary_sha: "q".into(),
                orchestrator_binary_sha: "o".into(),
                timestamp: timestamp.into(),
            },
            records: vec![],
        }
    }

    #[test]
    fn a_saved_file_is_named_by_timestamp_and_label_and_loads_back() {
        let dir = temp("save");
        let f = file("my label/x", "2026-10-03T12:00:00Z");
        let path = save_run_file(&dir, &f).unwrap();
        assert_eq!(
            path.file_name().unwrap(),
            "2026-10-03T12-00-00Z-my-label-x.json"
        );
        assert_eq!(load_run_file(&path).unwrap(), f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_label_can_never_be_a_parent_or_current_directory() {
        for label in ["", ".", "..", "..."] {
            assert_eq!(sanitize_label(label), "run");
        }
        assert_eq!(sanitize_label("../x"), "..-x");
    }

    #[test]
    fn listing_is_sorted_by_name_ignores_other_files_and_tolerates_a_missing_dir() {
        let dir = temp("list");
        assert!(list_run_files(&dir).unwrap().is_empty());
        save_run_file(&dir, &file("b", "2026-10-03T13:00:00Z")).unwrap();
        save_run_file(&dir, &file("a", "2026-10-03T12:00:00Z")).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let names: Vec<String> = list_run_files(&dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["2026-10-03T12-00-00Z-a.json", "2026-10-03T13-00-00Z-b.json"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_a_bad_file_is_an_error_naming_the_file() {
        let dir = temp("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("x.json");
        std::fs::write(&bad, "{").unwrap();
        assert!(load_run_file(&bad).unwrap_err().contains("x.json"));
        assert!(load_run_file(&dir.join("missing.json")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promoting_copies_a_valid_run_file_to_the_baseline_and_rejects_an_invalid_one() {
        let dir = temp("promote");
        let f = file("l", "2026-10-03T12:00:00Z");
        let path = save_run_file(&dir, &f).unwrap();
        let baseline = dir.join("sub/baseline.json");
        promote_to(&path, &baseline).unwrap();
        assert_eq!(load_run_file(&baseline).unwrap(), f);
        std::fs::write(dir.join("junk.json"), "nope").unwrap();
        assert!(promote_to(&dir.join("junk.json"), &dir.join("b2.json")).is_err());
        assert!(!dir.join("b2.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_baseline_lives_in_the_bench_crate_directory() {
        assert!(baseline_path().ends_with("bench/baseline.json"));
    }
}
