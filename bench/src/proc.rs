use std::io::Read;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

pub const MAX_CAPTURE: usize = 64 * 1024 * 1024;
pub const COLLECT_GRACE: Duration = Duration::from_secs(5);

pub fn kill_group(pid: u32) {
    if let Ok(pgid) = i32::try_from(pid) {
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

/// Keeps draining past `cap` so a chatty process never blocks on a full pipe, but stores only
/// the first `cap` bytes.
pub fn drain<R: Read + Send + 'static>(mut reader: R, cap: usize) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        let _ = tx.send(kept);
    });
    rx
}

pub fn collect(rx: mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    rx.recv_timeout(COLLECT_GRACE).unwrap_or_default()
}

/// Generated code and its build scripts run with the caller's privileges; keep credentials
/// out of their environment.
pub fn remove_secrets(cmd: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if ["_KEY", "_TOKEN", "_SECRET", "_PASSWORD"]
            .iter()
            .any(|suffix| upper.ends_with(suffix))
        {
            cmd.env_remove(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_keeps_only_the_first_cap_bytes_but_reads_everything() {
        let data = vec![b'x'; 100_000];
        let got = collect(drain(std::io::Cursor::new(data), 1000));
        assert_eq!(got.len(), 1000);
    }

    #[test]
    fn secrets_are_removed_by_name_suffix() {
        std::env::set_var("BENCH_TEST_API_KEY", "x");
        std::env::set_var("BENCH_TEST_PLAIN", "y");
        let mut cmd = Command::new("env");
        remove_secrets(&mut cmd);
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("BENCH_TEST_API_KEY"));
        assert!(text.contains("BENCH_TEST_PLAIN=y"));
    }
}
