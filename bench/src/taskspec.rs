use crate::types::TaskSpec;
use std::fs;
use std::path::{Path, PathBuf};

pub fn load_one(dir: &Path) -> Result<TaskSpec, String> {
    let dir_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let toml_path = dir.join("task.toml");
    let text = fs::read_to_string(&toml_path).map_err(|e| {
        format!(
            "task '{dir_name}': cannot read {}: {e}",
            toml_path.display()
        )
    })?;
    let spec: TaskSpec =
        toml::from_str(&text).map_err(|e| format!("task '{dir_name}': invalid task.toml: {e}"))?;
    if spec.name != dir_name {
        return Err(format!(
            "task '{dir_name}': task.toml name is '{}' but the directory is '{dir_name}'",
            spec.name
        ));
    }
    let name = &spec.name;
    if let Some(bin) = &spec.binary {
        if bin.is_empty() || bin.contains(['/', '\\']) || bin.starts_with('.') {
            return Err(format!(
                "task '{name}': binary '{bin}' must be a bare executable name"
            ));
        }
    }
    if !dir.join("source").is_dir() {
        return Err(format!("task '{name}': missing source/ directory"));
    }
    if !dir.join("mission.md").is_file() {
        return Err(format!("task '{name}': missing mission.md"));
    }
    let acceptance = dir.join("acceptance");
    if !acceptance.is_dir() {
        return Err(format!("task '{name}': missing acceptance/ directory"));
    }
    if spec.cargo_tests && !has_rs_files(&acceptance.join("tests")) {
        return Err(format!(
            "task '{name}': cargo_tests is true but acceptance/tests/ has no *.rs files"
        ));
    }
    if !spec.cargo_tests && spec.checks.is_empty() {
        return Err(format!(
            "task '{name}': no [[check]] entries and cargo_tests is false"
        ));
    }
    if !spec.checks.is_empty() && spec.binary.is_none() {
        return Err(format!("task '{name}': [[check]] entries need a `binary`"));
    }
    for side in ["good", "bad"] {
        let p = dir.join("solutions").join(side);
        if !p.join("Cargo.toml").is_file() {
            return Err(format!(
                "task '{name}': missing solutions/{side}/ (a full crate with Cargo.toml)"
            ));
        }
    }
    Ok(spec)
}

pub fn load_all(dir: &Path) -> Result<Vec<(TaskSpec, PathBuf)>, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("cannot read tasks directory {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        let hidden = path
            .file_name()
            .map(|n| n.to_string_lossy().starts_with('.'))
            .unwrap_or(true);
        if !path.is_dir() || hidden {
            continue;
        }
        out.push((load_one(&path)?, path));
    }
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok(out)
}

fn has_rs_files(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "rs"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("bench-taskspec-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_task(root: &Path, name: &str, toml: &str) -> PathBuf {
        let d = root.join(name);
        for sub in ["source", "acceptance", "solutions/good", "solutions/bad"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        fs::write(d.join("solutions/good/Cargo.toml"), "").unwrap();
        fs::write(d.join("solutions/bad/Cargo.toml"), "").unwrap();
        fs::write(d.join("mission.md"), "m").unwrap();
        fs::write(d.join("task.toml"), toml).unwrap();
        d
    }

    const OK: &str = "name = \"t\"\ndescription = \"d\"\nbinary = \"t\"\n[[check]]\nname = \"c\"\n";

    #[test]
    fn a_complete_task_loads() {
        let root = scratch("ok");
        let d = make_task(&root, "t", OK);
        assert_eq!(load_one(&d).unwrap().name, "t");
        let all = load_all(&root).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1, d);
    }

    #[test]
    fn missing_pieces_are_named() {
        let root = scratch("missing");
        let d = make_task(&root, "t", OK);
        fs::remove_file(d.join("mission.md")).unwrap();
        let e = load_one(&d).unwrap_err();
        assert!(e.contains("'t'") && e.contains("mission.md"), "{e}");
        fs::write(d.join("mission.md"), "m").unwrap();
        fs::remove_dir_all(d.join("solutions/bad")).unwrap();
        let e = load_one(&d).unwrap_err();
        assert!(e.contains("solutions/bad"), "{e}");
    }

    #[test]
    fn cargo_tests_require_test_files() {
        let root = scratch("ct");
        let d = make_task(
            &root,
            "t",
            "name = \"t\"\ndescription = \"d\"\ncargo_tests = true\n",
        );
        let e = load_one(&d).unwrap_err();
        assert!(e.contains("acceptance/tests"), "{e}");
        fs::create_dir_all(d.join("acceptance/tests")).unwrap();
        fs::write(d.join("acceptance/tests/a.rs"), "").unwrap();
        assert!(load_one(&d).is_ok());
    }

    #[test]
    fn a_name_that_differs_from_the_directory_is_rejected() {
        let root = scratch("name");
        let d = make_task(&root, "other", OK);
        assert!(load_one(&d).unwrap_err().contains("directory"));
    }

    #[test]
    fn a_binary_name_with_a_path_is_rejected() {
        let root = scratch("bin");
        for bad in ["../x", "a/b", ".hidden", ""] {
            let d = make_task(
                &root,
                "t",
                &OK.replace("binary = \"t\"", &format!("binary = \"{bad}\"")),
            );
            assert!(
                load_one(&d).unwrap_err().contains("bare executable"),
                "{bad}"
            );
        }
    }

    #[test]
    fn load_all_is_sorted_and_reports_the_broken_task() {
        let root = scratch("all");
        make_task(&root, "b", &OK.replace("\"t\"", "\"b\""));
        make_task(&root, "a", &OK.replace("\"t\"", "\"a\""));
        let names: Vec<String> = load_all(&root)
            .unwrap()
            .into_iter()
            .map(|t| t.0.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
        fs::remove_file(root.join("a/task.toml")).unwrap();
        assert!(load_all(&root).unwrap_err().contains("'a'"));
    }
}
