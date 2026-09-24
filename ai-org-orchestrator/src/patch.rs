use std::collections::HashMap;

/// SEARCH/REPLACE patching: best for a short, uniquely-identifiable snippet. Self-contained —
/// its `Edit` type, parser, and applier live only here, so it can be reused or dropped without
/// touching `line_range`.
mod search_replace {
    /// `## FILE: <path>\n<<<<<<< SEARCH\n...\n=======\n...\n>>>>>>> REPLACE`
    pub struct Edit {
        pub file: String,
        pub search: String,
        pub replace: String,
    }

    /// Parses one block's body. The caller has already consumed `## FILE: <path>` and
    /// confirmed the next line is `<<<<<<< SEARCH`.
    pub fn parse_body(
        file: String,
        lines: &mut std::iter::Peekable<std::str::Lines>,
    ) -> Result<Edit, String> {
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
        Ok(Edit { file, search: search_lines.join("\n"), replace: replace_lines.join("\n") })
    }

    /// Apply one file's SEARCH/REPLACE edits, in order, against `content`. Every SEARCH
    /// snippet must match exactly once — zero matches means the model didn't copy the
    /// existing text correctly, more than one means it needs more surrounding context to be
    /// unambiguous. Both are reported as errors rather than guessed at.
    pub fn apply(file: &str, mut content: String, edits: &[Edit]) -> Result<String, String> {
        for edit in edits {
            let occurrences = content.matches(edit.search.as_str()).count();
            if occurrences == 0 {
                return Err(format!(
                    "SEARCH text not found in {file} — it must match the current file content \
                     exactly, including whitespace:\n{}",
                    edit.search
                ));
            }
            if occurrences > 1 {
                return Err(format!(
                    "SEARCH text in {file} matches {occurrences} places — include more \
                     surrounding context to make it unique:\n{}",
                    edit.search
                ));
            }
            content = content.replacen(&edit.search, &edit.replace, 1);
        }
        Ok(content)
    }
}

/// Line-range patching: best for swapping out a whole method/block, or anywhere counting
/// exact lines is easier than reproducing exact text. Self-contained, same reasoning as
/// `search_replace`.
mod line_range {
    /// `## FILE: <path>:<start>-<end>\n<<<<<<< NEW\n...\n>>>>>>> NEW`. 1-indexed, inclusive.
    /// `end + 1 == start` (e.g. `12-11`) marks a pure insertion before line `start`, adding
    /// `content_lines` without removing anything.
    pub struct Edit {
        pub file: String,
        pub start: usize,
        pub end: usize,
        pub content_lines: Vec<String>,
    }

    impl Edit {
        fn is_insert(&self) -> bool {
            self.end + 1 == self.start
        }
    }

    /// Parses one block's body. The caller has already split `<path>:<start>-<end>` and
    /// confirmed the next line is `<<<<<<< NEW`.
    pub fn parse_body(
        file: String,
        start: usize,
        end: usize,
        lines: &mut std::iter::Peekable<std::str::Lines>,
    ) -> Result<Edit, String> {
        let mut content_lines = Vec::new();
        let mut closed = false;
        for l in lines.by_ref() {
            if l.trim() == ">>>>>>> NEW" {
                closed = true;
                break;
            }
            content_lines.push(l.to_string());
        }
        if !closed {
            return Err(format!("unterminated `<<<<<<< NEW` block for {file}:{start}-{end}"));
        }
        Ok(Edit { file, start, end, content_lines })
    }

    /// Apply one file's line-range edits together. Edits are applied from the bottom up
    /// (highest `start` first) so an earlier edit's line numbers — computed against the
    /// file's original content — are never shifted by a later one having already changed the
    /// line count above it. Two edits that touch the same line, or share a start line, are
    /// rejected as ambiguous rather than guessed at.
    pub fn apply(file: &str, content: &str, mut edits: Vec<Edit>) -> Result<String, String> {
        let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
        let total = lines.len();

        edits.sort_by_key(|e| e.start);
        for w in edits.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if a.start == b.start || a.end + 1 > b.start {
                return Err(format!(
                    "overlapping or ambiguous edits in {file}: {}-{} and {}-{} both touch the \
                     same line(s) — combine them into a single edit",
                    a.start, a.end, b.start, b.end
                ));
            }
        }
        for edit in &edits {
            if edit.is_insert() {
                if edit.start > total + 1 {
                    return Err(format!(
                        "{file}:{}-{} is out of range as an insertion point — file has {total} \
                         line(s), so it must be between 1 and {}",
                        edit.start,
                        edit.end,
                        total + 1
                    ));
                }
            } else if edit.end < edit.start {
                return Err(format!(
                    "{file}:{}-{} is invalid — end must be >= start for a replace, or exactly \
                     start-1 for an insertion before start",
                    edit.start, edit.end
                ));
            } else if edit.end > total {
                return Err(format!(
                    "{file}:{}-{} is out of range — file only has {total} line(s)",
                    edit.start, edit.end
                ));
            }
        }

        for edit in edits.into_iter().rev() {
            if edit.is_insert() {
                lines.splice(edit.start - 1..edit.start - 1, edit.content_lines);
            } else {
                lines.splice(edit.start - 1..edit.end, edit.content_lines);
            }
        }
        let mut updated = lines.join("\n");
        updated.push('\n');
        Ok(updated)
    }
}

enum AnyEdit {
    SearchReplace(search_replace::Edit),
    LineRange(line_range::Edit),
}

impl AnyEdit {
    fn file(&self) -> &str {
        match self {
            AnyEdit::SearchReplace(e) => &e.file,
            AnyEdit::LineRange(e) => &e.file,
        }
    }
}

/// Parse a patch made of any mix of the two block forms, one `## FILE:` marker per edit:
///
/// - `## FILE: <path>:<start>-<end>` (or `## FILE: <path>:<n>` for one line) followed by
///   `<<<<<<< NEW` / `>>>>>>> NEW` — a line-range edit.
/// - `## FILE: <path>` (no trailing `:<range>`) followed by `<<<<<<< SEARCH` / `=======` /
///   `>>>>>>> REPLACE` — a SEARCH/REPLACE edit.
///
/// Both forms are always available and can be mixed freely across, or even within, a single
/// patch response — whichever fits a given change best.
fn parse_patch(text: &str) -> Result<Vec<AnyEdit>, String> {
    let mut edits = Vec::new();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("## FILE:") else { continue };
        let spec = rest.trim();

        // A trailing `:<start>-<end>` or `:<n>` makes this a line-range edit; anything else
        // (a bare path, no such suffix) is a SEARCH/REPLACE edit.
        let range_spec = spec.rsplit_once(':').and_then(|(path, range)| {
            let parsed = match range.split_once('-') {
                Some((a, b)) => {
                    a.trim().parse::<usize>().ok().zip(b.trim().parse::<usize>().ok())
                }
                None => range.trim().parse::<usize>().ok().map(|n| (n, n)),
            };
            parsed.map(|(start, end)| (path.to_string(), start, end))
        });

        if let Some((file, start, end)) = range_spec {
            if start == 0 {
                return Err(format!("line numbers are 1-indexed, got start=0 in \"{spec}\""));
            }
            let opening = lines.next().map(str::trim);
            if opening != Some("<<<<<<< NEW") {
                return Err(format!(
                    "expected `<<<<<<< NEW` right after `## FILE: {spec}`, got {:?}",
                    opening.unwrap_or("end of input")
                ));
            }
            edits.push(AnyEdit::LineRange(line_range::parse_body(file, start, end, &mut lines)?));
        } else {
            let file = spec.to_string();
            let opening = lines.next().map(str::trim);
            if opening != Some("<<<<<<< SEARCH") {
                return Err(format!(
                    "expected `<<<<<<< SEARCH` (or a `:<start>-<end>` line range on the \
                     `## FILE:` line followed by `<<<<<<< NEW`) right after `## FILE: {spec}`, \
                     got {:?}",
                    opening.unwrap_or("end of input")
                ));
            }
            edits.push(AnyEdit::SearchReplace(search_replace::parse_body(file, &mut lines)?));
        }
    }

    if edits.is_empty() {
        return Err(
            "no edits found — expected `## FILE: <path>` + `<<<<<<< SEARCH`/`=======`/\
             `>>>>>>> REPLACE`, or `## FILE: <path>:<start>-<end>` + `<<<<<<< NEW`/`>>>>>>> NEW`"
                .into(),
        );
    }
    Ok(edits)
}

/// Apply a patch's edits to `sections` (path -> full file content), returning the updated map.
/// Every call site (quality-gate repair, integration repair, soundness repair, the single-
/// document repair in `manager.rs`) shares this one entry point, so both patch styles are
/// always available everywhere a patch can be applied — never a subset per agent.
///
/// When a file has edits of both kinds, line-range edits are applied first (against the
/// file's original line numbering), then SEARCH/REPLACE edits against whatever that leaves —
/// a deterministic order for the rare case one patch response mixes both styles on one file.
pub(crate) fn apply_patch(
    sections: &HashMap<String, String>,
    patch_text: &str,
) -> Result<HashMap<String, String>, String> {
    let edits = parse_patch(patch_text)?;

    let mut by_file: HashMap<String, Vec<AnyEdit>> = HashMap::new();
    for edit in edits {
        by_file.entry(edit.file().to_string()).or_default().push(edit);
    }

    let mut out = sections.clone();
    for (file, file_edits) in by_file {
        let Some(mut content) = out.get(&file).cloned() else {
            return Err(format!(
                "patch targets \"{file}\", which doesn't exist yet — a brand-new file must be \
                 emitted in full with a `// === {file} ===` marker, not patched"
            ));
        };

        let (line_range_edits, search_replace_edits): (Vec<_>, Vec<_>) =
            file_edits.into_iter().partition(|e| matches!(e, AnyEdit::LineRange(_)));

        if !line_range_edits.is_empty() {
            let edits: Vec<line_range::Edit> = line_range_edits
                .into_iter()
                .map(|e| match e {
                    AnyEdit::LineRange(e) => e,
                    AnyEdit::SearchReplace(_) => unreachable!("partitioned above"),
                })
                .collect();
            content = line_range::apply(&file, &content, edits)?;
        }
        if !search_replace_edits.is_empty() {
            let edits: Vec<search_replace::Edit> = search_replace_edits
                .into_iter()
                .map(|e| match e {
                    AnyEdit::SearchReplace(e) => e,
                    AnyEdit::LineRange(_) => unreachable!("partitioned above"),
                })
                .collect();
            content = search_replace::apply(&file, content, &edits)?;
        }

        out.insert(file, content);
    }

    Ok(out)
}

/// Shared prompt text describing both patch forms, so every call site that asks a model for a
/// patch (the quality-gate repair loop, integration repair, soundness repair, the single-
/// document repair in `manager.rs`) offers the identical two options rather than each
/// call site drifting its own description over time.
pub(crate) const PATCH_FORMAT_INSTRUCTIONS: &str = "\
Use whichever of these two forms fits best per change — both are always available, pick \
per-edit, and a response may mix them freely:\n\n\
Line-range replace (best for swapping out a whole method/block, or anywhere counting exact \
lines is easier than reproducing exact text) — line numbers are 1-indexed against the \
numbered listing above:\n\
## FILE: <path exactly as shown above>:<start>-<end>\n\
<<<<<<< NEW\n\
<replacement lines for lines start..end inclusive>\n\
>>>>>>> NEW\n\
(a single number, e.g. `:12`, is shorthand for replacing just that one line; end one less \
than start, e.g. `12-11`, inserts before line 12 without removing anything; an empty block \
between the markers deletes start..end.)\n\n\
Search/replace (best for a short, uniquely-identifiable snippet):\n\
## FILE: <path exactly as shown above>\n\
<<<<<<< SEARCH\n\
<exact existing text, including whitespace, that appears exactly once and pinpoints the change>\n\
=======\n\
<replacement text>\n\
>>>>>>> REPLACE\n\n\
One block per change — do not repeat unchanged code. Only emit a full `// === path ===` file \
if you are adding a brand-new file not shown above.";

/// Render `sections` with a 1-indexed line number on every content line, matching what a
/// line-range edit's `<start>-<end>` counts against. Used only in prompts that offer a patch —
/// never for the canonical crate content that actually gets built.
pub(crate) fn format_multi_file_numbered(sections: &HashMap<String, String>) -> String {
    let mut paths: Vec<&String> = sections.keys().collect();
    paths.sort();
    let mut out = String::new();
    for path in paths {
        out.push_str(&format!("// === {path} ===\n"));
        for (i, line) in sections[path].lines().enumerate() {
            out.push_str(&format!("{:>5}| {}\n", i + 1, line));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn applies_a_single_unique_search_replace_match() {
        let before = sections(&[("src/main.rs", "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n")]);
        let patch = "## FILE: src/main.rs\n<<<<<<< SEARCH\n    a - b\n=======\n    a + b\n>>>>>>> REPLACE\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
    }

    #[test]
    fn rejects_a_search_replace_missing_file() {
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
    fn applies_multiple_search_replace_edits_across_files() {
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

    #[test]
    fn line_range_replaces_a_single_line() {
        let before = sections(&[("src/main.rs", "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n")]);
        let patch = "## FILE: src/main.rs:2-2\n<<<<<<< NEW\n    a + b\n>>>>>>> NEW\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
    }

    #[test]
    fn line_range_single_number_is_shorthand_for_one_line() {
        let before = sections(&[("src/main.rs", "fn f() {}\nfn g() {}\n")]);
        let patch = "## FILE: src/main.rs:1\n<<<<<<< NEW\nfn f2() {}\n>>>>>>> NEW\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn f2() {}\nfn g() {}\n");
    }

    #[test]
    fn line_range_swaps_out_a_whole_method() {
        let before = sections(&[(
            "src/main.rs",
            "struct S;\nimpl S {\n    fn old(&self) -> i32 {\n        1\n    }\n}\n",
        )]);
        let patch = "## FILE: src/main.rs:3-5\n<<<<<<< NEW\n    fn new(&self) -> i32 {\n        2\n    }\n>>>>>>> NEW\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(
            after["src/main.rs"],
            "struct S;\nimpl S {\n    fn new(&self) -> i32 {\n        2\n    }\n}\n"
        );
    }

    #[test]
    fn line_range_empty_replacement_deletes_the_range() {
        let before = sections(&[("src/main.rs", "fn a() {}\nfn dead() {}\nfn b() {}\n")]);
        let patch = "## FILE: src/main.rs:2-2\n<<<<<<< NEW\n>>>>>>> NEW\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() {}\nfn b() {}\n");
    }

    #[test]
    fn line_range_end_one_less_than_start_inserts_without_removing_anything() {
        let before = sections(&[("src/main.rs", "fn a() {}\nfn c() {}\n")]);
        let patch = "## FILE: src/main.rs:2-1\n<<<<<<< NEW\nfn b() {}\n>>>>>>> NEW\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() {}\nfn b() {}\nfn c() {}\n");
    }

    #[test]
    fn line_range_can_append_past_the_last_line() {
        let before = sections(&[("src/main.rs", "fn a() {}\n")]);
        let patch = "## FILE: src/main.rs:2-1\n<<<<<<< NEW\nfn b() {}\n>>>>>>> NEW\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() {}\nfn b() {}\n");
    }

    #[test]
    fn line_range_rejects_a_missing_file() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let patch = "## FILE: src/lib.rs:1-1\n<<<<<<< NEW\nfn g() {}\n>>>>>>> NEW\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("doesn't exist yet"));
    }

    #[test]
    fn line_range_rejects_a_range_past_the_end_of_the_file() {
        let before = sections(&[("src/main.rs", "fn f() {}\n")]);
        let patch = "## FILE: src/main.rs:5-6\n<<<<<<< NEW\nx\n>>>>>>> NEW\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn line_range_rejects_two_edits_that_overlap_in_the_same_file() {
        let before = sections(&[("src/main.rs", "1\n2\n3\n4\n5\n")]);
        let patch = "## FILE: src/main.rs:1-3\n<<<<<<< NEW\na\n>>>>>>> NEW\n\n\
                     ## FILE: src/main.rs:3-4\n<<<<<<< NEW\nb\n>>>>>>> NEW\n";
        let err = apply_patch(&before, patch).unwrap_err();
        assert!(err.contains("overlapping"), "got: {err}");
    }

    #[test]
    fn line_range_applies_multiple_non_overlapping_edits_bottom_up() {
        // If naively applied top-down, replacing line 2 first would shift line 4 down to 3,
        // corrupting the second edit's target. Bottom-up avoids that.
        let before = sections(&[("src/main.rs", "1\n2\n3\n4\n5\n")]);
        let patch = "## FILE: src/main.rs:2-2\n<<<<<<< NEW\nTWO\nTWO-B\n>>>>>>> NEW\n\n\
                     ## FILE: src/main.rs:4-4\n<<<<<<< NEW\nFOUR\n>>>>>>> NEW\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "1\nTWO\nTWO-B\n3\nFOUR\n5\n");
    }

    #[test]
    fn applies_multiple_line_range_edits_across_files() {
        let before = sections(&[
            ("src/main.rs", "fn a() -> i32 { 1 }\n"),
            ("src/lib.rs", "fn b() -> i32 { 2 }\n"),
        ]);
        let patch = "## FILE: src/main.rs:1\n<<<<<<< NEW\nfn a() -> i32 { 10 }\n>>>>>>> NEW\n\n\
                     ## FILE: src/lib.rs:1\n<<<<<<< NEW\nfn b() -> i32 { 20 }\n>>>>>>> NEW\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() -> i32 { 10 }\n");
        assert_eq!(after["src/lib.rs"], "fn b() -> i32 { 20 }\n");
    }

    #[test]
    fn mixes_both_styles_in_one_patch_on_different_files() {
        let before = sections(&[
            ("src/main.rs", "fn a() -> i32 { 1 }\n"),
            ("src/lib.rs", "fn b() -> i32 { 2 }\n"),
        ]);
        let patch = "## FILE: src/main.rs:1\n<<<<<<< NEW\nfn a() -> i32 { 10 }\n>>>>>>> NEW\n\n\
                     ## FILE: src/lib.rs\n<<<<<<< SEARCH\n{ 2 }\n=======\n{ 20 }\n>>>>>>> REPLACE\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a() -> i32 { 10 }\n");
        assert_eq!(after["src/lib.rs"], "fn b() -> i32 { 20 }\n");
    }

    #[test]
    fn mixes_both_styles_on_the_same_file_line_range_applied_first() {
        let before = sections(&[("src/main.rs", "fn a() {}\nfn b() {}\n")]);
        // Line-range replaces line 1; SEARCH/REPLACE then targets text that only exists
        // after that edit, proving the documented apply order (line-range, then search).
        let patch = "## FILE: src/main.rs:1\n<<<<<<< NEW\nfn a2() { /* marker */ }\n>>>>>>> NEW\n\n\
                     ## FILE: src/main.rs\n<<<<<<< SEARCH\n/* marker */\n=======\n/* replaced */\n>>>>>>> REPLACE\n";
        let after = apply_patch(&before, patch).unwrap();
        assert_eq!(after["src/main.rs"], "fn a2() { /* replaced */ }\nfn b() {}\n");
    }

    #[test]
    fn numbered_listing_counts_from_one_per_file() {
        let s = sections(&[("src/main.rs", "a\nb\n"), ("src/lib.rs", "c\n")]);
        let out = format_multi_file_numbered(&s);
        assert!(out.contains("// === src/lib.rs ===\n    1| c\n"));
        assert!(out.contains("// === src/main.rs ===\n    1| a\n    2| b\n"));
    }
}
