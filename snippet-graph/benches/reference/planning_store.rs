use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("IO error at {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("note not found: {section}/{slug}")]
    NoteNotFound { section: String, slug: String },
    #[error("phase not found: {name}")]
    PhaseNotFound { name: String },
}

pub struct ProjectStore {
    pub root: PathBuf,
}

fn io_err(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn ensure_parent(path: &Path) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<(), StoreError> {
    ensure_parent(path)?;
    fs::write(path, content).map_err(|e| io_err(path, e))
}

fn read_file(path: &Path) -> Result<String, StoreError> {
    fs::read_to_string(path).map_err(|e| io_err(path, e))
}

fn list_md_files(dir: &Path) -> Result<Vec<String>, StoreError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| io_err(dir, e))? {
        let path = entry.map_err(|e| io_err(dir, e))?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn walk_md_files(base: &Path, dir: &Path) -> Result<Vec<String>, StoreError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| io_err(dir, e))? {
        let path = entry.map_err(|e| io_err(dir, e))?.path();
        if path.is_dir() {
            out.extend(walk_md_files(base, &path)?);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let rel = path.strip_prefix(base).unwrap_or(&path).with_extension("");
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out.sort();
    Ok(out)
}

impl ProjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ProjectStore { root: root.into() }
    }

    pub fn get_mission(&self) -> Result<String, StoreError> {
        let path = self.root.join("mission.md");
        if !path.exists() {
            return Ok(
                "# Mission\n\n*(not set — use set_mission to define the collapse objective)*\n"
                    .to_string(),
            );
        }
        read_file(&path)
    }

    pub fn set_mission(&self, content: &str) -> Result<(), StoreError> {
        write_file(&self.root.join("mission.md"), content)
    }

    pub fn set_phase(&self, name: &str, status: &str, description: &str) -> Result<(), StoreError> {
        let body = format!("# Phase: {name}\n\n**Status:** {status}\n\n{description}\n");
        write_file(&self.root.join("phases").join(format!("{name}.md")), &body)
    }

    pub fn get_phase(&self, name: &str) -> Result<String, StoreError> {
        let path = self.root.join("phases").join(format!("{name}.md"));
        if !path.exists() {
            return Err(StoreError::PhaseNotFound {
                name: name.to_string(),
            });
        }
        read_file(&path)
    }

    pub fn list_phases(&self) -> Result<Vec<String>, StoreError> {
        list_md_files(&self.root.join("phases"))
    }

    pub fn write_note(&self, section: &str, slug: &str, content: &str) -> Result<(), StoreError> {
        write_file(
            &self.root.join("notes").join(section).join(format!("{slug}.md")),
            content,
        )
    }

    pub fn read_note(&self, section: &str, slug: &str) -> Result<String, StoreError> {
        let path = self.root.join("notes").join(section).join(format!("{slug}.md"));
        if !path.exists() {
            return Err(StoreError::NoteNotFound {
                section: section.to_string(),
                slug: slug.to_string(),
            });
        }
        read_file(&path)
    }

    pub fn list_notes(&self, section: Option<&str>) -> Result<Vec<String>, StoreError> {
        let notes = self.root.join("notes");
        match section {
            Some(s) => Ok(list_md_files(&notes.join(s))?
                .into_iter()
                .map(|n| format!("{s}/{n}"))
                .collect()),
            None => walk_md_files(&notes, &notes),
        }
    }

    pub fn search_notes(&self, query: &str) -> Result<Vec<(String, String)>, StoreError> {
        let notes = self.root.join("notes");
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for rel in walk_md_files(&notes, &notes)? {
            let content = read_file(&notes.join(format!("{rel}.md")))?;
            let lines: Vec<&str> = content
                .lines()
                .filter(|l| l.to_lowercase().contains(&q))
                .take(3)
                .collect();
            if !lines.is_empty() {
                out.push((rel, lines.join("\n")));
            }
        }
        Ok(out)
    }
}
