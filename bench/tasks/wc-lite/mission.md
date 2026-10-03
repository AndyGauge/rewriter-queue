# wc-lite

Build a command-line program named `wc-lite` (Rust, standard library only, a single binary crate whose package and binary are both named `wc-lite`) that counts what it reads from standard input.

## Behaviour

Read all of standard input until end of input. Ignore any command-line arguments. Then print exactly one line to standard output:

    <lines> <words> <bytes>

that is, three decimal integers separated by single spaces, followed by exactly one `\n`, with no leading or trailing spaces and nothing else. Exit with status 0 and write nothing to standard error.

Definitions (they work on the raw bytes, so the input does not have to be valid UTF-8):

- `bytes` is the total number of bytes read.
- `lines` is the number of `\n` bytes read. Input that does not end with a newline does not count its last partial line (so `a b c` has 0 lines).
- `words` is the number of maximal runs of bytes that are not ASCII whitespace. ASCII whitespace is exactly: space, `\t`, `\n`, `\r`, vertical tab (0x0B) and form feed (0x0C). Runs of several whitespace bytes in a row separate words just like a single one, and leading or trailing whitespace creates no empty words.

Examples (the output always ends with one `\n`):

| input (as escaped text)  | output  |
|--------------------------|---------|
| empty input              | `0 0 0` |
| `hello world\n`          | `1 2 12` |
| `a b c`                  | `0 3 5` |
| `  a   b  \n\n`          | `2 2 11` |
| `\n`                     | `1 0 1` |
| `a\r\nb\r\n`             | `2 2 6` |
