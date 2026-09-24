use crate::manager::Manager;
use std::collections::HashMap;

const LINE_LIMIT: usize = 500;

impl<'a> Manager<'a> {
    /// Strip markdown code fences from LLM output, tolerating preamble text.
    pub(crate) fn strip_fences(s: &str) -> &str {
        let s = s.trim();
        let s = if let Some(pos) = s.find("```") {
            s[pos..].splitn(2, '\n').nth(1).unwrap_or(&s[pos..])
        } else {
            s
        };
        let s = s.trim_end();
        if s.ends_with("```") { s[..s.len() - 3].trim_end() } else { s }
    }

    pub(crate) fn cap_output(s: &str, max_lines: usize) -> String {
        let lines: Vec<&str> = s.lines().collect();
        if lines.len() <= max_lines {
            s.to_string()
        } else {
            format!(
                "{}\n[... {} more lines omitted]",
                lines[..max_lines].join("\n"),
                lines.len() - max_lines
            )
        }
    }

    /// Parse multi-file output using `// === path ===` section markers (`.rs` files and
    /// `Cargo.toml`). Returns a map of relative path -> content. Falls back to `src/main.rs`
    /// if no markers are present at all.
    ///
    /// Every marker line ends whatever section came before it, even a marker for a path we
    /// don't track (or trailing prose after the last real marker would too, if it had one) --
    /// a marker that isn't `.rs`/`Cargo.toml` still closes the previous section, it just
    /// doesn't open a new tracked one. Without this, a model emitting `// === Cargo.toml ===`
    /// after a `.rs` file (or closing prose with no marker at all) silently glues that content
    /// onto the end of the last `.rs` file's source, corrupting it -- this exact failure mode
    /// hit a real synthesis run (job #5): Cargo.toml content and a trailing prose summary both
    /// ended up appended inside `lib.rs`.
    ///
    /// A stray markdown code fence (the model wrapping a chunk in its own ```rust ... ```
    /// despite being told not to) also ends the current section, same as an unrecognized
    /// marker -- `strip_fences` only strips the outermost fence pair around the whole
    /// response, not interior ones between chunks, and a bare ``` (or ```lang) line is never
    /// legitimate Rust/TOML source, so it's safe to always treat as a boundary. This hit a
    /// second real run (job #6): a closing fence followed by "Now let's implement each
    /// module:" followed by another opening fence all glued onto the end of `main.rs`.
    pub(crate) fn parse_file_sections(code: &str) -> HashMap<String, String> {
        let mut sections: HashMap<String, String> = HashMap::new();
        let mut current_path: Option<String> = None;
        let mut current_content = String::new();

        let is_bare_fence = |line: &str| {
            let t = line.trim();
            t.starts_with("```") && t[3..].chars().all(|c| c.is_ascii_alphanumeric())
        };

        for line in code.lines() {
            if let Some(inner) = line
                .trim()
                .strip_prefix("// ===")
                .and_then(|s| s.strip_suffix("==="))
            {
                let path = inner.trim().to_string();
                if let Some(prev) = current_path.take() {
                    sections.insert(prev, std::mem::take(&mut current_content));
                }
                if path.ends_with(".rs") || path == "Cargo.toml" {
                    current_path = Some(path);
                }
                continue;
            }
            if is_bare_fence(line) {
                if let Some(prev) = current_path.take() {
                    sections.insert(prev, std::mem::take(&mut current_content));
                }
                continue;
            }
            if current_path.is_some() {
                current_content.push_str(line);
                current_content.push('\n');
            }
        }
        if let Some(path) = current_path {
            sections.insert(path, current_content);
        }
        if sections.is_empty() {
            sections.insert("src/main.rs".to_string(), code.to_string());
        }
        sections
    }

    /// Serialize file sections back to the canonical multi-file format.
    pub(crate) fn format_multi_file(sections: &HashMap<String, String>) -> String {
        let mut paths: Vec<&String> = sections.keys().collect();
        paths.sort();
        let mut out = String::new();
        for path in paths {
            out.push_str(&format!("// === {} ===\n", path));
            out.push_str(sections[path].as_str());
            out.push('\n');
        }
        out
    }

    /// Run cargo quality checks against the V2 crate draft.
    /// Handles multi-file output (`// === src/file.rs ===` markers).
    /// Applies `cargo fmt`, enforces 500-line limit per file, then checks
    /// build / clippy / test. Returns `(clean_source, errors)`.
    ///
    /// `variant` selects which crate directory this call builds against -- `None` is the
    /// shared default-sequential directory; `Some(id)` is one milestone's isolated
    /// workspace during an explicit parallel fan-out wave (see `Workspace::v2_dir`).
    pub(crate) fn quality_gate(&self, raw_code: &str, variant: Option<&str>) -> (String, String) {
        let code = Self::strip_fences(raw_code);
        let v2_dir = self.ws.v2_dir(variant);
        let manifest = v2_dir.join("Cargo.toml");

        if !manifest.exists() {
            return (code.to_string(), String::new());
        }

        let sections = Self::parse_file_sections(code);

        // Stale-file cleanup: an earlier iteration may have named a module differently
        // (e.g. `database.rs` renamed to `research_database.rs` on a later attempt) --
        // without this, the old file never goes away, and duplicate/dead modules
        // accumulate silently across iterations.
        for entry in walkdir::WalkDir::new(v2_dir.join("src"))
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&v2_dir)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .into_owned();
            if !sections.contains_key(&rel) {
                let _ = std::fs::remove_file(entry.path());
            }
        }

        let mut cargo_toml_issue: Option<String> = None;
        for (rel_path, content) in &sections {
            if rel_path == "Cargo.toml" {
                if let Err(e) = content.parse::<toml::Value>() {
                    // Don't let a malformed manifest (trailing prose, a stray code fence)
                    // overwrite a manifest that was actually working.
                    cargo_toml_issue = Some(format!(
                        "Cargo.toml did not parse as valid TOML ({e}) -- kept the existing \
                         manifest instead of overwriting it with this. Likely cause: trailing \
                         prose or a stray code fence after the TOML content. Emit Cargo.toml \
                         with nothing in it but valid TOML."
                    ));
                    continue;
                }
            }
            let full_path = v2_dir.join(rel_path);
            if let Some(parent) = full_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return (code.to_string(), format!("failed to create dir: {e}"));
                }
            }
            if let Err(e) = std::fs::write(&full_path, content) {
                return (code.to_string(), format!("failed to write {rel_path}: {e}"));
            }
        }

        let mf = manifest.to_string_lossy().into_owned();

        // fmt — auto-apply silently
        let _ = std::process::Command::new("cargo")
            .args(["fmt", "--manifest-path", &mf])
            .output();

        // Read back formatted sections
        let mut formatted: HashMap<String, String> = HashMap::new();
        for rel_path in sections.keys() {
            let full_path = v2_dir.join(rel_path);
            let content = std::fs::read_to_string(&full_path)
                .unwrap_or_else(|_| sections[rel_path].clone());
            formatted.insert(rel_path.clone(), content);
        }

        let clean = if formatted.len() == 1 && formatted.contains_key("src/main.rs") {
            formatted["src/main.rs"].clone()
        } else {
            Self::format_multi_file(&formatted)
        };

        let mut issues: Vec<String> = Vec::new();
        if let Some(issue) = cargo_toml_issue {
            issues.push(format!("## Cargo.toml\n{issue}"));
        }

        // 500-line modularity check
        let over_limit: Vec<String> = formatted
            .iter()
            .filter(|(_, content)| content.lines().count() > LINE_LIMIT)
            .map(|(path, content)| format!("{path} ({} lines)", content.lines().count()))
            .collect();
        if !over_limit.is_empty() {
            let mut list = over_limit.clone();
            list.sort();
            issues.push(format!(
                "## modularity\n\
                 Files exceeding the {LINE_LIMIT}-line limit must be split into submodules:\n\
                 {}\n\n\
                 Declare each submodule with `mod name;` in main.rs and emit its content \
                 prefixed with `// === src/name.rs ===`. Every file must be ≤{LINE_LIMIT} lines.",
                list.join("\n")
            ));
        }

        // build — short-circuit on failure
        let build = match std::process::Command::new("cargo")
            .args(["build", "--manifest-path", &mf])
            .output()
        {
            Err(e) => return (clean, format!("cargo build not available: {e}")),
            Ok(o) => o,
        };
        if !build.status.success() {
            let stderr = String::from_utf8_lossy(&build.stderr);
            issues.push(format!(
                "## cargo build\n```\n{}\n```",
                Self::cap_output(&stderr, 120)
            ));
            return (clean, issues.join("\n\n"));
        }

        // clippy
        if let Ok(o) = std::process::Command::new("cargo")
            .args(["clippy", "--manifest-path", &mf, "--", "-D", "warnings"])
            .output()
        {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                issues.push(format!(
                    "## cargo clippy\n```\n{}\n```",
                    Self::cap_output(&stderr, 80)
                ));
            }
        }

        // test
        if let Ok(o) = std::process::Command::new("cargo")
            .args(["test", "--manifest-path", &mf])
            .output()
        {
            if !o.status.success() {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                issues.push(format!(
                    "## cargo test\n```\n{}{}\n```",
                    Self::cap_output(&stdout, 60),
                    Self::cap_output(&stderr, 40)
                ));
            }
        }

        self.run_lint_plugins(&v2_dir);

        (clean, issues.join("\n\n"))
    }

    /// Run every ast-grep rule in `ai-org-orchestrator/lints/rules/` against the V2 crate.
    /// This is the plugin extension point — see `lints/rules/README.md`. Deliberately
    /// non-blocking: matches are logged to `deviations.md` like the LLM review panel's
    /// findings, never added to `issues`, so a structural pattern match can't trigger a
    /// patch/regen retry the way a real compiler error does. A pattern match can't
    /// distinguish a genuine problem from a justified exception; a human (or the next
    /// mission) should be the one to act on it.
    fn run_lint_plugins(&self, v2_dir: &std::path::Path) {
        let Some(config) = Self::lints_config_path() else { return };
        if !config.exists() {
            return;
        }
        let Ok(output) = std::process::Command::new("ast-grep")
            .arg("scan")
            .arg("--config")
            .arg(&config)
            .arg("--json=compact")
            .arg(v2_dir)
            .output()
        else {
            return; // ast-grep not installed — lints are optional, not a hard dependency
        };
        let Ok(matches) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
            return;
        };
        self.report_lint_matches(&matches);
    }

    /// Locate `ai-org-orchestrator/lints/sgconfig.yml` relative to this binary's own path
    /// (`target/{debug,release}/ai-org-orchestrator` -> repo root), so rules are found
    /// regardless of the worker's current directory when it spawned this process.
    fn lints_config_path() -> Option<std::path::PathBuf> {
        Some(
            std::env::current_exe()
                .ok()?
                .parent()? // target/{debug,release}
                .parent()? // target
                .parent()? // repo root
                .join("ai-org-orchestrator/lints/sgconfig.yml"),
        )
    }

    /// Group matches by (rule, file) and log one deviation per group — generic across
    /// whatever rules happen to be installed, not specific to any one of them.
    fn report_lint_matches(&self, matches: &[serde_json::Value]) {
        use std::collections::HashMap;
        let mut groups: HashMap<(&str, &str), Vec<&serde_json::Value>> = HashMap::new();
        for m in matches {
            let rule_id = m["ruleId"].as_str().unwrap_or("unknown");
            let file = m["file"].as_str().unwrap_or("");
            groups.entry((rule_id, file)).or_default().push(m);
        }
        for ((rule_id, file), hits) in groups {
            let message = hits.first().and_then(|m| m["message"].as_str()).unwrap_or("");
            let snippets: Vec<&str> = hits.iter().filter_map(|m| m["lines"].as_str()).collect();
            let _ = self.ws.log_deviation(
                &format!("ast-grep:{rule_id}"),
                &format!(
                    "{message}\n\n{file} \u{2014} {} match(es):\n{}",
                    hits.len(),
                    snippets.join("\n")
                ),
            );
        }
    }
}

#[cfg(test)]
mod parse_tests {
    use super::Manager;

    /// Regression test for the exact failure from job #5: a model response with a `.rs`
    /// file, then a `// === Cargo.toml ===` marker, then trailing prose -- all three used
    /// to collapse into one giant `lib.rs` because only `.rs`-suffixed markers ended a
    /// section.
    #[test]
    fn cargo_toml_marker_and_trailing_prose_do_not_leak_into_the_previous_rs_file() {
        let raw = "// === src/lib.rs ===\n\
                   pub mod research_database;\n\
                   pub use research_database::ResearchDatabase;\n\
                   ```\n\
                   \n\
                   ```rust\n\
                   // === Cargo.toml ===\n\
                   [package]\n\
                   name = \"async_footguns_research\"\n\
                   version = \"0.1.0\"\n\
                   ```\n\
                   \n\
                   This implementation provides a complete Rust solution.\n";

        let sections = Manager::parse_file_sections(raw);

        let lib_rs = &sections["src/lib.rs"];
        assert!(lib_rs.contains("pub mod research_database;"));
        assert!(!lib_rs.contains("Cargo.toml"), "Cargo.toml leaked into lib.rs:\n{lib_rs}");
        assert!(!lib_rs.contains("[package]"), "Cargo.toml content leaked into lib.rs:\n{lib_rs}");
        assert!(
            !lib_rs.contains("complete Rust solution"),
            "trailing prose leaked into lib.rs:\n{lib_rs}"
        );

        let cargo_toml = &sections["Cargo.toml"];
        assert!(cargo_toml.contains("name = \"async_footguns_research\""));
    }

    #[test]
    fn unrecognized_marker_ends_the_previous_section_without_starting_a_new_one() {
        let raw = "// === src/main.rs ===\n\
                   fn main() {}\n\
                   // === notes.txt ===\n\
                   some commentary that should be discarded\n";
        let sections = Manager::parse_file_sections(raw);
        assert_eq!(sections["src/main.rs"], "fn main() {}\n");
        assert!(!sections.contains_key("notes.txt"));
        assert!(!sections["src/main.rs"].contains("commentary"));
    }

    /// Regression test for job #6's actual failure: a closing ``` fence, then prose
    /// ("Now let's implement each module:"), then an opening ``` fence for the next chunk --
    /// no `// === ===` marker anywhere in between -- all used to glue onto the end of
    /// `main.rs`, and `cargo build` failed on the literal backticks as invalid tokens.
    #[test]
    fn stray_fences_and_trailing_prose_do_not_leak_into_the_previous_rs_file() {
        let raw = "// === src/main.rs ===\n\
                   fn main() {\n\
                   \x20   println!(\"hi\");\n\
                   }\n\
                   ```\n\
                   \n\
                   Now let's implement each module:\n\
                   \n\
                   ```\n\
                   // === src/database.rs ===\n\
                   pub struct Db;\n";

        let sections = Manager::parse_file_sections(raw);

        let main_rs = &sections["src/main.rs"];
        assert!(main_rs.contains("println!"));
        assert!(!main_rs.contains("```"), "stray fence leaked into main.rs:\n{main_rs}");
        assert!(
            !main_rs.contains("implement each module"),
            "trailing prose leaked into main.rs:\n{main_rs}"
        );
        assert_eq!(sections["src/database.rs"], "pub struct Db;\n");
    }
}

#[cfg(test)]
mod lint_tests {
    use super::*;
    use crate::workspace::Workspace;
    use inference_providers::Registry;
    use serde_json::json;

    fn temp_manager(tag: &str) -> (Workspace, Registry) {
        let dir = std::env::temp_dir().join(format!("aoo-quality-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::new(dir).unwrap();
        let registry = Registry::from_env(None, None, Vec::new());
        (ws, registry)
    }

    #[test]
    fn groups_matches_by_rule_and_file_into_one_deviation_each() {
        let (ws, registry) = temp_manager("group");
        let mgr = Manager::new(&ws, &registry);

        let matches = vec![
            json!({
                "ruleId": "reflexive-multi-mutex",
                "file": "src/main.rs",
                "message": "Arc<Mutex<_>> field.",
                "lines": "pub a: Arc<Mutex<A>>,"
            }),
            json!({
                "ruleId": "reflexive-multi-mutex",
                "file": "src/main.rs",
                "message": "Arc<Mutex<_>> field.",
                "lines": "pub b: Arc<Mutex<B>>,"
            }),
            json!({
                "ruleId": "reflexive-multi-mutex",
                "file": "src/lib.rs",
                "message": "Arc<Mutex<_>> field.",
                "lines": "pub c: Arc<Mutex<C>>,"
            }),
        ];
        mgr.report_lint_matches(&matches);

        let deviations = std::fs::read_to_string(ws.deviations_path()).unwrap();
        // One deviation entry per (rule, file) group — main.rs's two hits collapse into
        // one entry that mentions both, lib.rs's one hit is a separate entry.
        assert_eq!(deviations.matches("ast-grep:reflexive-multi-mutex").count(), 2);
        assert!(deviations.contains("src/main.rs \u{2014} 2 match(es)"));
        assert!(deviations.contains("src/lib.rs \u{2014} 1 match(es)"));
        assert!(deviations.contains("pub a: Arc<Mutex<A>>,"));
        assert!(deviations.contains("pub b: Arc<Mutex<B>>,"));
        assert!(deviations.contains("pub c: Arc<Mutex<C>>,"));
    }

    #[test]
    fn no_matches_means_no_deviation_written() {
        let (ws, registry) = temp_manager("empty");
        let mgr = Manager::new(&ws, &registry);
        mgr.report_lint_matches(&[]);
        assert!(std::fs::read_to_string(ws.deviations_path()).is_err());
    }

    #[test]
    fn lints_config_path_points_at_the_repo_lints_dir() {
        let path = Manager::lints_config_path().unwrap();
        assert!(path.ends_with("ai-org-orchestrator/lints/sgconfig.yml"));
    }
}
