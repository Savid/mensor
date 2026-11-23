# mensor

An AI agent split in two. The **mind** holds the history and calls the
model; a host (`mensor serve`) runs minds and serves their HTTP API. A
**hand** does the work: it connects to its mind, says what it can do, and
runs the calls the mind sends it. One mind drives several hands at once.

- **Keys away from commands.** Run the host apart from its hands, and the
  model's key and the history stay off the machines where commands run.
- **No call runs twice.** Calls are stored before they are sent, and
  survive dropped connections and restarts. One that may have run is
  reported as unknown, never sent again.
- **Forks run in parallel**, each child mind with the history so far and its
  own copy of the workspace.
- **A hand in any language**, over a small JSON protocol.

![You prompt a mind over HTTP. The mind holds the history and calls the model. Its hand tools are whatever its hands say they can do, and run on those hands, which dial in from where they run. Of its own tools, tell and wait reach its parent or children and need no hand, fork and clone_hand have a hand clone itself, and release_hand releases one.](docs/images/overview.svg)

## Quick start

From source, with Rust 1.97 (pinned in `rust-toolchain.toml`), or a
[release](#install)'s `mensor` in place of `cargo run -p mensor --`. `run`
hosts the mind `main` and its one hand, `local`, in one process:

```bash
export ANTHROPIC_API_KEY="$(cat path/to/anthropic.key)"
mkdir -p build /tmp/demo
cargo run -p mensor -- run --dir /tmp/demo --store build/mensor.sqlite
```

```bash
api=127.0.0.1:7878/v1/minds
curl -s -X POST $api/main/prompt -H 'content-type: application/json' \
  -d '{"text":"Write a haiku to haiku.txt. Then fork: one child translates it into French, one into German. Wait for both and pick one."}'
curl -sN $api/main/events   # the history as it grows, and each reply as it streams
curl -s $api                # main and its children
```

`run`'s commands run as you, beside the model's key: use it only on a
machine you trust the model with. A hand is no sandbox; see
[docs/security.md](docs/security.md).

## Split the mind from its hands

Run the host and each hand on their own, here or on other machines, or in
containers as [examples/docker](examples/docker) does. The first hand to
name a mind starts it:

```bash
mkdir -p build /tmp/a1 /tmp/b2
cargo run -p mensor -- serve --store build/mensor.sqlite
cargo run -p mensor -- hand --mind m --hand a1 --dir /tmp/a1   # another shell
cargo run -p mensor -- hand --mind m --hand b2 --dir /tmp/b2   # and another
```

```bash
api=127.0.0.1:7878/v1/minds
curl -s -X POST $api/m/prompt -H 'content-type: application/json' \
  -d '{"text":"Write a haiku in a1 and a limerick in b2."}'
curl -sN $api/m/events
```

## Hands

| Hand | Runs | Reaches its mind by | Can do |
| --- | --- | --- | --- |
| `mensor hand` | in its own process, anywhere | WebSocket | `commands`, `files`, `clone` |
| `local` of `mensor run` | in the host | memory | `commands`, `files`, `clone` |
| `local` on Cloudflare | in the mind's Durable Object | memory | `files`, `fetch` |
| your own program | anywhere | WebSocket, speaking [the protocol](docs/protocol.md) | what it declares |

| Capability | Tools | Does |
| --- | --- | --- |
| `commands` | `bash` | A command: 10 minutes unless `timeoutMs` says, 64 KiB of output a stream. |
| `files` | `read`, `write`, `edit`, `list` | A relative path starts at `--dir`; nothing confines it. `read` returns at most 256 KiB. |
| `fetch` | `fetch` | An HTTP request. Cloudflare only. |
| `clone` | | Copies the hand, for `fork` and `clone_hand`. |

- Every hand tool takes `hand`, optional while the mind has one. Calls to
  different hands run at once; calls to one hand, in order.
- A new capability is a `mensor_hand::Capability`. A hand in another
  language speaks the protocol, as [examples/own-hand](examples/own-hand)
  and [examples/opencode](examples/opencode) do.

![When something fails. A connection drops: the hand holds the result, and the mind gets it when the hand reconnects. A hand restarts mid-call: the call's outcome is unknown, and the model is told to check. A hand is away: the call waits, and is sent when the hand is back. A hand dies: release it, its call is unknown, and the turn goes on. An interrupt: a running call is cancelled, and one not yet sent does not run. The host stops mid-fork: the children are made first, so the fork completes once.](docs/images/failures.svg)

The mind asks its model only while a hand is connected, or once it has
released every hand: release a dead one with `POST /v1/minds/{m}/release`.

## The mind's own tools

| Tool | Does |
| --- | --- |
| `fork` | Takes 1 to 8 prompts. Clones a hand per prompt, and starts a child mind on each copy with the history so far and its prompt. |
| `wait` | Waits up to `seconds` (600, at most 3600) for children to end their turns, and returns what each said or is doing. |
| `tell` | Prompts the parent or a child, marked "(From m:)". |
| `clone_hand` | Clones a hand into the same mind, as another hand. |
| `release_hand` | Releases a hand. A clone's copy is deleted. |

![fork branches the history into a child mind per prompt, each with a hand on a copy of the directory. clone_hand gives the same mind a second hand on a copy, until release_hand releases it and deletes the copy.](docs/images/fork.svg)

![A parent forks two children, waits without a hand while they work, hears what each said, tells one to add a title, and waits for that one alone.](docs/images/talk.svg)

- They are offered while they apply: `fork`, `clone_hand` and
  `release_hand` while a hand can clone, `tell` and `wait` while a hand can
  clone or the mind has a parent or children.
- A child is `<root>.<code>`, such as `m.d3342iuj`, where the root is the
  mind it descends from, and has its parent's settings. A waiting parent
  needs no hand, and survives a host restart.

## Configuration

| `serve` and `run` | Default | |
| --- | --- | --- |
| `--listen` | `127.0.0.1:7878` | Anything else needs `MENSOR_TOKEN`. Plain HTTP: put TLS in front off loopback. |
| `--store` | memory | A SQLite file. `MENSOR_POSTGRES` names a Postgres database instead: [docs/postgres.md](docs/postgres.md). |
| `--model` | `claude-opus-5-5` | The model. A `messages:`, `chat:` or `responses:` prefix names its API; without one, Messages if a Claude key is set, else Chat. `--models a,responses:b` adds others a mind may be made with, on any API. |
| `--effort` | the model's | For `--model` and the models behind its API: `low`, `medium`, `high`, `xhigh` or `max` for Claude. |
| `--system-file` | none | The system prompt. |
| `--max-tokens` | `16000` | Caps one Messages reply, reasoning included: that API needs a cap. Chat and Responses are sent none, so a server's own limit applies. |
| `--compact-at` | `150000` | Tokens before the mind summarises the conversation and carries on. `0` never does. |
| `--no-fallbacks` | off | Turns off the API's fallback model for a request Claude declines. |
| `--mind-tools` | all five | A comma list of the mind's own tools to offer. `''` offers none. |
| `--max-forks` | `64` | Total forks under one root; each fork splits what's left between parent and children. `0` none; `unlimited` leaves it to the hand's `clone`. |
| `--max-clones` | `4` | A mind's `clone_hand` clones not yet released. |

| `hand` and `run` | Default | |
| --- | --- | --- |
| `--url` | `ws://127.0.0.1:7878/v1/attach` | Where the host is. `hand` only. |
| `--mind`, `--hand` | | Its mind and its name in it. `run` uses `--mind main` and the hand `local`. |
| `--dir` | `.` | Where commands run. One process per directory. |
| `--clones` | `<dir>.clones` | Where clones are copied. A process starts a hand on every copy there for its mind, so give each process its own. |
| `--capabilities` | `commands,files,clone` | What it can do. |

| Variable | Default | |
| --- | --- | --- |
| `MENSOR_TOKEN` | none | The host's bearer token, or a hand's. |
| `MENSOR_POSTGRES` | none | A Postgres URL, in place of `--store`. |
| `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` | none | Claude's Messages API. The auth token goes as a plain bearer, for a gateway. `ANTHROPIC_BASE_URL` moves it. |
| `OPENAI_API_KEY` | none | Chat Completions (OpenAI, OpenRouter, most others) and OpenAI's Responses API. `OPENAI_BASE_URL` moves it. |
| `MENSOR_LOG` | `info` | The log level, on standard error. |

The Messages API needs Claude 4.6 or later. The model sees at most 96 KiB
of a step's results, a long one losing its middle; the history keeps them
whole.

## API

| Request | Does |
| --- | --- |
| `GET /v1/minds` | Every mind you may reach that has not ended. |
| `GET /v1/minds/{m}` | `state`, `hands`, `waiting`, `watching`, `length`, `origin` and its settings. |
| `PUT /v1/minds/{m}` | Makes `m` with settings of its own: [below](#making-a-mind). |
| `POST /v1/minds/{m}/prompt` | `{"text", "key"?}`: 202 stored, 409 if `m` ended. A `key` makes retries safe: 200 if stored already, 409 if with other text. |
| `POST /v1/minds/{m}/interrupt` | Stops the turn. |
| `POST /v1/minds/{m}/release` | `{"hand"}`: releases that hand. |
| `POST /v1/minds/{m}/end` | Stops the turn, releases every hand and takes no new prompt. Ending a fork's child deletes its copy of the directory. The history stays. |
| `GET /v1/minds/{m}/history?after=` | Up to 512 items from index `after`, and the next cursor. |
| `GET /v1/minds/{m}/events?after=` | The same as server-sent events, live, plus `writing` events that stream each reply. Resume with `after` as the last id plus one. |
| `GET /v1/attach` | The WebSocket hands dial. |

- `state` is `idle`, `thinking`, `working`, `waiting` (for a hand named in
  `waiting`, or for its first) or `ended`. `origin` is `root`, `fork` (with
  `parent` and `inherited`) or `from` (with `from`).
- With `MENSOR_TOKEN`, send `Authorization: Bearer <token>`; `mensor token
  --mind m` makes one for `m` and its children alone. See
  [docs/security.md](docs/security.md).

### Making a mind

A hand that connects to a root no one made starts it with the host's
settings. `PUT` first to give it its own, fixed from then on:

| Field | |
| --- | --- |
| `model` | `--model` or one of `--models`, without its API prefix. |
| `effort` | For the Messages API `low` to `max`; for the others any the server takes. |
| `system` | The system prompt. |
| `from` | `{"mind", "at", "hand"}`: start from `mind`'s first `at` items, keeping its hand `hand`, such as a VM restored from a snapshot taken then. The other hands stay behind. Its model may be behind another API than that mind's. |

A setting left out is the host's, or with `from`, that mind's, except an
effort when the body names another model. It answers
201 made, 200 made the same way before, 409 made otherwise, 404 no such
`from`, 403 a `from` the token does not reach, and 400 a bad name (a `.`,
or over 55 bytes), model, effort or `from`.

## Install

Each release has static Linux binaries for x86-64 and arm64:

```bash
v=0.1.4 f="mensor-$v-$(uname -m)-unknown-linux-musl.tar.gz"
curl -fLO "https://github.com/Savid/mensor/releases/download/v$v/$f"
curl -fLO "https://github.com/Savid/mensor/releases/download/v$v/SHA256SUMS"
sha256sum -c --ignore-missing SHA256SUMS && tar -xzf "$f" && "${f%.tar.gz}/mensor" --version
```

The image, for both, is `ghcr.io/savid/mensor` (`0.1.4`, `0.1`, `latest`):
[examples/docker](examples/docker) runs a host and hands from it. Binaries
and image are built with Postgres, and attested:
[verify them](docs/releasing.md#verify).

## Building

`make build` makes `mensor`, and its halves `mensor-mind` (`serve`) and
`mensor-hand` (`hand`), so a machine carries only what it runs. The
`postgres` feature adds 0.9 MB to a host.

![What each runtime is built from: the hosts on top, and under them the pure crates, the same ones wherever they are needed. mensor is 5.4 MB, mensor-mind 4.9 MB and mensor-hand 2.5 MB, stripped, for x86-64.](docs/images/crates.svg)

The pure crates do no I/O and read no clock, so their tests drive them
directly and the Cloudflare worker reuses them unchanged.
[AGENTS.md](AGENTS.md) has the commands and rules for working here.

## Docs

- [docs/protocol.md](docs/protocol.md): what a mind and its hands say.
- [docs/security.md](docs/security.md): what commands are kept from, and tokens.
- [docs/postgres.md](docs/postgres.md): a host on Postgres.
- [docs/cloudflare.md](docs/cloudflare.md): a mind as one Durable Object. It cannot fork.
- [docs/releasing.md](docs/releasing.md): verifying and cutting a release.
- [examples/](examples): your own hand, hands in containers, opencode as a hand.

## License

[MIT](LICENSE)
