use crate::proc::{collect, drain, kill_group, remove_secrets};
use crate::types::{Check, CheckOutcome, TaskSpec};
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const BUILD_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_SHOWN: usize = 300;
const MAX_OUTPUT: usize = 1024 * 1024;

struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir, String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rewriter-bench-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        Ok(TempDir(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn run_checks(spec: &TaskSpec, task_dir: &Path, crate_dir: &Path) -> Vec<CheckOutcome> {
    let tmp = match TempDir::new() {
        Ok(t) => t,
        Err(e) => return vec![fail("setup", e)],
    };
    let work = tmp.0.join("crate");
    if let Err(e) = copy_dir(crate_dir, &work) {
        return vec![fail(
            "setup",
            format!("cannot copy {}: {e}", crate_dir.display()),
        )];
    }

    if let Err(detail) = cargo_build(&work) {
        return vec![fail("build", detail)];
    }

    let mut outcomes = Vec::new();
    if !spec.checks.is_empty() {
        match &spec.binary {
            None => outcomes.push(fail("binary", "task has checks but no `binary`".into())),
            Some(bin) => {
                let path = work.join("target/debug").join(bin);
                if path.is_file() {
                    for check in &spec.checks {
                        outcomes.push(run_one_check(&path, check));
                    }
                } else {
                    outcomes.push(fail(
                        "binary",
                        format!("the build did not produce a binary named '{bin}'"),
                    ));
                }
            }
        }
    }

    if spec.cargo_tests {
        outcomes.extend(run_cargo_tests(&task_dir.join("acceptance/tests"), &work));
    }
    outcomes
}

fn fail(name: &str, detail: String) -> CheckOutcome {
    CheckOutcome {
        name: name.into(),
        passed: false,
        detail,
    }
}

fn cargo_command(work: &Path) -> Command {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    remove_secrets(&mut cmd);
    cmd.current_dir(work)
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        .env("CARGO_TARGET_DIR", work.join("target"))
        .env("CARGO_TERM_COLOR", "never");
    cmd
}

fn cargo_build(work: &Path) -> Result<(), String> {
    let mut cmd = cargo_command(work);
    cmd.args(["build", "--offline", "--message-format", "short"]);
    let out = run_with_timeout(cmd, None, BUILD_TIMEOUT)?;
    if out.code == Some(0) {
        Ok(())
    } else {
        Err(first_compiler_error(&String::from_utf8_lossy(&out.stderr)))
    }
}

fn first_compiler_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .find(|l| l.contains("error") && !l.starts_with("error: could not compile"))
        .or_else(|| stderr.lines().rfind(|l| !l.trim().is_empty()));
    match line {
        Some(l) => truncate(l.trim()),
        None => "cargo build failed without output".into(),
    }
}

fn run_one_check(binary: &Path, check: &Check) -> CheckOutcome {
    let mut cmd = Command::new(binary);
    cmd.args(&check.args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .current_dir(binary.parent().unwrap_or(Path::new("/")));
    let stdin = check.stdin.as_deref().map(str::as_bytes);
    match run_with_timeout(cmd, stdin, CHECK_TIMEOUT) {
        Err(e) => fail(&check.name, e),
        Ok(out) => match compare(check, &out) {
            None => CheckOutcome {
                name: check.name.clone(),
                passed: true,
                detail: String::new(),
            },
            Some(detail) => fail(&check.name, detail),
        },
    }
}

fn compare(check: &Check, out: &Captured) -> Option<String> {
    if let Some(exp) = &check.expect_stdout {
        if let Some(d) = diff_text("stdout", exp, &String::from_utf8_lossy(&out.stdout)) {
            return Some(d);
        }
    }
    if let Some(exp) = &check.expect_stderr {
        if let Some(d) = diff_text("stderr", exp, &String::from_utf8_lossy(&out.stderr)) {
            return Some(d);
        }
    }
    if let Some(exp) = check.expect_exit {
        if out.code != Some(exp) {
            let got = out
                .code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none (killed by a signal)".into());
            return Some(format!("exit code: got {got} != expected {exp}"));
        }
    }
    None
}

fn diff_text(label: &str, expected: &str, actual: &str) -> Option<String> {
    if expected == actual {
        return None;
    }
    Some(format!(
        "{label}: got {} != expected {}",
        escaped(actual),
        escaped(expected)
    ))
}

fn escaped(s: &str) -> String {
    truncate(&format!("{s:?}"))
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_SHOWN {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_SHOWN).collect();
    format!("{head}...")
}

fn run_with_timeout(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<Captured, String> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start process: {e}"))?;

    if let Some(data) = stdin {
        let mut pipe = child.stdin.take().expect("stdin was piped");
        let data = data.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    let out_rx = drain(child.stdout.take().expect("stdout was piped"), MAX_OUTPUT);
    let err_rx = drain(child.stderr.take().expect("stderr was piped"), MAX_OUTPUT);

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs_f64()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                kill_group(child.id());
                return Err(format!("cannot wait for process: {e}"));
            }
        }
    };
    kill_group(child.id());
    Ok(Captured {
        stdout: collect(out_rx),
        stderr: collect(err_rx),
        code: status.code(),
    })
}

fn run_cargo_tests(acceptance_tests: &Path, work: &Path) -> Vec<CheckOutcome> {
    let mut stems = Vec::new();
    let dest = work.join("tests");
    if let Err(e) = fs::create_dir_all(&dest) {
        return vec![fail("cargo_tests", format!("cannot create tests/: {e}"))];
    }
    let entries = match fs::read_dir(acceptance_tests) {
        Ok(e) => e,
        Err(e) => {
            return vec![fail(
                "cargo_tests",
                format!("cannot read {}: {e}", acceptance_tests.display()),
            )]
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "rs") {
            let file_name = entry.file_name();
            if let Err(e) = fs::copy(&path, dest.join(&file_name)) {
                return vec![fail(
                    "cargo_tests",
                    format!("cannot copy {}: {e}", path.display()),
                )];
            }
            stems.push(
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    stems.sort();

    let mut cmd = cargo_command(work);
    cmd.args(["test", "--offline", "--message-format", "short"]);
    for stem in &stems {
        cmd.args(["--test", stem]);
    }
    let out = match run_with_timeout(cmd, None, TEST_TIMEOUT) {
        Ok(o) => o,
        Err(e) => return vec![fail("cargo_tests", e)],
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.code == Some(0) {
        return vec![CheckOutcome {
            name: "cargo_tests".into(),
            passed: true,
            detail: String::new(),
        }];
    }
    let failures = parse_failed_tests(&stdout);
    if failures.is_empty() {
        return vec![fail("cargo_tests", first_compiler_error(&stderr))];
    }
    failures
}

fn parse_failed_tests(stdout: &str) -> Vec<CheckOutcome> {
    let names: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("test ")?.strip_suffix(" ... FAILED"))
        .collect();
    names
        .into_iter()
        .map(|name| {
            let header = format!("---- {name} stdout ----");
            let detail = stdout
                .lines()
                .skip_while(|l| *l != header)
                .skip(1)
                .take_while(|l| !l.is_empty())
                .filter(|l| !l.starts_with("note:"))
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
            let detail = if detail.is_empty() {
                "test failed".to_string()
            } else {
                truncate(&detail)
            };
            fail(name, detail)
        })
        .collect()
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("target" | ".git" | ".cargo" | "rust-toolchain" | "rust-toolchain.toml")
        ) {
            continue;
        }
        let kind = entry.file_type()?;
        let dest = to.join(&name);
        if kind.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod corpus;
