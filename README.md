# mensor

mensor runs AI agents as a **mind** with **hands**. The mind holds the
history and calls the model. A hand does the work: it connects to a mind,
says what it can do, and runs what the mind asks. A hand can run on a
machine or a VM, inside the host's own process, or in a Durable Object. A
mind can drive several hands at once, and the model names the hand for each
call.

![You prompt a mind over HTTP. The mind holds the history and calls the model. Its hand tools are whatever its hands say they can do, and run on those hands, which dial in from where they run. Of its own tools, tell and wait reach its parent or children and need no hand, fork and clone_hand have a hand clone itself, and release_hand lets one go.](docs/images/overview.svg)

## Quick start

Needs Rust 1.97 (pinned in `rust-toolchain.toml`) and a model credential.

One process that hosts the minds and runs one hand, `local`, for the mind
`main`:

```bash
export ANTHROPIC_API_KEY="$(cat path/to/anthropic.key)"
mkdir -p build /tmp/demo
cargo run -p mensor -- run --dir /tmp/demo --store build/mensor.sqlite
```

Its commands run on this machine as you, next to the process that holds the
model's credential. To keep them apart, run the host and each hand on their
own. The first hand to name a mind starts it:

```bash
mkdir -p /tmp/a1 /tmp/b2
cargo run -p mensor -- serve --store build/mensor.sqlite
cargo run -p mensor -- hand --mind m --hand a1 --dir /tmp/a1   # another shell
cargo run -p mensor -- hand --mind m --hand b2 --dir /tmp/b2   # and another
```

Then talk to it:

```bash
api=127.0.0.1:7878/v1/minds
curl -s -X POST $api/m/prompt -H 'content-type: application/json' \
  -d '{"text":"Write a haiku in a1 and a limerick in b2."}'
curl -sN $api/m/events   # the history as it grows, and each reply as it is written
curl -s -X POST $api/m/prompt -H 'content-type: application/json' \
  -d '{"text":"Fork from a1: one child translates the haiku into French, one into German. Wait for both and pick one."}'
curl -s $api             # m and its children
```

## Hands

A hand belongs to one mind and has a name in it. When it connects, it
declares its capabilities, and each of their operations, but `clone`,
becomes a tool the model can call. The mind drives every hand the same way:
only what it can do, where it runs and how it reaches the mind differ.

![A hand runs wherever its work is: on its own machine, dialling the host over a WebSocket; inside mensor run, in memory; or in a Durable Object with its mind. Each says what it can do, and the messages are the same in every place.](docs/images/hands.svg)

| Hand | Runs in | Reaches its mind | Can do | Started by |
| --- | --- | --- | --- | --- |
| `mensor hand` | its own process, on any machine | a WebSocket | `commands`, `files`, `clone` | you |
| `local` in `mensor run` | the host's process | memory | `commands`, `files`, `clone` | `mensor run` |
| a clone | its original's process | as its original does | what its original can | its original, for `fork` or `clone_hand` |
| `local` on Cloudflare | the mind's Durable Object | memory | `files`, `fetch` | the object |

### Capabilities

| Capability | Operations | Does |
| --- | --- | --- |
| `commands` | `bash` | Runs a command with a timeout. Cancelling it kills everything it started. |
| `files` | `read`, `write`, `edit`, `list` | Reads and changes files. A relative path starts at the hand's directory. |
| `fetch` | `fetch` | Makes an HTTP request, and returns its status and the start of its body. Cloudflare's hand only. |
| `clone` | `clone` | Copies the hand's directory. Used by `fork` and `clone_hand`; never shown as a tool. |

`--capabilities` picks what the hand of `mensor hand` or `mensor run` can do
(`commands,files,clone` by default). A capability is a Rust trait, `mensor_hand::Capability`:
implement it, and run it in a hand built on `mensor-hand-host`, to give a
hand something new, or to back the same names with something else. Any
program that speaks [the protocol](docs/protocol.md) can be a hand too, in
any language.

Commands start without the host's token or the model's credentials in their
environment. A hand is not a sandbox: a command can reach whatever the
hand's process can, so confining it is up to whoever runs it.

### How calls run

- Calls to different hands run at once. Calls to one hand run in order.
- Every hand tool takes a `hand` argument, which may be left out while the
  mind has one hand.
- The mind asks its model only while a hand is connected, or once it has let
  every hand go.

### When something fails

![When something fails. A connection drops: the hand holds the result, and the mind gets it when the hand reconnects. A hand restarts mid-call: the call's outcome is unknown, and the model is told to check. A hand is away: the call waits, and is sent when the hand is back. A hand dies: release it, its call is unknown, and the turn goes on. An interrupt: a running call is cancelled, and one not yet sent does not run. The host stops mid-fork: the children are made first, so the fork completes once.](docs/images/failures.svg)

A call is stored before it is sent, and one that may have run is never sent
again: no call runs twice. A dead hand is let go with `POST .../release`, and
refused from then on.

## The mind's own tools

These act on the mind itself, its hands and its family, so no hand declares
them:

| Tool | Does |
| --- | --- |
| `fork` | Clones a hand once per prompt. Each clone starts a child mind that shares the history so far and works on its own prompt. |
| `wait` | Waits until children end their turns, and returns what each said last. |
| `tell` | Sends a message to the mind's parent or one of its children. |
| `clone_hand` | Clones a hand into the same mind, as another hand. |
| `release_hand` | Lets a hand go. |

`fork`, `clone_hand` and `release_hand` are offered while a hand can clone,
`tell` while the mind has a parent or children, and `wait` while it has
children.

## Forks and clones

![fork branches the history into a child mind per prompt, each with a hand on a copy of the directory. clone_hand gives the same mind a second hand on a copy, until release_hand lets it go and deletes the copy.](docs/images/fork.svg)

- The mind's own tools run after the hand tools of their step, one at a
  time. A fork runs alone: no other tool of the mind's runs in its step.
- `fork` and `clone_hand` use a hand's `clone` capability, which copies the
  hand's directory and starts a hand on each copy. One fork takes 1 to 8
  prompts.
- A child is made before the fork is recorded, so a host that stops in
  between completes the fork once when it is back.
- A child is named `<root>.<code>`, such as `m.d3342iuj`. Only a fork makes
  a name with a `.`.
- `--max-forks` (64) caps how many minds a root and its descendants may
  fork. Each fork splits what remains between the parent and its children.
- `--max-clones` (4) caps the clones a mind has made with `clone_hand` and
  not released. Hands that join by themselves, and the hand a child was
  forked onto, are not counted.
- Copies are kept in `--clones` (`<dir>.clones` by default) as
  `<mind>/<hand>.<code>`. A clone runs in its original's process and stops
  with it; starting the process again starts a hand on every copy of its
  mind and that mind's descendants. A copy is deleted when its mind releases
  its clone, and kept otherwise. The original directory is never deleted.
  Run one process per directory and clones directory.

## Parents and children

![A parent forks two children, waits at rest while they work, hears what each said, tells one to add a title, and waits for that one alone.](docs/images/talk.svg)

- `tell` reaches the other mind as a prompt marked "(From your parent m:)" or
  "(From your child m.d3342iuj:)". A message sent again after a restart is
  stored once.
- `wait` returns what each child said last, or that it was stopped or its
  model failed. Only a parent waits, so two minds never wait on each other.
  An interrupt ends a wait.
- A waiting parent needs no hand. It may rest, or its host may restart: it
  watches its children again when it wakes.

## API

| Request | Does |
| --- | --- |
| `GET /v1/minds` | Lists every mind. |
| `GET /v1/minds/{m}` | `{name, origin, state, hands, waiting, watching, length}`: what it is doing, its connected hands, and the hands and children its turn waits for. |
| `POST /v1/minds/{m}/prompt` | Stores `{"text", "key"?}`. 202 if new, 200 if the key is already stored, 409 if the key holds other text. |
| `POST /v1/minds/{m}/interrupt` | Stops the turn. |
| `POST /v1/minds/{m}/release` | Lets a hand go: `{"hand": "a1"}`. |
| `GET /v1/minds/{m}/history?after=` | Up to 512 items from index `after`, and the next cursor. |
| `GET /v1/minds/{m}/events?after=` | The same as server-sent events, live, with `writing` events that carry each reply as the model writes it. An event's id is its item's index: resume with `after` set to the last id plus one. |
| `GET /v1/attach` | The WebSocket a hand dials. See [docs/protocol.md](docs/protocol.md). |

With `MENSOR_TOKEN` set, every request needs `Authorization: Bearer <token>`,
and a hand sends the one in its own `MENSOR_TOKEN`. Without it, the host
refuses to listen anywhere but loopback.

`--listen` sets where the host listens (`127.0.0.1:7878`), and a hand's
`--url` where it dials. The host speaks plain HTTP: off loopback, put TLS in
front of it and have hands dial `wss://`.

## Models

| Credential | API | Address |
| --- | --- | --- |
| `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` | Claude's Messages API | `ANTHROPIC_BASE_URL`, or Anthropic's |
| `OPENAI_API_KEY` | Chat Completions (OpenAI, OpenRouter and most others) | `OPENAI_BASE_URL`, or OpenAI's |

With both set it is the Messages API, unless `MENSOR_API=chat`. `--model`
picks the model (`claude-opus-5-5` by default, so name one for Chat
Completions), and `--effort` how hard it works. A request Claude declines
may be answered by a fallback model, unless `--no-fallbacks` is set.

- Replies stream. `/events` carries each one as `writing` events before its
  step is stored.
- At `--compact-at` tokens (150,000; 0 turns it off) the model sums the
  conversation up, and the mind carries on from the summary. The store keeps
  the whole history.
- A hand cuts `bash` output to 64 KB a stream, and a `read` to 256 KB. The
  model is then shown at most 96 KB of a step's results, shared between its
  tools, and a longer result loses its middle. The history keeps what the
  hand returned.

## Storage and memory

- The store is one SQLite file (`--store`), for one host at a time. Without
  it, histories live in memory and end with the process. A child shares its
  parent's rows instead of copying them.
- A mind is loaded when something concerns it, and unloaded a minute after
  it has no hand, no follower and no model call. A host that starts wakes
  only the minds with work that needs no hand.

## Binaries

`make build` makes three, so that a machine carries only the half it runs:

| Binary | Runs | Size |
| --- | --- | --- |
| `mensor` | `serve`, `hand` and `run` | 5.3 MB |
| `mensor-mind` | what `serve` does, with its flags | 4.8 MB |
| `mensor-hand` | what `hand` does, with its flags | 2.5 MB |

Sizes are for x86-64 Linux, stripped. Each logs to standard error at
`info`, or at the level `MENSOR_LOG` names.

## Cloudflare

A mind can also run as one Durable Object that holds its history, one hand
(`local`) and the hand's files, behind the same API. It cannot fork. See
[docs/cloudflare.md](docs/cloudflare.md).

## Layout

![What each runtime is built from: the hosts on top, and under them the pure crates, the same ones wherever they are needed.](docs/images/crates.svg)

| Crate | Holds |
| --- | --- |
| `mensor-protocol` | The messages between a mind and a hand, and how a connection is settled. Pure. |
| `mensor-mind` | The history, the turn loop, the tools and forks, as a state machine, and the store trait. Pure. |
| `mensor-hand` | A hand's memory, the capability trait, and `files` over any byte store. Pure. |
| `mensor-model` | Requests and answers for the Messages API and Chat Completions, with retries. Pure. |
| `mensor-api` | The HTTP API's bodies, which both hosts serve. Pure. |
| `mensor-object` | A mind and its one hand as one object over a storage trait. Pure. |
| `mensor-mind-host` | The server: a task per mind, the attach endpoint, the API, the model client. |
| `mensor-hand-host` | A hand on a machine: the driver, the WebSocket dial, `commands`, `files` on disk, `clone`. |
| `mensor-cloudflare` | The Cloudflare worker. |
| `mensor-store-sqlite` | The SQLite store. |
| `mensor` | The three binaries, the in-memory dial, and the end-to-end tests. |

The pure crates do no I/O and read no clock, so their tests drive them
directly, and the Cloudflare worker reuses them unchanged.

## Not built yet

- A `wait` that times out.
- The Responses API.
- Deleting a mind.
- Clones made outside the hand, such as VM snapshots.
- Hands a host gives every mind, such as one that searches the web.
- `fetch` for `mensor hand`.
- On Cloudflare: forks, remote hands, and listing minds.

MCP is not planned.

## Docs

- [docs/protocol.md](docs/protocol.md): what a mind and its hands say to each other.
- [docs/cloudflare.md](docs/cloudflare.md): a mind as one Durable Object.
- [AGENTS.md](AGENTS.md): commands, layout and rules for working here.

## License

[MIT](LICENSE)
