# mensor

An agent as a mind with hands, in Rust. `mensor serve` hosts minds,
`mensor hand` runs a hand that dials one, and `mensor run` does both in one
process. `crates/cloudflare` runs a mind and its one hand as a Durable
Object. `README.md` says what a mind and a hand are, and `docs/protocol.md`
what they say to each other.

## Commands

`make help` lists every target.

```bash
make check    # fmt-check, lint, test and deny: a change is done when this exits 0
make fmt      # rustfmt
make lint     # clippy on every target, pedantic, and the docs: warnings refused
make test     # every native test, the end-to-end ones included
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
- The hosts (`crates/mind-host`, `hand-host` and `cloudflare`) own the
  tasks, sockets and files, and perform what the pure crates decide, in the
  order given: never send what is not stored, and never store a fork before
  its children exist.
- A capability implements `mensor_hand::Capability`, names its operations
  and bounds its own output.
- `crates/api` holds the HTTP bodies both hosts serve: neither declares a
  shape of its own.
- A protocol change updates `docs/protocol.md`, `crates/protocol` and both
  hosts.
- `crates/mind`, `mind-host`, `hand-host`, `model`, `store-sqlite`, `mensor`
  and `cloudflare` each have an `AGENTS.md` with their own rules. Read it
  before changing that crate.

## Dependencies and CI

- The release profile is built for size. Before adding a dependency, compare
  `target/release/mensor*` with and without it.
- `deny.toml` is what `make deny` holds dependencies to. A new licence must
  be permissive. A second version of a crate fails until it is fixed or its
  older version joins `skip` with the reason.
- `.github/workflows/ci.yml` runs `make check`, `make test-cloudflare`, and
  actionlint and zizmor. Actions are pinned by commit, tools by version.
- MCP is not wanted here, in any form. A capability is a Rust trait.

## Rust

- No `unsafe`, except glibc's `malloc_trim` in `crates/mensor/src/host.rs`.
  Add no other without asking.
- Fix a lint rather than suppress it. A suppression is
  `#[expect(<lint>, reason = "why")]` on one item. The lints allowed
  everywhere are in the root `Cargo.toml`.
- An error a caller tells apart is a `thiserror` enum. Only `crates/mensor`
  and the hosts' HTTP problems word an error for a person.
- Log with `tracing`, with fields. The worker logs with `console_*!`.
- TLS uses ring: a host, and any test that makes a client, installs it first.

## Tests

- Test behaviour through the public API, and name a test for what it shows:
  `a_hand_that_restarts_makes_only_its_own_calls_unknown`. Private code may
  be tested in its own module.
- Wait in a test by polling, as `eventually` does. Sleep only to show that
  something does not happen.
- Tests that run commands use a `tempfile` directory.

## Docs

- A change to a flag, endpoint, default or behaviour that `README.md`
  describes updates the README in the same change.
- Diagrams are plain SVGs in `docs/images`: colours are CSS variables,
  redefined under `prefers-color-scheme: dark`, background included. No
  mermaid. A change to what a diagram shows updates it.

## Secrets

- Never print, log or commit a model key or the host's token. Check a
  variable with `[ -n "$VAR" ]`.
- The host's token comes from `MENSOR_TOKEN`, never a flag.
- No credential goes into a history or a command's environment. A new
  credential variable joins `CREDENTIALS` in `crates/mensor/src/lib.rs`.
- `CLOUDFLARE_INCLUDE_PROCESS_ENV=true` hands a local worker every variable
  of the shell that starts it: start it with `env -i`, and never write a
  credential to `.dev.vars`.
