use std::path::{Path, PathBuf};
use std::process::Command;

fn benches() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("benches")
}

fn run(name: &str, reference: &str) -> bool {
    run_oracle(name, name, reference)
}

fn run_oracle(name: &str, dirname: &str, reference: &str) -> bool {
    let b = benches();
    let oracle = std::fs::read_to_string(b.join("oracles").join(format!("{name}.rs"))).unwrap();
    let dir = std::env::temp_dir().join(format!("sg_oracle_{}_{}", dirname, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"oracle_check\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\nthiserror = \"1\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), format!("{reference}\n{oracle}")).unwrap();
    let out = Command::new("cargo")
        .arg("test")
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .unwrap();
    let ok = out.status.success();
    if ok {
        let _ = std::fs::remove_dir_all(&dir);
    }
    ok
}

fn reference(name: &str) -> String {
    std::fs::read_to_string(benches().join("reference").join(format!("{name}.rs"))).unwrap()
}

#[test]
fn planning_store_reference_passes() {
    assert!(run("planning_store", &reference("planning_store")));
}

#[test]
fn second_reference_passes() {
    assert!(run("second", &reference("second")));
}

#[test]
fn planning_store_mission_txt_mutant_fails() {
    let r = reference("planning_store");
    let mutated = r.replace("join(\"mission.md\")", "join(\"mission.txt\")");
    assert_ne!(r, mutated);
    assert!(!run_oracle(
        "planning_store",
        "planning_store_mutant",
        &mutated
    ));
}
