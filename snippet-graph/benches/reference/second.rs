use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KvError {
    #[error("IO error at {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("key not found: {key}")]
    KeyNotFound { key: String },
    #[error("invalid key: {key:?}")]
    InvalidKey { key: String },
}

pub struct KvLog {
    pub path: PathBuf,
}

fn encode(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

fn decode(v: &str) -> String {
    let mut out = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(o) => out.push(o),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn check_key(key: &str) -> Result<(), KvError> {
    if key.is_empty() || key.contains('\t') || key.contains('\n') {
        return Err(KvError::InvalidKey {
            key: key.to_string(),
        });
    }
    Ok(())
}

impl KvLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        KvLog { path: path.into() }
    }

    fn io(&self, source: std::io::Error) -> KvError {
        KvError::Io {
            path: self.path.display().to_string(),
            source,
        }
    }

    fn append_line(&self, line: &str) -> Result<(), KvError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| self.io(e))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| self.io(e))?;
        f.write_all(line.as_bytes()).map_err(|e| self.io(e))
    }

    fn lines(&self) -> Result<Vec<String>, KvError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&self.path).map_err(|e| self.io(e))?;
        Ok(text.lines().map(|l| l.to_string()).collect())
    }

    fn live(&self) -> Result<std::collections::BTreeMap<String, String>, KvError> {
        let mut map = std::collections::BTreeMap::new();
        for line in self.lines()? {
            match line.split_once('\t') {
                Some((k, v)) => {
                    map.insert(k.to_string(), decode(v));
                }
                None => {
                    map.remove(&line);
                }
            }
        }
        Ok(map)
    }

    pub fn append(&self, key: &str, value: &str) -> Result<(), KvError> {
        check_key(key)?;
        self.append_line(&format!("{key}\t{}\n", encode(value)))
    }

    pub fn delete(&self, key: &str) -> Result<(), KvError> {
        check_key(key)?;
        self.append_line(&format!("{key}\n"))
    }

    pub fn get(&self, key: &str) -> Result<String, KvError> {
        self.live()?.remove(key).ok_or_else(|| KvError::KeyNotFound {
            key: key.to_string(),
        })
    }

    pub fn keys(&self) -> Result<Vec<String>, KvError> {
        Ok(self.live()?.into_keys().collect())
    }

    pub fn line_count(&self) -> Result<usize, KvError> {
        Ok(self.lines()?.len())
    }

    pub fn compact(&self) -> Result<usize, KvError> {
        if !self.path.exists() {
            return Ok(0);
        }
        let old = self.lines()?.len();
        let live = self.live()?;
        let mut text = String::new();
        for (k, v) in &live {
            text.push_str(&format!("{k}\t{}\n", encode(v)));
        }
        fs::write(&self.path, text).map_err(|e| self.io(e))?;
        Ok(old - live.len())
    }
}
