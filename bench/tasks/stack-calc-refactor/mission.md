# stack-calc-refactor

`source/` contains `calc`, a working reverse-polish-notation calculator written as one long `main` function in a single file. Restructure it into modules **without changing its behaviour at all**. Standard library only; the package and binary stay named `calc`.

## Structure requirements

- `src/main.rs` must be at most 40 lines long and only deal with reading the arguments, printing, and the process exit status.
- There must be at least three other `.rs` files under `src/` (for example for the error type, token parsing and evaluation), wired in with `mod` declarations.
- The crate must still build with no warnings.

## Behaviour that must stay exactly the same

Invocation: `calc <expression words...>`. All command-line arguments are joined with a single space and the result is split on whitespace into tokens, so `calc 3 4 +` and `calc "3 4 +"` are the same expression.

Evaluation is left to right on a stack of 64-bit signed integers (`i64`). A token is one of:

- `+ - * / %` pop `b` then `a` and push `a op b` (for example `10 3 -` is 7). `/` truncates toward zero and `%` is the remainder with the sign of the dividend, as in Rust (`-7 2 /` is -3 and `-7 2 %` is -1).
- `dup` pushes a copy of the top value; `swap` exchanges the top two values (`1 2 swap -` is 1).
- anything else must parse as an `i64` with Rust's `str::parse::<i64>` (so `-5` and `+5` are valid numbers, `99999999999999999999` and `1.5` are not) and is pushed.

When evaluation finishes with exactly one value on the stack, print it followed by a single `\n` to standard output and exit 0, writing nothing to standard error.

Errors are reported by printing `error: <message>` plus a single `\n` to standard error, printing nothing to standard output, and exiting with status 2. Evaluation stops at the first error, going left to right, and the checks for one token happen in this order: first the token is recognised (else `unknown token`), then the stack depth is checked (`stack underflow`), then division by zero, then overflow. The messages are:

- no tokens at all (no arguments, or only whitespace): `empty expression`
- an unrecognised token: `unknown token '<token>'`
- too few values for the token (`+ - * / %` and `swap` need two, `dup` needs one): `stack underflow at '<token>'`
- `/` or `%` with a zero right operand: `division by zero`
- arithmetic that overflows `i64` (including `-9223372036854775808 -1 /`): `overflow at '<operator>'`
- after the last token the stack does not hold exactly one value: `expected 1 value on stack, found <n>`

Examples:

    $ calc 3 4 +            ->  7
    $ calc 5 dup '*'        ->  25
    $ calc 1 foo +          ->  stderr: error: unknown token 'foo'     (exit 2)
    $ calc 1 +              ->  stderr: error: stack underflow at '+'  (exit 2)
    $ calc 1 0 /            ->  stderr: error: division by zero        (exit 2)
    $ calc 1 2              ->  stderr: error: expected 1 value on stack, found 2   (exit 2)
