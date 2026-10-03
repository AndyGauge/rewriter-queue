use super::*;

fn check(out: Option<&str>, err: Option<&str>, exit: Option<i32>) -> Check {
    Check {
        name: "c".into(),
        args: vec![],
        stdin: None,
        expect_stdout: out.map(Into::into),
        expect_stderr: err.map(Into::into),
        expect_exit: exit,
    }
}

fn captured(out: &str, err: &str, code: i32) -> Captured {
    Captured {
        stdout: out.into(),
        stderr: err.into(),
        code: Some(code),
    }
}

#[test]
fn matching_output_has_no_difference() {
    let c = check(Some("a\n"), Some(""), Some(0));
    assert_eq!(compare(&c, &captured("a\n", "", 0)), None);
}

#[test]
fn unset_fields_are_not_compared() {
    let c = check(None, None, None);
    assert_eq!(compare(&c, &captured("anything", "else", 9)), None);
}

#[test]
fn a_stray_newline_is_visible() {
    let c = check(Some("x\n"), None, None);
    let d = compare(&c, &captured("x\n\n", "", 0)).unwrap();
    assert_eq!(d, "stdout: got \"x\\n\\n\" != expected \"x\\n\"");
}

#[test]
fn the_first_difference_wins_in_stdout_stderr_exit_order() {
    let c = check(Some("a"), Some("b"), Some(1));
    let d = compare(&c, &captured("a", "x", 0)).unwrap();
    assert!(d.starts_with("stderr:"), "{d}");
    let c = check(Some("a"), Some("b"), Some(1));
    let d = compare(&c, &captured("a", "b", 0)).unwrap();
    assert!(d.starts_with("exit code: got 0 != expected 1"), "{d}");
}

#[test]
fn tabs_and_carriage_returns_are_escaped_and_long_text_is_cut() {
    let d = diff_text("stdout", "a\tb", "a\r\n").unwrap();
    assert!(d.contains("\\t") && d.contains("\\r\\n"), "{d}");
    let long = "x".repeat(1000);
    assert!(diff_text("stdout", "", &long).unwrap().len() < 400);
}

#[test]
fn a_process_that_outlives_the_timeout_is_killed() {
    let mut cmd = Command::new("sleep");
    cmd.arg("30");
    let start = Instant::now();
    let err = match run_with_timeout(cmd, None, Duration::from_millis(200)) {
        Err(e) => e,
        Ok(_) => panic!("expected a timeout"),
    };
    assert!(err.contains("timed out"), "{err}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_background_child_holding_the_pipes_does_not_hang_the_check() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "echo done; (sleep 30) & exit 0"]);
    let start = Instant::now();
    let out = run_with_timeout(cmd, None, Duration::from_secs(10)).unwrap();
    assert_eq!(out.stdout, b"done\n");
    assert!(start.elapsed() < Duration::from_secs(4));
}

#[test]
fn endless_output_is_capped_and_the_timeout_still_fires() {
    let mut cmd = Command::new("yes");
    cmd.arg("x");
    let err = match run_with_timeout(cmd, None, Duration::from_millis(500)) {
        Err(e) => e,
        Ok(_) => panic!("expected a timeout"),
    };
    assert!(err.contains("timed out"), "{err}");
}

#[test]
fn the_checked_binary_runs_in_its_own_directory_without_the_callers_environment() {
    let dir = std::env::temp_dir().join(format!("bench-acc-env-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("probe");
    fs::write(&bin, "#!/bin/sh\npwd\necho \"${HOME:-nohome}\"\n").unwrap();
    fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = {
        let mut cmd = Command::new(&bin);
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&dir);
        run_with_timeout(cmd, None, Duration::from_secs(5)).unwrap()
    };
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.ends_with("nohome\n"), "{text}");
    let outcome = run_one_check(
        &bin,
        &Check {
            name: "p".into(),
            args: vec![],
            stdin: None,
            expect_stdout: Some(format!(
                "{}\nnohome\n",
                fs::canonicalize(&dir).unwrap().display()
            )),
            expect_stderr: None,
            expect_exit: Some(0),
        },
    );
    assert!(outcome.passed, "{}", outcome.detail);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stdin_is_delivered_and_output_captured() {
    let cmd = Command::new("cat");
    let out = run_with_timeout(cmd, Some(b"hello\n"), Duration::from_secs(5)).unwrap();
    assert_eq!(out.stdout, b"hello\n");
    assert_eq!(out.code, Some(0));
}

#[test]
fn failed_tests_are_named_with_their_message() {
    let stdout = "running 2 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b stdout ----\n\nthread 'b' panicked at tests/x.rs:3:5:\nboom\nnote: run with `RUST_BACKTRACE=1`\n\nfailures:\n    b\n";
    let outcomes = parse_failed_tests(stdout);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].name, "b");
    assert!(!outcomes[0].passed);
}

#[test]
fn the_first_compiler_error_is_extracted() {
    let stderr = "   Compiling x v0.1.0\nsrc/main.rs:2:5: error[E0425]: cannot find value `y`\nerror: could not compile `x`\n";
    assert_eq!(
        first_compiler_error(stderr),
        "src/main.rs:2:5: error[E0425]: cannot find value `y`"
    );
}
