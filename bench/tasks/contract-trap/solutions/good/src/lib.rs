/// Sorts the non-blank input lines in ascending byte order, removes duplicates
/// and indents every line with four spaces. Blank lines are dropped.
pub fn tidy(input: &str) -> String {
    let mut lines: Vec<&str> = input.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.sort();
    lines.dedup();
    let mut out = String::new();
    for l in lines {
        out.push_str("    ");
        out.push_str(l);
        out.push('\n');
    }
    out
}
