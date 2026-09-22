use serde::Deserialize;
use std::io;
use std::path::PathBuf;

#[derive(Deserialize, Default)]
struct ConfigFile {
    queue: Option<QueueSection>,
}

#[derive(Deserialize)]
struct QueueSection {
    url: Option<String>,
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".config")
        });
    base.join("rewriter")
}

/// An empty REWRITER_QUEUE_URL forces local mode even when config.toml names a server.
pub fn server_url() -> Option<String> {
    let url = match std::env::var("REWRITER_QUEUE_URL") {
        Ok(v) => v,
        Err(_) => {
            let text = std::fs::read_to_string(config_dir().join("config.toml")).ok()?;
            toml::from_str::<ConfigFile>(&text).ok()?.queue?.url?
        }
    };
    Some(url.trim_end_matches('/').to_string()).filter(|u| !u.is_empty())
}

pub fn token() -> io::Result<String> {
    if let Ok(t) = std::env::var("REWRITER_QUEUE_TOKEN") {
        return Ok(t.trim().to_string());
    }
    let path = config_dir().join("queue-token");
    std::fs::read_to_string(&path)
        .map(|t| t.trim().to_string())
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            io::Error::other(format!(
                "no queue token: set REWRITER_QUEUE_TOKEN or put one in {}",
                path.display()
            ))
        })
}
