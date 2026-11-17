# mensor

The three binaries over one library, the in-memory dial, and the end-to-end
tests.

- `src/host.rs` is all a host needs and `src/hand.rs` all a hand needs.
  Nothing in `hand.rs` may name `mensor-mind-host`, `mensor-model` or a
  store's crate.
- A host keeps its histories in one store (`store()` in `src/host.rs`):
  `--store`, `MENSOR_POSTGRES`, or memory, and both set is an error.
  Postgres is behind the `postgres` feature, off by default, so that the
  binaries carry no Postgres client unless asked.
- `Hands` (`src/hand.rs`) runs a hand and all its clones as tasks of one
  process. The copies under `--clones` are the process's only record of its
  clones, and one is deleted only when its mind releases its clone.
- `src/local.rs` joins a hand to a host in memory, as the WebSocket does
  over a network.
