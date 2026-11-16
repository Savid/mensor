# mensor

mensor runs AI agents as a **mind** with **hands**. The mind holds the
history and calls the model. A hand runs commands and edits files on a
machine. One mind drives up to four hands at once, and the model names the
hand for each call.

![You prompt a mind over HTTP. The mind holds the history, calls the model and stores every item; hands on other machines dial in, take calls and send back results.](docs/images/overview.svg)

The mind also has tools of its own:

| Tool | Does |
| --- | --- |
| `fork` | Clones a hand once per prompt. Each clone starts a child mind that shares the history so far and works on its own prompt. |
| `wait` | Waits until children end their turns, and returns what each said last. |
| `tell` | Sends a message to the mind's parent or one of its children. |
| `clone_hand` | Clones a hand into the same mind, as another hand. |
| `release_hand` | Lets a hand go. |

## Quick start

Needs Rust 1.97 (pinned in `rust-toolchain.toml`) and a model credential.

One process that hosts the minds and runs one hand, `local`, for the mind
`main`:

```bash
export ANTHROPIC_API_KEY="$(cat path/to/anthropic.key)"
mkdir -p /tmp/demo
cargo run -p mensor -- run --dir /tmp/demo --store build/mensor.sqlite
```

Its commands run on this machine as you, next to the process that holds the
model's credential. To keep them apart, run the host and each hand on their
own. The first hand to name a mind starts it:

```bash
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

## API

| Request | Does |
| --- | --- |
| `GET /v1/minds` | Lists every mind. |
| `GET /v1/minds/{m}` | `{name, origin, state, hands, waiting, watching, length}`: what it is doing, its connected hands, and the hands and children its turn waits for. |
| `POST /v1/minds/{m}/prompt` | Stores `{"text", "key"?}`. 202 if new, 200 if the key is already stored, 409 if the key holds other text. |
| `POST /v1/minds/{m}/interrupt` | Stops the turn. |
| `POST /v1/minds/{m}/release` | Lets a hand go: `{"hand": "a1"}`. |
| `GET /v1/minds/{m}/history?after=` | Up to 512 items from a cursor, and the next cursor. |
| `GET /v1/minds/{m}/events?after=` | The same as server-sent events, live, with `writing` events that carry each reply as the model writes it. |
| `GET /v1/attach` | The WebSocket a hand dials. See [docs/protocol.md](docs/protocol.md). |

With `MENSOR_TOKEN` set, every request needs `Authorization: Bearer <token>`.
Without it, the host refuses to listen anywhere but loopback.

## Hands

- Calls to different hands run at once. Calls to one hand run in order.
- Every hand tool takes a `hand` argument, which may be left out while the
  mind has one hand.
- A call waits, unsent, while its hand is away. An interrupt ends it.
- No call runs twice. A call that may have run without a result, because
  its hand restarted, is recorded as unknown, and the model is told to check.
- A hand that dies mid-call can be let go with `POST .../release`. Its calls
  are recorded as unknown, and the turn goes on.
- A mind takes up to `--max-hands` hands (4).

## Forks and clones

![fork branches the history into a child mind per prompt, each with a hand on a copy of the directory. clone_hand gives the same mind a second hand on a copy, until release_hand lets it go and deletes the copy.](docs/images/fork.svg)

- The mind's own tools run after the hand tools of their step, one at a
  time. A fork runs alone: no other tool of the mind's runs in its step.
- `fork` and `clone_hand` use a hand's `clone` capability, which copies the
  hand's directory and starts a hand on each copy. They are offered only
  while a hand can clone.
- A child is made before the fork is recorded, so a host that stops in
  between completes the fork once when it is back.
- A child is named `<root>.<code>`, such as `m.k3x9q2ab`. Only a fork makes
  a name with a `.`.
- `--max-forks` (64) caps how many minds a root and its descendants may
  fork. Each fork splits what remains between the parent and its children.
- Copies are kept in `--clones` (`<dir>.clones` by default) as
  `<mind>/<hand>.<code>`. A clone runs in its original's process and stops
  with it; starting the process again starts a hand on every copy. A copy
  is deleted when its mind releases its clone, and kept otherwise. The
  original directory is never deleted. Run one process per directory.

## Parents and children

![A parent forks two children, waits at rest while they work, hears what each said, tells one to add a title, and waits for that one alone.](docs/images/talk.svg)

- `tell` reaches the other mind as a prompt marked "(From your parent m)" or
  "(From your child m.k3x9q2ab)". A message sent again after a restart is
  stored once.
- `wait` returns what each child said last, or that it was stopped or its
  model failed. Only a parent waits, so two minds never wait on each other.
  An interrupt ends a wait.
- A waiting parent needs no hand. It may rest, or its host may restart: it
  watches its children again when it wakes.

## Models

| Credential | API | Address |
| --- | --- | --- |
| `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` | Claude's Messages API | `ANTHROPIC_BASE_URL`, or Anthropic's |
| `OPENAI_API_KEY` | Chat Completions (OpenAI, OpenRouter and most others) | `OPENAI_BASE_URL`, or OpenAI's |

With both set it is the Messages API, unless `MENSOR_API=chat`. `--model`
picks the model (`claude-opus-5-5` by default), and `--effort` how hard it
works.

- Replies stream. `/events` carries each one as `writing` events before its
  step is stored.
- At `--compact-at` tokens (150,000; 0 turns it off) the model sums the
  conversation up, and the mind carries on from the summary. The store keeps
  the whole history.
- The model is shown at most 96 KB of a step's results, shared between its
  tools. A longer result loses its middle. The history keeps it whole.

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
| `mensor-mind` | `serve` | 4.8 MB |
| `mensor-hand` | `hand` | 2.5 MB |

Sizes are for x86-64 Linux, stripped. Each logs to standard error at
`info`, or at the level `MENSOR_LOG` names.

## Cloudflare

A mind can also run as one Durable Object that holds its history, one hand
(`local`) and the hand's files, behind the same API. It cannot fork. See
[docs/cloudflare.md](docs/cloudflare.md).

## Capabilities

A hand is a small core plus capabilities, which it declares when it
connects.

| Capability | Operations | Does |
| --- | --- | --- |
| `commands` | `bash` | Runs a command with a timeout. Cancelling it kills everything it started. |
| `files` | `read`, `write`, `edit`, `list` | Reads and changes files under the hand's directory. |
| `clone` | `clone` | Copies the hand's directory. Used by `fork` and `clone_hand`; never shown as a tool. |

`--capabilities` picks a hand's (`commands,files,clone` by default). A
capability is a Rust trait, so another hand can back the same names with
something else.

Commands never see the host's token or the model's credentials. A hand is
not a sandbox: confining it is up to whoever runs it.

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
- More capabilities, such as `fetch` on a machine, or ones the host provides.
- On Cloudflare: forks, remote hands, and listing minds.

MCP is not planned.

## Docs

- [docs/protocol.md](docs/protocol.md): what a mind and its hands say to each other.
- [docs/cloudflare.md](docs/cloudflare.md): a mind as one Durable Object.
- [AGENTS.md](AGENTS.md): commands, layout and rules for working here.

## License

[MIT](LICENSE)
