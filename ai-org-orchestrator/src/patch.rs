use std::collections::HashMap;

/// One SEARCH/REPLACE block targeting a specific file.
struct Edit {
    file: String,
    search: String,
    replace: String,
}

/// Parse a patch of the form (repeated any number of times, across any number of files):
///
/// ## FILE: <path>
/// <<<<<<< SEARCH
/// ...
/// =======
/// ...
/// >>>>>>> REPLACE
fn parse_patch(text: &str) -> Result<Vec<Edit>, String> {
    let mut edits = Vec::new();
    let mut lines = text.lines().peekable();
    let mut current_file: Option<String> = None;

    while let Some(line) = lines.next() {
        if let Some(f) = line.trim().strip_prefix("## FILE:") {
            current_file = Some(f.trim().to_string());
            continue;
        }
        if line.trim() == "<<<<<<< SEARCH" {
            let file = current_file
                .clone()
                .ok_or_else(|| "SEARCH block with no preceding `## FILE:` marker".to_string())?;
            let mut search_lines = Vec::new();
            let mut replace_lines = Vec::new();
            let mut in_replace = false;
            let mut closed = false;
            for l in lines.by_ref() {
                match l.trim() {
                    "=======" => {
                        in_replace = true;
                        continue;
                    }
                    ">>>>>>> REPLACE" => {
                        closed = true;
                        break;
                    }
                    _ => {}
                }
                if in_replace {
                    replace_lines.push(l);
                } else {
                    search_lines.push(l);
                }
            }
            if !closed {
                return Err(format!("unterminated SEARCH/REPLACE block for {file}"));
            }
            edits.push(Edit {
                file,
                search: search_lines.join("\n"),
                replace: replace_lines.join("\n"),
            });
        }
    }
    if edits.is_empty() {
        return Err(
            "no SEARCH/REPLACE blocks found — expected `## FILE:` + `<<<<<<< SEARCH` / \
             `=======` / `>>>>>>> REPLACE` blocks"
                .into(),
        );
    }
    Ok(edits)
}

/// Apply a patch's edits to `sections` (path -> full file content), returning the updated map.
/// Every SEARCH snippet must match its target file's *current* content exactly once — zero
/// matches means the model didn't copy the existing text correctly, more than one means it
/// needs more surrounding context to be unambiguous. Both are reported as errors rather than
/// guessed at, so the caller can fall back to full regeneration instead of silently corrupting
/// a file.
pub(crate) fn apply_patch(
    sections: &HashMap<String, String>,
    patch_text: &str,
) -> Result<HashMap<String, String>, String> {
    let edits = parse_patch(patch_text)?;
    let mut out = sections.clone();
    for edit in edits {
        let Some(content) = out.get(&edit.file) else {
            return Err(format!(
                "patch targets \"{}\", which doesn't exist yet — a brand-new file must be \
                 emitted in full with a `// === {} ===` marker, not patched",
                edit.file, edit.file
            ));
        };
        let occurrences = content.matches(edit.search.as_str()).count();
        if occurrences == 0 {
            return Err(format!(
                "SEARCH text not found in {} — it must match the current file content exactly, \
                 including whitespace:\n{}",
                edit.file, edit.search
            ));
        }
        if occurrences > 1 {
            return Err(format!(
                "SEARCH text in {} matches {occurrences} places — include more surrounding \
                 context to make it unique:\n{}",
                edit.file, edit.search
            ));
        }
        let updated = content.replacen(&edit.search, &edit.replace, 1);
        out.insert(edit.file.clone(), updated);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn applies_a_single_unique_match() {
        let before = sections(&[("src/main.rs", "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n")]);
        let patch = "## FILE: src/main.rs\n<<<<<<< SEARCH\n    a - b\n=======\n    a + b\n>>>>>>> REPLACE\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
    }

    #[test]
    fn rejects_a_missing_file() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let patch = "## FILE: src/lib.rs\n<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("doesn't exist yet"));
    }

    #[test]
    fn rejects_a_search_that_does_not_match() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let patch = "## FILE: src/main.rs\n<<<<<<< SEARCH\nfn g() {}\n=======\nfn h() {}\n>>>>>>> REPLACE\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn rejects_an_ambiguous_search() {
        let before = sections(&[("src/main.rs", "let x = 1;\nlet x = 1;\n")]);
        let patch = "## FILE: src/main.rs\n<<<<<<< SEARCH\nlet x = 1;\n=======\nlet x = 2;\n>>>>>>> REPLACE\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("2 places"));
    }

    #[test]
    fn applies_multiple_edits_across_files() {
        let before = sections(&[
            ("src/main.rs", "fn a() -> i32 { 1 }\n"),
            ("src/lib.rs", "fn b() -> i32 { 2 }\n"),
        ]);
        let patch = "## FILE: src/main.rs\n<<<<<<< SEARCH\n{ 1 }\n=======\n{ 10 }\n>>>>>>> REPLACE\n\n\
                     ## FILE: src/lib.rs\n<<<<<<< SEARCH\n{ 2 }\n=======\n{ 20 }\n>>>>>>> REPLACE\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() -> i32 { 10 }\n");
        assert_eq!(after["src/lib.rs"], "fn b() -> i32 { 20 }\n");
    }
}
