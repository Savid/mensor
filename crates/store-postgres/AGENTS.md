# mensor-store-postgres

The Postgres `Store` (`crates/mind/src/store.rs`), behind `mensor`'s
`postgres` feature. Its rows are those of `crates/store-sqlite`.

- Each connection belongs to one thread (`src/worker.rs`), which runs what
  it is sent in turn. Nothing here reaches the client from another thread:
  its `Drop` blocks on a runtime of its own, which panics on a runtime's
  thread.
- Only the writer's connection takes the schema's advisory lock, on each
  new connection, before anything is written. It also checks the count in
  `hold` is still the one this store set: if not, another host held the
  schema in between, and the store refuses every write from then on.
- The writer's session commits synchronously and lets the server notice a
  dead host within a minute (`WRITER`); the client gives up on a quiet
  server as fast (`src/connect.rs`).
- Appends commit in groups, and one returns only once its commit has. Work
  that found its connection lost runs again only if it may run twice: a
  read, a `create`, or a batch that had not asked to commit.
- An item is stored as the JSON serde wrote, in a `json` column, and a key
  as its UTF-8, in `bytea`: `jsonb` and `text` refuse a NUL. A database not
  in UTF8 is refused.
- The URL takes libpq's options. This crate reads `sslmode`, `sslrootcert`
  and `tcp_user_timeout` itself, as libpq means them: TLS 1.3, checked as
  `verify-full`, unless `disable` or a Unix socket. No error quotes the URL,
  which holds the password.
- A mind's row holds its record in a `json` column, and its settings once
  in `settings` by digest, as the SQLite store's does. A schema without
  those is refused.
- Tests that need Postgres are `#[ignore]`d, and `make test-postgres` runs
  them against Postgres in Docker, a database each.
