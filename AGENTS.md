# mensor

An agent as a mind with hands. A mind holds the history, calls the model and
drives up to four hands, which run commands and change files. The mind has
tools of its own: `fork`, `wait`, `tell`, `clone_hand` and `release_hand`.
`mensor serve` hosts minds, `mensor hand` runs a hand that dials them, and
`mensor run` does both in one process. `crates/cloudflare` runs a mind and
its one hand as a Durable Object. The protocol is `docs/protocol.md`.

## Commands

`make help` lists every target.

```bash
make check    # fmt-check, lint, test and deny: a change is done when this exits 0
make fmt      # rustfmt
make lint     # clippy on every target, pedantic, and the docs: warnings refused
make test     # every test, the end-to-end ones included
make deny     # cargo-deny: advisories, licences, duplicate versions, sources
make tools    # installs the pinned cargo-deny and worker-build
make build    # target/release/mensor, mensor-mind and mensor-hand
make test-cloudflare    # builds the worker and runs it on workerd under wrangler (needs Node)
```

`make test-cloudflare` is not part of `make check`: run it after changing
`crates/cloudflare` or any pure crate, all of which the worker builds on.
While iterating, test the crate you touched: `cargo test -p mensor-mind`.

## Layout

- `crates/protocol`, `mind`, `hand`, `model`, `api` and `object` are pure:
  no I/O, no clock, no randomness, no Tokio, and they build for
  `wasm32-unknown-unknown`. A decision goes there, as a function of what it
  is given, and is tested there.
- `crates/mind-host` and `crates/hand-host` own the tasks, sockets and
  files, and perform what the pure crates decide, in the order given. A
  mind's host stores `Output::Append` and makes `Output::Fork` before the
  outputs after them: never send what is not stored, and never store a
  fork before its children exist. Store calls go through `blocking`
  (`mind-host/src/task.rs`), never on a runtime thread.
- `crates/mind/src/tools.rs` is what the model is shown and what each tool
  does. A tool use for a hand that is away waits unissued. The mind's own
  tools run after their step's hand tools, one at a time, and a fork runs
  alone among them. Child names and fork budgets are decided in the pure
  mind (`fork.rs`).
- Minds talk only to their parent and children. `Output::Tell` is a prompt
  under a key made from the tool use, so a message sent again is the same
  prompt. The host delivers it on a task of its own: a mind's task never
  awaits another mind's. `Output::Watch` lives in the host's memory only:
  a mind watches again when it loads (`Mind::start`), and `Host::open` wakes
  every mind with work that needs no hand. A resting mind's status comes
  from `Store::recent`, which includes the items that name its hands and kin.
- A transport is two halves: a hand dials through a `Dial`, which returns a
  `Link`; a host takes it through `Host::connect`, which returns a
  `Connection`. The WebSocket joins them over a network, and
  `crates/mensor/src/local.rs` in memory.
- A capability implements `mensor_hand::Capability`, names its operations
  and bounds its own output. `clone` (`hand-host/src/capabilities/clone.rs`)
  is never shown to the model. It makes every copy or none, under a `.part`
  name until the copy is whole. `Hands` (`mensor/src/hand.rs`) runs a hand
  and all its clones as tasks of one process; the copies under `--clones`
  are the only record of the clones, and one is deleted only when its mind
  releases its clone.
- `crates/model` builds each API's requests and reads its answers
  (`messages.rs`, `chat.rs`), retries included. Hosts only send. A request
  is written from structs that borrow the conversation, never through a
  `serde_json::Value`. Every request streams, and `Reading` turns the
  events back into one answer.
- `crates/api` holds the HTTP bodies both hosts serve: neither declares a
  shape of its own.
- What a mind tells its followers goes down one ordered channel each, so a
  reply's text always comes before the step that holds it.
- `crates/mensor` is three binaries over one library: `host.rs` is all a
  host needs and `hand.rs` all a hand needs. Nothing in `hand.rs` may name
  `mensor-mind-host`, `mensor-model` or `mensor-store-sqlite`.
- `crates/store-sqlite` implements the `Store` trait in
  `crates/mind/src/store.rs`. Appends commit in groups, and one returns only
  once its commit has. Reads go through a second, read-only connection
  (`reading()`), writes through `lock()`. `create` writes a mind and its
  first items in one transaction, and a mind that exists with the same
  origin counts as made.
- `crates/object` is a mind and its hand `local` over a `Storage`, and
  `crates/cloudflare` its only host. Put what does not need the platform in
  `crates/object` or `crates/cloudflare/src/api.rs`, where native tests
  reach it.
- A protocol change updates `docs/protocol.md`, `crates/protocol` and both
  hosts.

## Size and speed

- A mind holds only the end of its history: everything after its last
  summary, which carries what it needs from before (`from`, `hands`,
  `lastCall`, `forksLeft`, `parent`, `children`). What reaches further back
  asks the store, a page at a time. Nothing holds a whole history in memory.
- What a mind looks up (a call, an outcome, the last step) is in the round
  in progress: no lookup scans the history.
- A child shares its parent's rows in the store without copying them.
- A mind is in memory only while it is awake. The way to reach its task is
  cloned only under the lock on the host's map of minds, and dropped once
  used: a task that rests relies on being the only holder.
- The release profile is built for size. Before adding a dependency, compare
  `target/release/mensor*` with and without it. TLS uses ring: a host, and
  any test that makes a client, installs it first.

## Dependencies and CI

- `deny.toml` is what `make deny` holds dependencies to. A new licence must
  be permissive. A second version of a crate fails until it is fixed or its
  older version joins `skip` with the reason.
- `.github/workflows/ci.yml` runs `make check`, `make test-cloudflare`, and
  actionlint and zizmor. Actions are pinned by commit, tools by version.
- MCP is not wanted here, in any form. A capability is a Rust trait.

## Rust

- No `unsafe`, except glibc's `malloc_trim` in `crates/mensor/src/host.rs`.
  Add no other without asking.
- Every public item has a doc comment.
- Fix a lint rather than suppress it. A suppression is
  `#[expect(clippy::<lint>, reason = "why")]` on one item.
- Libraries return `thiserror` enums. Only a binary turns an error into
  text for a person.
- Log with `tracing`, with fields.

## Tests

- Test behaviour through the public API, and name a test for what it shows:
  `a_hand_that_restarts_makes_only_its_own_calls_unknown`.
- Wait in an end-to-end test with `eventually`, never a bare sleep.
- Tests that run commands use a `tempfile` directory.
- A workerd test starts the worker with `--var` values made for it and an
  empty environment.

## Docs

- Diagrams are SVGs in `docs/images`, written by hand: colours are CSS
  variables, redefined under `prefers-color-scheme: dark`, background
  included. No mermaid. A change to what a diagram shows updates it.

## Secrets

- Never print, log or commit a model key or the host's token. Check a
  variable with `[ -n "$VAR" ]`.
- The host's token comes from `MENSOR_TOKEN`, never a flag.
- No credential goes into a history or a command's environment. A new
  credential variable joins `CREDENTIALS` in `crates/mensor/src/lib.rs`.
- `CLOUDFLARE_INCLUDE_PROCESS_ENV=true` hands a local worker every variable
  of the shell that starts it: start it with `env -i`, and never write a
  credential to `.dev.vars`.
