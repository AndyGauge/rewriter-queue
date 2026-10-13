use crate::manager::Manager;
use std::path::Path;

pub(crate) const SCHEMA_REPAIR_ATTEMPTS: usize = 3;

/// Overlay the schema's `[dependencies]` onto the seed manifest. Returns `None` when the
/// schema declares none, or when either side is not valid TOML.
pub(crate) fn merge_dependencies(seed: &str, schema_manifest: &str) -> Option<String> {
    let mut base: toml::Value = seed.parse().ok()?;
    let theirs: toml::Value = schema_manifest.parse().ok()?;
    let deps = theirs.get("dependencies")?.as_table()?.clone();
    let table = base
        .as_table_mut()?
        .entry("dependencies")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()?;
    for (name, spec) in deps {
        table.insert(name, spec);
    }
    toml::to_string(&base).ok()
}

impl<'a> Manager<'a> {
    /// The V2 manifest with the schema's declared dependencies merged in, if it declares any.
    pub(crate) fn schema_manifest(&self, schema: &str) -> Option<String> {
        let sections = Self::parse_file_sections(Self::strip_fences(schema));
        let declared = sections.get("Cargo.toml")?;
        let seed = std::fs::read_to_string(self.ws.v2_dir(None).join("Cargo.toml")).ok()?;
        merge_dependencies(&seed, declared)
    }

    /// Compile the schema alone in a scratch crate. Empty string means it builds. A schema
    /// that does not build poisons every milestone gate downstream, because each
    /// implementer starts from it.
    pub(crate) fn schema_errors(&self, schema: &str) -> String {
        let v2_dir = self.ws.v2_dir(None);
        let Some(manifest) = self
            .schema_manifest(schema)
            .or_else(|| std::fs::read_to_string(v2_dir.join("Cargo.toml")).ok())
        else {
            return String::new();
        };

        let dir = self.ws.root.join("schema_check");
        let src = dir.join("src");
        let _ = std::fs::remove_dir_all(&src);
        if std::fs::create_dir_all(&src).is_err() || std::fs::write(dir.join("Cargo.toml"), manifest).is_err() {
            return String::new();
        }

        let sections = Self::parse_file_sections(Self::strip_fences(schema));
        for (rel, content) in &sections {
            if !rel.ends_with(".rs") {
                continue;
            }
            let target = if sections.len() == 1 && rel == "src/main.rs" { "src/lib.rs" } else { rel.as_str() };
            let path = dir.join(target);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::write(&path, content).is_err() {
                return String::new();
            }
        }

        let output = std::process::Command::new("cargo")
            .args(["check", "--manifest-path"])
            .arg(dir.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", Path::new(&v2_dir).join("target"))
            .output();
        match output {
            Ok(o) if !o.status.success() => {
                Self::cap_output(&String::from_utf8_lossy(&o.stderr), 80)
            }
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "[package]\nname = \"v2\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n";

    #[test]
    fn schema_dependencies_are_merged_into_the_seed_manifest() {
        let merged = merge_dependencies(SEED, "[dependencies]\nserde = \"1\"\n").unwrap();
        let parsed: toml::Value = merged.parse().unwrap();
        assert_eq!(parsed["package"]["name"].as_str(), Some("v2"));
        assert_eq!(parsed["dependencies"]["serde"].as_str(), Some("1"));
    }

    #[test]
    fn schema_dependencies_override_the_seed_entry_of_the_same_name() {
        let seed = format!("{SEED}serde = \"0.9\"\n");
        let merged = merge_dependencies(&seed, "[dependencies]\nserde = \"1\"\n").unwrap();
        let parsed: toml::Value = merged.parse().unwrap();
        assert_eq!(parsed["dependencies"]["serde"].as_str(), Some("1"));
    }

    #[test]
    fn a_manifest_without_dependencies_or_with_bad_toml_merges_nothing() {
        assert!(merge_dependencies(SEED, "[package]\nname = \"x\"\n").is_none());
        assert!(merge_dependencies(SEED, "not toml ===").is_none());
        assert!(merge_dependencies("not toml ===", "[dependencies]\nserde = \"1\"\n").is_none());
    }
}
