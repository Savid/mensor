# mensor

An agent as a mind with hands: a mind holds the history and calls the model,
and drives up to four hands, which do what it asks. The mind has tools of its
own: `fork` has a hand clone itself into child minds, `clone_hand` into the
same mind, and `release_hand` lets a hand go. `mensor serve` hosts the minds;
`mensor hand` runs a hand that dials them; `mensor run` does both in one
process. `crates/cloudflare` is a mind and its one hand as one Durable
Object. What a mind and a hand say to each other: `docs/protocol.md`.

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
make test-cloudflare    # builds the worker, then runs it on workerd under wrangler
```

`make check` needs cargo-deny, and `make test-cloudflare` needs Node and
worker-build: `make tools` installs both cargo tools. `make test-cloudflare`
is not part of `make check`. Run it after a change to `crates/cloudflare`,
`crates/object` or `crates/model`.

While iterating, test the crate you touched: `cargo test -p mensor-mind`.

## Layout

- `crates/protocol`, `crates/mind`, `crates/hand`, `crates/model`,
  `crates/api` and `crates/object` are pure: no I/O, no clock, no randomness, no Tokio, and
  they build for `wasm32-unknown-unknown`. A decision belongs there, as a
  function of what it is given. Their tests drive them directly.
- `crates/mind-host` and `crates/hand-host` are the hosts. They own the
  tasks, sockets, processes and files, and perform what the pure crates
  decide, in the order given.
- `crates/object` is a mind and its one hand, `local`, in one place: a
  mind and a hand's core over a `Storage`, driven by `step`. `crates/cloudflare` is its only host:
  the Durable Object, its SQL, the model call over `fetch`, and the alarm
  that does the work. What can be decided without the platform goes in
  `crates/object` or in `crates/cloudflare/src/api.rs`, where a native test
  reaches it; `tests/workerd.rs` covers what only the runtime shows.
- `crates/api` holds the bodies of the HTTP API's requests and answers.
  Both hosts serve them, so a change to one is a change to both: neither
  host declares a shape of its own.
- A mind's host calls its store through `blocking` (`mind-host/src/task.rs`),
  never on a runtime thread: an append waits on a disk.
- `crates/model` decides what a model request holds and what an answer
  means, retries included, for each API: `messages.rs` and `chat.rs`. A
  host only sends: `mind-host` with reqwest, the worker with `fetch`. A
  request is written from structs that borrow the conversation, never
  through a `serde_json::Value` of it. Every request is streamed: a host
  hands the bytes to a `Reading`, which puts the events back together as
  the body an unstreamed answer has, so each API has one reader of replies.
- What a mind tells those following it (its history grew, its model wrote
  this) goes down one ordered channel per follower, so the text of a reply
  is always told before the step that holds it. Keep it one channel.
- `crates/mensor` is three binaries over one library: `host.rs` is all a
  mind's host needs and `hand.rs` all a hand needs, so `mensor-mind` and
  `mensor-hand` each link one half. Nothing in `hand.rs` may name
  `mensor-mind-host`, `mensor-model` or `mensor-store-sqlite`.
- `crates/store-sqlite` is one `Store`. A new store implements the trait in
  `crates/mind/src/store.rs`. Its appends commit in groups: one thread
  commits everything that waits, and an append returns only once the
  commit that holds it has. Keep both. A file has a second connection
  that only reads, so a read never waits for the disk: read through
  `reading()`, write through `lock()`. A file that holds other tables than
  its own is refused. `create` writes a mind with its first items in one
  transaction, and a mind that exists with the same origin counts as made:
  a fork performed twice, after a stop, makes its children once.
- `crates/mensor` is the binary. Its `tests/e2e.rs` joins a real host and
  real hands, over loopback and in memory, with a scripted model.
- A transport is two halves. A hand reaches its mind through a
  `mensor_hand_host::Dial`, which returns a `Link`. A mind's host takes a
  hand through `Host::connect`, which returns a `Connection`. The WebSocket
  joins the two halves over a network; `crates/mensor/src/local.rs` joins
  them in memory.
- A mind's host performs `Output::Append` and `Output::Fork` before any
  output after them. Never send what is not stored, and never store a fork
  before its children are made.
- A capability implements `mensor_hand::Capability`. It names itself and its
  operations, and bounds its own output. `files` is one capability over a
  `FileSystem`: a disk in `hand-host`, rows in the worker. `clone`
  (`hand-host/src/capabilities/clone.rs`) is never shown to the model: the
  mind's own tools call it. It makes every copy or none, and starts each
  through a `Spawn`: `Hands` (`mensor/src/hand.rs`) runs a hand and every
  clone of it as tasks of one process, over a WebSocket or in memory, and
  starts a clone again from its copy. The copies under `--clones` are the
  only record of the clones; a copy is removed only when its mind lets its
  clone go, and removing one copes with directories no one may write to.
- What the model is shown, and what each tool does, is `mind/src/tools.rs`.
  A tool use for a hand that is not connected waits unissued: a call is
  stored only when it can be sent. The mind's own tools run after the hand
  tools of their step, one at a time, and a fork runs alone among them. A
  fork's children are named and budgeted in the pure mind
  (`mind/src/fork.rs`), so nothing about a fork depends on chance or on the
  host.
- Minds talk only to their parent and their children. `Output::Tell` is a
  prompt under a key made from the tool use, so one said again after a stop
  is the same prompt; the host delivers it on a task of its own, never
  awaiting another mind from inside a mind's task. `Output::Watch` is kept
  in the host's memory alone: a mind that waits watches again when it wakes,
  and `Host::open` wakes every mind with work that needs no hand. What a
  resting mind is doing is told from `Store::recent`, which carries the
  items that say who its hands and kin are (`Store::known`).
- A protocol change updates `docs/protocol.md`, `crates/protocol` and both
  hosts.

## Dependencies and CI

- `deny.toml` is what `make deny` holds every dependency to. A new licence
  joins `allow` only if it is permissive. A second version of a crate fails
  until the dependency that forces it is fixed or the older version joins
  `skip`, with the reason.
- `.github/workflows/ci.yml` runs `make check`, `make test-cloudflare`, and
  actionlint and zizmor over `.github`. An action is pinned by commit, with
  its tag in a comment. A tool is pinned by version: the cargo tools in the
  Makefile's `tools` target, Node in `.node-version`, the rest in the
  workflow, each download with its checksum.

## Size and speed

- The release profile is built for size, and `.cargo/config.toml` leaves
  out the parts of SQLite the store never uses. TLS uses ring: a host
  installs it before it makes a client, and so does a test that makes one.
- A mind reads its history from the end: what it looks up (a call, an
  outcome, the last step) is in the round in progress, so no lookup scans
  the whole history. Keep it so.
- A mind holds only the end of its history. A summary carries what a mind
  needs from before it (`from`, `hands`, `lastCall`, `forksLeft`, `parent`,
  `children`), the mind sheds the
  rest when it stores one, and a host loads a store's `tail`. Whatever
  reaches further back asks the store, a page at a time: a prompt's key,
  `/history` and `/events`. A child shares its parent's items in the store
  without a copy, so making one reads none of them. Nothing holds a whole history in
  memory, whatever it is asked. A summary carries every hand the history
  recorded, released ones included, the forks left, and the mind's kin.
- A host holds a mind in memory only while it is awake (`mind-host/src/
  task.rs`). The way to reach a mind's task is cloned only under the lock
  on the host's map of minds, and dropped once used: a task that rests
  relies on being the only holder.
- Before adding a dependency, build with and without it and compare
  `target/release/mensor*`. `make deny` refuses a second version of a crate.
- MCP is not wanted here, in any form. A capability is a Rust trait.

## Rust

- `unsafe` is refused, with one exception that says why: the declaration
  of glibc's `malloc_trim` in `crates/mensor/src/host.rs`, by which a host
  gives freed memory back to the system. Add no other without asking.
  Every public item has a doc comment.
- Fix a lint finding rather than suppressing it. A suppression is
  `#[expect(clippy::<lint>, reason = "why")]` on one item.
- Model a lifecycle as one enum whose variants carry what that phase has.
- Errors are `thiserror` enums in libraries. Only the binary turns one into
  text for a person.
- Log with `tracing`, with fields, not formatted strings. `MENSOR_LOG`
  sets the level a binary logs at.

## Tests

- Test behaviour through the public API, and name a test for what it shows:
  `a_hand_that_restarts_makes_only_its_own_calls_unknown`.
- Timing in a test goes through `eventually` in `tests/e2e.rs`, never a bare
  sleep that a slow machine would fail.
- Tests that run commands use a `tempfile` directory.
- A workerd test starts the worker with `--var` values made for the test
  and an empty environment, so nothing of this machine reaches it.

## Secrets

- Never print, log or commit a model key or the host's token. Check a
  variable with `[ -n "$VAR" ]`.
- The host's token comes from `MENSOR_TOKEN`, never from a flag.
- No credential goes into a history or the environment of a command the
  model runs. A new credential variable joins `CREDENTIALS` in
  `crates/mensor/src/lib.rs`.
- `CLOUDFLARE_INCLUDE_PROCESS_ENV=true` hands a local worker every variable
  of the shell that starts it. Start it from a shell that holds only what it
  needs (`env -i`), and never write a credential to `.dev.vars`.
