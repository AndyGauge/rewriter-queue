# hello-trivial

Build a tiny command-line program named `hello` (Rust, standard library only, a single binary crate whose package and binary are both named `hello`).

    hello [name]

It prints `Hello, <name>!` followed by exactly one `\n` to standard output, where `<name>` is the first command-line argument exactly as given, or `World` when there is no argument. Any further arguments are ignored. An empty-string argument is a name like any other (so `hello ""` prints `Hello, !`). The exit status is always 0 and nothing is written to standard error.

    $ hello
    Hello, World!
    $ hello Ada
    Hello, Ada!
