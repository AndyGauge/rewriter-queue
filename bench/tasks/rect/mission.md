# rect

Build a command-line program named `rect` (Rust, standard library only, a single binary crate whose package and binary are both named `rect`) that draws the outline of a rectangle in ASCII.

## Invocation

    rect <x1> <y1> <x2> <y2>

All four arguments are base-10 integers (an optional leading `-` is allowed, for example `-3`). `(x1, y1)` and `(x2, y2)` are two opposite corners of the rectangle, given in any order (for example the second corner may be to the left of or above the first). Coordinates are inclusive: the rectangle covers `|x2 - x1| + 1` columns and `|y2 - y1| + 1` rows.

## Output

Write the outline to standard output, one text row per rectangle row, each row terminated by exactly one `\n`, and nothing else: no blank line before or after, no extra trailing newline, no trailing spaces. The drawing always starts at the left edge of the output; the absolute coordinates only decide the size, not any indentation.

Each cell is drawn as follows, where the rectangle's first and last rows and first and last columns form its border:

- a border cell that lies on a border row and also on a border column is `+`
- a border cell on a border row only is `-`
- a border cell on a border column only is `|`
- every interior cell is a space

Examples (the exact bytes after each line are `\n`):

    $ rect 0 0 4 2
    +---+
    |   |
    +---+

    $ rect 4 2 0 0          (same rectangle, corners reversed)
    +---+
    |   |
    +---+

    $ rect 1 1 2 2
    ++
    ++

A rectangle with zero width or zero height is drawn as a line using the same rules:

    $ rect 0 0 3 0
    +--+

    $ rect 0 0 0 2
    +
    |
    +

    $ rect 5 5 5 5
    +

## Errors

If the number of arguments is not exactly four, or any argument is not a valid integer, print exactly this to standard error and nothing to standard output, then exit with status 1:

    Usage: rect <x1> <y1> <x2> <y2>

followed by exactly one `\n`. On success the exit status is 0 and nothing is written to standard error.
