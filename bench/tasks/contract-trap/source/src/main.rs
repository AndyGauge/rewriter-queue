use std::io::{self, Read};

/// Sorts the input lines in descending order, removes duplicates and indents
/// every line with a single tab character. Blank lines are kept as they are.
fn tidy(input: &str) -> String {
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

fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).unwrap();
    print!("{}", tidy(&input));
}
