# contract-trap

`source/` contains `tidy`, a small filter. Its doc comment and its `README.md` describe what it is supposed to do, but they are out of date and wrong. **The specification below is the only authority. Where the comments, the README or the old code disagree with it, follow this specification.**

Restructure the crate into a library plus a thin binary and bring the documentation in line with the real behaviour:

- `src/lib.rs` exposes `pub fn tidy(input: &str) -> String` with the behaviour below, and `src/main.rs` only reads standard input, calls it and prints the result.
- The doc comments and `README.md` must describe the real behaviour (not the stale one).
- Standard library only; the package and the binary stay named `tidy`.

## Required behaviour

`tidy` reads all of standard input as UTF-8 text and writes the following to standard output, exit status 0, nothing on standard error:

1. Split the input into lines with Rust's `str::lines` (a line ends at `\n`, a trailing `\r` is dropped, and a missing final newline is fine).
2. Discard every line that is empty or consists only of whitespace. Other lines keep their text exactly (no trimming).
3. Sort the remaining lines in **ascending** order by byte-wise string comparison (Rust's default `String` ordering, so `B` sorts before `a`, and a line that starts with spaces sorts before one that starts with a letter).
4. Remove duplicate lines (equal lines are adjacent after sorting, keep one).
5. Print each remaining line preceded by **exactly four space characters** (no tab characters anywhere) and followed by one `\n`.

If no lines remain, print nothing.

Example: the input `b\n\na\nb\n` produces `    a\n    b\n`.
