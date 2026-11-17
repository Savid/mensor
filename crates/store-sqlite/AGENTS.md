# mensor-store-sqlite

The SQLite `Store` (`crates/mind/src/store.rs`).

- Appends commit in groups, and one returns only once its commit has.
- Reads go through a second, read-only connection (`reading()`), writes
  through `lock()`.
- `create` writes a mind and its first items in one transaction, and a mind
  that exists with the same record counts as made.
- A mind's row holds its record as JSON, and beside it the mind whose items
  it shares and how many, which a read walks up. A child, of a fork or made
  from another's history, shares those rows without copying them.
- A file whose minds have no record is an older mensor's, and refused.
- It passes `mensor_mind::store_suite!`. Its own tests are of its file and
  rows.
