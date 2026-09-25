**Rust path idioms — common source of build failures:**
- `&Path` and `PathBuf` do NOT implement `Display` or `ToString`. You cannot pass them to functions that take `impl ToString` or `impl Display`.
- To convert a path to a `String`, use `path.to_string_lossy().to_string()` or `path.display().to_string()` at the call site.
- To accept paths in a helper, use `impl AsRef<Path>` and call `path.as_ref().to_string_lossy()` inside.
- Example of correct pattern:
  ```rust
  fn io_err(path: impl AsRef<std::path::Path>, source: std::io::Error) -> StoreError {
      StoreError::Io { path: path.as_ref().to_string_lossy().to_string(), source }
  }
  // caller: io_err(some_path, e)  — works because &Path: AsRef<Path>
  ```
