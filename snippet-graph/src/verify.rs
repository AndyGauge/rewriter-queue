use crate::assemble::Assembled;
use crate::model::NodeId;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Default)]
pub struct Report {
    pub ok: bool,
    /// Compiler messages attributed to the node whose span contains them.
    pub by_node: BTreeMap<NodeId, Vec<String>>,
    /// Messages that fall outside every node span.
    pub unattributed: Vec<String>,
}

/// Write `assembled.files` under `crate_dir` (an existing cargo crate), run
/// `cargo check --message-format=json`, and attribute each error to a node via
/// `assembled.spans`. Warnings are ignored.
pub fn check(crate_dir: &Path, assembled: &Assembled) -> std::io::Result<Report> {
    for (path, text) in &assembled.files {
        let full = crate_dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(full, text)?;
    }

    let output = Command::new("cargo")
        .args(["check", "--message-format=json"])
        .current_dir(crate_dir)
        .env_remove("CARGO_TARGET_DIR")
        .stdin(Stdio::null())
        .output()?;

    let mut report = Report {
        ok: output.status.success(),
        ..Report::default()
    };

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["reason"] != "compiler-message" {
            continue;
        }
        let message = &value["message"];
        if message["level"] != "error" {
            continue;
        }
        let text = message["rendered"]
            .as_str()
            .or_else(|| message["message"].as_str())
            .unwrap_or_default()
            .to_string();
        match primary_location(message) {
            Some((file, line)) => match find_node(assembled, Path::new(&file), line) {
                Some(id) => report.by_node.entry(id).or_default().push(text),
                None => report.unattributed.push(text),
            },
            None => report.unattributed.push(text),
        }
    }

    if !report.ok && report.by_node.is_empty() && report.unattributed.is_empty() {
        report
            .unattributed
            .push(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(report)
}

fn primary_location(message: &Value) -> Option<(String, usize)> {
    let span = message["spans"]
        .as_array()?
        .iter()
        .find(|s| s["is_primary"] == true)?;
    Some((
        span["file_name"].as_str()?.to_string(),
        span["line_start"].as_u64()? as usize,
    ))
}

fn find_node(assembled: &Assembled, file: &Path, line: usize) -> Option<NodeId> {
    assembled
        .spans
        .iter()
        .find(|(_, s)| s.file == file && s.start_line <= line && line <= s.end_line)
        .map(|(id, _)| id.clone())
}

fn write_files(crate_dir: &Path, assembled: &Assembled) -> std::io::Result<()> {
    for (path, text) in &assembled.files {
        let full = crate_dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(full, text)?;
    }
    Ok(())
}

fn attribute(
    assembled: &Assembled,
    message: &Value,
    by_node: &mut BTreeMap<NodeId, Vec<String>>,
    unattributed: &mut Vec<String>,
) {
    let text = message["rendered"]
        .as_str()
        .or_else(|| message["message"].as_str())
        .unwrap_or_default()
        .to_string();
    match primary_location(message) {
        Some((file, line)) => match find_node(assembled, Path::new(&file), line) {
            Some(id) => by_node.entry(id).or_default().push(text),
            None => unattributed.push(text),
        },
        None => unattributed.push(text),
    }
}

fn test_fn_name(signature: &str) -> Option<&str> {
    let rest = signature.split("fn ").nth(1)?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

fn test_names(assembled: &Assembled) -> BTreeMap<NodeId, String> {
    let mut names = BTreeMap::new();
    for (id, span) in &assembled.spans {
        let Some(text) = assembled.files.get(&span.file) else {
            continue;
        };
        let snippet: Vec<&str> = text
            .lines()
            .skip(span.start_line.saturating_sub(1))
            .take(span.end_line + 1 - span.start_line.min(span.end_line + 1))
            .collect();
        let snippet = snippet.join("\n");
        if !snippet.contains("#[test]") {
            continue;
        }
        if let Some(name) = test_fn_name(&snippet) {
            names.insert(id.clone(), name.to_string());
        }
    }
    names
}

fn leaf(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// A test that starts and never reports took the test process down (a stack
/// overflow or an abort); the last lines of stderr say how.
fn abort_text(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(6)..];
    format!(
        "test aborted the test process without reporting a result:\n{}",
        tail.join("\n")
    )
}

/// Write the files, run `cargo test`, and report each test's outcome with its
/// panic text. A test's id is the Test node whose function name matches.
/// `only` limits the run to those test nodes (all when empty). Compile errors
/// go to `build_errors` as in `check`.
pub fn run_tests(
    crate_dir: &Path,
    assembled: &Assembled,
    only: &[NodeId],
) -> std::io::Result<crate::model::TestReport> {
    use crate::model::{TestOutcome, TestReport};

    write_files(crate_dir, assembled)?;
    let names = test_names(assembled);

    let mut command = Command::new("cargo");
    command
        .args(["test", "--no-fail-fast", "--message-format=json"])
        .args(["--", "-Z", "unstable-options", "--format", "json"])
        .current_dir(crate_dir)
        .env("RUSTC_BOOTSTRAP", "1")
        .env_remove("CARGO_TARGET_DIR")
        .stdin(Stdio::null());
    for id in only {
        let name = names.get(id).map(String::as_str).unwrap_or(id.0.as_str());
        command.arg(name);
    }
    let output = command.output()?;

    let mut report = TestReport::default();
    let mut build_ok = true;
    let mut results: Vec<(String, bool)> = Vec::new();
    let mut failures: BTreeMap<String, String> = BTreeMap::new();
    let mut started: Vec<String> = Vec::new();

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match (value["type"].as_str(), value["reason"].as_str()) {
            (Some("test"), _) => {
                let Some(name) = value["name"].as_str() else {
                    continue;
                };
                match value["event"].as_str() {
                    Some("started") => started.push(name.to_string()),
                    Some("ok") => results.push((name.to_string(), true)),
                    Some("failed") | Some("timeout") => {
                        results.push((name.to_string(), false));
                        failures.insert(
                            name.to_string(),
                            value["stdout"].as_str().unwrap_or_default().to_string(),
                        );
                    }
                    _ => {}
                }
            }
            (_, Some("compiler-message")) if value["message"]["level"] == "error" => {
                attribute(
                    assembled,
                    &value["message"],
                    &mut report.build_errors,
                    &mut report.unattributed,
                );
            }
            (_, Some("build-finished")) if value["success"] == false => build_ok = false,
            _ => {}
        }
    }

    let aborted = abort_text(&String::from_utf8_lossy(&output.stderr));
    for name in started {
        if !results.iter().any(|(n, _)| *n == name) {
            results.push((name.clone(), false));
            failures.insert(name, aborted.clone());
        }
    }

    report.compiled = build_ok;
    if !report.compiled {
        if report.build_errors.is_empty() && report.unattributed.is_empty() {
            report
                .unattributed
                .push(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        return Ok(report);
    }

    for (name, passed) in results {
        let Some(id) = names
            .iter()
            .find(|(_, n)| n.as_str() == leaf(&name))
            .map(|(id, _)| id.clone())
        else {
            continue;
        };
        let message = if passed {
            String::new()
        } else {
            failures
                .get(&name)
                .map(|m| m.trim().to_string())
                .unwrap_or_default()
        };
        report.outcomes.push(TestOutcome {
            test: id,
            passed,
            message,
        });
    }
    Ok(report)
}

/// Run `cargo clippy` and attribute each warning or error to a node via spans.
/// `ok` is true only when there are none.
pub fn clippy(crate_dir: &Path, assembled: &Assembled) -> std::io::Result<Report> {
    write_files(crate_dir, assembled)?;

    let output = Command::new("cargo")
        .args(["clippy", "--message-format=json"])
        .current_dir(crate_dir)
        .env_remove("CARGO_TARGET_DIR")
        .stdin(Stdio::null())
        .output()?;

    let mut report = Report::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["reason"] != "compiler-message" {
            continue;
        }
        let message = &value["message"];
        let level = message["level"].as_str().unwrap_or_default();
        if level != "warning" && level != "error" {
            continue;
        }
        if level == "warning"
            && message["spans"]
                .as_array()
                .is_none_or(|spans| spans.is_empty())
        {
            continue;
        }
        attribute(
            assembled,
            message,
            &mut report.by_node,
            &mut report.unattributed,
        );
    }

    if !output.status.success() && report.by_node.is_empty() && report.unattributed.is_empty() {
        report
            .unattributed
            .push(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    report.ok =
        output.status.success() && report.by_node.is_empty() && report.unattributed.is_empty();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble::Span;
    use std::path::PathBuf;

    fn temp_crate(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sg_verify_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        dir
    }

    fn assembled(src: &str, spans: &[(&str, usize, usize)]) -> Assembled {
        let file = PathBuf::from("src/lib.rs");
        Assembled {
            files: BTreeMap::from([(file.clone(), src.to_string())]),
            spans: spans
                .iter()
                .map(|(id, a, b)| {
                    (
                        NodeId(id.to_string()),
                        Span {
                            file: file.clone(),
                            start_line: *a,
                            end_line: *b,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn passing_crate_is_ok() {
        let dir = temp_crate("pass");
        let a = assembled("pub fn good() -> u32 { 1 }\n", &[("good", 1, 1)]);
        let report = check(&dir, &a).unwrap();
        assert!(report.ok);
        assert!(report.by_node.is_empty() && report.unattributed.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failing_node_is_attributed() {
        let dir = temp_crate("fail");
        let src = "pub fn good() -> u32 { 1 }\npub fn bad() -> u32 { \"x\" }\n";
        let a = assembled(src, &[("good", 1, 1), ("bad", 2, 2)]);
        let report = check(&dir, &a).unwrap();
        assert!(!report.ok);
        assert_eq!(report.by_node.len(), 1);
        assert!(report.by_node.contains_key(&NodeId("bad".into())));
        assert!(report.unattributed.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn error_outside_spans_is_unattributed() {
        let dir = temp_crate("unattr");
        let src = "pub fn bad() -> u32 { \"x\" }\n";
        let a = assembled(src, &[]);
        let report = check(&dir, &a).unwrap();
        assert!(!report.ok);
        assert_eq!(report.unattributed.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    const TESTED: &str = "pub fn add(a: u32, b: u32) -> u32 { a + b }
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adds() { assert_eq!(add(1, 2), 3); }
    #[test]
    fn broken() { assert_eq!(add(1, 1), 3, \"sum was wrong\"); }
}
";

    fn tested() -> Assembled {
        assembled(
            TESTED,
            &[("add", 1, 1), ("t_adds", 5, 6), ("t_broken", 7, 8)],
        )
    }

    #[test]
    fn run_tests_reports_pass_and_fail() {
        let dir = temp_crate("tests");
        let report = run_tests(&dir, &tested(), &[]).unwrap();
        assert!(report.compiled);
        let adds = report
            .outcomes
            .iter()
            .find(|o| o.test.0 == "t_adds")
            .unwrap();
        assert!(adds.passed);
        let broken = report
            .outcomes
            .iter()
            .find(|o| o.test.0 == "t_broken")
            .unwrap();
        assert!(!broken.passed);
        assert!(broken.message.contains("sum was wrong"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_tests_ignores_lookalike_lines_in_test_output() {
        let src = "pub fn add(a: u32, b: u32) -> u32 { a + b }
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adds() { assert_eq!(add(1, 2), 3); }
    #[test]
    fn noisy() {
        println!(\"test adds ... FAILED\");
        println!(\"---- adds stdout ----\");
        panic!(\"real failure\");
    }
}
";
        let a = assembled(src, &[("add", 1, 1), ("t_adds", 5, 6), ("t_noisy", 7, 12)]);
        let dir = temp_crate("lookalike");
        let report = run_tests(&dir, &a, &[]).unwrap();
        let adds: Vec<_> = report.outcomes.iter().filter(|o| o.test.0 == "t_adds").collect();
        assert_eq!(adds.len(), 1);
        assert!(adds[0].passed);
        let noisy = report.outcomes.iter().find(|o| o.test.0 == "t_noisy").unwrap();
        assert!(!noisy.passed);
        assert!(noisy.message.contains("real failure"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_tests_reports_a_test_that_aborts_the_process() {
        let src = "pub fn f(n: u64) -> u64 { if n == u64::MAX { 0 } else { f(n + 1) + 1 } }
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fine() { assert_eq!(1, 1); }
    #[test]
    fn overflows() { assert_eq!(f(0), 1); }
}
";
        let a = assembled(src, &[("f", 1, 1), ("t_fine", 5, 6), ("t_overflows", 7, 8)]);
        let dir = temp_crate("abort");
        let report = run_tests(&dir, &a, &[]).unwrap();
        let overflows = report
            .outcomes
            .iter()
            .find(|o| o.test.0 == "t_overflows")
            .expect("an aborting test must still have an outcome");
        assert!(!overflows.passed);
        assert!(overflows.message.contains("aborted"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_tests_only_filters() {
        let dir = temp_crate("only");
        let report = run_tests(&dir, &tested(), &[NodeId("t_adds".into())]).unwrap();
        assert_eq!(report.outcomes.len(), 1);
        assert!(report.outcomes[0].passed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_tests_build_failure() {
        let dir = temp_crate("buildfail");
        let a = assembled("pub fn bad() -> u32 { \"x\" }\n", &[("bad", 1, 1)]);
        let report = run_tests(&dir, &a, &[]).unwrap();
        assert!(!report.compiled);
        assert!(report.build_errors.contains_key(&NodeId("bad".into())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clippy_attributes_warning() {
        let dir = temp_crate("clippy");
        let src = "pub fn clean() -> u32 { 1 }\npub fn dirty(v: &Vec<u32>) -> usize { v.len() }\n";
        let a = assembled(src, &[("clean", 1, 1), ("dirty", 2, 2)]);
        let report = clippy(&dir, &a).unwrap();
        assert!(!report.ok);
        assert!(report.by_node.contains_key(&NodeId("dirty".into())));
        assert!(!report.by_node.contains_key(&NodeId("clean".into())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clippy_clean_is_ok() {
        let dir = temp_crate("clippyok");
        let a = assembled("pub fn good() -> u32 { 1 }\n", &[("good", 1, 1)]);
        assert!(clippy(&dir, &a).unwrap().ok);
        let _ = std::fs::remove_dir_all(dir);
    }
}
