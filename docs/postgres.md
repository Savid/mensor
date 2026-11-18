# Postgres

A host keeps its histories in a SQLite file (`--store`), a Postgres
database (`MENSOR_POSTGRES`), or memory. Both set is an error.

Postgres needs `mensor` or `mensor-mind` built with the `postgres` feature
(0.9 MB more), as release binaries and the image are. The URL holds the
password, so it comes only from `MENSOR_POSTGRES`, never a flag:

```bash
cargo build --release --locked -p mensor --features postgres
export MENSOR_POSTGRES="postgres://mensor:$(cat path/to/db.password)@db.example/mensor"
target/release/mensor serve
```

[examples/docker](../examples/docker) runs one in Compose.

## The database

- Reach Postgres itself, or a pooler in session mode: transaction mode
  breaks the lock below.
- It must be UTF8. On its first start the host creates its tables (`minds`,
  `items`, `hold`) in the role's current schema, so the role needs to
  create tables there. The schema must hold no others: to share a database,
  `CREATE SCHEMA mensor` and add `?options=-c%20search_path%3Dmensor` to the
  URL.
- Items are JSON, so SQL reads them:
  `SELECT item ->> 'text' FROM items WHERE kind = 'prompt'`.

## TLS

`sslmode=disable` is plain, and so is a Unix socket. Any other `sslmode`,
`prefer` included, is TLS 1.3 checked as libpq's `verify-full`: the
certificate against the system's roots, or `sslrootcert`'s, and its name
against the host's. `channel_binding=require` is refused: the checked
certificate already proves the server.

## One host at a time

The host holds its schema with an advisory lock, so a second is refused.

- A host whose machine vanishes keeps the lock for about a minute, until
  the server drops its connection.
- A connection gone quiet is dropped within about a minute and made again.
  A write lost before its commit is retried on the new connection; one lost
  during its commit may have been stored, so it fails instead.
- A reconnecting host takes the lock before it writes. If another took the
  schema meanwhile, it writes nothing more and must be restarted.
