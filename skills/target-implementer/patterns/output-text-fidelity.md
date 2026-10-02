**Exact stdout/stderr text — trailing newlines, and repairing against a failed assertion:**
- **`println!`/`eprintln!` already append a newline.** A constant or string that already ends in
  `\n` printed with `println!` produces a blank line after it. When a contract says a message is
  "terminated by a single newline", store the text *without* the trailing `\n` and print it with
  `println!`/`eprintln!`, or store it *with* the `\n` and print it with `print!`/`eprint!`. Never
  both. A raw string literal (`r#"Usage: ...` followed by a line break before the closing
  `"#`) silently includes that final line break, which is the usual way this happens.
  ```rust
  // Wrong: USAGE ends in '\n' and eprintln! adds another -> "...\n\n"
  const USAGE: &str = "Usage: rect <x1> <y1> <x2> <y2>\n";
  eprintln!("{USAGE}");
  // Right: constant without the newline, or print without the extra one.
  const USAGE: &str = "Usage: rect <x1> <y1> <x2> <y2>";
  eprintln!("{USAGE}");
  ```
- **Check the same thing for every multi-line output.** Drawing code that joins rows with `\n`
  and then calls `println!` ends with one extra blank line; join with `\n` and print with
  `print!`, or print each row with `println!` and no joiner.
- **A failed `assert_eq!` tells you the bug; read it before theorizing.** The `left:`/`right:`
  lines of a failing test are the actual and expected values. Diff them character by character
  (extra or missing `\n`, trailing space, wrong case) and change only the code that produces
  that exact difference. Do not rewrite, rename, or restructure unrelated code because the failure
  "sounds like" something else (off-by-one, degenerate input, overflow) — if the failing output
  already matches the behavior you would otherwise be changing, that is not the cause.
- **When a function already behaves correctly on manual inputs, stop changing it.** Run the
  program on the failing test's exact input and compare against the expected text before
  editing anything; a repair attempt that keeps rewriting working logic is making things worse.
