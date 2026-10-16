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

/// What to write when the model emits a `Cargo.toml` section. A complete manifest is taken as
/// it is. One without a `[package]` table (just a `[dependencies]` block, as models often
/// emit) is overlaid onto the manifest already on disk, so the package and its targets are
/// never lost (job #36: a bare `[dependencies]` file replaced the manifest and every later
/// cargo command failed with "no targets specified").
pub(crate) fn reconcile_manifest(existing: Option<&str>, incoming: &str) -> Result<String, String> {
    let theirs: toml::Value = incoming.parse().map_err(|e| format!("not valid TOML ({e})"))?;
    if theirs.get("package").is_some() {
        return Ok(incoming.to_string());
    }
    let mut base: toml::Value = existing
        .and_then(|e| e.parse().ok())
        .filter(|b: &toml::Value| b.get("package").is_some())
        .ok_or("it has no [package] table and there is no working manifest to add it to")?;
    for section in ["dependencies", "dev-dependencies"] {
        let Some(deps) = theirs.get(section).and_then(|d| d.as_table()) else { continue };
        let table = base
            .as_table_mut()
            .expect("a parsed manifest is a table")
            .entry(section)
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .ok_or_else(|| format!("the existing [{section}] is not a table"))?;
        for (name, spec) in deps {
            table.insert(name.clone(), spec.clone());
        }
    }
    toml::to_string(&base).map_err(|e| e.to_string())
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
    fn a_complete_manifest_replaces_the_existing_one() {
        let full = "[package]\nname = \"x\"\nversion = \"0.2.0\"\nedition = \"2021\"\n";
        assert_eq!(reconcile_manifest(Some(SEED), full).unwrap(), full);
    }

    #[test]
    fn a_bare_dependencies_block_is_added_to_the_existing_package() {
        let merged = reconcile_manifest(Some(SEED), "[dependencies]\nserde = \"1\"\n[dev-dependencies]\ncsv = \"1\"\n").unwrap();
        let parsed: toml::Value = merged.parse().unwrap();
        assert_eq!(parsed["package"]["name"].as_str(), Some("v2"));
        assert_eq!(parsed["dependencies"]["serde"].as_str(), Some("1"));
        assert_eq!(parsed["dev-dependencies"]["csv"].as_str(), Some("1"));
    }

    #[test]
    fn a_bare_block_with_nothing_to_add_it_to_is_refused() {
        assert!(reconcile_manifest(None, "[dependencies]\nserde = \"1\"\n").is_err());
        assert!(reconcile_manifest(Some("[dependencies]\n"), "[dependencies]\nserde = \"1\"\n").is_err());
    }

    #[test]
    fn a_manifest_without_dependencies_or_with_bad_toml_merges_nothing() {
        assert!(merge_dependencies(SEED, "[package]\nname = \"x\"\n").is_none());
        assert!(merge_dependencies(SEED, "not toml ===").is_none());
        assert!(merge_dependencies("not toml ===", "[dependencies]\nserde = \"1\"\n").is_none());
    }
}
