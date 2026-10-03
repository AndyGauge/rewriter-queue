use crate::types::EnvStamp;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNKNOWN: &str = "unknown";

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

pub fn git_commit() -> String {
    command_output("git", &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| UNKNOWN.into())
}

pub fn host_name() -> String {
    command_output("hostname", &[])
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| UNKNOWN.into())
}

pub fn provider_names(config_text: &str) -> Vec<String> {
    let Ok(value) = config_text.parse::<toml::Value>() else {
        return Vec::new();
    };
    value
        .get("providers")
        .and_then(toml::Value::as_array)
        .map(|providers| {
            providers
                .iter()
                .filter_map(|p| p.get("name").and_then(toml::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn configured_providers() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let path = PathBuf::from(home).join(".config/rewriter/config.toml");
    std::fs::read_to_string(path)
        .map(|t| provider_names(&t))
        .unwrap_or_default()
}

pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn parse_sha(output: &str) -> Option<String> {
    let first = output.split_whitespace().next()?;
    (first.len() == 64 && first.chars().all(|c| c.is_ascii_hexdigit())).then(|| first.to_string())
}

pub fn sha256_of(path: &Path) -> String {
    let p = path.to_string_lossy();
    command_output("sha256sum", &[&p])
        .or_else(|| command_output("shasum", &["-a", "256", &p]))
        .and_then(|o| parse_sha(&o))
        .unwrap_or_else(|| UNKNOWN.into())
}

fn binary_sha(explicit: Option<&Path>, name: &str) -> String {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| find_in_path(name))
        .map(|p| sha256_of(&p))
        .unwrap_or_else(|| UNKNOWN.into())
}

pub fn stamp(queue_bin: Option<&Path>, orchestrator_bin: Option<&Path>) -> EnvStamp {
    EnvStamp {
        commit: git_commit(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        providers: configured_providers(),
        host: host_name(),
        queue_binary_sha: binary_sha(queue_bin, "rewriter-queue"),
        orchestrator_binary_sha: binary_sha(orchestrator_bin, "ai-org-orchestrator"),
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_names_come_from_the_providers_tables() {
        let text = "[[providers]]\nname = \"spark\"\nurl = \"x\"\n\n[[providers]]\nname = \"claude\"\n\n[queue]\nurl = \"u\"\n";
        assert_eq!(provider_names(text), vec!["spark", "claude"]);
    }

    #[test]
    fn missing_or_malformed_config_gives_no_providers() {
        assert!(provider_names("").is_empty());
        assert!(provider_names("this is = = not toml").is_empty());
        assert!(provider_names("[queue]\nurl = \"u\"").is_empty());
    }

    #[test]
    fn a_sha_line_is_parsed_and_junk_is_not() {
        let sha = "a".repeat(64);
        assert_eq!(parse_sha(&format!("{sha}  file")), Some(sha));
        assert_eq!(parse_sha("short  file"), None);
    }

    #[test]
    fn a_missing_file_hashes_to_unknown_and_a_real_one_to_a_digest() {
        assert_eq!(sha256_of(Path::new("/definitely/not/here")), "unknown");
        let path = std::env::temp_dir().join(format!("bench-env-sha-{}", std::process::id()));
        std::fs::write(&path, "abc").unwrap();
        let sha = sha256_of(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            sha,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_stamp_carries_the_crate_version_and_a_timestamp() {
        let s = stamp(Some(Path::new("/nope/q")), Some(Path::new("/nope/o")));
        assert_eq!(s.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(s.queue_binary_sha, "unknown");
        assert_eq!(s.orchestrator_binary_sha, "unknown");
        assert!(chrono::DateTime::parse_from_rfc3339(&s.timestamp).is_ok());
        assert!(!s.commit.is_empty() && !s.host.is_empty());
    }
}
