A small text statistics library. Crate deps: std only.

Public functions, no other public items:

- `tokenize(text: &str) -> Vec<String>`: lowercase ASCII words. A token is a maximal run of ASCII letters and digits; every other character separates tokens. Letters are lowercased. No empty tokens.
- `word_counts(tokens: &[String]) -> Vec<(String, usize)>`: the number of occurrences of each distinct token, sorted by count descending, then by word ascending.
- `top_words(text: &str, n: usize) -> Vec<(String, usize)>`: the first `n` entries of `word_counts(&tokenize(text))`, or all of them if there are fewer.
- `unique_words(text: &str) -> usize`: the number of distinct tokens in the text.

Private helpers are free functions (not methods) and must all be used.

Performance: `word_counts`, `top_words` and `unique_words` are called on inputs of several million tokens, so avoid quadratic behavior and avoid needless allocation.
