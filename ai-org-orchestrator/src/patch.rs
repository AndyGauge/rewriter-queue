//! Unified-diff patching (the same format `git diff`/`diff -u` produce). Replaced two earlier,
//! bespoke formats (a custom SEARCH/REPLACE block, and an even more bespoke 1-indexed
//! line-range block) that real jobs kept getting wrong in the same handful of ways: mixing the
//! two formats' closing markers, miscounting line ranges, and not knowing a brand-new file
//! needed a third convention entirely (`// === path ===`) instead of either patch form. Unified
//! diff collapses all three into one format that's almost certainly the single most-represented
//! "here is a code change" pattern in any model's training data (every `git diff`, every GitHub
//! PR, endless tutorials), and it has its own standard convention for a new file
//! (`--- /dev/null`) instead of needing a separate marker style layered on top.
//!
//! Deliberately not a byte-for-byte implementation of `patch`/`git apply`: hunk headers
//! (`@@ -a,b +c,d @@`) are parsed only as a boundary between hunks, never trusted for where a
//! hunk applies -- that's resolved the same way the old SEARCH/REPLACE format did it, by
//! requiring the hunk's own context/removed lines to match the current file's content exactly
//! once. A model that gets its line-count arithmetic wrong (a real, recurring failure with the
//! old line-range format) still produces a hunk that applies correctly here, because the
//! numbers were never load-bearing in the first place.

use std::collections::HashMap;

/// One hunk's before/after content, derived from its `-`/`+`/` `-prefixed lines. `old_block` is
/// what must exist in the current file to locate this hunk (context + removed lines, in order);
/// `new_block` is what it becomes (context + added lines, in order). Both are joined with `\n`
/// so they can be matched/substituted as one contiguous block, the same technique the old
/// SEARCH/REPLACE format used -- unified diff just derives the two blocks from `-`/`+`/` `
/// prefixes instead of separate `<<<<<<< SEARCH`/`=======`/`>>>>>>> REPLACE` sections.
struct Hunk {
    old_block: String,
    new_block: String,
}

/// One file's diff: zero or more hunks under a `--- `/`+++ ` header pair. `is_new_file` is true
/// when the old side is `/dev/null` -- unified diff's own standard convention for file creation,
/// used here instead of a separate bespoke "new file" marker.
struct FileDiff {
    file: String,
    is_new_file: bool,
    hunks: Vec<Hunk>,
}

fn is_dev_null(spec: &str) -> bool {
    let s = spec.trim();
    s == "/dev/null" || s.ends_with("/dev/null")
}

/// Strip a leading `a/` or `b/` (the conventional prefix `git diff` uses to distinguish the two
/// sides) if present; a model that omits it and writes the bare path is just as valid.
fn strip_diff_prefix(spec: &str) -> String {
    let s = spec.split('\t').next().unwrap_or(spec).trim();
    s.strip_prefix("a/").or_else(|| s.strip_prefix("b/")).unwrap_or(s).to_string()
}

/// Parse one hunk's body: every line up to (but not including) the next `@@` hunk marker or
/// `--- ` file header, or end of input. Tolerant of a context line missing its leading space
/// (some models drop it on blank lines) and of a trailing `\ No newline at end of file` marker.
fn parse_hunk_body(lines: &mut std::iter::Peekable<std::str::Lines>) -> Result<Hunk, String> {
    let mut old_lines = Vec::new();
    let mut new_lines = Vec::new();
    let mut any = false;

    while let Some(&peeked) = lines.peek() {
        if peeked.starts_with("@@") || peeked.starts_with("--- ") {
            break;
        }
        let line = lines.next().unwrap();
        any = true;
        if let Some(rest) = line.strip_prefix('-') {
            old_lines.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('+') {
            new_lines.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix(' ') {
            old_lines.push(rest.to_string());
            new_lines.push(rest.to_string());
        } else if line.starts_with('\\') {
            continue; // `\ No newline at end of file` and similar -- not content
        } else if line.trim().is_empty() {
            old_lines.push(String::new());
            new_lines.push(String::new());
        } else {
            // No recognized prefix on a non-empty line -- ambiguous, but treating it as
            // unchanged context is a safer default than silently dropping or misfiling it.
            old_lines.push(line.to_string());
            new_lines.push(line.to_string());
        }
    }

    if !any {
        return Err("empty hunk — a `@@ ... @@` line with no content after it".to_string());
    }
    Ok(Hunk { old_block: old_lines.join("\n"), new_block: new_lines.join("\n") })
}

/// Parse a patch made of one or more unified diffs, one `--- `/`+++ ` header pair per file
/// followed by one or more `@@ ... @@` hunks.
fn parse_patch(text: &str) -> Result<Vec<FileDiff>, String> {
    let mut diffs = Vec::new();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let Some(old_spec) = line.strip_prefix("--- ") else { continue };
        let Some(new_line) = lines.next() else {
            return Err(format!(
                "expected `+++ <path>` right after `--- {}`, got end of input",
                old_spec.trim()
            ));
        };
        let Some(new_spec) = new_line.strip_prefix("+++ ") else {
            return Err(format!(
                "expected `+++ <path>` right after `--- {}`, got {:?}",
                old_spec.trim(),
                new_line.trim()
            ));
        };

        let is_new_file = is_dev_null(old_spec);
        let file = strip_diff_prefix(new_spec);
        if file.is_empty() {
            return Err(format!("could not read a file path from `+++ {}`", new_spec.trim()));
        }

        let mut hunks = Vec::new();
        while let Some(&peeked) = lines.peek() {
            if !peeked.starts_with("@@") {
                break;
            }
            lines.next();
            hunks.push(parse_hunk_body(&mut lines)?);
        }
        if hunks.is_empty() {
            return Err(format!("diff for \"{file}\" has no `@@ ... @@` hunks"));
        }
        diffs.push(FileDiff { file, is_new_file, hunks });
    }

    if diffs.is_empty() {
        return Err(
            "no diffs found — expected unified diff format: `--- a/<path>` / `+++ b/<path>` \
             (or `--- /dev/null` for a brand-new file), each followed by one or more \
             `@@ ... @@` hunks"
                .into(),
        );
    }
    Ok(diffs)
}

/// Apply one file's hunks in order against its current content. Each hunk's `old_block` must
/// match exactly once — zero matches means the model didn't copy the existing context/removed
/// lines exactly, more than one means it needs more surrounding context to be unambiguous. Both
/// are reported as errors rather than guessed at, the same contract the old SEARCH/REPLACE
/// format had.
fn apply_hunks(file: &str, mut content: String, hunks: &[Hunk]) -> Result<String, String> {
    for hunk in hunks {
        if hunk.old_block.is_empty() {
            return Err(format!(
                "a hunk for \"{file}\" has no context or removed lines to locate it in the \
                 existing file — for a brand-new file, diff it against `--- /dev/null` instead"
            ));
        }
        let occurrences = content.matches(hunk.old_block.as_str()).count();
        if occurrences == 0 {
            return Err(format!(
                "hunk context/removed lines not found in {file} — they must match the current \
                 file content exactly, including whitespace:\n{}",
                hunk.old_block
            ));
        }
        if occurrences > 1 {
            return Err(format!(
                "hunk context in {file} matches {occurrences} places — include more \
                 surrounding context lines to make it unique:\n{}",
                hunk.old_block
            ));
        }
        content = content.replacen(&hunk.old_block, &hunk.new_block, 1);
    }
    Ok(content)
}

/// Apply a patch's diffs to `sections` (path -> full file content), returning the updated map.
/// Every call site (quality-gate repair, integration repair, soundness repair, the single-
/// document repair in `manager.rs`) shares this one entry point.
pub(crate) fn apply_patch(
    sections: &HashMap<String, String>,
    patch_text: &str,
) -> Result<HashMap<String, String>, String> {
    let diffs = parse_patch(patch_text)?;
    let mut out = sections.clone();

    for diff in diffs {
        if diff.is_new_file {
            if out.contains_key(&diff.file) {
                return Err(format!(
                    "diff for \"{}\" is against `--- /dev/null` (a new file), but that file \
                     already exists — diff it against its current content instead",
                    diff.file
                ));
            }
            let mut content =
                diff.hunks.iter().map(|h| h.new_block.as_str()).collect::<Vec<_>>().join("\n");
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            out.insert(diff.file, content);
        } else {
            let Some(content) = out.get(&diff.file).cloned() else {
                return Err(format!(
                    "diff targets \"{}\", which doesn't exist yet — for a brand-new file, diff \
                     it against `--- /dev/null` instead",
                    diff.file
                ));
            };
            let updated = apply_hunks(&diff.file, content, &diff.hunks)?;
            out.insert(diff.file, updated);
        }
    }

    Ok(out)
}

/// Shared prompt text describing the diff format, so every call site that asks a model for a
/// patch (the quality-gate repair loop, integration repair, soundness repair, the single-
/// document repair in `manager.rs`) offers identical instructions rather than each call site
/// drifting its own description over time.
pub(crate) const PATCH_FORMAT_INSTRUCTIONS: &str = "\
Respond with a unified diff (the same format `git diff`/`diff -u` produce) — one `--- `/`+++ ` \
header pair per file, one or more `@@ ... @@` hunks each:\n\
--- a/<path exactly as shown above>\n\
+++ b/<same path>\n\
@@ ... @@\n\
 <context line, unchanged, copied exactly as shown above>\n\
-<line to remove, copied exactly as shown above>\n\
+<line to add>\n\
 <context line, unchanged>\n\n\
Rules:\n\
- Every removed/context line must match the current content exactly, including whitespace — \
  the leading ` `/`-`/`+` column is the diff marker, not part of the line's own content.\n\
- Include a line or two of unchanged context (space-prefixed) around each change so the hunk \
  can be located unambiguously. The numbers on `@@ -a,b +c,d @@` are only a hunk separator and \
  are never checked — write `@@ ... @@` if you're not sure of them, exact counts are not \
  required.\n\
- For a brand-new file, diff it against `/dev/null` instead of patching a file that doesn't \
  exist yet:\n\
--- /dev/null\n\
+++ b/<new path>\n\
@@ ... @@\n\
+<full content of the new file, one line per `+`>\n\n\
One diff per file — do not repeat unchanged files. Multiple edits to the same file can be \
multiple `@@` hunks under one header pair, or separate header pairs; both work.";

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn applies_a_single_hunk_with_context() {
        let before = sections(&[("src/main.rs", "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n")]);
        let patch = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ ... @@
 fn add(a: i32, b: i32) -> i32 {
-    a - b
+    a + b
 }
";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
    }

    #[test]
    fn applies_edits_across_multiple_files() {
        let before = sections(&[
            ("src/main.rs", "fn a() -> i32 { 1 }\n"),
            ("src/lib.rs", "fn b() -> i32 { 2 }\n"),
        ]);
        let patch = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ ... @@
-fn a() -> i32 { 1 }
+fn a() -> i32 { 10 }
--- a/src/lib.rs
+++ b/src/lib.rs
@@ ... @@
-fn b() -> i32 { 2 }
+fn b() -> i32 { 20 }
";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() -> i32 { 10 }\n");
        assert_eq!(after["src/lib.rs"], "fn b() -> i32 { 20 }\n");
    }

    #[test]
    fn applies_multiple_hunks_in_one_file() {
        let before = sections(&[("src/main.rs", "1\n2\n3\n4\n5\n")]);
        let patch = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ ... @@
-2
+TWO
@@ ... @@
-4
+FOUR
";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "1\nTWO\n3\nFOUR\n5\n");
    }

    #[test]
    fn a_pure_insertion_hunk_adds_without_removing() {
        let before = sections(&[("src/main.rs", "fn a() {}\nfn c() {}\n")]);
        let patch = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ ... @@
 fn a() {}
+fn b() {}
 fn c() {}
";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() {}\nfn b() {}\nfn c() {}\n");
    }

    #[test]
    fn a_pure_deletion_hunk_removes_without_adding() {
        let before = sections(&[("src/main.rs", "fn a() {}\nfn dead() {}\nfn b() {}\n")]);
        let patch = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ ... @@
 fn a() {}
-fn dead() {}
 fn b() {}
";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() {}\nfn b() {}\n");
    }

    #[test]
    fn creates_a_brand_new_file_diffed_against_dev_null() {
        let before = sections(&[("src/main.rs", "fn main() {}\n")]);
        let patch = "\
--- /dev/null
+++ b/src/helper.rs
@@ ... @@
+pub fn helper() -> i32 {
+    42
+}
";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/helper.rs"], "pub fn helper() -> i32 {\n    42\n}\n");
        assert_eq!(after["src/main.rs"], "fn main() {}\n"); // untouched
    }

    #[test]
    fn rejects_a_dev_null_diff_for_a_file_that_already_exists() {
        let before = sections(&[("src/main.rs", "fn main() {}\n")]);
        let patch = "--- /dev/null\n+++ b/src/main.rs\n@@ ... @@\n+fn main() {}\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
    }

    #[test]
    fn rejects_a_patch_targeting_a_missing_file() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let patch = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ ... @@\n-fn f() {}\n+fn g() {}\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("doesn't exist yet"), "got: {err}");
        assert!(err.contains("/dev/null"), "got: {err}");
    }

    #[test]
    fn rejects_context_that_does_not_match() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let patch = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ ... @@\n-fn g() {}\n+fn h() {}\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn rejects_ambiguous_context_matching_more_than_once() {
        let before = sections(&[("src/main.rs", "let x = 1;\nlet x = 1;\n")]);
        let patch = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ ... @@\n-let x = 1;\n+let x = 2;\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("2 places"), "got: {err}");
    }

    #[test]
    fn rejects_a_hunk_with_no_context_or_removed_lines() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let patch = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ ... @@\n+fn g() {}\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("/dev/null"), "got: {err}");
    }

    #[test]
    fn tolerates_a_bare_a_and_b_prefix_being_omitted() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let patch = "--- src/main.rs\n+++ src/main.rs\n@@ ... @@\n-fn f() {}\n+fn g() {}\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn g() {}\n");
    }

    #[test]
    fn tolerates_a_context_line_missing_its_leading_space() {
        // Some models drop the leading space on a genuinely blank context line -- the single
        // blank line here is unchanged context shared by both sides, not doubled.
        let before = sections(&[("src/main.rs", "fn a() {}\n\nfn b() {}\n")]);
        let patch = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ ... @@\n-fn a() {}\n+fn a2() {}\n\n fn b() {}\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a2() {}\n\nfn b() {}\n");
    }

    #[test]
    fn no_diff_markers_at_all_is_a_clear_error() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let err = apply_patch(&before, "just some prose, no diff here").unwrap_err();
        assert!(err.contains("no diffs found"), "got: {err}");
    }

    #[test]
    fn missing_plus_plus_plus_line_is_a_clear_error() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let err = apply_patch(&before, "--- a/src/main.rs\nnot a plus plus plus line\n").unwrap_err();
        assert!(err.contains("expected `+++"), "got: {err}");
    }
}
