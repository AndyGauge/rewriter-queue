A file-backed key-value append log, crate deps: std and thiserror only.

`KvError` (thiserror): `Io { path: String, source: std::io::Error }` ("IO error at {path}: {source}"), `KeyNotFound { key }`, `InvalidKey { key }`.

`KvLog { path: PathBuf }` with `new(path: impl Into<PathBuf>)`. The log is one file at `path`; it need not exist.

Line format: a set is `<key>\t<value>\n` where in the value a backslash is written as `\\` and a newline as `\n` (backslash, n). A delete is `<key>\n` (no tab). Later lines override earlier lines for the same key.

A key is invalid if it is empty or contains a tab or a newline.

Methods, all returning Result<_, KvError>:
- append(key, value): validates the key (InvalidKey), creates parent directories and the file if needed, appends one set line.
- delete(key): validates the key (InvalidKey), appends one delete line (works even if the key was never set).
- get(key) -> String: the latest value with escapes decoded; KeyNotFound if never set, deleted last, or the file is missing.
- keys() -> Vec<String>: sorted live keys; empty if the file is missing.
- line_count() -> usize: number of lines in the file; 0 if the file is missing.
- compact() -> usize: rewrites the file with exactly one set line per live key, sorted by key, and returns the number of lines removed (old line count minus new line count). A missing file is left missing and returns 0.
