**Ownership/borrow pitfalls — common source of build failures:**
- **Using a value after moving it into a struct literal.** `Foo { field: v, other: v.len() }` moves
  `v` into `field` before `other` can borrow it. Compute anything you need from a value (`.len()`,
  `.clone()`, etc.) *before* the struct literal moves it, or reorder fields so the borrow happens
  first, or clone deliberately if you actually need two independent copies.
- **Using a value right after pushing it into a collection.** This is the same mistake in a more
  common shape: `results.push(item); println!("{}", item.field);` moves `item` on the `push` line,
  so the next line referencing it fails to borrow-check. This pattern shows up constantly in
  "process an item, then log what happened" code — get the field you need to log *before* the
  push/move, or log first and push second, don't push then read.
  ```rust
  // Wrong: item is moved into push() on the line before it's read.
  validated.push(report);
  println!("Validated: {}", report.section_num);
  // Right: read what you need first, or reorder so the move comes last.
  println!("Validated: {}", report.section_num);
  validated.push(report);
  ```
