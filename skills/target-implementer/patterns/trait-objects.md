**`Box<dyn Trait>` field ergonomics — common source of build failures:**
- **A `Box<dyn Trait>` field blocks `#[derive(Default)]` and `#[derive(Debug)]` on the struct
  that holds it**, and blocks using that struct's value across an `.await` unless the trait
  object's bound includes `Send` (`Box<dyn Trait + Send>`, or `+ Send + Sync`). If the schema
  you were given already declares a struct with one of these derives over a `dyn Trait` field, or
  a trait without a `Send` bound that gets used from async code, that's a schema defect — write a
  named constructor (`impl Foo { fn new_default() -> Self { ... } }`) instead of deriving `Default`,
  skip deriving `Debug` (or implement it by hand) unless the trait requires it as a supertrait, and
  add `+ Send + Sync` to the trait object's bound anywhere it crosses an `.await`. Record the schema
  defect as a deviation; don't silently redesign the public type the contract committed to.
