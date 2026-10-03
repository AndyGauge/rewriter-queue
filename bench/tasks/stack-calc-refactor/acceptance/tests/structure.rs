use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn has_at_least_three_source_files_besides_main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let others = files.iter().filter(|p| *p != &src.join("main.rs")).count();
    assert!(others >= 3, "found {others} source files besides src/main.rs, need at least 3");
}

#[test]
fn main_rs_is_at_most_40_lines() {
    let main = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let lines = fs::read_to_string(main).unwrap().lines().count();
    assert!(lines <= 40, "src/main.rs has {lines} lines, the limit is 40");
}
