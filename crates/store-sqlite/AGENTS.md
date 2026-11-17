# mensor-store-sqlite

The SQLite `Store` (`crates/mind/src/store.rs`).

- Appends commit in groups, and one returns only once its commit has.
- Reads go through a second, read-only connection (`reading()`), writes
  through `lock()`.
- `create` writes a mind and its first items in one transaction, and a mind
  that exists with the same origin counts as made.
- A child shares its parent's rows without copying them.
