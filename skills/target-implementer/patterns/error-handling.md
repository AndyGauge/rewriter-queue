**Error type and derive correctness — common source of build failures:**
- **`thiserror`'s `#[source]` field must itself be an error type** (implement `std::error::Error`)
  — a `String` or `Option<String>` field cannot be `#[source]`. Use `#[error("{0}")]` with a
  `String` payload instead if you just need a message, not a wrapped error to chain from. Likewise,
  any enum or struct you compare with `==` or print with `{:?}` needs `#[derive(PartialEq)]` /
  `#[derive(Debug)]` respectively — don't assume derives are implied.
- **`chrono::DateTime::from_timestamp(...)` returns `Option<DateTime<Utc>>`, not `DateTime<Utc>`**
  (a timestamp can be out of the representable range). Match or `.unwrap_or_else(...)` on the
  `Option` itself before calling a `DateTime` method like `.fixed_offset()` on it — you cannot call
  a `DateTime` method directly on the `Option` that wraps one.
