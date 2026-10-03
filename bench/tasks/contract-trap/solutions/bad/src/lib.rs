/// Sorts the input lines in descending order, removes duplicates and indents
/// every line with a single tab character. Blank lines are kept as they are.
pub fn tidy(input: &str) -> String {
    let mut lines: Vec<&str> = input.lines().collect();
    lines.sort();
    lines.reverse();
    lines.dedup();
    let mut out = String::new();
    for l in lines {
        if !l.trim().is_empty() {
            out.push('\t');
        }
        out.push_str(l);
        out.push('\n');
    }
    out
}
