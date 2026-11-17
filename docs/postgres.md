# Postgres

A host keeps every history in one store: a SQLite file (`--store`), a
Postgres database (`MENSOR_POSTGRES`), or, with neither, memory that ends
with the process. Both set is an error. This page is the Postgres one.

## Run it

`mensor` and `mensor-mind` reach Postgres when built with the `postgres`
feature, which makes each 0.9 MB larger. `MENSOR_POSTGRES` names the
database by a URL, and only it does, never a flag: the URL holds the
password.

```bash
cargo build --release --locked -p mensor --features postgres
export MENSOR_POSTGRES="postgres://mensor:$(cat path/to/db.password)@db.example/mensor"
target/release/mensor serve
```

[examples/docker](../examples/docker) runs a host on Postgres in Compose.

## The database

- The URL must reach Postgres itself, or a pooler in session mode: in
  transaction mode the lock below holds nothing.
- The database is UTF8, or it is refused. The tables go in an empty schema,
  and one that holds other tables is refused. To share a database, give
  mensor a schema of its own: `CREATE SCHEMA mensor`, and
  `?options=-c%20search_path%3Dmensor` in the URL.
- A commit waits for the disk (`synchronous_commit = on`), whatever the
  database's default.
- An item is kept as the JSON it was written as, in a `json` column, so SQL
  reads it: `SELECT item ->> 'text' FROM items WHERE kind = 'prompt'`.

## TLS

`sslmode=disable` is plain. Every other `sslmode`, `prefer` and `require`
included, is TLS 1.3 checked as libpq's `verify-full` checks it: the
server's certificate against the system's roots, or those in the file
`sslrootcert` names (`system` names the system's), and its name against the
host's. A Unix socket is reached plain, as libpq reaches it.
`channel_binding=require` is refused: the checked certificate already
proves the server.

## One host at a time

- The host holds its schema with an advisory lock, so a second host is
  refused while the first holds it.
- A host whose machine vanishes keeps the lock for about a minute, until
  the server gives up on its connection.
- A connection gives up on a server gone quiet within about a minute, and
  is made again. What found it lost runs again, unless it may already have
  been stored.
- A host that reconnects takes the lock again before it writes; if another
  host took the schema meanwhile, it writes nothing more and must start
  again.
